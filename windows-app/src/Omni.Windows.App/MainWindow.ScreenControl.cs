using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;

namespace Omni.Windows.App;

// Remote screen control (docs/decisions/remote-screen-control.md) — the shell's wiring. Sharer: the
// hand button grants the peer control of the monitor we share, and the injector applies what the
// core verified. Viewer: the "Управлять" toggle turns the peer's video into an input surface;
// pointer, buttons, wheel and keys go to the core, which signs and sends them.
public sealed partial class MainWindow
{
    private ScreenControlInjector? screenControlInjector;
    private ScreenControlViewer? screenControlViewer;
    /// The element whose area IS the sharer's screen while we control (pointer → 0..1 across it).
    private FrameworkElement? screenControlSurface;
    private bool screenControlKeyboardHooked;
    private CallScreenControlState callScreenControlState = new(false, false);

    private ScreenControlInjector ScreenControlInjector =>
        screenControlInjector ??= new ScreenControlInjector(() => callVideoService.SharedMonitor);

    /// The core hands verified input to our injector; installed before anything is granted.
    private void InstallScreenControlSink() =>
        rustBridge.SetScreenControlSink(ScreenControlInjector.SinkPointer, IntPtr.Zero);

    // --- 1:1, sharer ---------------------------------------------------------------------------

    private void OnToggleCurrentCallScreenControlClicked(object sender, RoutedEventArgs e) =>
        SetActionStatus(SetCallScreenControlGrant(!callScreenControlState.IGrant));

    /// Grant or withdraw control of the screen we share in the current 1:1 call. Returns the status
    /// line for the user (or the test channel).
    private string SetCallScreenControlGrant(bool grant, CallSummary? forCall = null)
    {
        // A remote-control session grants for ITS call, whichever chat is open on the owner's side.
        CallSummary? call = forCall ?? currentChatCallSummary;
        if (call is null || call.State != "active")
        {
            return "Управление экраном доступно только во время звонка.";
        }

        if (grant && callVideoService.SharedMonitor is null)
        {
            return "Управлять можно только всем экраном: покажите экран целиком, а не окно.";
        }

        try
        {
            if (grant)
            {
                InstallScreenControlSink();
            }

            rustBridge.SetCallScreenControl(call.CallId, grant);
            if (!grant)
            {
                screenControlInjector?.ReleaseAll();
            }

            RefreshCallScreenControlState();
            return grant ? "Собеседник может управлять вашим экраном." : "Управление экраном отключено.";
        }
        catch (Exception ex)
        {
            return $"Не удалось изменить управление экраном: {NormalizeUserError(ex)}";
        }
    }

    /// Read the 1:1 grant state and bring the buttons in line with it. A grant outlives nothing: if we
    /// stopped sharing the monitor (or switched to a window), it is withdrawn here.
    private void RefreshCallScreenControlState()
    {
        CallSummary? call = currentChatCallSummary;
        bool live = call is not null && call.State == "active" && callVideoService.IsActiveForDialog(call.DialogKey);
        try
        {
            callScreenControlState = live ? rustBridge.GetCallScreenControlState() : new(false, false);
        }
        catch (Exception)
        {
            callScreenControlState = new(false, false);
        }

        if (callScreenControlState.IGrant && callVideoService.SharedMonitor is null && call is not null)
        {
            try
            {
                rustBridge.SetCallScreenControl(call.CallId, false);
            }
            catch (Exception)
            {
                // The call is going away; the core drops the grant with it.
            }

            screenControlInjector?.ReleaseAll();
            callScreenControlState = callScreenControlState with { IGrant = false };
        }

        ToggleCurrentCallScreenControlButton.Visibility =
            live && callVideoService.SharedMonitor is not null ? Visibility.Visible : Visibility.Collapsed;
        ToggleCurrentCallScreenControlButton.IsOn = callScreenControlState.IGrant;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ToggleCurrentCallScreenControlButton,
            callScreenControlState.IGrant ? "Запретить управление экраном" : "Разрешить управление экраном");

        bool mayControl = live && callScreenControlState.PeerGrantsMe && callVideoService.HasRemoteVideo;
        ToggleRemoteScreenControlButton.Visibility = mayControl ? Visibility.Visible : Visibility.Collapsed;
        if (!mayControl && screenControlViewer is { Sharer: null })
        {
            StopRemoteScreenControl();
        }

        AutoStartRemoteControl(mayControl);
        RenderRemoteControlNotice(call);
        bool controlling1to1 = screenControlViewer is { Sharer: null };
        bool inSession = call?.RemoteControl is { IsController: true };
        ToggleRemoteScreenControlText.Text = controlling1to1
            ? (inSession ? "Только смотреть" : "Перестать управлять")
            : "Управлять";
        RemoteScreenControlSurface.Visibility = controlling1to1 ? Visibility.Visible : Visibility.Collapsed;
    }

    // --- viewer ----------------------------------------------------------------------------------

    private void OnToggleRemoteScreenControlClicked(object sender, RoutedEventArgs e)
    {
        if (screenControlViewer is not null)
        {
            StopRemoteScreenControl();
            remoteControlViewOnly = true; // in a session: stay watching until asked to control again
        }
        else if (currentChatCallSummary is { } call && callScreenControlState.PeerGrantsMe)
        {
            StartRemoteScreenControl(call.DialogKey, sharer: null, RemoteScreenControlSurface);
            remoteControlViewOnly = false;
        }

        RefreshCallScreenControlState();
    }

    private void StartRemoteScreenControl(string dialogKey, string? sharer, FrameworkElement surface)
    {
        StopRemoteScreenControl();
        screenControlViewer = new ScreenControlViewer(rustBridge, dialogKey, sharer);
        screenControlSurface = surface;
        HookScreenControlKeyboard();
        SetActionStatus("Вы управляете экраном собеседника. Клавиатура и мышь над видео уходят ему.");
    }

    private void StopRemoteScreenControl()
    {
        screenControlViewer?.Dispose();
        screenControlViewer = null;
        screenControlSurface = null;
    }

    private readonly System.Runtime.CompilerServices.ConditionalWeakTable<FrameworkElement, object> wiredScreenControlSurfaces = new();

    private void WireRemoteScreenControlSurface(FrameworkElement surface)
    {
        if (wiredScreenControlSurfaces.TryGetValue(surface, out _))
        {
            return; // a tile Image comes back here on every start; one set of handlers is enough
        }

        wiredScreenControlSurfaces.Add(surface, new object());
        surface.PointerMoved += OnScreenControlPointer;
        surface.PointerPressed += OnScreenControlPointer;
        surface.PointerReleased += OnScreenControlPointer;
        surface.PointerWheelChanged += OnScreenControlWheel;
    }

    private void OnScreenControlPointer(object sender, PointerRoutedEventArgs e)
    {
        if (screenControlViewer is not { } viewer || !ReferenceEquals(sender, screenControlSurface) || sender is not FrameworkElement surface)
        {
            return;
        }

        PointerPoint point = e.GetCurrentPoint(surface);
        // 1:1: what the GPU renderer last presented, or — on the software fallback — the Image's bitmap.
        (int Width, int Height)? picture = callVideoService.RemotePresentedSize
            ?? (RemoteCallVideoImage.Source is Microsoft.UI.Xaml.Media.Imaging.BitmapSource { PixelWidth: > 0, PixelHeight: > 0 } bitmap
                ? (bitmap.PixelWidth, bitmap.PixelHeight)
                : null);
        if (MapToSharedScreen(surface, point.Position, picture) is { } at)
        {
            viewer.PointerAt(at.X, at.Y);
        }

        if (ButtonOf(point.Properties.PointerUpdateKind) is { } change)
        {
            if (change.Down)
            {
                surface.CapturePointer(e.Pointer);
            }

            viewer.Button(change.Button, change.Down);
        }

        e.Handled = true;
    }

    private void OnScreenControlWheel(object sender, PointerRoutedEventArgs e)
    {
        if (screenControlViewer is not { } viewer || !ReferenceEquals(sender, screenControlSurface) || sender is not FrameworkElement surface)
        {
            return;
        }

        PointerPointProperties properties = e.GetCurrentPoint(surface).Properties;
        short delta = (short)Math.Clamp(properties.MouseWheelDelta, short.MinValue, short.MaxValue);
        if (properties.IsHorizontalMouseWheel)
        {
            viewer.Wheel(delta, 0);
        }
        else
        {
            viewer.Wheel(0, delta);
        }

        e.Handled = true;
    }

    /// Where `position` falls on the sharer's screen, 0..1 on each axis. Both the 1:1 swapchain and a
    /// group screen tile show the picture whole, centred, with bars (FitViewport in
    /// d3d11_video_renderer.h; `Stretch.Uniform` on the tile), so it is the picture's own rectangle
    /// inside the surface that maps. The picture's size: the tile's bitmap, or for 1:1 what the
    /// native renderer last presented (`swapchainPicture`). Unknown — no mapping.
    private static (double X, double Y)? MapToSharedScreen(
        FrameworkElement surface,
        global::Windows.Foundation.Point position,
        (int Width, int Height)? swapchainPicture)
    {
        (int Width, int Height)? picture = surface is Image { Source: Microsoft.UI.Xaml.Media.Imaging.BitmapSource { PixelWidth: > 0, PixelHeight: > 0 } source }
            ? (source.PixelWidth, source.PixelHeight)
            : swapchainPicture;
        return picture is { } size ? FitPoint(surface.ActualWidth, surface.ActualHeight, size.Width, size.Height, position.X, position.Y) : null;
    }

    /// `(x, y)` on a `width`×`height` surface showing a `pictureWidth`×`pictureHeight` picture fitted
    /// whole and centred → 0..1 across the picture (outside it, beyond 0..1; the viewer clamps).
    internal static (double X, double Y)? FitPoint(double width, double height, int pictureWidth, int pictureHeight, double x, double y)
    {
        if (width <= 0 || height <= 0 || pictureWidth <= 0 || pictureHeight <= 0)
        {
            return null;
        }

        double scale = Math.Min(width / pictureWidth, height / pictureHeight);
        double shownWidth = pictureWidth * scale;
        double shownHeight = pictureHeight * scale;
        return ((x - (width - shownWidth) / 2) / shownWidth, (y - (height - shownHeight) / 2) / shownHeight);
    }

    private static (string Button, bool Down)? ButtonOf(PointerUpdateKind kind) => kind switch
    {
        PointerUpdateKind.LeftButtonPressed => ("left", true),
        PointerUpdateKind.LeftButtonReleased => ("left", false),
        PointerUpdateKind.RightButtonPressed => ("right", true),
        PointerUpdateKind.RightButtonReleased => ("right", false),
        PointerUpdateKind.MiddleButtonPressed => ("middle", true),
        PointerUpdateKind.MiddleButtonReleased => ("middle", false),
        PointerUpdateKind.XButton1Pressed => ("x1", true),
        PointerUpdateKind.XButton1Released => ("x1", false),
        PointerUpdateKind.XButton2Pressed => ("x2", true),
        PointerUpdateKind.XButton2Released => ("x2", false),
        _ => null,
    };

    /// While we control, every key goes to the sharer (tunnelling handler on the window root, so no
    /// focused control eats it first). Keys the system keeps for itself (Win, Alt+Tab, Ctrl+Alt+Del)
    /// never reach an app and are not sent.
    private void HookScreenControlKeyboard()
    {
        if (screenControlKeyboardHooked || Content is not UIElement root)
        {
            return;
        }

        root.PreviewKeyDown += (_, e) => ForwardScreenControlKey(e, down: true);
        root.PreviewKeyUp += (_, e) => ForwardScreenControlKey(e, down: false);
        screenControlKeyboardHooked = true;
    }

    private void ForwardScreenControlKey(KeyRoutedEventArgs e, bool down)
    {
        if (screenControlViewer is not { } viewer)
        {
            return;
        }

        viewer.Key((ushort)e.OriginalKey, (ushort)e.KeyStatus.ScanCode, e.KeyStatus.IsExtendedKey, down);
        e.Handled = true;
    }
}
