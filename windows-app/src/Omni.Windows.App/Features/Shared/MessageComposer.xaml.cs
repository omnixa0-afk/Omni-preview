using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;
using Windows.UI.Core;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The composer (see MessageComposer.xaml). <see cref="SendRequested"/> comes from the button and
/// from Enter alike, and from Enter only while the send button is enabled; <see cref="Above"/>
/// takes the host's lines over the field. Where the host gives <see cref="SetMentionCandidates"/>,
/// `@` in the field offers whom to name; <see cref="TakeMentions"/> says who was chosen.
/// </summary>
public sealed partial class MessageComposer : UserControl
{
    public static readonly DependencyProperty AboveProperty = DependencyProperty.Register(
        nameof(Above), typeof(object), typeof(MessageComposer),
        new PropertyMetadata(null, (d, e) => ((MessageComposer)d).AbovePart.Content = e.NewValue));

    public MessageComposer()
    {
        InitializeComponent();
        // The microphone follows what the paperclip says of this conversation.
        AttachPart.RegisterPropertyChangedCallback(IsEnabledProperty, (_, _) => UpdateMic());
        Unloaded += (_, _) => CancelVoice();
        VideoNoteLivePart.Loaded += (_, _) => CutLiveCircle();
        Loaded += (_, _) =>
        {
            RecordKindChanged += ShowRecordKind;
            ShowRecordKind();
        };
        Unloaded += (_, _) => RecordKindChanged -= ShowRecordKind;
        // A button takes the pointer's events for itself: these see them all the same.
        RecordPart.AddHandler(PointerPressedEvent, new PointerEventHandler(OnRecordPointerPressed), true);
        RecordPart.AddHandler(PointerMovedEvent, new PointerEventHandler(OnRecordPointerMoved), true);
        RecordPart.AddHandler(PointerReleasedEvent, new PointerEventHandler(OnRecordPointerReleased), true);
        RecordPart.AddHandler(PointerCaptureLostEvent, new PointerEventHandler(OnRecordPointerLost), true);
        SizeChanged += (_, _) => PlaceFloating();
        ShowRecordKind();
        UpdateMic();
    }

    public object? Above
    {
        get => GetValue(AboveProperty);
        set => SetValue(AboveProperty, value);
    }

    /// The paperclip; off where the conversation cannot take files (a group we are no longer in).
    public bool ShowAttach
    {
        get => attachWanted;
        set
        {
            attachWanted = value;
            AttachPart.Visibility = value && !Recording ? Visibility.Visible : Visibility.Collapsed;
            UpdateMic();
        }
    }

    private bool attachWanted = true;

    public event EventHandler? SendRequested;

    /// The cross on the file about to go.
    public event EventHandler? DraftRemoveRequested;

    /// «Сжать как фото» ticked (true) or unticked.
    public event EventHandler<bool>? DraftAsPhotoChanged;

    private bool showingDraft;

    /// The file about to go; a picture that can go as a photo shows itself and the tick.
    public void ShowDraft(string path, string name, string sizeText, bool photoCandidate, bool asPhoto)
    {
        showingDraft = true;
        DraftNamePart.Text = name;
        DraftSizePart.Text = sizeText;
        DraftAsPhotoPart.Visibility = photoCandidate ? Visibility.Visible : Visibility.Collapsed;
        DraftAsPhotoPart.IsChecked = asPhoto;
        DraftThumbnailPart.LocalPath = photoCandidate ? path : null;
        DraftThumbnailPart.Visibility = photoCandidate ? Visibility.Visible : Visibility.Collapsed;
        DraftFileIconPart.Visibility = photoCandidate ? Visibility.Collapsed : Visibility.Visible;
        DraftPart.Visibility = Visibility.Visible;
        showingDraft = false;
        UpdateMic();
    }

    public void ClearDraft()
    {
        DraftPart.Visibility = Visibility.Collapsed;
        DraftThumbnailPart.LocalPath = null;
        UpdateMic();
    }

    private void OnDraftRemoveClicked(object sender, RoutedEventArgs e) => DraftRemoveRequested?.Invoke(this, EventArgs.Empty);

    private void OnDraftAsPhotoToggled(object sender, RoutedEventArgs e)
    {
        if (!showingDraft)
        {
            DraftAsPhotoChanged?.Invoke(this, DraftAsPhotoPart.IsChecked == true);
        }
    }

    /// The cross on the message being answered.
    public event EventHandler? ReplyDismissed;

    /// The message the next one answers: whose it is and how it began.
    public void ShowReply(string author, string text)
    {
        ReplyAuthorPart.Text = author;
        ReplyTextPart.Text = text.ReplaceLineEndings(" ");
        ReplyPart.Visibility = Visibility.Visible;
        InputPart.Focus(FocusState.Programmatic);
    }

    public void ClearReply() => ReplyPart.Visibility = Visibility.Collapsed;

    private void OnReplyDismissClicked(object sender, RoutedEventArgs e) => ReplyDismissed?.Invoke(this, EventArgs.Empty);

    public event EventHandler? AttachRequested;

    // ---- `@` -------------------------------------------------------------------------------------

    /// How many letters may follow `@` while still looking for a name: names can have spaces.
    private const int MentionQueryMax = 40;

    private const int MentionRowsShown = 6;

    private IReadOnlyList<MentionCandidate> mentionCandidates = [];
    private readonly List<MentionCandidate> mentionsChosen = [];
    private int mentionStart = -1;

    /// Who a line here can name; none: `@` is a plain letter.
    public void SetMentionCandidates(IReadOnlyList<MentionCandidate> candidates)
    {
        mentionCandidates = MentionCandidate.Distinct(candidates);
        if (candidates.Count == 0)
        {
            CloseMentions();
        }
    }

    /// The accounts chosen from the list whose name is still in the text, in the order chosen. The core
    /// binds the rest of the names and refuses one it cannot tell (`resolve_mentions`).
    public List<string> TakeMentions()
    {
        string text = InputPart.Text ?? string.Empty;
        return mentionsChosen
            .Where(chosen => text.Contains("@" + chosen.Name, StringComparison.OrdinalIgnoreCase))
            .Select(chosen => chosen.Account)
            .ToList();
    }

    /// Put a member's name at the start of the line, as if chosen from the list; an answer to an agent
    /// addresses it this way.
    public void AddressTo(MentionCandidate candidate)
    {
        string text = InputPart.Text ?? string.Empty;
        if (mentionsChosen.Any(chosen => chosen.Account == candidate.Account) && text.Contains("@" + candidate.Name, StringComparison.OrdinalIgnoreCase))
        {
            return;
        }

        mentionsChosen.Add(candidate);
        InputPart.Text = $"@{candidate.Name} {text}";
        InputPart.SelectionStart = InputPart.Text.Length;
    }

    private void OnInputTextChanged(object sender, TextChangedEventArgs e)
    {
        if (string.IsNullOrEmpty(InputPart.Text))
        {
            mentionsChosen.Clear();
        }

        UpdateMentions();
        UpdateMic();
        TextEdited?.Invoke(this, EventArgs.Empty);
    }

    /// The text changed, by a person or by the host.
    public event EventHandler? TextEdited;

    private void OnInputSelectionChanged(object sender, RoutedEventArgs e) => UpdateMentions();

    private void UpdateMentions()
    {
        if (mentionCandidates.Count == 0)
        {
            return;
        }

        string text = InputPart.Text ?? string.Empty;
        int caret = Math.Min(InputPart.SelectionStart, text.Length);
        int at = -1;
        for (int i = caret - 1; i >= 0 && caret - i <= MentionQueryMax + 1; i--)
        {
            if (text[i] is '\r' or '\n')
            {
                break;
            }

            if (text[i] == '@')
            {
                at = i;
                break;
            }
        }

        // `a@b` is an address, not a mention.
        if (at < 0 || InputPart.SelectionLength > 0 || (at > 0 && !char.IsWhiteSpace(text[at - 1])))
        {
            CloseMentions();
            return;
        }

        string query = text[(at + 1)..caret];
        List<MentionCandidate> fit = mentionCandidates
            .Where(c => c.Name.Contains(query, StringComparison.CurrentCultureIgnoreCase) ||
                        c.Note.Contains(query, StringComparison.CurrentCultureIgnoreCase))
            .OrderByDescending(c => c.Name.StartsWith(query, StringComparison.CurrentCultureIgnoreCase))
            .Take(MentionRowsShown)
            .ToList();
        // A name typed out whole, and nothing more to offer for it: the list is done.
        if (fit.Count == 0 || (fit.Count == 1 && string.Equals(fit[0].Name, query, StringComparison.CurrentCultureIgnoreCase)))
        {
            CloseMentions();
            return;
        }

        mentionStart = at;
        MentionPart.ItemsSource = fit;
        MentionPart.SelectedIndex = 0;
        MentionPart.Visibility = Visibility.Visible;
    }

    private void CloseMentions()
    {
        mentionStart = -1;
        MentionPart.Visibility = Visibility.Collapsed;
    }

    /// Up, down and the keys that choose or close, while the list is open.
    private bool HandleMentionKey(VirtualKey key)
    {
        int count = (MentionPart.ItemsSource as IReadOnlyList<MentionCandidate>)?.Count ?? 0;
        switch (key)
        {
            case VirtualKey.Down when count > 0:
                MentionPart.SelectedIndex = (MentionPart.SelectedIndex + 1) % count;
                return true;
            case VirtualKey.Up when count > 0:
                MentionPart.SelectedIndex = (MentionPart.SelectedIndex + count - 1) % count;
                return true;
            case VirtualKey.Enter or VirtualKey.Tab when MentionPart.SelectedItem is MentionCandidate chosen:
                ChooseMention(chosen);
                return true;
            case VirtualKey.Escape:
                CloseMentions();
                return true;
            default:
                return false;
        }
    }

    private void OnMentionClicked(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is MentionCandidate chosen)
        {
            ChooseMention(chosen);
        }
    }

    /// Test channel: the list as it stands, or press its row by the text of the name and note.
    internal string MentionsForUiTest(string? choose)
    {
        // Text set by the channel leaves the caret at the start; a person's stands after what they typed.
        InputPart.SelectionStart = InputPart.Text.Length;
        UpdateMentions();
        if (MentionPart.ItemsSource is not IReadOnlyList<MentionCandidate> shown || MentionPart.Visibility != Visibility.Visible)
        {
            return "closed";
        }

        if (choose is not null && shown.FirstOrDefault(c => $"{c.Name} {c.Note}".Contains(choose, StringComparison.CurrentCultureIgnoreCase)) is { } row)
        {
            ChooseMention(row);
            return "chosen " + row.Account;
        }

        return string.Join(" | ", shown.Select(c => $"{c.Name} [{c.Note}]"));
    }

    private void ChooseMention(MentionCandidate chosen)
    {
        if (mentionStart < 0)
        {
            return;
        }

        string text = InputPart.Text ?? string.Empty;
        int end = Math.Min(InputPart.SelectionStart, text.Length);
        string inserted = $"@{chosen.Name} ";
        int start = mentionStart;
        int caret = start + inserted.Length;
        mentionsChosen.Add(chosen);
        CloseMentions();
        InputPart.Text = text[..start] + inserted + text[end..];
        InputPart.SelectionStart = caret;
        InputPart.Focus(FocusState.Programmatic);
    }

    public TextBox Input => InputPart;
    public Button SendButton => SendPart;
    public Button AttachButton => AttachPart;

    private void OnSendClicked(object sender, RoutedEventArgs e) => SendRequested?.Invoke(this, EventArgs.Empty);

    private void OnAttachClicked(object sender, RoutedEventArgs e) => AttachRequested?.Invoke(this, EventArgs.Empty);

    // ---- a voice message ---------------------------------------------------------------------------

    private VoiceRecorder? recorder;
    private DispatcherTimer? recordingTimer;

    /// A recording ready to go; the host stages and sends it.
    public event EventHandler<VoiceRecording>? VoiceRecorded;

    /// Why there is no recording, in words for a person.
    public event EventHandler<string>? VoiceFailed;

    private bool Recording => recorder is not null || videoNote is not null || videoNoteStarting;

    private VideoNoteRecorder? videoNote;
    private bool videoNoteStarting;
    private bool videoNoteCancelledWhileStarting;

    /// A video message ready to go; the host stages and sends it.
    public event EventHandler<VideoNoteRecording>? VideoNoteRecorded;

    private void CutLiveCircle()
    {
        Microsoft.UI.Composition.Visual visual = Microsoft.UI.Xaml.Hosting.ElementCompositionPreview.GetElementVisual(VideoNoteLivePart);
        Microsoft.UI.Composition.Compositor compositor = visual.Compositor;
        Microsoft.UI.Composition.CompositionRoundedRectangleGeometry circle = compositor.CreateRoundedRectangleGeometry();
        float side = (float)VideoNoteLivePart.Width;
        circle.Size = new System.Numerics.Vector2(side, side);
        circle.CornerRadius = new System.Numerics.Vector2(side / 2, side / 2);
        visual.Clip = compositor.CreateGeometricClip(circle);
    }

    /// What the record button makes, the same in every conversation. The window loads and keeps it.
    internal static bool RecordsVideo { get; set; }

    internal static Action<bool>? SaveRecordsVideo { get; set; }

    private static event Action? RecordKindChanged;

    private void ShowRecordKind()
    {
        RecordIconPart.Glyph = RecordsVideo ? "\uE722" : "\uE720";
        string what = RecordsVideo
            ? "Записать видеосообщение. Правая кнопка: голосовое"
            : "Записать голосовое. Правая кнопка: видеосообщение";
        ToolTipService.SetToolTip(RecordPart, what);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(RecordPart, what);
    }

    // ---- recording by holding, as in Telegram: let go inside the composer and it goes, outside and it is
    // thrown away, up to the lock and it goes on with the button let go.

    /// A press shorter than this is a click, not a recording.
    private const int HoldAtLeastMs = 300;

    /// How far over the button the lock is reached, and how far aside the finger may be.
    private const double LockReach = 72;
    private const double LockAside = 48;

    private const double FloatingGap = 16;
    private const double LiveCircleSide = 300;

    private bool holding;
    private bool holdOutside;
    private bool pressedByPointer;
    private long holdStartedAtMs;

    private void OnRecordPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!e.GetCurrentPoint(RecordPart).Properties.IsLeftButtonPressed)
        {
            return;
        }

        pressedByPointer = true;
        RecordPart.CapturePointer(e.Pointer);
        HoldBegin();
    }

    private void OnRecordPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!holding)
        {
            return;
        }

        global::Windows.Foundation.Point onButton = e.GetCurrentPoint(RecordPart).Position;
        if (onButton.Y < -LockReach && Math.Abs(onButton.X - RecordPart.ActualWidth / 2) < LockAside)
        {
            HoldLock();
            return;
        }

        HoldOutside(IsOutside(e.GetCurrentPoint(this).Position));
    }

    private void OnRecordPointerReleased(object sender, PointerRoutedEventArgs e)
    {
        if (holding)
        {
            HoldEnd(IsOutside(e.GetCurrentPoint(this).Position));
        }
    }

    // The pointer was taken away (another window, a dialog): nobody chose to send this.
    private void OnRecordPointerLost(object sender, PointerRoutedEventArgs e)
    {
        if (holding)
        {
            HoldEnd(outside: true);
        }
    }

    private bool IsOutside(global::Windows.Foundation.Point point)
        => point.X < 0 || point.Y < 0 || point.X > ActualWidth || point.Y > ActualHeight;

    private void HoldBegin()
    {
        if (Recording)
        {
            return;
        }

        holding = true;
        holdOutside = false;
        holdStartedAtMs = Environment.TickCount64;
        if (RecordsVideo)
        {
            _ = StartVideoNoteAsync();
        }
        else
        {
            StartVoice();
        }

        // A recording that could not start has ended already and said why.
        if (holding)
        {
            ShowHold();
            PlaceFloating();
        }
    }

    private void HoldOutside(bool outside)
    {
        if (holdOutside != outside)
        {
            holdOutside = outside;
            ShowHints(deaf: false);
        }
    }

    private void HoldLock()
    {
        holding = false;
        holdOutside = false;
        ShowHold();
        UpdateMic();
        VoiceSendPart.Focus(FocusState.Programmatic);
    }

    private void HoldEnd(bool outside)
    {
        bool tooShort = Environment.TickCount64 - holdStartedAtMs < HoldAtLeastMs;
        bool starting = videoNoteStarting;
        holding = false;
        if (outside || tooShort || starting)
        {
            CancelVoice();
            if (!outside)
            {
                VoiceFailed?.Invoke(this, "Удерживайте кнопку, пока говорите, и отпустите, чтобы отправить.");
            }

            return;
        }

        if (videoNote is not null)
        {
            _ = FinishVideoNoteAsync();
        }
        else
        {
            FinishVoice();
        }
    }

    private void ShowHold()
    {
        RecordHaloPart.Visibility = holding ? Visibility.Visible : Visibility.Collapsed;
        if (holding)
        {
            RecordIconPart.Foreground = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.White);
        }
        else
        {
            RecordIconPart.ClearValue(FontIcon.ForegroundProperty);
        }

        RecordingCancelPart.Visibility = holding ? Visibility.Collapsed : Visibility.Visible;
        LockPart.Visibility = holding ? Visibility.Visible : Visibility.Collapsed;
        ShowHints(deaf: false);
    }

    /// What the strip says: a warning first, then what to do with the held button, else the level.
    private void ShowHints(bool deaf)
    {
        string? warning = holding && holdOutside ? "Отпустите, чтобы удалить"
            : deaf ? "Микрофон ничего не слышит: проверьте его в настройках"
            : null;
        RecordingHintPart.Text = warning ?? string.Empty;
        RecordingHintPart.Visibility = warning is null ? Visibility.Collapsed : Visibility.Visible;
        RecordingHoldPart.Visibility = holding && warning is null ? Visibility.Visible : Visibility.Collapsed;
        RecordingLevelPart.Visibility = !holding && warning is null && recorder is not null ? Visibility.Visible : Visibility.Collapsed;
    }

    // Over the feed: they cover it for as long as the recording goes and do not push it up.
    private void PlaceFloating()
    {
        VideoNoteLivePart.Width = VideoNoteLivePart.Height = Math.Min(LiveCircleSide, VideoNoteSizing.Instance.Limit);
        Canvas.SetLeft(VideoNoteLivePart, Math.Max(0, (FloatingPart.ActualWidth - VideoNoteLivePart.Width) / 2));
        Canvas.SetTop(VideoNoteLivePart, -VideoNoteLivePart.Height - FloatingGap);
        Canvas.SetLeft(LockPart, Math.Max(0, FloatingPart.ActualWidth - LockPart.Width));
        Canvas.SetTop(LockPart, -LockPart.Height - FloatingGap);
    }

    /// Test channel: the gesture's steps, as the pointer would make them.
    internal string HoldForUiTest(string what)
    {
        switch (what)
        {
            case "begin":
                HoldBegin();
                break;
            case "outside":
                HoldOutside(true);
                break;
            case "inside":
                HoldOutside(false);
                break;
            case "lock":
                HoldLock();
                break;
            case "release":
                HoldEnd(holdOutside);
                break;
            default:
                return "error: hold <begin|outside|inside|lock|release>";
        }

        return $"holding={holding} outside={holdOutside} recording={Recording} lock={LockPart.Visibility} cancel={RecordingCancelPart.Visibility} send={VoiceSendPart.Visibility} button={RecordPart.Visibility}";
    }

    // Pressed with a pointer, the holding above has dealt with it; this is the keyboard: it records until
    // sent, since no key is held.
    private void OnRecordClicked(object sender, RoutedEventArgs e)
    {
        if (pressedByPointer)
        {
            pressedByPointer = false;
            return;
        }

        if (RecordsVideo)
        {
            _ = StartVideoNoteAsync();
        }
        else
        {
            StartVoice();
        }
    }

    private void OnRecordRightTapped(object sender, RightTappedRoutedEventArgs e)
    {
        e.Handled = true;
        ToggleRecordKind();
    }

    private static void ToggleRecordKind()
    {
        RecordsVideo = !RecordsVideo;
        SaveRecordsVideo?.Invoke(RecordsVideo);
        RecordKindChanged?.Invoke();
    }

    // ---- emoji ---------------------------------------------------------------------------------------

    private EmojiPanel? emojiPanel;

    private void OnEmojiOpening(object? sender, object e)
    {
        if (emojiPanel is null)
        {
            emojiPanel = new EmojiPanel();
            emojiPanel.Chosen += (_, emoji) => InsertAtCaret(emoji);
            EmojiFlyoutPart.Content = emojiPanel;
        }
    }

    // The panel stays open for the next one; the field takes the typing back when it closes.
    private void OnEmojiClosed(object? sender, object e) => InputPart.Focus(FocusState.Programmatic);

    private void InsertAtCaret(string emoji)
    {
        string text = InputPart.Text ?? string.Empty;
        int start = Math.Min(InputPart.SelectionStart, text.Length);
        int length = Math.Min(InputPart.SelectionLength, text.Length - start);
        InputPart.Text = text[..start] + emoji + text[(start + length)..];
        InputPart.SelectionStart = start + emoji.Length;
    }

    /// Test channel: the record button's right click; the smiley opened, or an emoji from it pressed.
    internal string RecordKindForUiTest()
    {
        ToggleRecordKind();
        return $"{(RecordsVideo ? "camera" : "microphone")} glyph={(int)RecordIconPart.Glyph[0]:X} button={RecordPart.Visibility}";
    }

    internal string EmojiForUiTest(string? emoji)
    {
        if (emoji is null)
        {
            EmojiFlyoutPart.ShowAt(EmojiPart);
            return "open";
        }

        InsertAtCaret(emoji);
        EmojiFlyoutPart.Hide();
        return $"text={InputPart.Text}";
    }

    private async Task StartVideoNoteAsync()
    {
        if (Recording)
        {
            return;
        }

        if (VoiceRecorder.CallIsOn?.Invoke() == true)
        {
            VoiceFailed?.Invoke(this, "Во время звонка видеосообщение не записать.");
            return;
        }

        videoNoteStarting = true;
        videoNoteCancelledWhileStarting = false;
        RecordingTimePart.Text = "0:00";
        RecordingHintPart.Visibility = Visibility.Collapsed;
        RecordingLevelPart.Visibility = Visibility.Collapsed;
        PlaceFloating();
        RecordingPart.Visibility = Visibility.Visible;
        VideoNoteLivePart.Visibility = Visibility.Visible;
        AttachPart.Visibility = Visibility.Collapsed;
        InputPart.Visibility = Visibility.Collapsed;
        UpdateMic();
        (VideoNoteRecorder? started, string? why) = await VideoNoteRecorder.TryStartAsync(VideoNoteSurfacePart);
        videoNoteStarting = false;
        if (started is null || videoNoteCancelledWhileStarting)
        {
            started?.Dispose();
            EndVoiceUi();
            if (started is null)
            {
                VoiceFailed?.Invoke(this, why ?? "Не удалось начать запись.");
            }

            return;
        }

        videoNote = started;
        if (!holding)
        {
            VoiceSendPart.Focus(FocusState.Programmatic);
        }

        recordingTimer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(VoiceRecorder.LevelIntervalMs) };
        recordingTimer.Tick += OnVideoNoteTick;
        recordingTimer.Start();
    }

    private void OnVideoNoteTick(object? sender, object e)
    {
        if (videoNote is not { } running)
        {
            return;
        }

        if (!IsOnScreen())
        {
            CancelVoice();
            return;
        }

        if (VoiceRecorder.CallIsOn?.Invoke() == true)
        {
            CancelVoice();
            VoiceFailed?.Invoke(this, "Запись отменена: начался звонок.");
            return;
        }

        RecordingTimePart.Text = Presentation.MainWindowPresentation.FormatVoiceTime(running.ElapsedMs);
        if (running.ElapsedMs >= VideoNoteRecorder.MaxMs)
        {
            _ = FinishVideoNoteAsync();
        }
    }

    private async Task FinishVideoNoteAsync()
    {
        if (videoNote is not { } running)
        {
            return;
        }

        videoNote = null;
        recordingTimer?.Stop();
        VideoNoteRecording? recording = await running.FinishAsync();
        EndVoiceUi();
        if (recording is null)
        {
            VoiceFailed?.Invoke(this, "Слишком короткая запись.");
            return;
        }

        VideoNoteRecorded?.Invoke(this, recording);
    }

    /// Test channel: the circle's buttons, as a person presses them. `tone` hands the host a coloured
    /// recording; `check` closes a real one and has the core check it, and nothing is sent.
    internal async Task<string> VideoNoteForUiTestAsync(string what, double seconds)
    {
        switch (what)
        {
            case "start":
                if (RecordPart.Visibility != Visibility.Visible)
                {
                    return "error: no record button here";
                }

                await StartVideoNoteAsync();
                return videoNote is not null ? "recording" : "not recording";
            case "cancel":
                CancelVoice();
                return "ok";
            case "send":
                await FinishVideoNoteAsync();
                return "ok";
            case "state":
                return videoNote is { } running
                    ? $"recording ms={running.ElapsedMs} bytes={running.BytesSoFar} first_frame={running.HasPreview}"
                    : $"idle button={RecordPart.Visibility} live={VideoNoteLivePart.Visibility}";
            case "tone":
                VideoNoteRecorded?.Invoke(this, await VideoNoteRecorder.ToneAsync(seconds));
                return "ok";
            case "check":
                if (videoNote is not { } checking)
                {
                    return "error: not recording";
                }

                videoNote = null;
                recordingTimer?.Stop();
                VideoNoteRecording? recording = await checking.FinishAsync();
                EndVoiceUi();
                if (recording is null)
                {
                    return "error: nothing recorded";
                }

                string staged = Core.MediaFiles.StageVideoNote(recording.Video, recording.Preview);
                File.Delete(staged);
                return $"checked bytes={recording.Video.Length} first_frame_bytes={recording.Preview.Length}";
            default:
                return "error: circle <start|cancel|send|state|tone seconds|check>";
        }
    }

    private void UpdateMic()
    {
        bool idle = !Recording;
        EmojiPart.Visibility = idle ? Visibility.Visible : Visibility.Collapsed;
        bool canAttach = AttachPart.IsEnabled && attachWanted;
        bool nothingToSend = string.IsNullOrEmpty(InputPart.Text) && DraftPart.Visibility != Visibility.Visible;
        // Held, the button stays where it is: the finger is on it.
        RecordPart.Visibility = holding || (idle && canAttach && nothingToSend) ? Visibility.Visible : Visibility.Collapsed;
        SendPart.Visibility = idle && RecordPart.Visibility != Visibility.Visible ? Visibility.Visible : Visibility.Collapsed;
        VoiceSendPart.Visibility = idle || holding ? Visibility.Collapsed : Visibility.Visible;
    }

    private void StartVoice()
    {
        if (Recording)
        {
            return;
        }

        if (!VoiceRecorder.TryStart(out VoiceRecorder? started, out string? why))
        {
            VoiceFailed?.Invoke(this, why ?? "Не удалось начать запись.");
            return;
        }

        recorder = started;
        RecordingTimePart.Text = "0:00";
        RecordingHintPart.Visibility = Visibility.Collapsed;
        RecordingLevelPart.Visibility = Visibility.Visible;
        RecordingLevelPart.Bars = [];
        RecordingPart.Visibility = Visibility.Visible;
        AttachPart.Visibility = Visibility.Collapsed;
        InputPart.Visibility = Visibility.Collapsed;
        UpdateMic();
        if (!holding)
        {
            VoiceSendPart.Focus(FocusState.Programmatic);
        }

        recordingTimer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(VoiceRecorder.LevelIntervalMs) };
        recordingTimer.Tick += OnRecordingTick;
        recordingTimer.Start();
    }

    private void OnRecordingTick(object? sender, object e)
    {
        if (recorder is not { } running)
        {
            return;
        }

        // Another section or conversation took the screen and hid this field. A microphone must not go
        // on recording behind the person's back.
        if (!IsOnScreen())
        {
            CancelVoice();
            return;
        }

        running.SampleLevel();
        if (VoiceRecorder.CallIsOn?.Invoke() == true)
        {
            // A call took the microphone's meaning away: what follows is the call, not this message.
            CancelVoice();
            VoiceFailed?.Invoke(this, "Запись отменена: начался звонок.");
            return;
        }

        RecordingTimePart.Text = Presentation.MainWindowPresentation.FormatVoiceTime(running.ElapsedMs);
        RecordingLevelPart.Bars = running.RecentBars();
        ShowHints(running.HearsNothing);
        if (running.ElapsedMs >= VoiceRecorder.MaxMs)
        {
            FinishVoice();
        }
    }

    private bool IsOnScreen()
    {
        for (DependencyObject? node = this; node is not null; node = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetParent(node))
        {
            if (node is UIElement { Visibility: Visibility.Collapsed })
            {
                return false;
            }
        }

        return true;
    }

    private void OnVoiceSendClicked(object sender, RoutedEventArgs e)
    {
        if (videoNote is not null)
        {
            _ = FinishVideoNoteAsync();
        }
        else
        {
            FinishVoice();
        }
    }

    private void OnVoiceCancelClicked(object sender, RoutedEventArgs e) => CancelVoice();

    private void FinishVoice()
    {
        if (recorder is not { } running)
        {
            return;
        }

        VoiceRecording? recording = running.Finish();
        EndVoiceUi();
        if (recording is null)
        {
            VoiceFailed?.Invoke(this, "Слишком короткая запись.");
            return;
        }

        VoiceRecorded?.Invoke(this, recording);
    }

    /// A recording under way is thrown away: the conversation changed, it was not meant for this one.
    public void CancelVoice()
    {
        if (videoNoteStarting)
        {
            videoNoteCancelledWhileStarting = true;
            return;
        }

        if (videoNote is { } camera)
        {
            camera.Dispose();
            EndVoiceUi();
            return;
        }

        if (recorder is not { } running)
        {
            return;
        }

        running.Dispose();
        EndVoiceUi();
    }

    private void EndVoiceUi()
    {
        recorder = null;
        videoNote = null;
        holding = false;
        holdOutside = false;
        ShowHold();
        RecordingCancelPart.Visibility = Visibility.Visible;
        VideoNoteLivePart.Visibility = Visibility.Collapsed;
        recordingTimer?.Stop();
        recordingTimer = null;
        RecordingPart.Visibility = Visibility.Collapsed;
        AttachPart.Visibility = attachWanted ? Visibility.Visible : Visibility.Collapsed;
        InputPart.Visibility = Visibility.Visible;
        UpdateMic();
        InputPart.Focus(FocusState.Programmatic);
    }

    private void OnRootPreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (Recording && e.Key == VirtualKey.Escape)
        {
            e.Handled = true;
            CancelVoice();
        }
    }

    /// Test channel: the recording's buttons, as a person presses them; `tone` hands the host a recording
    /// made of a tone, for machines with no microphone.
    internal string VoiceForUiTest(string what, double seconds)
    {
        switch (what)
        {
            case "start":
                if (RecordPart.Visibility != Visibility.Visible)
                {
                    return "error: no microphone button here";
                }

                StartVoice();
                return Recording ? "recording" : "not recording";
            case "cancel":
                CancelVoice();
                return "ok";
            case "send":
                FinishVoice();
                return "ok";
            case "state":
                return Recording
                    ? $"recording ms={recorder!.ElapsedMs} packets={recorder.PacketCount} deaf={recorder.HearsNothing} hint={RecordingHintPart.Visibility}"
                    : $"idle mic={RecordPart.Visibility} send={SendPart.Visibility}";
            case "tone":
                if (VoiceRecorder.ToneRecording(seconds) is not { } tone)
                {
                    return "error: no encoder";
                }

                VoiceRecorded?.Invoke(this, tone);
                return "ok";
            default:
                return "error: voice <start|cancel|send|state|tone seconds>";
        }
    }

    private void OnInputPreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (MentionPart.Visibility == Visibility.Visible && HandleMentionKey(e.Key))
        {
            e.Handled = true;
            return;
        }

        if (e.Key != VirtualKey.Enter)
        {
            return;
        }

        bool shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(CoreVirtualKeyStates.Down);
        if (shift)
        {
            return; // a new line
        }

        e.Handled = true;
        if (SendPart.IsEnabled)
        {
            SendRequested?.Invoke(this, EventArgs.Empty);
        }
    }
}
