using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Chats;

internal sealed class ChatMessageListItem : INotifyPropertyChanged
{
    private bool isOutgoing;
    private string body = string.Empty;
    private Visibility bodyVisibility;
    private string statusText = string.Empty;
    private string timeText = string.Empty;
    private string dayHeaderText = string.Empty;
    private Visibility deliveryIndicatorVisibility;
    private MessageDeliveryVisualState deliveryVisualState;
    private string deliveryPrimaryGlyph = string.Empty;
    private Thickness deliveryPrimaryGlyphMargin;
    private Visibility deliveryIndicatorNeutralVisibility;
    private Visibility deliveryIndicatorErrorVisibility;
    private Visibility deliverySecondaryGlyphVisibility;
    private string deliverySecondaryGlyph = string.Empty;
    private Thickness deliverySecondaryGlyphMargin;
    private string deliveryIndicatorToolTipText = string.Empty;
    private string deliveryIndicatorAutomationName = string.Empty;
    private bool deliveryIndicatorIsError;
    private HorizontalAlignment bubbleHorizontalAlignment;
    private Thickness bubbleMargin;
    private double bubbleMaxWidth = MainWindowPresentation.TranscriptBubbleMaxWidthHardCap;
    private Visibility attachmentVisibility;
    private string attachmentFileName = string.Empty;
    private string attachmentSizeText = string.Empty;
    private Visibility attachmentFileVisibility;
    private Visibility photoVisibility;
    private string? photoLocalPath;
    private string? photoPreviewBase64;
    private double photoDisplayWidth;
    private double photoDisplayHeight;
    private int photoDecodeWidth;
    private string attachmentStatusText = string.Empty;
    private Visibility attachmentProgressVisibility;
    private double attachmentProgressValue;
    private bool attachmentCanDownload;
    private bool attachmentCanRevokeAccess;
    private bool attachmentHasLocalCopy;
    private ChatMessageRecord? sourceMessage;
    private ChatTranscriptPeer? peer;
    private bool isRunTail = true;
    private MessageSecurityDetails? securityDetails;

    public ChatMessageListItem(string messageId)
    {
        MessageId = messageId;
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    public string MessageId { get; }

    public bool IsOutgoing
    {
        get => isOutgoing;
        private set => SetField(ref isOutgoing, value);
    }

    public string Body
    {
        get => body;
        private set => SetField(ref body, value);
    }

    public Visibility BodyVisibility
    {
        get => bodyVisibility;
        private set => SetField(ref bodyVisibility, value);
    }

    public string StatusText
    {
        get => statusText;
        private set => SetField(ref statusText, value);
    }

    public string TimeText
    {
        get => timeText;
        private set => SetField(ref timeText, value);
    }

    /// When the message was written (for the day headers); 0 before the first Apply.
    public long CreatedAtUnixMs => sourceMessage?.CreatedAtUnixMs ?? 0;

    /// «Сегодня» / «8 октября» over the first message of its day, empty on the rest. Set by the
    /// transcript, which knows the neighbours (`TranscriptDayHeaders`).
    public string DayHeaderText
    {
        get => dayHeaderText;
        private set
        {
            if (string.Equals(dayHeaderText, value, StringComparison.Ordinal))
            {
                return;
            }

            dayHeaderText = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(DayHeaderVisibility));
        }
    }

    public Visibility DayHeaderVisibility
        => string.IsNullOrEmpty(dayHeaderText) ? Visibility.Collapsed : Visibility.Visible;

    public void SetDayHeader(string? text) => DayHeaderText = text ?? string.Empty;

    /// The other side of the 1:1 dialog, shared by every item: its avatar sits beside the incoming
    /// bubbles (owner, 2026-10-10, «как в Телеграме»). Set by `TranscriptLayout`.
    public ChatTranscriptPeer? Peer
    {
        get => peer;
        private set => SetField(ref peer, value);
    }

    /// Last message of a run from one side (the next one is the other side's, another day's, or
    /// none): the avatar shows on it and the gap after it is wider; inside a run bubbles stack close.
    public bool IsRunTail
    {
        get => isRunTail;
        private set
        {
            if (isRunTail == value)
            {
                return;
            }

            isRunTail = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(AvatarOpacity));
            UpdateBubbleMargin();
        }
    }

    /// The avatar column exists on incoming rows only; inside a run it is kept (as empty space) so
    /// the bubbles line up.
    public Visibility AvatarSlotVisibility => IsOutgoing ? Visibility.Collapsed : Visibility.Visible;

    public double AvatarOpacity => isRunTail ? 1d : 0d;

    internal void SetLayout(bool runTail, ChatTranscriptPeer? transcriptPeer)
    {
        Peer = transcriptPeer;
        IsRunTail = runTail;
    }

    private void UpdateBubbleMargin()
    {
        double below = isRunTail ? 10 : 3;
        BubbleMargin = IsOutgoing ? new Thickness(72, 0, 0, below) : new Thickness(0, 0, 72, below);
    }

    public Visibility DeliveryIndicatorVisibility
    {
        get => deliveryIndicatorVisibility;
        private set => SetField(ref deliveryIndicatorVisibility, value);
    }

    public MessageDeliveryVisualState DeliveryVisualState
    {
        get => deliveryVisualState;
        private set => SetField(ref deliveryVisualState, value);
    }

    public string DeliveryPrimaryGlyph
    {
        get => deliveryPrimaryGlyph;
        private set => SetField(ref deliveryPrimaryGlyph, value);
    }

    public Thickness DeliveryPrimaryGlyphMargin
    {
        get => deliveryPrimaryGlyphMargin;
        private set => SetField(ref deliveryPrimaryGlyphMargin, value);
    }

    public Visibility DeliveryIndicatorNeutralVisibility
    {
        get => deliveryIndicatorNeutralVisibility;
        private set => SetField(ref deliveryIndicatorNeutralVisibility, value);
    }

    public Visibility DeliveryIndicatorErrorVisibility
    {
        get => deliveryIndicatorErrorVisibility;
        private set => SetField(ref deliveryIndicatorErrorVisibility, value);
    }

    public Visibility DeliverySecondaryGlyphVisibility
    {
        get => deliverySecondaryGlyphVisibility;
        private set => SetField(ref deliverySecondaryGlyphVisibility, value);
    }

    public string DeliverySecondaryGlyph
    {
        get => deliverySecondaryGlyph;
        private set => SetField(ref deliverySecondaryGlyph, value);
    }

    public Thickness DeliverySecondaryGlyphMargin
    {
        get => deliverySecondaryGlyphMargin;
        private set => SetField(ref deliverySecondaryGlyphMargin, value);
    }

    public string DeliveryIndicatorToolTipText
    {
        get => deliveryIndicatorToolTipText;
        private set => SetField(ref deliveryIndicatorToolTipText, value);
    }

    public string DeliveryIndicatorAutomationName
    {
        get => deliveryIndicatorAutomationName;
        private set => SetField(ref deliveryIndicatorAutomationName, value);
    }

    public bool DeliveryIndicatorIsError
    {
        get => deliveryIndicatorIsError;
        private set => SetField(ref deliveryIndicatorIsError, value);
    }

    public HorizontalAlignment BubbleHorizontalAlignment
    {
        get => bubbleHorizontalAlignment;
        private set => SetField(ref bubbleHorizontalAlignment, value);
    }

    public Thickness BubbleMargin
    {
        get => bubbleMargin;
        private set => SetField(ref bubbleMargin, value);
    }

    public double BubbleMaxWidth
    {
        get => bubbleMaxWidth;
        private set => SetField(ref bubbleMaxWidth, value);
    }

    public Visibility AttachmentVisibility
    {
        get => attachmentVisibility;
        private set => SetField(ref attachmentVisibility, value);
    }

    public string AttachmentFileName
    {
        get => attachmentFileName;
        private set => SetField(ref attachmentFileName, value);
    }

    public string AttachmentSizeText
    {
        get => attachmentSizeText;
        private set => SetField(ref attachmentSizeText, value);
    }

    /// The file plate — for an attachment that is not shown as a photo.
    public Visibility AttachmentFileVisibility
    {
        get => attachmentFileVisibility;
        private set => SetField(ref attachmentFileVisibility, value);
    }

    public Visibility PhotoVisibility
    {
        get => photoVisibility;
        private set => SetField(ref photoVisibility, value);
    }

    /// The file a photo is drawn from once it is on this machine; until then the preview.
    public string? PhotoLocalPath
    {
        get => photoLocalPath;
        private set => SetField(ref photoLocalPath, value);
    }

    public string? PhotoPreviewBase64
    {
        get => photoPreviewBase64;
        private set => SetField(ref photoPreviewBase64, value);
    }

    public double PhotoDisplayWidth
    {
        get => photoDisplayWidth;
        private set => SetField(ref photoDisplayWidth, value);
    }

    public double PhotoDisplayHeight
    {
        get => photoDisplayHeight;
        private set => SetField(ref photoDisplayHeight, value);
    }

    /// Decoded at twice the shown width (sharp on a 200% screen), never at the file's own size.
    public int PhotoDecodeWidth
    {
        get => photoDecodeWidth;
        private set => SetField(ref photoDecodeWidth, value);
    }

    public string AttachmentStatusText
    {
        get => attachmentStatusText;
        private set
        {
            SetField(ref attachmentStatusText, value);
            OnPropertyChanged(nameof(AttachmentStatusVisibility));
        }
    }

    // A file that is simply here says nothing, and the empty line takes no room.
    public Visibility AttachmentStatusVisibility =>
        string.IsNullOrEmpty(attachmentStatusText) ? Visibility.Collapsed : Visibility.Visible;

    public Visibility AttachmentProgressVisibility
    {
        get => attachmentProgressVisibility;
        private set => SetField(ref attachmentProgressVisibility, value);
    }

    public double AttachmentProgressValue
    {
        get => attachmentProgressValue;
        private set => SetField(ref attachmentProgressValue, value);
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

    internal void UpdateBubbleMaxWidth(double maxWidth)
    {
        if (double.IsNaN(maxWidth) || double.IsInfinity(maxWidth) || maxWidth <= 0d)
        {
            maxWidth = MainWindowPresentation.TranscriptBubbleMaxWidthHardCap;
        }

        BubbleMaxWidth = maxWidth;
    }

    internal void ApplyFrom(
        ChatMessageRecord message,
        MessageSecurityDetails? messageSecurityDetails,
        ChatSecuritySummary? chatSecuritySummary,
        ContactCardSnapshot? linkedContact)
    {
        _ = chatSecuritySummary;
        _ = linkedContact;

        if (!string.Equals(MessageId, message.Id, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                $"Cannot apply message '{message.Id}' to transcript item '{MessageId}'.");
        }

        SourceMessage = message;
        SecurityDetails = messageSecurityDetails;

        bool outgoing = message.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase);
        AttachmentRecord? attachment = message.Attachment;
        string resolvedBody = !string.IsNullOrWhiteSpace(message.Body)
            ? message.Body
            : message.CallHistoryEvent is null
                ? string.Empty
                : MainWindowPresentation.BuildCallHistorySummary(message.CallHistoryEvent);
        bool hasBody = !string.IsNullOrWhiteSpace(resolvedBody);
        bool hasAttachment = attachment is not null;
        string semanticStatusText = MainWindowPresentation.MapMessageStatus(message.Status);
        MessageDeliveryVisualState nextDeliveryVisualState = outgoing
            ? MainWindowPresentation.MapMessageDeliveryVisualState(message.Status)
            : MessageDeliveryVisualState.None;

        IsOutgoing = outgoing;
        OnPropertyChanged(nameof(AvatarSlotVisibility));
        Body = resolvedBody;
        BodyVisibility = hasBody ? Visibility.Visible : Visibility.Collapsed;
        StatusText = semanticStatusText;
        TimeText = message.ToldByContact
            ? $"{MainWindowPresentation.FormatBubbleTime(message.CreatedAtUnixMs)} · со слов собеседника"
            : MainWindowPresentation.FormatBubbleTime(message.CreatedAtUnixMs);
        DeliveryIndicatorVisibility = MainWindowPresentation.BuildMessageDeliveryIndicatorVisibility(
            outgoing,
            nextDeliveryVisualState);
        DeliveryVisualState = nextDeliveryVisualState;
        DeliveryPrimaryGlyph = MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(nextDeliveryVisualState);
        DeliveryPrimaryGlyphMargin = MainWindowPresentation.BuildMessageDeliveryPrimaryGlyphMargin(
            nextDeliveryVisualState);
        DeliveryIndicatorNeutralVisibility = MainWindowPresentation.BuildMessageDeliveryNeutralVisibility(
            outgoing,
            nextDeliveryVisualState);
        DeliveryIndicatorErrorVisibility = MainWindowPresentation.BuildMessageDeliveryErrorVisibility(
            outgoing,
            nextDeliveryVisualState);
        DeliverySecondaryGlyphVisibility = MainWindowPresentation.BuildMessageDeliverySecondaryGlyphVisibility(
            nextDeliveryVisualState);
        DeliverySecondaryGlyph = MainWindowPresentation.BuildMessageDeliverySecondaryGlyph(nextDeliveryVisualState);
        DeliverySecondaryGlyphMargin = MainWindowPresentation.BuildMessageDeliverySecondaryGlyphMargin(
            nextDeliveryVisualState);
        DeliveryIndicatorToolTipText = outgoing
            ? MainWindowPresentation.BuildMessageDeliveryToolTipText(semanticStatusText)
            : string.Empty;
        DeliveryIndicatorAutomationName = outgoing
            ? MainWindowPresentation.BuildMessageDeliveryAutomationName(semanticStatusText)
            : string.Empty;
        DeliveryIndicatorIsError = MainWindowPresentation.IsMessageDeliveryError(nextDeliveryVisualState);
        BubbleHorizontalAlignment = outgoing ? HorizontalAlignment.Right : HorizontalAlignment.Left;
        UpdateBubbleMargin();
        // Bubble fill + text colours (outgoing = accent, incoming = neutral surface, with
        // contrasting foregrounds) are resolved at render time by ChatBubbleBrushConverter off
        // IsOutgoing — this item stays a headless-constructable POCO (no WinRT Brush objects,
        // which can't be created off the UI thread / in unit tests).

        AttachmentVisibility = hasAttachment ? Visibility.Visible : Visibility.Collapsed;
        AttachmentFileName = attachment?.FileName ?? string.Empty;
        AttachmentSizeText = hasAttachment
            ? MainWindowPresentation.FormatFileSize(attachment!.FileSizeBytes)
            : string.Empty;
        AttachmentPresentation? photo = attachment?.Presentation is { IsPhoto: true } shownAsPhoto ? shownAsPhoto : null;
        PhotoVisibility = photo is not null ? Visibility.Visible : Visibility.Collapsed;
        AttachmentFileVisibility = hasAttachment && photo is null ? Visibility.Visible : Visibility.Collapsed;
        (double photoWidth, double photoHeight) = photo is null
            ? (0d, 0d)
            : MainWindowPresentation.FitPhotoInBubble(photo.Width, photo.Height);
        PhotoDisplayWidth = photoWidth;
        PhotoDisplayHeight = photoHeight;
        PhotoDecodeWidth = (int)Math.Ceiling(photoWidth * 2);
        PhotoLocalPath = photo is null ? null : attachment!.LocalCopyPath;
        PhotoPreviewBase64 = photo?.PreviewJpegBase64;
        AttachmentStatusText = hasAttachment
            ? MainWindowPresentation.BuildAttachmentStatusText(attachment!, outgoing)
            : string.Empty;

        double progressValue = hasAttachment
            ? MainWindowPresentation.BuildAttachmentProgressValue(attachment!)
            : 0d;
        AttachmentProgressValue = progressValue;
        AttachmentProgressVisibility =
            hasAttachment && MainWindowPresentation.ShouldShowAttachmentProgress(attachment!)
                ? Visibility.Visible
                : Visibility.Collapsed;

        AttachmentCanDownload = hasAttachment && MainWindowPresentation.CanDownloadAttachment(attachment!);
        AttachmentCanRevokeAccess = hasAttachment && MainWindowPresentation.CanRevokeAttachmentAccess(attachment!);
        AttachmentHasLocalCopy = hasAttachment &&
                                 (attachment!.LocalBlobAvailable ||
                                  !string.IsNullOrWhiteSpace(attachment.LocalSavedPath));
    }

    private void OnPropertyChanged([CallerMemberName] string? propertyName = null)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    private void SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return;
        }

        field = value;
        OnPropertyChanged(propertyName);
    }
}
