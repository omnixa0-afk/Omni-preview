using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Shared;

/// What one message shows, whichever conversation it is in: a 1:1 chat, a group or a discussion. The
/// one template, `MessageBubble`, draws this and nothing else, so a new part of a message is added once
/// and appears everywhere (owner, 2026-11-04). Stays free of WinRT brushes: it is built in unit tests.
public class MessageRow : INotifyPropertyChanged
{
    private bool isOutgoing;
    private string body = string.Empty;
    private Visibility bodyVisibility = Visibility.Collapsed;
    private string timeText = string.Empty;
    private string dayHeaderText = string.Empty;
    private string systemText = string.Empty;
    private string senderName = string.Empty;
    private bool showSender;
    private string noteText = string.Empty;
    private string chipText = string.Empty;
    private string quoteAuthor = string.Empty;
    private string quoteText = string.Empty;
    private Visibility deliveryIndicatorVisibility = Visibility.Collapsed;
    private MessageDeliveryVisualState deliveryVisualState;
    private string deliveryPrimaryGlyph = string.Empty;
    private Thickness deliveryPrimaryGlyphMargin;
    private Visibility deliveryIndicatorNeutralVisibility = Visibility.Collapsed;
    private Visibility deliveryIndicatorErrorVisibility = Visibility.Collapsed;
    private Visibility deliverySecondaryGlyphVisibility = Visibility.Collapsed;
    private string deliverySecondaryGlyph = string.Empty;
    private Thickness deliverySecondaryGlyphMargin;
    private string deliveryIndicatorToolTipText = string.Empty;
    private string deliveryIndicatorAutomationName = string.Empty;
    private bool deliveryIndicatorIsError;
    private HorizontalAlignment bubbleHorizontalAlignment = HorizontalAlignment.Left;
    private Thickness bubbleMargin = new(0, 0, 72, 10);
    private double bubbleMaxWidth = MainWindowPresentation.TranscriptBubbleMaxWidthHardCap;
    private Visibility attachmentVisibility = Visibility.Collapsed;
    private string attachmentFileName = string.Empty;
    private string attachmentSizeText = string.Empty;
    private Visibility attachmentFileVisibility = Visibility.Collapsed;
    private Visibility photoVisibility = Visibility.Collapsed;
    private string? photoLocalPath;
    private string? photoPreviewBase64;
    private double photoDisplayWidth;
    private double photoDisplayHeight;
    private int photoDecodeWidth;
    private string attachmentStatusText = string.Empty;
    private Visibility attachmentProgressVisibility = Visibility.Collapsed;
    private double attachmentProgressValue;
    private Visibility voiceVisibility = Visibility.Collapsed;
    private byte[] voiceWaveform = [];
    private int voiceDurationMs;
    private string voiceTimeText = string.Empty;
    private double voiceProgress;
    private bool voicePlaying;
    private bool voiceLoading;
    private Visibility videoNoteVisibility = Visibility.Collapsed;
    private string? videoNotePreviewBase64;
    private int videoNoteDurationMs;
    private string videoNoteTimeText = string.Empty;
    private double videoNoteProgress;
    private bool videoNotePlaying;
    private ChatTranscriptPeer? peer;
    private bool isRunTail = true;
    private IReadOnlyList<MessageReactionChip> reactions = [];
    private bool isPinned;
    private bool isSelecting;
    private bool isSelected;

    public MessageRow(string messageId)
    {
        MessageId = messageId;
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    public string MessageId { get; }

    public bool IsOutgoing
    {
        get => isOutgoing;
        internal set
        {
            if (SetField(ref isOutgoing, value))
            {
                OnPropertyChanged(nameof(AvatarSlotVisibility));
                OnPropertyChanged(nameof(SenderVisibility));
            }
            BubbleHorizontalAlignment = value ? HorizontalAlignment.Right : HorizontalAlignment.Left;
            UpdateBubbleMargin();
        }
    }

    public string Body
    {
        get => body;
        internal set
        {
            SetField(ref body, value);
            BodyVisibility = string.IsNullOrWhiteSpace(value) ? Visibility.Collapsed : Visibility.Visible;
        }
    }

    public Visibility BodyVisibility
    {
        get => bodyVisibility;
        private set => SetField(ref bodyVisibility, value);
    }

    public string TimeText
    {
        get => timeText;
        internal set => SetField(ref timeText, value);
    }

    /// A line of the conversation itself («X в группе»): centred, with no bubble.
    public string SystemText
    {
        get => systemText;
        internal set
        {
            if (SetField(ref systemText, value))
            {
                OnPropertyChanged(nameof(SystemVisibility));
                OnPropertyChanged(nameof(BubbleVisibility));
                OnPropertyChanged(nameof(AvatarSlotVisibility));
            }
        }
    }

    public Visibility SystemVisibility => systemText.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    public Visibility BubbleVisibility => systemText.Length > 0 ? Visibility.Collapsed : Visibility.Visible;

    /// «Сегодня» / «8 октября» over the first message of its day, empty on the rest.
    public string DayHeaderText
    {
        get => dayHeaderText;
        private set
        {
            if (SetField(ref dayHeaderText, value))
            {
                OnPropertyChanged(nameof(DayHeaderVisibility));
            }
        }
    }

    public Visibility DayHeaderVisibility
        => string.IsNullOrEmpty(dayHeaderText) ? Visibility.Collapsed : Visibility.Visible;

    public void SetDayHeader(string? text) => DayHeaderText = text ?? string.Empty;

    /// The author's name over an incoming message, where there is more than one other side.
    public string SenderName
    {
        get => senderName;
        internal set => SetField(ref senderName, value);
    }

    /// The first message of a run names its author; the rest of the run does not repeat it.
    internal bool ShowSender
    {
        get => showSender;
        set
        {
            if (SetField(ref showSender, value))
            {
                OnPropertyChanged(nameof(SenderVisibility));
                OnPropertyChanged(nameof(AuthorLineVisibility));
            }
        }
    }

    public Visibility SenderVisibility => showSender && !isOutgoing ? Visibility.Visible : Visibility.Collapsed;

    /// Beside the author: who they are, such as whose assistant.
    public string NoteText
    {
        get => noteText;
        internal set
        {
            if (SetField(ref noteText, value))
            {
                OnPropertyChanged(nameof(NoteVisibility));
                OnPropertyChanged(nameof(AuthorLineVisibility));
            }
        }
    }

    public Visibility NoteVisibility => noteText.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    /// Beside the author: what the message is («поручение: …»).
    public string ChipText
    {
        get => chipText;
        internal set
        {
            if (SetField(ref chipText, value))
            {
                OnPropertyChanged(nameof(ChipVisibility));
                OnPropertyChanged(nameof(AuthorLineVisibility));
            }
        }
    }

    public Visibility ChipVisibility => chipText.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    public Visibility AuthorLineVisibility
        => SenderVisibility == Visibility.Visible || noteText.Length > 0 || chipText.Length > 0
            ? Visibility.Visible
            : Visibility.Collapsed;

    /// The message this one answers: whose it is and how it began.
    public string QuoteAuthor
    {
        get => quoteAuthor;
        private set => SetField(ref quoteAuthor, value);
    }

    public string QuoteText
    {
        get => quoteText;
        private set
        {
            if (SetField(ref quoteText, value))
            {
                OnPropertyChanged(nameof(QuoteVisibility));
            }
        }
    }

    public Visibility QuoteVisibility => quoteText.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    /// The answered message's id: a tap on the quote goes there.
    public string? QuotedMessageId { get; private set; }

    internal void SetQuote(string? messageId, string author, string text)
    {
        QuotedMessageId = messageId;
        QuoteAuthor = author;
        QuoteText = text.ReplaceLineEndings(" ");
    }

    /// The emoji on the message.
    public IReadOnlyList<MessageReactionChip> Reactions
    {
        get => reactions;
        private set
        {
            reactions = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(ReactionsVisibility));
        }
    }

    public bool IsPinned
    {
        get => isPinned;
        set
        {
            if (isPinned == value)
            {
                return;
            }

            isPinned = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(PinVisibility));
        }
    }

    public Visibility PinVisibility => isPinned ? Visibility.Visible : Visibility.Collapsed;

    public bool IsSelecting => isSelecting;

    public Visibility SelectingVisibility => isSelecting ? Visibility.Visible : Visibility.Collapsed;

    public Visibility SelectedVisibility => isSelected ? Visibility.Visible : Visibility.Collapsed;

    internal void SetSelection(bool selecting, bool selected)
    {
        if (isSelecting == selecting && isSelected == selected)
        {
            return;
        }

        isSelecting = selecting;
        isSelected = selected;
        OnPropertyChanged(nameof(SelectingVisibility));
        OnPropertyChanged(nameof(SelectedVisibility));
    }

    /// How a reply or a pin names a message with no text.
    internal string AttachmentLabel
        => voiceVisibility == Visibility.Visible ? "Голосовое сообщение"
            : videoNoteVisibility == Visibility.Visible ? "Видеосообщение"
            : photoVisibility == Visibility.Visible ? "Фото"
            : attachmentFileName;

    /// The file on this device, to forward; none until it is downloaded.
    internal string? AttachmentLocalPath { get; private set; }

    public Visibility ReactionsVisibility => reactions.Count > 0 ? Visibility.Visible : Visibility.Collapsed;

    /// Our emoji on it, if any: choosing it again takes it away.
    internal string MyReaction => reactions.FirstOrDefault(r => r.Mine)?.Emoji ?? string.Empty;

    /// A signature of the emoji, so a row redrawn from the same state keeps its chips.
    internal string ReactionsSignature => string.Join(';', reactions.Select(r => $"{r.Emoji},{r.Count},{r.Mine},{r.ToolTip}"));

    internal void ApplyReactions(IReadOnlyList<MessageReactionView>? views)
    {
        List<MessageReactionChip> next = (views ?? []).Select(v => new MessageReactionChip(v, MessageId)).ToList();
        if (next.Count == reactions.Count && next.SequenceEqual(reactions))
        {
            return;
        }

        Reactions = next;
    }

    /// The core's quote of the answered message; `otherSide` names its author where the core has no name
    /// for them (a 1:1 chat, where there is only one other side).
    internal void ApplyReply(MessageReplyPreview? reply, string otherSide)
    {
        if (reply is null)
        {
            SetQuote(null, string.Empty, string.Empty);
            return;
        }

        string author = reply.FromMe ? "Вы" : reply.Author.Length > 0 ? reply.Author : otherSide;
        SetQuote(reply.MessageId, author, reply.Text.Length > 0 ? reply.Text : "Сообщение");
    }

    /// The other side's look, shared by its rows where one person fills the conversation.
    public ChatTranscriptPeer? Peer
    {
        get => peer;
        internal set => SetField(ref peer, value);
    }

    /// Last message of a run from one side: the avatar shows on it and the gap after it is wider;
    /// inside a run bubbles stack close.
    public bool IsRunTail
    {
        get => isRunTail;
        internal set
        {
            if (SetField(ref isRunTail, value))
            {
                OnPropertyChanged(nameof(AvatarOpacity));
            }
            UpdateBubbleMargin();
        }
    }

    /// The avatar column exists on incoming rows only; inside a run it is kept (as empty space) so
    /// the bubbles line up.
    public Visibility AvatarSlotVisibility
        => isOutgoing || systemText.Length > 0 ? Visibility.Collapsed : Visibility.Visible;

    public double AvatarOpacity => isRunTail ? 1d : 0d;

    private void UpdateBubbleMargin()
    {
        double below = isRunTail ? 10 : 3;
        BubbleMargin = isOutgoing ? new Thickness(72, 0, 0, below) : new Thickness(0, 0, 72, below);
    }

    public Visibility DeliveryIndicatorVisibility
    {
        get => deliveryIndicatorVisibility;
        private set => SetField(ref deliveryIndicatorVisibility, value);
    }

    internal MessageDeliveryVisualState DeliveryVisualState
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
    /// A voice message: the play button and its outline, instead of the file plate.
    public Visibility VoiceVisibility
    {
        get => voiceVisibility;
        private set => SetField(ref voiceVisibility, value);
    }

    public byte[] VoiceWaveform
    {
        get => voiceWaveform;
        private set
        {
            voiceWaveform = value;
            OnPropertyChanged();
        }
    }

    public string VoiceTimeText
    {
        get => voiceTimeText;
        private set => SetField(ref voiceTimeText, value);
    }

    public double VoiceProgress
    {
        get => voiceProgress;
        private set => SetField(ref voiceProgress, value);
    }

    public string VoiceGlyph => voicePlaying ? "\uE769" : voiceLoading ? "\uE895" : "\uE768";

    public string VoiceActionName => voicePlaying ? "Пауза" : "Слушать";

    internal int VoiceDurationMs => voiceDurationMs;

    internal bool VoicePlaying => voicePlaying;

    /// What the player says of this message: playing or not, how far. A voice message nobody plays
    /// shows its length.
    internal void SetVoiceState(bool playing, bool loading, double progress, int positionMs)
    {
        voicePlaying = playing;
        voiceLoading = loading;
        OnPropertyChanged(nameof(VoiceGlyph));
        OnPropertyChanged(nameof(VoiceActionName));
        VoiceProgress = progress;
        VoiceTimeText = MainWindowPresentation.FormatVoiceTime(playing || progress > 0 ? positionMs : voiceDurationMs);
    }

    /// A video message: its circle, instead of the file plate, and no bubble around it.
    public Visibility VideoNoteVisibility
    {
        get => videoNoteVisibility;
        private set
        {
            if (SetField(ref videoNoteVisibility, value))
            {
                OnPropertyChanged(nameof(IsVideoNote));
                OnPropertyChanged(nameof(BubbleBorderThickness));
                OnPropertyChanged(nameof(BubblePadding));
            }
        }
    }

    public bool IsVideoNote => videoNoteVisibility == Visibility.Visible;

    /// The circle's side, as in Telegram (owner, 2026-11-05); a narrow conversation takes what fits.
    public double VideoNoteSide => Math.Min(VideoNoteFullSide, bubbleMaxWidth);

    private const double VideoNoteFullSide = 360;

    public Thickness BubbleBorderThickness => IsVideoNote ? new Thickness(0) : new Thickness(1);

    public Thickness BubblePadding => IsVideoNote ? new Thickness(0) : new Thickness(12, 8, 10, 6);

    public string? VideoNotePreviewBase64
    {
        get => videoNotePreviewBase64;
        private set => SetField(ref videoNotePreviewBase64, value);
    }

    public string VideoNoteTimeText
    {
        get => videoNoteTimeText;
        private set => SetField(ref videoNoteTimeText, value);
    }

    public double VideoNoteProgress
    {
        get => videoNoteProgress;
        private set => SetField(ref videoNoteProgress, value);
    }

    public Visibility VideoNoteProgressVisibility => videoNotePlaying || videoNoteProgress > 0 ? Visibility.Visible : Visibility.Collapsed;

    /// The sign over a circle at rest; while it plays the picture is the sign.
    public Visibility VideoNotePlaySignVisibility => videoNotePlaying ? Visibility.Collapsed : Visibility.Visible;

    internal void SetVideoNoteState(bool playing, double progress, int positionMs)
    {
        videoNotePlaying = playing;
        VideoNoteProgress = progress * 100;
        OnPropertyChanged(nameof(VideoNoteProgressVisibility));
        OnPropertyChanged(nameof(VideoNotePlaySignVisibility));
        VideoNoteTimeText = MainWindowPresentation.FormatVoiceTime(playing || progress > 0 ? positionMs : videoNoteDurationMs);
    }

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

    internal void UpdateBubbleMaxWidth(double maxWidth)
    {
        if (double.IsNaN(maxWidth) || double.IsInfinity(maxWidth) || maxWidth <= 0d)
        {
            maxWidth = MainWindowPresentation.TranscriptBubbleMaxWidthHardCap;
        }

        BubbleMaxWidth = maxWidth;
        OnPropertyChanged(nameof(VideoNoteSide));
    }

    /// The photo or file of the message; `statusText` is the conversation's own wording of its state.
    internal void ApplyAttachment(AttachmentRecord? attachment, string statusText)
    {
        bool hasAttachment = attachment is not null;
        AttachmentVisibility = hasAttachment ? Visibility.Visible : Visibility.Collapsed;
        AttachmentFileName = attachment?.FileName ?? string.Empty;
        AttachmentLocalPath = attachment?.LocalCopyPath;
        AttachmentSizeText = hasAttachment
            ? MainWindowPresentation.FormatFileSize(attachment!.FileSizeBytes)
            : string.Empty;
        AttachmentPresentation? photo = attachment?.Presentation is { IsPhoto: true } shownAsPhoto ? shownAsPhoto : null;
        PhotoVisibility = photo is not null ? Visibility.Visible : Visibility.Collapsed;
        AttachmentPresentation? voice = attachment?.Presentation is { IsVoice: true } shownAsVoice ? shownAsVoice : null;
        VoiceVisibility = voice is not null ? Visibility.Visible : Visibility.Collapsed;
        AttachmentPresentation? videoNote = attachment?.Presentation is { IsVideoNote: true } shownAsCircle ? shownAsCircle : null;
        VideoNoteVisibility = videoNote is not null ? Visibility.Visible : Visibility.Collapsed;
        AttachmentFileVisibility = hasAttachment && photo is null && voice is null && videoNote is null ? Visibility.Visible : Visibility.Collapsed;
        if (videoNote is not null)
        {
            videoNoteDurationMs = videoNote.DurationMs ?? 0;
            VideoNotePreviewBase64 = videoNote.PreviewJpegBase64;
            if (!videoNotePlaying && videoNoteProgress <= 0)
            {
                VideoNoteTimeText = MainWindowPresentation.FormatVoiceTime(videoNoteDurationMs);
            }
        }

        if (voice is not null)
        {
            voiceDurationMs = voice.DurationMs ?? 0;
            VoiceWaveform = MainWindowPresentation.DecodeVoiceWaveform(voice.WaveformBase64);
            if (!voicePlaying && voiceProgress <= 0)
            {
                VoiceTimeText = MainWindowPresentation.FormatVoiceTime(voiceDurationMs);
            }

            VoicePlayer.Reattach(this);
        }

        (double photoWidth, double photoHeight) = photo is null
            ? (0d, 0d)
            : MainWindowPresentation.FitPhotoInBubble(photo.Width, photo.Height);
        PhotoDisplayWidth = photoWidth;
        PhotoDisplayHeight = photoHeight;
        PhotoDecodeWidth = (int)Math.Ceiling(photoWidth * 2);
        PhotoLocalPath = photo is null ? null : attachment!.LocalCopyPath;
        PhotoPreviewBase64 = photo?.PreviewJpegBase64;
        AttachmentStatusText = statusText;
        AttachmentProgressValue = hasAttachment
            ? MainWindowPresentation.BuildAttachmentProgressValue(attachment!)
            : 0d;
        AttachmentProgressVisibility =
            hasAttachment && MainWindowPresentation.ShouldShowAttachmentProgress(attachment!)
                ? Visibility.Visible
                : Visibility.Collapsed;
    }

    /// The ticks of an outgoing message; `semanticStatusText` says the same state in words.
    internal void ApplyDelivery(bool outgoing, string status, string semanticStatusText)
    {
        MessageDeliveryVisualState next = outgoing
            ? MainWindowPresentation.MapMessageDeliveryVisualState(status)
            : MessageDeliveryVisualState.None;
        DeliveryIndicatorVisibility = MainWindowPresentation.BuildMessageDeliveryIndicatorVisibility(outgoing, next);
        DeliveryVisualState = next;
        DeliveryPrimaryGlyph = MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(next);
        DeliveryPrimaryGlyphMargin = MainWindowPresentation.BuildMessageDeliveryPrimaryGlyphMargin(next);
        DeliveryIndicatorNeutralVisibility = MainWindowPresentation.BuildMessageDeliveryNeutralVisibility(outgoing, next);
        DeliveryIndicatorErrorVisibility = MainWindowPresentation.BuildMessageDeliveryErrorVisibility(outgoing, next);
        DeliverySecondaryGlyphVisibility = MainWindowPresentation.BuildMessageDeliverySecondaryGlyphVisibility(next);
        DeliverySecondaryGlyph = MainWindowPresentation.BuildMessageDeliverySecondaryGlyph(next);
        DeliverySecondaryGlyphMargin = MainWindowPresentation.BuildMessageDeliverySecondaryGlyphMargin(next);
        DeliveryIndicatorToolTipText = outgoing
            ? MainWindowPresentation.BuildMessageDeliveryToolTipText(semanticStatusText)
            : string.Empty;
        DeliveryIndicatorAutomationName = outgoing
            ? MainWindowPresentation.BuildMessageDeliveryAutomationName(semanticStatusText)
            : string.Empty;
        DeliveryIndicatorIsError = MainWindowPresentation.IsMessageDeliveryError(next);
    }

    protected void OnPropertyChanged([CallerMemberName] string? propertyName = null)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    protected bool SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return false;
        }

        field = value;
        OnPropertyChanged(propertyName);
        return true;
    }
}

/// One emoji on a message, as its chip draws it.
public sealed record MessageReactionChip
{
    internal MessageReactionChip(MessageReactionView view, string messageId)
    {
        Emoji = view.Emoji;
        Count = view.Count;
        Mine = view.Mine;
        MessageId = messageId;
        ToolTip = view.Who is { Count: > 0 } who ? string.Join(", ", who) : view.Emoji;
    }

    public string Emoji { get; }

    public int Count { get; }

    public bool Mine { get; }

    public string MessageId { get; }

    public string ToolTip { get; }

    public string Label => Count > 1 ? $"{Emoji} {Count}" : Emoji;

    public Visibility MineVisibility => Mine ? Visibility.Visible : Visibility.Collapsed;

    public string AutomationName => Mine ? $"{Emoji}, ваша реакция" : $"Реакция {Emoji}";
}
