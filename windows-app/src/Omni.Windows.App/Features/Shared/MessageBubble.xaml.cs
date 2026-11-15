using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App.Features.Shared;

/// One message in any conversation (see MessageBubble.xaml). The host hears what a person asked for;
/// a photo's and a file's events come with the pressed element as the sender, its Tag the message id.
public sealed partial class MessageBubble : UserControl
{
    public static readonly DependencyProperty RowProperty = DependencyProperty.Register(
        nameof(Row), typeof(MessageRow), typeof(MessageBubble), new PropertyMetadata(null, (_, e) => VideoNotePlayer.StopIf(e.OldValue as MessageRow)));

    private static readonly Features.Chats.ChatBubbleBrushConverter BubbleBrushes = new();

    // The quick row, as in Telegram; the rest open under it.
    private static readonly string[] QuickEmoji = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

    private static readonly string[] AllEmoji =
    [
        "👍", "👎", "❤️", "🔥", "🎉", "😂", "🤣", "😊", "😍", "🤔", "😮", "😢",
        "😭", "😡", "🙏", "👏", "💪", "🤝", "👌", "✅", "❌", "⚡", "💯", "👀",
        "🤯", "😎", "🥳", "😴", "🤷", "🚀", "🐛", "💡",
    ];

    public MessageBubble()
    {
        InitializeComponent();
        // Every part that would open a menu of its own opens the message's.
        BodyPart.ContextFlyout = BubblePart.ContextFlyout;
        Unloaded += (_, _) => VideoNotePlayer.StopIf(Row);
    }

    // A row holds only what it shows: 125 elements on each, whatever the message, made opening a chat
    // slow and heavy (owner, 2026-11-06). The parts below come and go with their message.
    public bool IsShown(Visibility part) => part == Visibility.Visible;

    private void OnPhotoPartLoaded(object sender, RoutedEventArgs e)
        => ((UIElement)sender).ContextFlyout = BubblePart.ContextFlyout;

    private void OnVideoNotePartLoaded(object sender, RoutedEventArgs e)
    {
        FrameworkElement circle = (FrameworkElement)sender;
        circle.ContextFlyout = BubblePart.ContextFlyout;
        circle.SizeChanged += (_, _) => CutCircle();
        CutCircle();
    }

    private bool menuMade;

    // The menu's content comes to life here, and only for the messages somebody right-clicks.
    private void EnsureMenu()
    {
        if (menuMade)
        {
            return;
        }

        menuMade = true;
        _ = FindName(nameof(MenuContentPart));
        foreach (string emoji in QuickEmoji)
        {
            QuickEmojiPart.Children.Add(EmojiButton(emoji));
        }

        Button more = EmojiButton("⋯");
        more.Click -= OnEmojiClicked;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(more, "Другие реакции");
        more.Click += (_, _) =>
        {
            if (AllEmojiPart.Children.Count == 0)
            {
                foreach (string emoji in AllEmoji)
                {
                    AllEmojiPart.Children.Add(EmojiButton(emoji));
                }
            }

            AllEmojiPart.Visibility = Visibility.Visible;
            more.Visibility = Visibility.Collapsed;
        };
        QuickEmojiPart.Children.Add(more);
        // The long list is a step within one opening of the menu; closed, it folds again.
        MenuPart.Closed += (_, _) =>
        {
            AllEmojiPart.Visibility = Visibility.Collapsed;
            more.Visibility = Visibility.Visible;
        };
        foreach (Button item in new[] { ReplyItem, CopyItem, CopyImageItem, PinItem, ForwardItem, SelectItem, DeleteItem })
        {
            item.Click += (_, _) => MenuPart.Hide();
        }

        OpenPhotoItem.Click += (_, _) => MenuPart.Hide();
        SavePhotoItem.Click += (_, _) => MenuPart.Hide();
    }

    public MessageRow? Row
    {
        get => (MessageRow?)GetValue(RowProperty);
        set => SetValue(RowProperty, value);
    }

    /// «Ответить» shows only where the conversation listens for it.
    public event EventHandler<MessageRow>? ReplyRequested;

    /// An emoji chosen for this message; empty takes ours away. It shows only where the host listens.
    public event EventHandler<string>? ReactionChosen;

    /// A tap on the quote: the host goes to the answered message.
    public event EventHandler<MessageRow>? QuoteOpened;

    /// These show only where the host listens.
    public event EventHandler<MessageRow>? PinRequested;

    public event EventHandler<MessageRow>? ForwardRequested;

    public event EventHandler<MessageRow>? SelectRequested;

    public event EventHandler<MessageRow>? DeleteRequested;

    /// While messages are being picked, a tap on this one.
    public event EventHandler<MessageRow>? SelectionToggled;

    public event RoutedEventHandler? PhotoClicked;

    public event RoutedEventHandler? PhotoSaveClicked;

    public event RoutedEventHandler? FileClicked;

    private Button EmojiButton(string emoji)
    {
        Button button = new()
        {
            Content = new TextBlock { Text = emoji, FontSize = 20, HorizontalAlignment = HorizontalAlignment.Center },
            Width = 36,
            Height = 36,
            MinWidth = 0,
            MinHeight = 0,
            Padding = new Thickness(0),
            Background = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent),
            BorderThickness = new Thickness(0),
            Tag = emoji,
        };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, "Реакция " + emoji);
        button.Click += OnEmojiClicked;
        return button;
    }

    private void OnEmojiClicked(object sender, RoutedEventArgs e)
    {
        MenuPart.Hide();
        if (sender is Button { Tag: string emoji })
        {
            React(emoji);
        }
    }

    private void OnReactionChipClicked(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: string emoji })
        {
            React(emoji);
        }
    }

    // One's own emoji again takes it away.
    private void React(string emoji)
    {
        if (Row is { } row)
        {
            ReactionChosen?.Invoke(this, row.MyReaction == emoji ? string.Empty : emoji);
        }
    }

    /// Test channel: the menu opened and its item pressed, by the item's text or its emoji.
    internal string PressMenuItemForUiTest(string wanted)
    {
        MenuPart.ShowAt(BubblePart);
        OnMenuOpening(null, EventArgs.Empty);
        if (QuickEmoji.Contains(wanted) || AllEmoji.Contains(wanted))
        {
            React(wanted);
            MenuPart.Hide();
            return "ok";
        }

        Button? item = wanted switch
        {
            "Ответить" => ReplyItem,
            "Копировать текст" => CopyItem,
            "Открыть" => OpenPhotoItem,
            "Сохранить как…" => SavePhotoItem,
            "Копировать изображение" => CopyImageItem,
            "Закрепить" or "Открепить" => PinItem,
            "Переслать" => ForwardItem,
            "Выбрать" => SelectItem,
            "Удалить" => DeleteItem,
            _ => null,
        };
        if (item is not { Visibility: Visibility.Visible })
        {
            MenuPart.Hide();
            return "error: no such item: " + wanted;
        }

        new Microsoft.UI.Xaml.Automation.Peers.ButtonAutomationPeer(item).Invoke();
        return "ok";
    }

    /// Test channel: the menu opened on this message, to look at.
    internal void OpenMenuForUiTest() => MenuPart.ShowAt(BubblePart);

    private void OnMenuOpening(object? sender, object e)
    {
        EnsureMenu();
        // A flyout stands outside the tree that hands the family down.
        foreach (Button button in QuickEmojiPart.Children.Concat(AllEmojiPart.Children).OfType<Button>())
        {
            if (button.Content is TextBlock face)
            {
                face.FontFamily = EmojiFonts.Family;
            }
        }

        bool hasPhoto = Row?.PhotoVisibility == Visibility.Visible;
        // A call's line is not a message anyone can answer or react to.
        bool isMessage = Row is { } shown && !shown.MessageId.StartsWith("call-history:", StringComparison.Ordinal);
        ReplyItem.Visibility = ReplyRequested is null || !isMessage ? Visibility.Collapsed : Visibility.Visible;
        ReactionsMenuPart.Visibility = ReactionChosen is null || !isMessage ? Visibility.Collapsed : Visibility.Visible;
        CopyItem.Visibility = Row?.BodyVisibility == Visibility.Visible ? Visibility.Visible : Visibility.Collapsed;
        CopyItemText.Text = BodyPart.SelectedText.Length > 0 ? "Копировать выделенное" : "Копировать текст";
        OpenPhotoItem.Visibility = hasPhoto ? Visibility.Visible : Visibility.Collapsed;
        SavePhotoItem.Visibility = hasPhoto ? Visibility.Visible : Visibility.Collapsed;
        CopyImageItem.Visibility = hasPhoto && Row?.PhotoLocalPath is { Length: > 0 } ? Visibility.Visible : Visibility.Collapsed;
        PinItem.Visibility = PinRequested is null || !isMessage ? Visibility.Collapsed : Visibility.Visible;
        PinItemText.Text = Row?.IsPinned == true ? "Открепить" : "Закрепить";
        PinItemIcon.Glyph = Row?.IsPinned == true ? "\uE77A" : "\uE718";
        ForwardItem.Visibility = ForwardRequested is null || !isMessage ? Visibility.Collapsed : Visibility.Visible;
        SelectItem.Visibility = SelectRequested is null || !isMessage ? Visibility.Collapsed : Visibility.Visible;
        DeleteItem.Visibility = DeleteRequested is null || !isMessage || (DeleteOwnOnly && Row?.IsOutgoing != true) ? Visibility.Collapsed : Visibility.Visible;
    }

    /// Only one's own lines can be deleted here (a discussion: a line goes from everyone or stays).
    public bool DeleteOwnOnly { get; set; }

    private void OnPinClicked(object sender, RoutedEventArgs e) => Raise(PinRequested);

    private void OnForwardClicked(object sender, RoutedEventArgs e) => Raise(ForwardRequested);

    private void OnSelectClicked(object sender, RoutedEventArgs e) => Raise(SelectRequested);

    private void OnDeleteClicked(object sender, RoutedEventArgs e) => Raise(DeleteRequested);

    private void OnPickTapped(object sender, Microsoft.UI.Xaml.Input.TappedRoutedEventArgs e) => Raise(SelectionToggled);

    private void Raise(EventHandler<MessageRow>? handler)
    {
        if (Row is { } row)
        {
            handler?.Invoke(this, row);
        }
    }

    private async void OnCopyImageClicked(object sender, RoutedEventArgs e)
    {
        if (Row?.PhotoLocalPath is not { Length: > 0 } path)
        {
            return;
        }

        try
        {
            global::Windows.Storage.StorageFile file = await global::Windows.Storage.StorageFile.GetFileFromPathAsync(path);
            DataPackage package = new();
            package.SetBitmap(global::Windows.Storage.Streams.RandomAccessStreamReference.CreateFromFile(file));
            Clipboard.SetContent(package);
            Clipboard.Flush();
        }
        catch (Exception ex) when (ex is System.IO.IOException or UnauthorizedAccessException or System.Runtime.InteropServices.COMException)
        {
            // The picture went from the disk: nothing to copy, and nothing else to do from a menu.
        }
    }

    private void OnReplyClicked(object sender, RoutedEventArgs e)
    {
        if (Row is { } row)
        {
            ReplyRequested?.Invoke(this, row);
        }
    }

    private void OnCopyClicked(object sender, RoutedEventArgs e)
    {
        string text = BodyPart.SelectedText.Length > 0 ? BodyPart.SelectedText : Row?.Body ?? string.Empty;
        if (text.Length == 0)
        {
            return;
        }

        DataPackage package = new();
        package.SetText(text);
        Clipboard.SetContent(package);
    }

    private void OnQuoteClicked(object sender, RoutedEventArgs e)
    {
        if (Row is { } row)
        {
            QuoteOpened?.Invoke(this, row);
        }
    }

    private void OnPhotoClicked(object sender, RoutedEventArgs e) => PhotoClicked?.Invoke(PhotoPart, e);

    private void OnPhotoOpenItemClicked(object sender, RoutedEventArgs e) => PhotoClicked?.Invoke(PhotoPart, e);

    private void OnPhotoSaveItemClicked(object sender, RoutedEventArgs e) => PhotoSaveClicked?.Invoke(PhotoPart, e);

    private void OnFileClicked(object sender, RoutedEventArgs e) => FileClicked?.Invoke(sender, e);

    // Not here yet (past the bound on files asking for themselves): the same step as a file's.
    private void OnVoicePlayClicked(object sender, RoutedEventArgs e)
    {
        if (Row is not { } row)
        {
            return;
        }

        if (row.AttachmentLocalPath is { Length: > 0 })
        {
            VoicePlayer.Toggle(row);
        }
        else
        {
            FileClicked?.Invoke(sender, e);
        }
    }

    private void OnVoiceSought(object? sender, double fraction)
    {
        if (Row is { AttachmentLocalPath.Length: > 0 } row)
        {
            VoicePlayer.Seek(row, fraction);
        }
    }

    /// A video message has no bubble around it: only its time shows on the conversation's background.
    public Microsoft.UI.Xaml.Media.Brush BubbleFill(bool outgoing, bool videoNote)
        => videoNote
            ? new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent)
            : (Microsoft.UI.Xaml.Media.Brush)BubbleBrushes.Convert(outgoing, typeof(Microsoft.UI.Xaml.Media.Brush), null!, string.Empty);

    public double CircleSide(double side, double limit) => Math.Min(side, limit);

    // A voice message's time and ticks stand in the line of its length, as in Telegram (owner, 2026-11-06),
    // unless something shares that corner with them.
    public Thickness LastLineMargin(Visibility voice, Visibility body, Visibility reactions, Visibility status, Visibility progress)
        => voice == Visibility.Visible
            && body != Visibility.Visible
            && reactions != Visibility.Visible
            && status != Visibility.Visible
            && progress != Visibility.Visible
                ? new Thickness(0, -VoiceLengthLineHeight, 0, 0)
                : default;

    private const double VoiceLengthLineHeight = 20;

    // In the line's own thicknesses, as a dash is measured: the played part, then a gap past the end.
    public Microsoft.UI.Xaml.Media.DoubleCollection ProgressDash(double percent, double side, double limit)
    {
        double length = Math.PI * (CircleSide(side, limit) - 4 - 3) / 3;
        return [length * percent / 100, length * 2];
    }

    // A clip on the visual: it takes in the first frame, the player's surface and all over them.
    private void CutCircle()
    {
        Microsoft.UI.Composition.Visual visual = Microsoft.UI.Xaml.Hosting.ElementCompositionPreview.GetElementVisual(VideoNotePart);
        Microsoft.UI.Composition.Compositor compositor = visual.Compositor;
        Microsoft.UI.Composition.CompositionRoundedRectangleGeometry circle = compositor.CreateRoundedRectangleGeometry();
        float side = (float)VideoNotePart.ActualWidth;
        circle.Size = new System.Numerics.Vector2(side, side);
        circle.CornerRadius = new System.Numerics.Vector2(side / 2, side / 2);
        visual.Clip = compositor.CreateGeometricClip(circle);
    }

    private void OnVideoNoteTapped(object sender, Microsoft.UI.Xaml.Input.TappedRoutedEventArgs e)
    {
        e.Handled = true;
        PressVideoNote();
    }

    // Not here yet (past the bound on files asking for themselves): the same step as a file's.
    private void PressVideoNote()
    {
        if (Row is not { } row)
        {
            return;
        }

        if (row.AttachmentLocalPath is { Length: > 0 })
        {
            VideoNotePlayer.Toggle(row, VideoNotePart);
        }
        else
        {
            FileClicked?.Invoke(VideoNotePart, new RoutedEventArgs());
        }
    }

    /// Test channel: the circle pressed.
    internal void PressVideoNoteForUiTest() => PressVideoNote();

    /// Test channel: the play button pressed.
    internal void PressVoicePlayForUiTest() => OnVoicePlayClicked(this, new RoutedEventArgs());
}
