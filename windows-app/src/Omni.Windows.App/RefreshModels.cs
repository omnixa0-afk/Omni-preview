using System.Collections.Generic;
using System.Linq;
using Omni.Windows.App.Core;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

internal sealed record BackgroundRefreshRequest(
    int Generation,
    AuthenticatedShellSection CurrentSection,
    string? DialogKey,
    string? SelectedContactId,
    long? SelectedContactRevision,
    string? ActiveProfileId,
    string LastRenderedMessageSignature,
    string LastObservedChatRefreshSignature,
    string LastObservedContactRefreshSignature,
    long TranscriptLoadVersion,
    int CurrentTranscriptItemCount,
    bool CurrentTranscriptServerHasOlderMessages,
    long? CurrentTranscriptOldestLoadedCreatedAtUnixMs,
    string? CurrentTranscriptOldestLoadedMessageId,
    bool LifecycleDue,
    bool FootprintDue,
    bool TransportDue,
    bool CallDue,
    bool ChatHeaderDue,
    bool ChatListDue,
    bool MessageDue,
    bool QueueDue,
    bool ContactsDue,
    bool SelectedContactDue,
    bool SettingsDue)
{
    public bool HasWork =>
        LifecycleDue ||
        FootprintDue ||
        TransportDue ||
        CallDue ||
        ChatHeaderDue ||
        ChatListDue ||
        MessageDue ||
        QueueDue ||
        ContactsDue ||
        SelectedContactDue ||
        SettingsDue;
}

internal sealed class BackgroundRefreshResult
{
    public BackgroundRefreshResult(int generation)
    {
        Generation = generation;
    }

    public int Generation { get; }

    public LifecycleRefreshPayload? Lifecycle { get; init; }

    public FootprintRefreshPayload? Footprint { get; init; }

    public TransportRefreshPayload? Transport { get; init; }

    public CallInboxRefreshPayload? CallInbox { get; init; }

    public ChatHeaderRefreshPayload? ChatHeader { get; init; }

    public ChatThreadRefreshPayload? ChatThreads { get; init; }

    public MessageRefreshPayload? Messages { get; init; }

    public QueueDiagnosticsRefreshPayload? QueueDiagnostics { get; init; }

    public ContactsRefreshPayload? Contacts { get; init; }

    public SelectedContactRefreshPayload? SelectedContact { get; init; }

    public PasswordProtectionRefreshPayload? PasswordProtection { get; init; }
}

internal sealed record LifecycleRefreshPayload(
    string? ActiveProfileId,
    LocalAccountSnapshot? LocalAccount,
    LocalDeviceSnapshot? LocalDevice,
    ProfileGateSummary? GateSummary,
    ProfileLifecycleSummary? LifecycleSummary);

internal sealed record FootprintRefreshPayload(
    string? ProfileId,
    LocalProfileFootprintSummary? FootprintSummary);

internal sealed record TransportRefreshPayload(
    TransportSnapshot Snapshot);

internal sealed record CallInboxRefreshPayload(
    CallInboxSnapshot Snapshot);

internal sealed record ChatHeaderRefreshPayload(
    string DialogKey,
    ChatHeaderContactSummary HeaderSummary,
    ChatSecuritySummary SecuritySummary,
    ChatSessionSummary SessionSummary,
    CallSummary? CallSummary);

internal sealed record ChatThreadRefreshPayload(
    string ObservedSignature,
    IReadOnlyList<RustBridge.ChatThreadProjection> Projections,
    ChatGroupRows Groups);

// Groups we're in + invitations to groups — they share the chat list with 1:1 dialogs.
// Calls: group calls we're in or could join (row indicator + the ring bar).
internal sealed record ChatGroupRows(
    IReadOnlyList<GroupSummary> Groups,
    IReadOnlyList<GroupInviteView> Invites,
    IReadOnlyList<GroupCallNotice> Calls)
{
    internal static ChatGroupRows Fetch(RustBridge rustBridge)
        => new(rustBridge.ListGroups(), rustBridge.ListGroupInvites(), rustBridge.ListGroupCallInbox());

    // What the list shows of invitations and calls (group messages are covered by the
    // messages-table marker). Cheap reads: one SQLite query + the in-memory call registry.
    internal static string Signature(IReadOnlyList<GroupInviteView> invites, IReadOnlyList<GroupCallNotice> calls)
        => string.Join(',', invites.Select(invite => $"{invite.GroupId}:{invite.Status}"))
            + "|" + string.Join(',', calls.Select(call => $"{call.GroupId}:{call.CallId}:{call.ParticipantCount}:{call.Ringing}:{call.Joined}"));
}

internal sealed record MessageRefreshPayload(
    string DialogKey,
    string RenderSignature,
    bool IsFullReload,
    IReadOnlyList<ChatMessageRecord> Messages,
    IReadOnlyDictionary<string, MessageSecurityDetails> SecurityDetailsByMessageId,
    DialogDeltaCursor Cursor,
    long LoadVersion,
    bool ServerHasOlderMessages,
    long? OldestLoadedCreatedAtUnixMs,
    string? OldestLoadedMessageId);

internal sealed record TranscriptHistoryPagePayload(
    string DialogKey,
    IReadOnlyList<ChatMessageRecord> Messages,
    IReadOnlyDictionary<string, MessageSecurityDetails> SecurityDetailsByMessageId,
    long LoadVersion,
    bool ServerHasOlderMessages,
    long? OldestLoadedCreatedAtUnixMs,
    string? OldestLoadedMessageId);

internal sealed record QueueDiagnosticsRefreshPayload(
    QueueDiagnosticsSnapshot Diagnostics,
    string? DialogKey);

internal sealed record ContactsRefreshPayload(
    string ObservedSignature,
    IReadOnlyList<ContactSnapshot> Contacts);

internal sealed record SelectedContactRefreshPayload(
    string ContactId,
    ContactCardSnapshot? ContactCard,
    long? Revision);

internal sealed record PasswordProtectionRefreshPayload(
    string? ProfileId,
    PasswordProtectionSummary? Summary,
    string? ErrorText);
