using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Shared;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Chats;

/// A 1:1 message: the record behind the shared `MessageRow`, patched in place as its state moves.
internal sealed class ChatMessageListItem : MessageRow
{
    private string statusText = string.Empty;
    private bool attachmentCanDownload;
    private bool attachmentCanRevokeAccess;
    private bool attachmentHasLocalCopy;
    private ChatMessageRecord? sourceMessage;
    private MessageSecurityDetails? securityDetails;

    public ChatMessageListItem(string messageId) : base(messageId)
    {
    }

    public string StatusText
    {
        get => statusText;
        private set => SetField(ref statusText, value);
    }

    /// When the message was written (for the day headers); 0 before the first Apply.
    public long CreatedAtUnixMs => sourceMessage?.CreatedAtUnixMs ?? 0;

    internal void SetLayout(bool runTail, ChatTranscriptPeer? transcriptPeer)
    {
        Peer = transcriptPeer;
        IsRunTail = runTail;
    }

    public bool AttachmentCanDownload
    {
        get => attachmentCanDownload;
        private set => SetField(ref attachmentCanDownload, value);
    }

    public bool AttachmentCanRevokeAccess
    {
        get => attachmentCanRevokeAccess;
        private set => SetField(ref attachmentCanRevokeAccess, value);
    }

    public bool AttachmentHasLocalCopy
    {
        get => attachmentHasLocalCopy;
        private set => SetField(ref attachmentHasLocalCopy, value);
    }

    internal ChatMessageRecord? SourceMessage
    {
        get => sourceMessage;
        private set => SetField(ref sourceMessage, value);
    }

    internal MessageSecurityDetails? SecurityDetails
    {
        get => securityDetails;
        private set => SetField(ref securityDetails, value);
    }

    internal void ApplyFrom(
        ChatMessageRecord message,
        MessageSecurityDetails? messageSecurityDetails,
        ChatSecuritySummary? chatSecuritySummary,
        ContactCardSnapshot? linkedContact)
    {
        _ = chatSecuritySummary;

        if (!string.Equals(MessageId, message.Id, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                $"Cannot apply message '{message.Id}' to transcript item '{MessageId}'.");
        }

        SourceMessage = message;
        SecurityDetails = messageSecurityDetails;

        bool outgoing = message.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase);
        AttachmentRecord? attachment = message.Attachment;
        bool hasAttachment = attachment is not null;
        string semanticStatusText = MainWindowPresentation.MapMessageStatus(message.Status);

        IsOutgoing = outgoing;
        Body = !string.IsNullOrWhiteSpace(message.Body)
            ? message.Body
            : message.CallHistoryEvent is null
                ? string.Empty
                : MainWindowPresentation.BuildCallHistorySummary(message.CallHistoryEvent);
        StatusText = semanticStatusText;
        TimeText = message.ToldByContact
            ? $"{MainWindowPresentation.FormatBubbleTime(message.CreatedAtUnixMs)} · со слов собеседника"
            : MainWindowPresentation.FormatBubbleTime(message.CreatedAtUnixMs);
        ApplyDelivery(outgoing, message.Status, semanticStatusText);
        ApplyReactions(message.Reactions);
        IsPinned = message.Pinned;
        ApplyReply(
            message.Reply,
            message.Reply is { FromMe: false } && linkedContact is not null
                ? MainWindowPresentation.ContactTitle(linkedContact.Contact)
                : "Собеседник");
        ApplyAttachment(
            attachment,
            hasAttachment ? MainWindowPresentation.BuildAttachmentStatusText(attachment!, outgoing) : string.Empty);
        AttachmentCanDownload = hasAttachment && MainWindowPresentation.CanDownloadAttachment(attachment!);
        AttachmentCanRevokeAccess = hasAttachment && MainWindowPresentation.CanRevokeAttachmentAccess(attachment!);
        AttachmentHasLocalCopy = hasAttachment &&
                                 (attachment!.LocalBlobAvailable ||
                                  !string.IsNullOrWhiteSpace(attachment.LocalSavedPath));
    }
}
