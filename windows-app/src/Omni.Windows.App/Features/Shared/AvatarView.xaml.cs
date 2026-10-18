using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using Microsoft.UI.Xaml.Shapes;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The app's one avatar (see AvatarView.xaml). <see cref="Size"/> sets the circle, the letter, the
/// loading ring and the presence dot together. The parts are open to the code that fills them
/// (MainWindow.ApplyAvatarVisual / ApplyPresenceDot take a circle, an image, a text, a dot); a list
/// template binds <see cref="Text"/>, <see cref="Picture"/>, <see cref="Fill"/> instead.
/// A tap on an avatar that shows a picture opens the picture on the whole window, as a photo
/// (owner, 2026-10-12: «везде, где можно на неё тыкнуть … один шаблон») — <see cref="PictureRequested"/>,
/// which the window handles once for every avatar.
/// </summary>
public sealed partial class AvatarView : UserControl
{
    public static readonly DependencyProperty SizeProperty = DependencyProperty.Register(
        nameof(Size), typeof(double), typeof(AvatarView), new PropertyMetadata(40d, (d, _) => ((AvatarView)d).ApplySize()));

    public static readonly DependencyProperty TextProperty = DependencyProperty.Register(
        nameof(Text), typeof(string), typeof(AvatarView),
        new PropertyMetadata(null, (d, e) => ((AvatarView)d).InitialsPart.Text = e.NewValue as string ?? "?"));

    public static readonly DependencyProperty PictureProperty = DependencyProperty.Register(
        nameof(Picture), typeof(ImageSource), typeof(AvatarView),
        new PropertyMetadata(null, (d, e) => ((AvatarView)d).ShowImage(e.NewValue as ImageSource)));

    public static readonly DependencyProperty FillProperty = DependencyProperty.Register(
        nameof(Fill), typeof(Brush), typeof(AvatarView),
        new PropertyMetadata(null, (d, e) =>
        {
            if (e.NewValue is Brush brush)
            {
                ((AvatarView)d).CirclePart.Background = brush;
            }
        }));

    public static readonly DependencyProperty LoadingVisibilityProperty = DependencyProperty.Register(
        nameof(LoadingVisibility), typeof(Visibility), typeof(AvatarView),
        new PropertyMetadata(Visibility.Collapsed, (d, e) =>
        {
            var view = (AvatarView)d;
            bool loading = e.NewValue is Visibility.Visible;
            view.LoadingRingPart.Visibility = loading ? Visibility.Visible : Visibility.Collapsed;
            view.LoadingRingPart.IsActive = loading;
        }));

    public static readonly DependencyProperty PictureTitleProperty = DependencyProperty.Register(
        nameof(PictureTitle), typeof(string), typeof(AvatarView), new PropertyMetadata(null));

    /// A tap on an avatar showing a picture: the picture's file and whose it is.
    public static event Action<string, string>? PictureRequested;

    /// A tap on an avatar with no picture (initials): the host may do what the rest of its row does.
    public event EventHandler? TappedWithoutPicture;

    /// Whether a tap on the picture opens it. Off where the avatar is a button for something else
    /// (our own, in the pane footer, opens the profile): the tap then goes on to the host.
    public bool OpensPicture { get; set; } = true;

    public AvatarView()
    {
        InitializeComponent();
        ApplySize();
    }

    /// The circle's diameter in px.
    public double Size
    {
        get => (double)GetValue(SizeProperty);
        set => SetValue(SizeProperty, value);
    }

    /// The letter(s) shown while there is no picture.
    public string? Text
    {
        get => (string?)GetValue(TextProperty);
        set => SetValue(TextProperty, value);
    }

    /// The picture; null = the letter on the colour.
    public ImageSource? Picture
    {
        get => (ImageSource?)GetValue(PictureProperty);
        set => SetValue(PictureProperty, value);
    }

    /// The circle's colour (AvatarPalette).
    public Brush? Fill
    {
        get => (Brush?)GetValue(FillProperty);
        set => SetValue(FillProperty, value);
    }

    /// The ring over the circle while a new picture is on its way.
    public Visibility LoadingVisibility
    {
        get => (Visibility)GetValue(LoadingVisibilityProperty);
        set => SetValue(LoadingVisibilityProperty, value);
    }

    /// Whose picture it is — the viewer's caption.
    public string? PictureTitle
    {
        get => (string?)GetValue(PictureTitleProperty);
        set => SetValue(PictureTitleProperty, value);
    }

    public Border Circle => CirclePart;
    public Image Photo => PhotoPart;
    public TextBlock Initials => InitialsPart;
    public ProgressRing LoadingRing => LoadingRingPart;
    public Border PresenceDot => PresenceDotPart;
    public Ellipse PresenceEllipse => PresenceEllipsePart;

    /// The file of the picture shown now: every avatar picture is decoded from a file
    /// (MainWindowPresentation.BuildAvatarImage), which remembers which one.
    public string? PicturePath =>
        PhotoPart.Visibility == Visibility.Visible &&
        Presentation.MainWindowPresentation.AvatarImageFile(PhotoPart.Source) is { } path &&
        File.Exists(path)
            ? path
            : null;

    protected override void OnTapped(TappedRoutedEventArgs e)
    {
        base.OnTapped(e);
        if (!OpensPicture)
        {
            return;
        }

        if (PicturePath is { } path)
        {
            e.Handled = true;
            PictureRequested?.Invoke(path, PictureTitle ?? string.Empty);
        }
        else
        {
            TappedWithoutPicture?.Invoke(this, EventArgs.Empty);
        }
    }

    protected override void OnPointerEntered(PointerRoutedEventArgs e)
    {
        base.OnPointerEntered(e);
        ProtectedCursor = PicturePath is null || !OpensPicture ? null : InputSystemCursor.Create(InputSystemCursorShape.Hand);
    }

    private void ShowImage(ImageSource? image)
    {
        PhotoPart.Source = image;
        PhotoPart.Visibility = image is null ? Visibility.Collapsed : Visibility.Visible;
        InitialsPart.Visibility = image is null ? Visibility.Visible : Visibility.Collapsed;
    }

    private void ApplySize()
    {
        double size = Size;
        Root.Width = size;
        Root.Height = size;
        CirclePart.Width = size;
        CirclePart.Height = size;
        CirclePart.CornerRadius = new CornerRadius(size / 2);
        InitialsPart.FontSize = Math.Round(size * 0.4);
        LoadingRingPart.Width = LoadingRingPart.Height = Math.Round(size * 0.55);
        // The dot: a ring of the panel's colour around a smaller disc, as in Telegram — 14 on a 40.
        double dot = Math.Max(12, Math.Round(size * 0.34 / 2) * 2);
        double ring = dot >= 20 ? 3 : 2;
        PresenceDotPart.Width = PresenceDotPart.Height = dot;
        PresenceDotPart.CornerRadius = new CornerRadius(dot / 2);
        PresenceEllipsePart.Width = PresenceEllipsePart.Height = dot - 2 * ring;
        // On a big circle the dot sits on the rim, not outside the box.
        double inset = size >= 72 ? Math.Round(size * 0.04) : 0;
        PresenceDotPart.Margin = new Thickness(0, 0, inset, inset);
    }
}
