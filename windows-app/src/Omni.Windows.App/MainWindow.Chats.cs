using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.Foundation;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private const double TranscriptTopLoadThresholdPx = 32d;
    private const double MessagesNearBottomThresholdPx = 80d;

    private readonly ActiveChatTranscriptState activeChatTranscriptState = new();
    private CancellationTokenSource? transcriptTailLoadCancellation;
    private CancellationTokenSource? transcriptOlderLoadCancellation;
    private ScrollViewer? messagesListScrollViewer;
    private bool messagesListScrollViewerHooked;
    private double currentTranscriptBubbleMaxWidth = MainWindowPresentation.TranscriptBubbleMaxWidthHardCap;

    private void RefreshMessages(bool force = false)
    {
        string? peerKey = GetResolvedCurrentDialogKey();
        if (!rustBridge.IsInitialized ||
            windowState.Onboarding.ShellMode != AppShellMode.Authenticated ||
            string.IsNullOrWhiteSpace(peerKey))
        {
            ClearMessagesView(peerKey);
            return;
        }

        if (force)
        {
            StartTailRebaseTranscriptLoad(peerKey);
            return;
        }

        if (activeChatTranscriptState.LiveCursor is null)
        {
            return;
        }

        activeChatTranscriptState.DialogKey = peerKey;

        MessageRefreshPayload? payload = chatRefreshService.FetchLiveMessages(
            rustBridge,
            peerKey,
            windowState.LastRenderedMessageSignature,
            activeChatTranscriptState.LoadVersion,
            messageItems.Count,
            activeChatTranscriptState.ServerHasOlderMessages,
            activeChatTranscriptState.OldestLoadedCreatedAtUnixMs,
            activeChatTranscriptState.OldestLoadedMessageId,
            CancellationToken.None);
        if (payload is null)
        {
            return;
        }

        ApplyMessageRefreshPayload(payload);
    }

    private void ClearMessagesView(string? peerKey)
    {
        CancelTranscriptLoads();
        chatRefreshService.ClearDialog(peerKey ?? activeChatTranscriptState.DialogKey ?? GetResolvedCurrentDialogKey());
        activeChatTranscriptState.DialogKey = null;
        activeChatTranscriptState.LoadVersion++;
        activeChatTranscriptState.LiveCursor = null;
        activeChatTranscriptState.ServerHasOlderMessages = false;
        activeChatTranscriptState.HasTrimmedOlderMessages = false;
        activeChatTranscriptState.IsLoadingOlderMessages = false;
        activeChatTranscriptState.IsRestoringAnchor = false;
        activeChatTranscriptState.OldestLoadedCreatedAtUnixMs = null;
        activeChatTranscriptState.OldestLoadedMessageId = null;
        pendingChatAutoScrollToLatest = false;
        windowState.LastRenderedMessageSignature = string.Empty;
        ClearCurrentChatMessageCache();

        messageItems.Clear();
        MessagesListView.Visibility = Visibility.Collapsed;
        MessagesEmptyStateBorder.Visibility = Visibility.Visible;
        MessagesEmptyStateTitleTextBlock.Text = string.IsNullOrWhiteSpace(peerKey)
            ? ChatPresentationText.BuildTranscriptSelectionHeading(chatThreadItems.Count > 0)
            : ChatPresentationText.BuildTranscriptEmptyHeading();
        MessagesEmptyStateTextBlock.Text = string.IsNullOrWhiteSpace(peerKey)
            ? ChatPresentationText.BuildTranscriptChatSelectionPrompt(chatThreadItems.Count > 0)
            : IsCurrentChatAwaitingFirstConnection()
                ? ChatPresentationText.BuildTranscriptAwaitingFirstConnectionHistoryText()
                : ChatPresentationText.BuildTranscriptUnavailableText();
    }
    private void ApplyMessageRefreshPayload(MessageRefreshPayload payload)
    {
        if (!IsTranscriptResultCurrent(payload.DialogKey, payload.LoadVersion))
        {
            return;
        }

        UpdateCurrentTranscriptBubbleMaxWidth(MessagesHostBorder.ActualWidth);
        bool shouldAutoScroll = pendingChatAutoScrollToLatest || IsMessagesListNearBottom(MessagesNearBottomThresholdPx);
        TranscriptScrollAnchor? anchor = shouldAutoScroll ? null : CaptureTranscriptScrollAnchor();
        activeChatTranscriptState.DialogKey = GetResolvedCurrentDialogKey() ?? payload.DialogKey;
        activeChatTranscriptState.LiveCursor = payload.Cursor;
        activeChatTranscriptState.ServerHasOlderMessages = payload.ServerHasOlderMessages;
        windowState.LastRenderedMessageSignature = payload.RenderSignature;
        if (payload.IsFullReload)
        {
            activeChatTranscriptState.HasTrimmedOlderMessages = false;
            RebuildTranscriptFromPayload(payload);
        }
        else
        {
            ApplyIncrementalTranscriptDelta(payload);
        }
        RecalculateTranscriptOldestBoundary(payload.OldestLoadedCreatedAtUnixMs, payload.OldestLoadedMessageId);
        UpdateTranscriptEmptyState();
        TryTrimTranscriptWindow();
        TranscriptLayout.Apply(messageItems, DateTime.Today, transcriptPeer);

        if (shouldAutoScroll)
        {
            ScrollTranscriptToBottom();
        }
        else if (anchor is not null)
        {
            RestoreTranscriptScrollAnchor(anchor);
        }

        pendingChatAutoScrollToLatest = false;
    }
    private void RebuildTranscriptFromPayload(MessageRefreshPayload payload)
    {
        List<ChatMessageRecord> orderedMessages = payload.Messages
            .OrderBy(message => message.CreatedAtUnixMs)
            .ThenBy(message => message.Id, StringComparer.Ordinal)
            .ToList();
        HashSet<string> nextMessageIds = orderedMessages
            .Select(message => message.Id)
            .ToHashSet(StringComparer.Ordinal);

        for (int index = messageItems.Count - 1; index >= 0; index--)
        {
            ChatMessageListItem existingItem = messageItems[index];
            if (nextMessageIds.Contains(existingItem.MessageId))
            {
                continue;
            }

            messageItems.RemoveAt(index);
            currentChatMessageItemsById.Remove(existingItem.MessageId);
            currentChatMessagesById.Remove(existingItem.MessageId);
            currentChatSecurityDetailsByMessageId.Remove(existingItem.MessageId);
        }

        for (int index = 0; index < orderedMessages.Count; index++)
        {
            ChatMessageRecord message = orderedMessages[index];
            payload.SecurityDetailsByMessageId.TryGetValue(message.Id, out MessageSecurityDetails? securityDetails);
            currentChatMessagesById[message.Id] = message;
            if (securityDetails is not null)
            {
                currentChatSecurityDetailsByMessageId[message.Id] = securityDetails;
            }
            else
            {
                currentChatSecurityDetailsByMessageId.Remove(message.Id);
            }

            if (!currentChatMessageItemsById.TryGetValue(message.Id, out ChatMessageListItem? item))
            {
                item = MainWindowPresentation.CreateChatItem(
                    message,
                    securityDetails,
                    currentChatSecuritySummary,
                    currentChatHeaderSummary?.ContactCard);
                ApplyCurrentTranscriptBubbleMaxWidth(item);
                messageItems.Insert(index, item);
                currentChatMessageItemsById[item.MessageId] = item;
                continue;
            }

            item.ApplyFrom(
                message,
                securityDetails,
                currentChatSecuritySummary,
                currentChatHeaderSummary?.ContactCard);
            ApplyCurrentTranscriptBubbleMaxWidth(item);

            int existingIndex = messageItems.IndexOf(item);
            if (existingIndex < 0)
            {
                messageItems.Insert(index, item);
                continue;
            }

            if (existingIndex != index)
            {
                messageItems.Move(existingIndex, index);
            }
        }
    }

    private void ApplyIncrementalTranscriptDelta(MessageRefreshPayload payload)
    {
        HashSet<string> updatedMessageIds = new(StringComparer.Ordinal);

        foreach (ChatMessageRecord message in payload.Messages
                     .OrderBy(message => message.CreatedAtUnixMs)
                     .ThenBy(message => message.Id, StringComparer.Ordinal))
        {
            if (IsMessageOlderThanCurrentTranscriptWindow(message) &&
                !currentChatMessageItemsById.ContainsKey(message.Id))
            {
                continue;
            }

            UpsertTranscriptMessageItem(message, payload.SecurityDetailsByMessageId);
            updatedMessageIds.Add(message.Id);
        }

        foreach (KeyValuePair<string, MessageSecurityDetails> entry in payload.SecurityDetailsByMessageId)
        {
            if (updatedMessageIds.Contains(entry.Key) ||
                !currentChatMessagesById.TryGetValue(entry.Key, out ChatMessageRecord? message) ||
                !currentChatMessageItemsById.TryGetValue(entry.Key, out ChatMessageListItem? existing))
            {
                continue;
            }

            currentChatSecurityDetailsByMessageId[entry.Key] = entry.Value;
            existing.ApplyFrom(
                message,
                entry.Value,
                currentChatSecuritySummary,
                currentChatHeaderSummary?.ContactCard);
            ApplyCurrentTranscriptBubbleMaxWidth(existing);
        }
    }

    private void UpsertTranscriptMessageItem(
        ChatMessageRecord message,
        IReadOnlyDictionary<string, MessageSecurityDetails> refreshedSecurityDetails)
    {
        MessageSecurityDetails? securityDetails = null;
        if (refreshedSecurityDetails.TryGetValue(message.Id, out MessageSecurityDetails? refreshedSecurity))
        {
            securityDetails = refreshedSecurity;
            currentChatSecurityDetailsByMessageId[message.Id] = refreshedSecurity;
        }
        else
        {
            currentChatSecurityDetailsByMessageId.TryGetValue(message.Id, out securityDetails);
        }

        currentChatMessagesById[message.Id] = message;

        if (currentChatMessageItemsById.TryGetValue(message.Id, out ChatMessageListItem? existing))
        {
            existing.ApplyFrom(
                message,
                securityDetails,
                currentChatSecuritySummary,
                currentChatHeaderSummary?.ContactCard);
            ApplyCurrentTranscriptBubbleMaxWidth(existing);
            return;
        }

        ChatMessageListItem item = MainWindowPresentation.CreateChatItem(
            message,
            securityDetails,
            currentChatSecuritySummary,
            currentChatHeaderSummary?.ContactCard);
        ApplyCurrentTranscriptBubbleMaxWidth(item);
        int insertIndex = FindMessageInsertIndex(message);
        messageItems.Insert(insertIndex, item);
        currentChatMessageItemsById[item.MessageId] = item;
    }

    private int FindMessageInsertIndex(ChatMessageRecord message)
        => TranscriptWindowing.FindInsertIndex(messageItems, message);

    private void StartTailRebaseTranscriptLoad(string dialogKey)
    {
        bool sameDialog = string.Equals(activeChatTranscriptState.DialogKey, dialogKey, StringComparison.Ordinal) &&
                          activeChatTranscriptState.LoadVersion > 0;
        int currentLoadedCount = sameDialog ? messageItems.Count : 0;
        string lastRenderedSignature = sameDialog ? windowState.LastRenderedMessageSignature : string.Empty;

        PrepareTranscriptForTailRebase(dialogKey, clearVisibleTranscript: !sameDialog);
        CancellationTokenSource cancellation = ReplaceTranscriptTailLoadCancellation();
        _ = LoadTailWindowAsync(
            dialogKey,
            lastRenderedSignature,
            currentLoadedCount,
            activeChatTranscriptState.LoadVersion,
            cancellation);
    }

    private async Task LoadTailWindowAsync(
        string dialogKey,
        string lastRenderedSignature,
        int currentLoadedCount,
        long loadVersion,
        CancellationTokenSource cancellation)
    {
        try
        {
            MessageRefreshPayload? payload = await Task.Run(() => chatRefreshService.FetchTailWindow(
                rustBridge,
                dialogKey,
                lastRenderedSignature,
                loadVersion,
                currentLoadedCount,
                cancellation.Token),
                cancellation.Token);
            if (payload is null)
            {
                return;
            }

            DispatcherQueue.TryEnqueue(() => ApplyMessageRefreshPayload(payload));
        }
        catch (OperationCanceledException)
        {
            // Dialog changed or a newer tail rebase superseded this one.
        }
        finally
        {
            if (ReferenceEquals(transcriptTailLoadCancellation, cancellation))
            {
                transcriptTailLoadCancellation = null;
            }

            cancellation.Dispose();
        }
    }

    private void PrepareTranscriptForTailRebase(string dialogKey, bool clearVisibleTranscript)
    {
        string? previousDialogKey = activeChatTranscriptState.DialogKey ?? GetResolvedCurrentDialogKey();
        bool sameDialog = !string.IsNullOrWhiteSpace(previousDialogKey) &&
                          string.Equals(previousDialogKey, dialogKey, StringComparison.Ordinal);

        if (!string.IsNullOrWhiteSpace(previousDialogKey) &&
            !string.Equals(previousDialogKey, dialogKey, StringComparison.Ordinal))
        {
            chatRefreshService.ClearDialog(previousDialogKey);
        }

        CancelTranscriptTailLoad();
        CancelTranscriptOlderLoad();

        activeChatTranscriptState.DialogKey = dialogKey;
        activeChatTranscriptState.LoadVersion++;
        activeChatTranscriptState.IsLoadingOlderMessages = false;
        activeChatTranscriptState.IsRestoringAnchor = false;

        if (!sameDialog || clearVisibleTranscript)
        {
            chatRefreshService.ClearDialog(dialogKey);
            activeChatTranscriptState.LiveCursor = null;
            activeChatTranscriptState.ServerHasOlderMessages = false;
            activeChatTranscriptState.HasTrimmedOlderMessages = false;
            activeChatTranscriptState.OldestLoadedCreatedAtUnixMs = null;
            activeChatTranscriptState.OldestLoadedMessageId = null;
        }

        if (!clearVisibleTranscript)
        {
            return;
        }

        windowState.LastRenderedMessageSignature = string.Empty;
        ClearCurrentChatMessageCache();

        messageItems.Clear();
        UpdateTranscriptEmptyState();
    }

    private void ApplyOlderMessagesPage(TranscriptHistoryPagePayload payload, TranscriptScrollAnchor? anchor)
    {
        if (!IsTranscriptResultCurrent(payload.DialogKey, payload.LoadVersion))
        {
            if (transcriptOlderLoadCancellation is null)
            {
                activeChatTranscriptState.IsLoadingOlderMessages = false;
            }

            return;
        }

        UpdateCurrentTranscriptBubbleMaxWidth(MessagesHostBorder.ActualWidth);
        activeChatTranscriptState.DialogKey = GetResolvedCurrentDialogKey() ?? payload.DialogKey;
        activeChatTranscriptState.ServerHasOlderMessages = payload.ServerHasOlderMessages;
        if (!activeChatTranscriptState.ServerHasOlderMessages)
        {
            activeChatTranscriptState.HasTrimmedOlderMessages = false;
        }

        HashSet<string> seenMessageIds = new(StringComparer.Ordinal);
        List<ChatMessageRecord> messagesToPrepend = payload.Messages
            .OrderBy(message => message.CreatedAtUnixMs)
            .ThenBy(message => message.Id, StringComparer.Ordinal)
            .Where(message => seenMessageIds.Add(message.Id) && !currentChatMessageItemsById.ContainsKey(message.Id))
            .ToList();

        int insertIndex = 0;
        foreach (ChatMessageRecord message in messagesToPrepend)
        {
            payload.SecurityDetailsByMessageId.TryGetValue(message.Id, out MessageSecurityDetails? securityDetails);
            currentChatMessagesById[message.Id] = message;
            if (securityDetails is not null)
            {
                currentChatSecurityDetailsByMessageId[message.Id] = securityDetails;
            }

            ChatMessageListItem item = MainWindowPresentation.CreateChatItem(
                message,
                securityDetails,
                currentChatSecuritySummary,
                currentChatHeaderSummary?.ContactCard);
            ApplyCurrentTranscriptBubbleMaxWidth(item);
            messageItems.Insert(insertIndex, item);
            currentChatMessageItemsById[item.MessageId] = item;
            insertIndex++;
        }

        RecalculateTranscriptOldestBoundary(
            payload.OldestLoadedCreatedAtUnixMs,
            payload.OldestLoadedMessageId);
        UpdateTranscriptEmptyState();
        TranscriptLayout.Apply(messageItems, DateTime.Today, transcriptPeer);
        activeChatTranscriptState.IsLoadingOlderMessages = false;

        if (anchor is not null && messagesToPrepend.Count > 0)
        {
            RestoreTranscriptScrollAnchor(anchor);
        }
    }

    private void UpdateTranscriptEmptyState()
    {
        if (messageItems.Count > 0)
        {
            MessagesListView.Visibility = Visibility.Visible;
            MessagesEmptyStateBorder.Visibility = Visibility.Collapsed;
            return;
        }
        MessagesListView.Visibility = Visibility.Collapsed;
        MessagesEmptyStateBorder.Visibility = Visibility.Visible;
        MessagesEmptyStateTitleTextBlock.Text = ChatPresentationText.BuildTranscriptEmptyHeading();
        MessagesEmptyStateTextBlock.Text = IsCurrentChatAwaitingFirstConnection()
            ? ChatPresentationText.BuildTranscriptAwaitingFirstConnectionEmptyText()
            : ChatPresentationText.BuildTranscriptEmptyText();
    }
    private void ClearCurrentChatMessageCache()
    {
        currentChatMessagesById.Clear();
        currentChatSecurityDetailsByMessageId.Clear();
        currentChatMessageItemsById.Clear();
    }

    private void RefreshChatsList(bool force = false, string? selectDialogKey = null)
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            chatThreadItems.Clear();
            windowState.LastRenderedChatSignature = string.Empty;
            ChatsEmptyStateBorder.Visibility = Visibility.Visible;
            return;
        }

        ApplyChatThreadProjections(
            rustBridge.ListChatThreadProjections(),
            force,
            selectDialogKey,
            groupRows: ChatGroupRows.Fetch(rustBridge));
    }

    private void ApplyChatThreadProjections(
        IReadOnlyList<RustBridge.ChatThreadProjection> projections,
        bool force = false,
        string? selectDialogKey = null,
        bool refreshQueueDiagnostics = true,
        ChatGroupRows? groupRows = null)
    {
        List<ChatThreadListItem> threads = BuildChatThreadItems(projections, groupRows);
        NoteUnreadCounts(threads);
        if (groupRows is not null)
        {
            UpdateGroupCallRing(groupRows.Calls);
        }
        // Search filter: drop non-matching dialogs before the patcher runs, so selection stability
        // and the patcher are reused as-is. An open chat that's filtered out simply loses its list
        // selection (the conversation pane stays — OnChatsSelectionChanged no-ops on null).
        string query = chatThreadSearchQuery.Trim();
        if (query.Length > 0)
        {
            threads = threads.Where(thread => ChatThreadMatchesQuery(thread, query)).ToList();
        }
        // Query is part of the signature so a query change re-patches (otherwise an unchanged
        // projection set would short-circuit and the filter wouldn't apply).
        string signature = query + "␟" + string.Join('|', threads.Select(thread => thread.Signature));
        if (!force &&
            signature == windowState.LastRenderedChatSignature &&
            chatThreadItems.Count > 0)
        {
            return;
        }

        windowState.LastRenderedChatSignature = signature;
        WithSelectionSyncSuppressed(ref suppressChatSelectionSync, () =>
        {
            // The selected row is about to go (an invitation withdrawn while its card is open): drop
            // the selection BEFORE the patch removes it. Removing a ListView's selected item and then
            // selecting a new row with the same key crashed inside Microsoft.UI.Xaml (access violation,
            // reproduced twice live 2026-10-04: invitation withdrawn, then sent again).
            if (ChatsListView.SelectedItem is ChatThreadListItem selectedNow
                && !threads.Any(t => string.Equals(t.DialogKey, selectedNow.DialogKey, StringComparison.Ordinal)))
            {
                ChatsListView.SelectedItem = null;
            }
            PatchChatThreadCollection(threads);
            ChatsEmptyStateBorder.Visibility = chatThreadItems.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
            // The middle says «choose one» or «add the first one» by whether there is anything to choose.
            if (MessagesListView.Visibility != Visibility.Visible && string.IsNullOrWhiteSpace(GetResolvedCurrentDialogKey()) && currentGroupRowKey is null)
            {
                MessagesEmptyStateTitleTextBlock.Text = ChatPresentationText.BuildTranscriptSelectionHeading(chatThreadItems.Count > 0);
                MessagesEmptyStateTextBlock.Text = ChatPresentationText.BuildTranscriptChatSelectionPrompt(chatThreadItems.Count > 0);
            }

            // An open group keeps its row selected across refreshes (it is not the 1:1 "current peer").
            string? requestedDialogKey = selectDialogKey ?? currentGroupRowKey ?? GetResolvedCurrentDialogKey();
            if (string.IsNullOrWhiteSpace(requestedDialogKey))
            {
                ChatsListView.SelectedItem = null;
                windowState.CurrentPeerKey = null;
                RefreshChatHeaderSummary();
                RefreshMessages(force: true);
                if (refreshQueueDiagnostics)
                {
                    RefreshQueueDiagnostics();
                }
                UpdateCurrentPeerText();
                UpdateControlStates();
                return;
            }

            ChatThreadListItem? currentSelected = ChatsListView.SelectedItem as ChatThreadListItem;
            ChatThreadListItem? selected = chatThreadItems.FirstOrDefault(item =>
                string.Equals(item.DialogKey, requestedDialogKey, StringComparison.Ordinal))
                ?? ResolveChatSelectionFallback(requestedDialogKey);
            if (selected is not null)
            {
                if (selected.IsGroup)
                {
                    if (!ReferenceEquals(currentSelected, selected))
                    {
                        ChatsListView.SelectedItem = selected;
                    }
                    return;
                }

                windowState.CurrentPeerKey = selected.DialogKey;

                if (!string.Equals(currentSelected?.DialogKey, selected.DialogKey, StringComparison.Ordinal))
                {
                    ChatsListView.SelectedItem = selected;
                }

                return;
            }

            ChatsListView.SelectedItem = null;
            windowState.CurrentPeerKey = null;
            RefreshChatHeaderSummary();
            RefreshMessages(force: true);
            if (refreshQueueDiagnostics)
            {
                RefreshQueueDiagnostics();
            }
            UpdateCurrentPeerText();
            UpdateControlStates();
        });
    }


    private bool IsActiveChatForContact(ContactSnapshot contact)
    {
        if (currentChatHeaderSummary?.ContactCard?.Contact.ContactId is not null &&
            string.Equals(currentChatHeaderSummary.ContactCard.Contact.ContactId, contact.ContactId, StringComparison.Ordinal))
        {
            return true;
        }

        if (!string.IsNullOrWhiteSpace(contact.PrimaryDialogKey) &&
            (string.Equals(windowState.CurrentPeerKey, contact.PrimaryDialogKey, StringComparison.Ordinal) ||
             string.Equals(windowState.CurrentTransportPeerKey, contact.PrimaryDialogKey, StringComparison.Ordinal)))
        {
            return true;
        }

        string? accountDialogKey = BuildDialogKeyFromAccountId(contact.RemoteAccountId);
        if (!string.IsNullOrWhiteSpace(accountDialogKey) &&
            (string.Equals(windowState.CurrentPeerKey, accountDialogKey, StringComparison.Ordinal) ||
             string.Equals(windowState.CurrentTransportPeerKey, accountDialogKey, StringComparison.Ordinal)))
        {
            return true;
        }

        return false;
    }

    private void CloseCurrentChatSelection()
    {
        string? currentDialogKey = GetResolvedCurrentDialogKey();
        ClearPendingAttachmentDraft(preserveStatusText: true);
        CancelTranscriptLoads();
        chatRefreshService.ClearDialog(currentDialogKey);
        activeChatTranscriptState.DialogKey = null;
        activeChatTranscriptState.LoadVersion++;
        activeChatTranscriptState.LiveCursor = null;
        activeChatTranscriptState.ServerHasOlderMessages = false;
        activeChatTranscriptState.HasTrimmedOlderMessages = false;
        activeChatTranscriptState.OldestLoadedCreatedAtUnixMs = null;
        activeChatTranscriptState.OldestLoadedMessageId = null;
        windowState.LastRenderedMessageSignature = string.Empty;
        ClearCurrentChatMessageCache();

        messageItems.Clear();
        _ = callVideoService.StopAsync();
        pendingChatAutoScrollToLatest = false;
        windowState.CurrentPeerKey = null;
        windowState.CurrentTransportPeerKey = null;
        currentChatCallSummary = null;
        WithSelectionSyncSuppressed(ref suppressChatSelectionSync, () =>
        {
            ChatsListView.SelectedItem = null;
        });

        MessagesListView.Visibility = Visibility.Collapsed;
        MessagesEmptyStateBorder.Visibility = Visibility.Visible;
        MessagesEmptyStateTitleTextBlock.Text = ChatPresentationText.BuildTranscriptSelectionHeading(chatThreadItems.Count > 0);
        MessagesEmptyStateTextBlock.Text = ChatPresentationText.BuildTranscriptChatSelectionPrompt(chatThreadItems.Count > 0);
        RenderCurrentChatCallSummary();
    }

    private void OpenChatThread(string dialogKey, string? contactId = null)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return;
        }

        // Every path into a 1:1 dialog (list tap, "write" from contacts, incoming call) leaves a group.
        CloseGroupConversation();
        if (!string.Equals(dialogKey, windowState.CurrentPeerKey, StringComparison.Ordinal))
        {
            CloseChatContactInfo();
        }
        string? previousContactId = windowState.SelectedContactId;
        if (!string.Equals(windowState.CurrentPeerKey, dialogKey, StringComparison.Ordinal))
        {
            ClearPendingAttachmentDraft(preserveStatusText: true);
        }

        windowState.CurrentPeerKey = dialogKey;
        windowState.CurrentTransportPeerKey = dialogKey;
        windowState.SelectedContactId = string.IsNullOrWhiteSpace(contactId) ? null : contactId;
        if (!string.Equals(previousContactId, windowState.SelectedContactId, StringComparison.Ordinal))
        {
            windowState.SelectedContactRevision = null;
        }

        try
        {
            currentChatSessionSummary = rustBridge.OpenChatSessionLease(dialogKey);
        }
        catch
        {
            // Keep chat opening resilient even if the runtime lease cannot be refreshed immediately.
        }
        WatchCallVideoDialog(dialogKey);
        pendingChatAutoScrollToLatest = true;

        UpdateCurrentPeerText();
        RefreshChatHeaderSummary();
        // User intent to reach this contact: if there's no mesh link yet, nudge the core to
        // reach it over serverless rendezvous now instead of waiting for the periodic sweep
        // (idempotent + gated in the core; covers the selected-contact-call path too, which
        // opens the chat). See docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
        TryReachContactOverRendezvous(currentChatHeaderSummary?.ContactCard?.Contact?.RemoteAccountId);
        RefreshMessages(force: true);
        RefreshQueueDiagnostics();
        WakeChatRefresh();
        if (!string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            WakeSelectedContactRefresh();
        }
        ReadChatOnScreen();
        UpdateControlStates();
        SaveUiState();
    }

    /// Best-effort connectivity nudge: ask the core to reach an approved contact over serverless
    /// rendezvous when the user engages it (opens its chat / starts a call) and there's no mesh
    /// link yet, so cross-network reach is immediate instead of waiting for the periodic sweep.
    /// The core gates + runs the slow dial on a background thread; we fire-and-forget off the UI
    /// thread and never let it disrupt opening a chat / placing a call.
    /// See docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
    private void TryReachContactOverRendezvous(string? remoteAccountId)
    {
        if (string.IsNullOrWhiteSpace(remoteAccountId))
        {
            return;
        }

        string accountId = remoteAccountId;
        _ = Task.Run(() =>
        {
            try
            {
                rustBridge.RendezvousReachContact(accountId);
            }
            catch
            {
                // Best-effort: the periodic rendezvous sweep is the fallback.
            }
        });
    }

    // "Подключиться" — the explicit, user-driven twin of the auto reconnect-on-open nudge. Fires the
    // same core rendezvous re-find (gated + async in Rust), gives immediate feedback on the status
    // line, and wakes the chat refresh so the header + button update as the link comes up. Briefly
    // disabled to debounce; the header render self-hides it once a live link exists.
    private async void OnReconnectContactClicked(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
    {
        string? accountId = currentChatHeaderSummary?.ContactCard?.Contact.RemoteAccountId;
        if (string.IsNullOrWhiteSpace(accountId))
        {
            return;
        }

        ReconnectContactButton.IsEnabled = false;
        ChatConnectionStatusTextBlock.Text = "⏳ Подключаюсь по вашему запросу…";
        TryReachContactOverRendezvous(accountId);
        WakeChatRefresh();
        await Task.Delay(TimeSpan.FromSeconds(6));
        ReconnectContactButton.IsEnabled = true;
    }

    private void ShowChatsSection()
    {
        AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
        windowState.CurrentSection = AuthenticatedShellSection.Chats;
        ChatsSectionRoot.Visibility = Visibility.Visible;
        ContactsSectionRoot.Visibility = Visibility.Collapsed;
        SettingsSectionRoot.Visibility = Visibility.Collapsed;
        DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
    }

    private static string BuildDialogKeyFromInstanceId(string instanceId)
    {
        string trimmed = instanceId.Trim();
        return string.IsNullOrWhiteSpace(trimmed)
            ? string.Empty
            : $"dialog:instance:{trimmed}";
    }

    private string? ResolveDialogKeyFromTransportSnapshot(TransportSnapshot snapshot)
    {
        if (snapshot.State is not ("connected" or "listening_and_connected"))
        {
            return null;
        }

        if (!string.IsNullOrWhiteSpace(snapshot.PeerKey))
        {
            return snapshot.PeerKey.Trim();
        }

        if (!string.IsNullOrWhiteSpace(snapshot.RemoteInstanceId))
        {
            return BuildDialogKeyFromInstanceId(snapshot.RemoteInstanceId);
        }

        return null;
    }

    private string? GetResolvedCurrentDialogKey()
    {
        string? currentPeerKey = windowState.CurrentPeerKey?.Trim();
        return string.IsNullOrWhiteSpace(currentPeerKey) ? null : currentPeerKey;
    }

    private string? GetAttachableCurrentDialogKey()
        => GetResolvedCurrentDialogKey()
        ?? windowState.CurrentTransportPeerKey?.Trim();

    private void PatchChatThreadCollection(IReadOnlyList<ChatThreadListItem> threads)
        => ChatThreadCollectionPatcher.Patch(chatThreadItems, threads);

    private List<ChatThreadListItem> BuildChatThreadItems(
        IReadOnlyList<RustBridge.ChatThreadProjection> projections,
        ChatGroupRows? groupRows = null)
    {
        Dictionary<string, ChatThreadListItem> items = new(StringComparer.Ordinal);
        HashSet<string> linkedAccountIds = CurrentlyLinkedAccountIds();
        NoteContactPhotos(projections.Select(projection => projection.Contact));

        foreach (RustBridge.ChatThreadProjection projection in projections)
        {
            ContactSnapshot contact = projection.Contact;
            if (contact.RemovedAtUnixMs.HasValue)
            {
                continue;
            }

            if (!string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            string? dialogKey = ResolveChatDialogKey(contact);
            if (string.IsNullOrWhiteSpace(dialogKey))
            {
                continue;
            }

            try
            {
                ChatThreadListItem item = BuildChatThreadItem(projection, linkedAccountIds);
                items[item.DialogKey] = item;
            }
            catch (Exception ex)
            {
                string title = MainWindowPresentation.ContactTitle(contact);
                ChatThreadListItem item = new(dialogKey);
                item.ApplyFrom(
                    contact.ContactId,
                    MainWindowPresentation.BuildAvatarText(title),
                    null,
                    Visibility.Collapsed,
                    Visibility.Visible,
                    Visibility.Collapsed,
                    title,
                    $"Не удалось загрузить чат: {NormalizeUserError(ex)}",
                    "Ошибка",
                    MainWindowPresentation.FormatMessageTime(contact.UpdatedAtUnixMs),
                    contact.UpdatedAtUnixMs,
                    $"{contact.ContactId}:{dialogKey}:error:{contact.UpdatedAtUnixMs}:{ex.Message}",
                    nextAvatarSeed: MainWindowPresentation.AvatarSeed(contact));
                items[dialogKey] = item;
            }
        }

        if (groupRows is not null)
        {
            foreach (ChatThreadListItem item in BuildGroupThreadItems(groupRows))
            {
                items[item.DialogKey] = item;
            }
        }

        return items.Values
            .OrderByDescending(item => item.SortUnixMs)
            .ThenBy(item => item.Title, StringComparer.CurrentCultureIgnoreCase)
            .ToList();
    }
    private ChatThreadListItem BuildChatThreadItem(RustBridge.ChatThreadProjection projection, IReadOnlySet<string> linkedAccountIds)
    {
        ContactSnapshot contact = projection.Contact;
        string dialogKey = ResolveChatDialogKey(contact)
            ?? throw new InvalidOperationException("contact has no dialog key");
        ChatMessageRecord? lastMessage = projection.LastMessage;
        string title = MainWindowPresentation.ContactTitle(contact);
        string previewText = ChatPresentationText.BuildChatPreviewText(lastMessage);
        // Only an unusual relationship is worth a word in the chat list: «В контактах» stood on every
        // ordinary chat and said nothing (owner's survey, 2026-10-10).
        string statusText = string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase)
            ? string.Empty
            : MainWindowPresentation.MapRelationshipState(contact.RelationshipState);
        long sortUnixMs = lastMessage?.CreatedAtUnixMs ?? contact.UpdatedAtUnixMs;
        string timeText = lastMessage is null
            ? MainWindowPresentation.FormatMessageTime(contact.UpdatedAtUnixMs)
            : MainWindowPresentation.FormatMessageTime(lastMessage.CreatedAtUnixMs);
        var avatarImage = MainWindowPresentation.BuildAvatarImage(
            contact.PublicAvatarImagePath,
            96,
            BuildContactAvatarVersionToken(contact));
        bool isAvatarLoading = !string.IsNullOrWhiteSpace(contact.PublicAvatarPendingHashHex);

        long nowPresenceUnixMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        ContactPresenceDotKind presenceDotKind =
            MainWindowPresentation.MapContactPresenceDotKind(contact, nowPresenceUnixMs, linkedAccountIds);
        (Visibility presenceDotVisibility, Brush presenceDotBrush) =
            MainWindowPresentation.MapContactPresenceDot(presenceDotKind);

        ChatThreadListItem item = new(dialogKey);
        item.ApplyFrom(
            contact.ContactId,
            MainWindowPresentation.BuildAvatarText(title),
            avatarImage,
            avatarImage is null ? Visibility.Collapsed : Visibility.Visible,
            avatarImage is null ? Visibility.Visible : Visibility.Collapsed,
            isAvatarLoading ? Visibility.Visible : Visibility.Collapsed,
            title,
            previewText,
            statusText,
            timeText,
            sortUnixMs,
            // Решение по точке — В ПОДПИСИ: оно зависит от живости линка и от текущего времени, а
            // ни то, ни другое из полей контакта не выводится. Без этого список показывал зелёную
            // точку на выключенной машине, пока шапка рядом честно писала «Офлайн».
            $"{contact.ContactId}:{dialogKey}:{contact.Petname}:{contact.PublicDisplayName}:{contact.PublicAvatarHashHex}:{contact.PublicAvatarPendingHashHex}:{contact.PublicAvatarPendingMimeType}:{contact.PublicProfileUpdatedAtUnixMs}:{contact.RemoteAccountId}:{contact.RemoteInstanceId}:{lastMessage?.Id ?? "none"}:{lastMessage?.UpdatedAtUnixMs ?? 0}:{contact.RelationshipState}:{contact.IsBlocked}:{contact.RemovedAtUnixMs}:{contact.PresenceState}:{contact.PresenceLastActivityAtUnixMs}:{presenceDotKind}:{projection.UnreadCount}",
            presenceDotVisibility,
            presenceDotBrush,
            MainWindowPresentation.AvatarSeed(contact),
            projection.UnreadCount);
        return item;
    }

    private void OnChatsSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressChatSelectionSync)
        {
            return;
        }

        if (ChatsListView.SelectedItem is not ChatThreadListItem item)
        {
            return;
        }

        if (item.IsGroup)
        {
            OpenGroupRow(item);
            return;
        }

        OpenChatThread(item.DialogKey, item.ContactId);
    }

    private void OnChatSearchTextChanged(object sender, TextChangedEventArgs e)
    {
        string next = ChatSearchBox.Text ?? string.Empty;
        if (string.Equals(next, chatThreadSearchQuery, StringComparison.Ordinal))
        {
            return;
        }

        chatThreadSearchQuery = next;
        RefreshChatsList(force: true);
    }

    private static bool ChatThreadMatchesQuery(ChatThreadListItem thread, string query)
        => thread.Title.Contains(query, StringComparison.OrdinalIgnoreCase)
            || thread.PreviewText.Contains(query, StringComparison.OrdinalIgnoreCase);

    private static string? ResolveChatDialogKey(ContactSnapshot contact)
    {
        if (!string.IsNullOrWhiteSpace(contact.PrimaryDialogKey))
        {
            return contact.PrimaryDialogKey;
        }

        if (!string.IsNullOrWhiteSpace(contact.RemoteInstanceId))
        {
            return $"dialog:instance:{contact.RemoteInstanceId}";
        }

        return BuildDialogKeyFromAccountId(contact.RemoteAccountId);
    }

}
