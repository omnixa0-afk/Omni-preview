using System.Collections.Generic;
using System.Linq;
using System.Threading;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

internal sealed class ChatRefreshService
{
    internal const int TranscriptPageSize = 80;
    internal const int TranscriptSoftItemLimit = 250;
    internal const int TranscriptHardItemLimit = 400;

    private readonly Dictionary<string, DialogDeltaCursor> dialogDeltaCursors = new(StringComparer.Ordinal);

    private static string BuildMessageRefreshSignature(DialogMessageRefreshSummary summary)
        => string.Join(
            ':',
            summary.DialogKey,
            summary.MessageCount,
            summary.LatestMessageId,
            summary.LastMessageUpdatedAtUnixMs,
            summary.LastAttachmentUpdatedAtUnixMs,
            summary.LastSecurityProjectionUpdatedAtUnixMs);

    public ChatHeaderRefreshPayload? FetchHeader(
        RustBridge rustBridge,
        string? dialogKey,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return null;
        }

        cancellationToken.ThrowIfCancellationRequested();
        ChatHeaderScreenSnapshot snapshot = rustBridge.GetChatHeaderScreenSnapshot(dialogKey);
        ChatHeaderContactSummary headerSummary = snapshot.HeaderSummary;
        string resolvedDialogKey = string.IsNullOrWhiteSpace(headerSummary.DialogKey)
            ? dialogKey
            : headerSummary.DialogKey;

        return new ChatHeaderRefreshPayload(
            resolvedDialogKey,
            headerSummary,
            snapshot.SecuritySummary,
            snapshot.SessionSummary,
            snapshot.CallSummary);
    }

    public ChatThreadRefreshPayload? FetchThreadList(
        RustBridge rustBridge,
        string lastObservedSignature,
        CancellationToken cancellationToken)
    {
        cancellationToken.ThrowIfCancellationRequested();
        ChatThreadListRefreshSummary summary = rustBridge.GetChatThreadListRefreshSummary();
        string signature = string.Join(
            ':',
            summary.ThreadCount,
            summary.LastContactUpdatedAtUnixMs,
            summary.LastDialogLinkUpdatedAtUnixMs,
            summary.LastPublicProfileUpdatedAtUnixMs,
            summary.LastMessageUpdatedAtUnixMs,
            summary.LastAttachmentUpdatedAtUnixMs,
            summary.LastReadMarkUpdatedAtUnixMs);

        // Group activity (messages, system lines) already moves LastMessageUpdatedAtUnixMs — group
        // threads live in the same messages table; an invitation does not, so its id joins in.
        // Presence is not in storage: a link that drops or comes back changes nothing above, and the
        // list kept a green dot for a contact that had been gone for minutes (live, 2026-10-05). So
        // the live link set is part of what we observe — and a minute bucket, because "recently
        // online" fades to offline by the clock alone.
        signature += ":" + LinkedPresenceSignature(rustBridge) + ":" + (DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 60_000);

        IReadOnlyList<GroupInviteView> invites = rustBridge.ListGroupInvites();
        IReadOnlyList<GroupCallNotice> calls = rustBridge.ListGroupCallInbox();
        signature += ":" + ChatGroupRows.Signature(invites, calls);

        if (string.Equals(signature, lastObservedSignature, StringComparison.Ordinal))
        {
            return null;
        }

        cancellationToken.ThrowIfCancellationRequested();
        return new ChatThreadRefreshPayload(
            signature,
            rustBridge.ListChatThreadProjections(),
            new ChatGroupRows(rustBridge.ListGroups(), invites, calls));
    }

    internal static string LinkedPresenceSignature(RustBridge rustBridge)
    {
        try
        {
            return string.Join(',', rustBridge.MeshConnectionState().Contacts
                .Where(contact => contact.Link is "online_direct" or "online_via_relay")
                .Select(contact => contact.AccountId)
                .OrderBy(id => id, StringComparer.Ordinal));
        }
        catch (Exception)
        {
            return "?"; // mesh not up: nothing is linked, and "?" still differs from any real set
        }
    }

    public MessageRefreshPayload? FetchLiveMessages(
        RustBridge rustBridge,
        string? dialogKey,
        string lastRenderedSignature,
        long loadVersion,
        int currentTranscriptItemCount,
        bool currentTranscriptServerHasOlderMessages,
        long? currentTranscriptOldestLoadedCreatedAtUnixMs,
        string? currentTranscriptOldestLoadedMessageId,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return null;
        }

        return FetchLiveMessagesCore(
            rustBridge,
            dialogKey,
            lastRenderedSignature,
            loadVersion,
            currentTranscriptItemCount,
            currentTranscriptServerHasOlderMessages,
            currentTranscriptOldestLoadedCreatedAtUnixMs,
            currentTranscriptOldestLoadedMessageId,
            cancellationToken,
            retryOnMidFlightChange: true);
    }

    public MessageRefreshPayload? FetchTailWindow(
        RustBridge rustBridge,
        string? dialogKey,
        string lastRenderedSignature,
        long loadVersion,
        int currentTranscriptItemCount,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return null;
        }

        return FetchTailWindowCore(
            rustBridge,
            dialogKey,
            lastRenderedSignature,
            loadVersion,
            currentTranscriptItemCount,
            cancellationToken,
            retryOnMidFlightChange: true);
    }

    public TranscriptHistoryPagePayload? FetchOlderMessagesPage(
        RustBridge rustBridge,
        string? dialogKey,
        long loadVersion,
        long? beforeCreatedAtUnixMs,
        string? beforeMessageId,
        CancellationToken cancellationToken)
    {
        if (string.IsNullOrWhiteSpace(dialogKey) ||
            !beforeCreatedAtUnixMs.HasValue ||
            string.IsNullOrWhiteSpace(beforeMessageId))
        {
            return null;
        }

        cancellationToken.ThrowIfCancellationRequested();
        ListMessagesPageResponse page = rustBridge.ListMessagesPage(
            dialogKey,
            beforeCreatedAtUnixMs,
            beforeMessageId,
            TranscriptPageSize);
        cancellationToken.ThrowIfCancellationRequested();

        Dictionary<string, MessageSecurityDetails> securityDetailsByMessageId = FetchSecurityDetailsBatch(
            rustBridge,
            page.Messages.Select(message => message.Id),
            cancellationToken);

        return new TranscriptHistoryPagePayload(
            dialogKey,
            page.Messages.ToList(),
            securityDetailsByMessageId,
            loadVersion,
            page.HasOlder,
            page.OldestLoadedCreatedAtUnixMs,
            page.OldestLoadedMessageId);
    }

    private MessageRefreshPayload? FetchLiveMessagesCore(
        RustBridge rustBridge,
        string dialogKey,
        string lastRenderedSignature,
        long loadVersion,
        int currentTranscriptItemCount,
        bool currentTranscriptServerHasOlderMessages,
        long? currentTranscriptOldestLoadedCreatedAtUnixMs,
        string? currentTranscriptOldestLoadedMessageId,
        CancellationToken cancellationToken,
        bool retryOnMidFlightChange)
    {
        cancellationToken.ThrowIfCancellationRequested();
        DialogMessageRefreshSummary refreshSummary = rustBridge.GetDialogMessageRefreshSummary(dialogKey);
        string signature = BuildMessageRefreshSignature(refreshSummary);
        if (string.Equals(signature, lastRenderedSignature, StringComparison.Ordinal))
        {
            return null;
        }

        if (!dialogDeltaCursors.TryGetValue(refreshSummary.DialogKey, out DialogDeltaCursor? existingCursor))
        {
            return FetchTailWindowCore(
                rustBridge,
                dialogKey,
                lastRenderedSignature,
                loadVersion,
                currentTranscriptItemCount,
                cancellationToken,
                retryOnMidFlightChange);
        }

        cancellationToken.ThrowIfCancellationRequested();
        DialogDeltaSnapshot deltaSnapshot = rustBridge.GetDialogMessageDelta(refreshSummary.DialogKey, existingCursor);
        if (deltaSnapshot.RequiresFullReload)
        {
            return FetchTailWindowCore(
                rustBridge,
                dialogKey,
                lastRenderedSignature,
                loadVersion,
                currentTranscriptItemCount,
                cancellationToken,
                retryOnMidFlightChange);
        }

        Dictionary<string, MessageSecurityDetails> deltaSecurityDetailsByMessageId = deltaSnapshot.ChangedSecurityDetails
            .Where(details => !string.IsNullOrWhiteSpace(details.MessageId))
            .ToDictionary(details => details.MessageId, StringComparer.Ordinal);

        MessageRefreshPayload payload = new(
            dialogKey,
            signature,
            false,
            deltaSnapshot.ChangedMessages.ToList(),
            deltaSecurityDetailsByMessageId,
            deltaSnapshot.Cursor,
            loadVersion,
            currentTranscriptServerHasOlderMessages,
            currentTranscriptOldestLoadedCreatedAtUnixMs,
            currentTranscriptOldestLoadedMessageId);

        return TryResolveMidFlightRefresh(
            rustBridge,
            dialogKey,
            cancellationToken,
            retryOnMidFlightChange,
            signature,
            payload,
            retryFetch: () => FetchLiveMessagesCore(
                rustBridge,
                dialogKey,
                lastRenderedSignature,
                loadVersion,
                currentTranscriptItemCount,
                currentTranscriptServerHasOlderMessages,
                currentTranscriptOldestLoadedCreatedAtUnixMs,
                currentTranscriptOldestLoadedMessageId,
                cancellationToken,
                retryOnMidFlightChange: false));
    }

    private MessageRefreshPayload? FetchTailWindowCore(
        RustBridge rustBridge,
        string dialogKey,
        string lastRenderedSignature,
        long loadVersion,
        int currentTranscriptItemCount,
        CancellationToken cancellationToken,
        bool retryOnMidFlightChange)
    {
        cancellationToken.ThrowIfCancellationRequested();
        DialogMessageRefreshSummary refreshSummary = rustBridge.GetDialogMessageRefreshSummary(dialogKey);
        string signature = BuildMessageRefreshSignature(refreshSummary);
        if (string.Equals(signature, lastRenderedSignature, StringComparison.Ordinal))
        {
            dialogDeltaCursors[refreshSummary.DialogKey] = DialogDeltaCursor.FromRefreshSummary(refreshSummary);
            return null;
        }

        int limit = Math.Clamp(
            currentTranscriptItemCount,
            TranscriptPageSize,
            TranscriptHardItemLimit);

        cancellationToken.ThrowIfCancellationRequested();
        ListMessagesPageResponse page = rustBridge.ListMessagesPage(
            dialogKey,
            beforeCreatedAtUnixMs: null,
            beforeMessageId: null,
            limit);
        cancellationToken.ThrowIfCancellationRequested();

        Dictionary<string, MessageSecurityDetails> securityDetailsByMessageId = FetchSecurityDetailsBatch(
            rustBridge,
            page.Messages.Select(message => message.Id),
            cancellationToken);

        MessageRefreshPayload payload = new(
            dialogKey,
            signature,
            true,
            page.Messages.ToList(),
            securityDetailsByMessageId,
            DialogDeltaCursor.FromRefreshSummary(refreshSummary),
            loadVersion,
            page.HasOlder,
            page.OldestLoadedCreatedAtUnixMs,
            page.OldestLoadedMessageId);

        return TryResolveMidFlightRefresh(
            rustBridge,
            dialogKey,
            cancellationToken,
            retryOnMidFlightChange,
            signature,
            payload,
            retryFetch: () => FetchTailWindowCore(
                rustBridge,
                dialogKey,
                lastRenderedSignature,
                loadVersion,
                currentTranscriptItemCount,
                cancellationToken,
                retryOnMidFlightChange: false));
    }

    private MessageRefreshPayload? TryResolveMidFlightRefresh(
        RustBridge rustBridge,
        string dialogKey,
        CancellationToken cancellationToken,
        bool retryOnMidFlightChange,
        string initialSignature,
        MessageRefreshPayload payload,
        Func<MessageRefreshPayload?> retryFetch)
    {
        cancellationToken.ThrowIfCancellationRequested();
        DialogMessageRefreshSummary latestSummary = rustBridge.GetDialogMessageRefreshSummary(dialogKey);
        string latestSignature = BuildMessageRefreshSignature(latestSummary);
        if (string.Equals(latestSignature, initialSignature, StringComparison.Ordinal))
        {
            dialogDeltaCursors[payload.Cursor.DialogKey] = payload.Cursor;
            return payload;
        }

        if (!retryOnMidFlightChange)
        {
            return null;
        }

        cancellationToken.ThrowIfCancellationRequested();
        return retryFetch();
    }

    private static Dictionary<string, MessageSecurityDetails> FetchSecurityDetailsBatch(
        RustBridge rustBridge,
        IEnumerable<string> messageIds,
        CancellationToken cancellationToken)
    {
        List<string> ids = messageIds
            .Where(messageId => !string.IsNullOrWhiteSpace(messageId))
            .Distinct(StringComparer.Ordinal)
            .ToList();
        if (ids.Count == 0)
        {
            return new Dictionary<string, MessageSecurityDetails>(StringComparer.Ordinal);
        }

        try
        {
            cancellationToken.ThrowIfCancellationRequested();
            List<MessageSecurityDetails> securityDetails = rustBridge.GetMessageSecurityDetailsBatch(ids);
            cancellationToken.ThrowIfCancellationRequested();
            return securityDetails
                .Where(details => !string.IsNullOrWhiteSpace(details.MessageId))
                .ToDictionary(details => details.MessageId, StringComparer.Ordinal);
        }
        catch
        {
            return new Dictionary<string, MessageSecurityDetails>(StringComparer.Ordinal);
        }
    }

    public void InvalidateDialog(string? dialogKey)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return;
        }

        dialogDeltaCursors.Remove(dialogKey);
    }

    public void ClearDialog(string? dialogKey)
        => InvalidateDialog(dialogKey);

    public void ResetAllDialogState()
        => dialogDeltaCursors.Clear();
}
