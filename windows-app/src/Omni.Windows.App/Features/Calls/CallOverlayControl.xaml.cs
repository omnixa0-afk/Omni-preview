using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Omni.Windows.App.Features.Calls;

// Full-window call surface. Hosted in MainWindow's root grid (on top of the
// authenticated shell) and toggled via CallOverlayRoot.Visibility from
// MainWindow.Calls.cs. Element x:Names are resolved by MainWindow through
// ResolveSectionElement, and button Click handlers are wired in
// MainWindow.SectionEvents.cs. The only logic here is the view's own: over live
// video the top bar and the control bar fade while the mouse rests, and the
// self-preview can be dragged anywhere inside the call surface.
public sealed partial class CallOverlayControl : UserControl
{
    private static readonly TimeSpan ChromeIdle = TimeSpan.FromSeconds(3);
    private readonly DispatcherTimer chromeIdleTimer = new() { Interval = ChromeIdle };
    private bool chromeAutoHide;
    private bool pointerOverControls;
    private readonly TranslateTransform pipOffset = new();
    private global::Windows.Foundation.Point? pipDragStart;
    private (double X, double Y) pipOffsetAtDragStart;

    public CallOverlayControl()
    {
        InitializeComponent();
        chromeIdleTimer.Tick += (_, _) =>
        {
            chromeIdleTimer.Stop();
            if (chromeAutoHide && !pointerOverControls)
            {
                CallChrome.Opacity = 0;
            }
        };
        CallOverlayRoot.PointerMoved += (_, _) => ShowChrome();
        CallOverlayRoot.PointerPressed += (_, _) => ShowChrome();
        ChatCallActionPanel.PointerEntered += (_, _) => pointerOverControls = true;
        ChatCallActionPanel.PointerExited += (_, _) => pointerOverControls = false;

        // The self-preview is dragged with the mouse (or a finger) and stays where it is dropped,
        // kept inside the call surface when the window shrinks.
        LocalCallVideoPip.RenderTransform = pipOffset;
        LocalCallVideoPip.PointerPressed += OnPipPointerPressed;
        LocalCallVideoPip.PointerMoved += OnPipPointerMoved;
        LocalCallVideoPip.PointerReleased += OnPipPointerReleased;
        LocalCallVideoPip.PointerCaptureLost += (_, _) => pipDragStart = null;
        LocalCallVideoPip.SizeChanged += (_, _) => KeepPipInside();
        if (LocalCallVideoPip.Parent is FrameworkElement surface)
        {
            surface.SizeChanged += (_, _) => KeepPipInside();
        }
    }

    private void OnPipPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        if (LocalCallVideoPip.Parent is not UIElement surface)
        {
            return;
        }

        pipDragStart = e.GetCurrentPoint(surface).Position;
        pipOffsetAtDragStart = (pipOffset.X, pipOffset.Y);
        LocalCallVideoPip.CapturePointer(e.Pointer);
        e.Handled = true;
    }

    private void OnPipPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (pipDragStart is not { } start || LocalCallVideoPip.Parent is not UIElement surface)
        {
            return;
        }

        global::Windows.Foundation.Point now = e.GetCurrentPoint(surface).Position;
        pipOffset.X = pipOffsetAtDragStart.X + now.X - start.X;
        pipOffset.Y = pipOffsetAtDragStart.Y + now.Y - start.Y;
        KeepPipInside();
        e.Handled = true;
    }

    private void OnPipPointerReleased(object sender, PointerRoutedEventArgs e)
    {
        pipDragStart = null;
        LocalCallVideoPip.ReleasePointerCapture(e.Pointer);
        e.Handled = true;
    }

    /// Clamp the dragged offset so the whole preview stays on the call surface.
    private void KeepPipInside()
    {
        if (LocalCallVideoPip.Parent is not FrameworkElement surface ||
            surface.ActualWidth <= 0 || LocalCallVideoPip.ActualWidth <= 0)
        {
            return;
        }

        // Where the preview sits with no offset (its docked corner).
        global::Windows.Foundation.Point placed = LocalCallVideoPip
            .TransformToVisual(surface)
            .TransformPoint(new global::Windows.Foundation.Point(-pipOffset.X, -pipOffset.Y));
        double maxLeft = Math.Max(0, surface.ActualWidth - LocalCallVideoPip.ActualWidth);
        double maxTop = Math.Max(0, surface.ActualHeight - LocalCallVideoPip.ActualHeight);
        pipOffset.X = Math.Clamp(placed.X + pipOffset.X, 0, maxLeft) - placed.X;
        pipOffset.Y = Math.Clamp(placed.Y + pipOffset.Y, 0, maxTop) - placed.Y;
    }

    /// <summary>
    /// Over a live picture the chrome gets scrims and fades out after <see cref="ChromeIdle"/>;
    /// on the face card (audio, ringing) it simply stays.
    /// </summary>
    internal void SetChromeAutoHide(bool autoHide)
    {
        if (autoHide == chromeAutoHide)
        {
            return;
        }

        chromeAutoHide = autoHide;
        Visibility scrims = autoHide ? Visibility.Visible : Visibility.Collapsed;
        CallTopScrim.Visibility = scrims;
        CallBottomScrim.Visibility = scrims;
        ShowChrome();
    }

    private void ShowChrome()
    {
        CallChrome.Opacity = 1;
        chromeIdleTimer.Stop();
        if (chromeAutoHide)
        {
            chromeIdleTimer.Start();
        }
    }
}
