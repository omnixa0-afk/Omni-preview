using System.Runtime.InteropServices;
using Windows.Graphics.Capture;
using Windows.Graphics.DirectX;
using Windows.Graphics.DirectX.Direct3D11;
using Windows.Graphics.Imaging;
using WinRT;
using WinRT.Interop;

namespace Omni.Windows.App.Features.Calls;

// Screen-share capture source (Zoom/Telemost "share screen"): captures a monitor or window the user
// picks and delivers each frame as a BGRA SoftwareBitmap, which CallVideoService feeds into the SAME
// encode+send path as camera frames. Screen sharing broadcasts the screen on the video track INSTEAD
// of the camera (a source swap; see docs/decisions/screen-share.md).
//
// Windows.Graphics.Capture is the modern, consent-based API: GraphicsCapturePicker shows the system
// window/monitor chooser (matching Zoom's picker), and no special app manifest capability is required
// because the user explicitly picks the target. Capture needs an IDirect3DDevice, so this owns a small
// D3D11 device created via the documented DXGI interop (D3D11CreateDevice +
// CreateDirect3D11DeviceFromDXGIDevice). Frames arrive as Direct3D surfaces; we copy each to a BGRA
// SoftwareBitmap (SoftwareBitmap.CreateCopyFromSurfaceAsync) so the rest of the pipeline is unchanged.
internal sealed class ScreenShareCaptureSource : IDisposable
{
    private readonly object sync = new();
    private IDirect3DDevice? device;
    private Direct3D11CaptureFramePool? framePool;
    private GraphicsCaptureSession? session;
    private GraphicsCaptureItem? item;
    private Action<SoftwareBitmap>? onFrame;
    private Action<string>? onError;
    private int frameInFlight;
    private bool disposed;

    // Frame-rate floor for a STILL screen.
    //
    // Windows.Graphics.Capture delivers a frame only when the captured content CHANGES, which
    // is right for a compositor and wrong for a video call: share a static desktop and the
    // peer gets roughly one frame every couple of seconds — measured 2026-08-11, four small
    // batches in nine seconds against the camera's twelve packets every 400 ms — so the far
    // side sits on a blank placeholder (or, before the staleness fix, on the previous camera
    // picture) and the share looks broken. Re-deliver the last frame at this cadence so the
    // receiver always gets a picture promptly and recovers from loss.
    private const int RepeatIntervalMs = 500;
    private System.Threading.Timer? repeatTimer;
    private SoftwareBitmap? lastFrame;
    private long lastDeliveryAtMs;

    // Show the system picker (monitor/window chooser). `hwnd` is the app window handle the picker
    // anchors to. Returns null if the user cancelled. Must be called on the UI thread.
    internal static async Task<GraphicsCaptureItem?> PickAsync(IntPtr hwnd)
    {
        if (!GraphicsCaptureSession.IsSupported())
        {
            throw new NotSupportedException("Windows.Graphics.Capture is not supported on this system.");
        }

        GraphicsCapturePicker picker = new();
        InitializeWithWindow.Initialize(picker, hwnd);
        return await picker.PickSingleItemAsync();
    }

    internal static bool IsSupported => GraphicsCaptureSession.IsSupported();

    /// <summary>
    /// The primary monitor as a capture target, WITHOUT the system picker.
    ///
    /// Only the UI test channel uses this. Interactive screen sharing keeps going through
    /// <see cref="PickAsync"/>, because the picker IS the user's consent — this bypass
    /// exists so an automated pass can start a share without a human clicking a system
    /// dialog, and it is reachable only when the channel is explicitly enabled.
    /// See docs/decisions/ui-test-channel.md.
    /// </summary>
    internal static GraphicsCaptureItem CreateForPrimaryMonitor()
    {
        if (!GraphicsCaptureSession.IsSupported())
        {
            throw new NotSupportedException("Windows.Graphics.Capture is not supported on this system.");
        }

        IntPtr monitor = MonitorFromWindow(IntPtr.Zero, MonitorDefaultToPrimary);
        if (monitor == IntPtr.Zero)
        {
            throw new InvalidOperationException("No primary monitor to capture.");
        }

        // IGraphicsCaptureItemInterop lives on the ACTIVATION FACTORY, not on an instance.
        // Called straight through the vtable: this app runs with the CsWinRT projection and
        // built-in COM interop off, so both `GraphicsCaptureItem.As<T>()` and
        // `Marshal.GetObjectForIUnknown` + [ComImport] fail with InvalidCastException here
        // (both were tried). Slot 4 is CreateForMonitor — IUnknown takes 0..2,
        // CreateForWindow is 3.
        return CreateFromMonitorHandle(monitor);
    }

    private static unsafe GraphicsCaptureItem CreateFromMonitorHandle(IntPtr monitor)
    {
        const string runtimeClass = "Windows.Graphics.Capture.GraphicsCaptureItem";
        Guid interopIid = new("3628E81B-3CAC-4C60-B7F4-23CE0E0C3356");
        Marshal.ThrowExceptionForHR(
            WindowsCreateString(runtimeClass, runtimeClass.Length, out IntPtr classId));
        try
        {
            Marshal.ThrowExceptionForHR(
                RoGetActivationFactory(classId, ref interopIid, out IntPtr factory));
            if (factory == IntPtr.Zero)
            {
                throw new InvalidOperationException("No GraphicsCaptureItem interop factory.");
            }

            try
            {
                IntPtr* vtable = *(IntPtr**)factory;
                var createForMonitor =
                    (delegate* unmanaged[Stdcall]<IntPtr, IntPtr, Guid*, IntPtr*, int>)vtable[4];
                // The ABI IID of IGraphicsCaptureItem. NOT typeof(GraphicsCaptureItem).GUID —
                // that is the managed projection's type guid, and passing it makes
                // CreateForMonitor answer E_NOINTERFACE.
                Guid itemIid = new("79C3F95B-31F7-4EC2-A464-632EF5D30760");
                IntPtr itemPtr;
                Marshal.ThrowExceptionForHR(
                    createForMonitor(factory, monitor, &itemIid, &itemPtr));
                if (itemPtr == IntPtr.Zero)
                {
                    throw new InvalidOperationException("CreateForMonitor returned no capture item.");
                }

                try
                {
                    return GraphicsCaptureItem.FromAbi(itemPtr);
                }
                finally
                {
                    Marshal.Release(itemPtr);
                }
            }
            finally
            {
                Marshal.Release(factory);
            }
        }
        finally
        {
            WindowsDeleteString(classId);
        }
    }

    /// <summary>
    /// The monitor <paramref name="item"/> is, if it is a whole monitor rather than a window: its
    /// rectangle on the virtual desktop (where remote screen control maps the viewer's pointer).
    ///
    /// The picker does not say what was picked. A capture item made for each monitor is compared with
    /// the picked one by display name and size — a window does not share both with a monitor. See
    /// docs/decisions/remote-screen-control.md («Весь экран или окно»).
    /// </summary>
    internal static SharedMonitor? FindMonitorOf(GraphicsCaptureItem item)
    {
        var monitors = new List<(IntPtr Handle, NativeRect Rect)>();
        EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (IntPtr monitor, IntPtr _, ref NativeRect rect, IntPtr _) =>
        {
            monitors.Add((monitor, rect));
            return true;
        }, IntPtr.Zero);
        foreach ((IntPtr handle, NativeRect rect) in monitors)
        {
            GraphicsCaptureItem candidate;
            try
            {
                candidate = CreateFromMonitorHandle(handle);
            }
            catch (Exception)
            {
                continue; // a monitor that cannot be captured cannot be the picked one either
            }

            if (candidate.DisplayName == item.DisplayName &&
                candidate.Size.Width == item.Size.Width &&
                candidate.Size.Height == item.Size.Height)
            {
                return new SharedMonitor(rect.Left, rect.Top, rect.Right - rect.Left, rect.Bottom - rect.Top);
            }
        }

        return null;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NativeRect
    {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }

    private delegate bool MonitorEnumProc(IntPtr monitor, IntPtr hdc, ref NativeRect rect, IntPtr data);

    [DllImport("user32.dll")]
    private static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonitorEnumProc callback, IntPtr data);

    private const uint MonitorDefaultToPrimary = 1;

    [DllImport("user32.dll")]
    private static extern IntPtr MonitorFromWindow(IntPtr hwnd, uint dwFlags);

    [DllImport("combase.dll", CharSet = CharSet.Unicode)]
    private static extern int WindowsCreateString(string sourceString, int length, out IntPtr hstring);

    [DllImport("combase.dll")]
    private static extern int WindowsDeleteString(IntPtr hstring);

    [DllImport("combase.dll")]
    private static extern int RoGetActivationFactory(IntPtr activatableClassId, ref Guid iid, out IntPtr factory);

    // Begin capturing `captureItem`. `frameCallback` is invoked (off the UI thread) with a fresh BGRA
    // SoftwareBitmap per frame — the caller OWNS and must Dispose it. `errorCallback` reports capture
    // faults (e.g. the shared window closed). Idempotent-safe: call Stop()/Dispose() before re-Start().
    internal void Start(
        GraphicsCaptureItem captureItem,
        Action<SoftwareBitmap> frameCallback,
        Action<string> errorCallback)
    {
        ArgumentNullException.ThrowIfNull(captureItem);
        ArgumentNullException.ThrowIfNull(frameCallback);

        lock (sync)
        {
            if (disposed)
            {
                throw new ObjectDisposedException(nameof(ScreenShareCaptureSource));
            }

            StopLocked();

            item = captureItem;
            onFrame = frameCallback;
            onError = errorCallback;
            device = Direct3D11Interop.CreateDevice();

            // Double-buffered pool at the item's current size; BGRA8 matches the encode path's fast copy.
            // FREE-THREADED: `Create` raises FrameArrived through the DispatcherQueue of the thread that
            // made the pool, and a thread-pool thread has none — so a share started off the UI thread
            // (the remote-control owner starts it from Task.Run) got the yellow capture border and
            // never a single frame. Live 2026-10-06: `screen_share=capturing loop=0fps` on a PC whose
            // screen was on and in use; the earlier night reading "monitor is off" was this bug.
            // The handler is already thread-safe (frameInFlight + sync).
            framePool = Direct3D11CaptureFramePool.CreateFreeThreaded(
                device,
                DirectXPixelFormat.B8G8R8A8UIntNormalized,
                numberOfBuffers: 2,
                captureItem.Size);
            framePool.FrameArrived += OnFrameArrived;

            // If the target closes (window shared then closed), surface it as an error so the caller
            // can stop the share cleanly instead of silently freezing on the last frame.
            captureItem.Closed += OnItemClosed;

            session = framePool.CreateCaptureSession(captureItem);
            session.StartCapture();

            lastDeliveryAtMs = Environment.TickCount64;
            repeatTimer = new System.Threading.Timer(
                _ => RepeatLastFrame(),
                null,
                RepeatIntervalMs,
                RepeatIntervalMs);
        }
    }

    private void OnItemClosed(GraphicsCaptureItem sender, object args)
    {
        onError?.Invoke("Источник демонстрации закрыт.");
    }

    /// Re-deliver the most recent frame when the screen has been still. Skips whenever a real
    /// frame arrived within the interval, so a moving screen never pays for this.
    private void RepeatLastFrame()
    {
        SoftwareBitmap? copy = null;
        Action<SoftwareBitmap>? callback;
        try
        {
            lock (sync)
            {
                if (disposed ||
                    lastFrame is null ||
                    Environment.TickCount64 - lastDeliveryAtMs < RepeatIntervalMs)
                {
                    return;
                }

                callback = onFrame;
                if (callback is null)
                {
                    return;
                }

                copy = SoftwareBitmap.Copy(lastFrame);
                lastDeliveryAtMs = Environment.TickCount64;
            }

            SoftwareBitmap delivered = copy;
            copy = null;
            callback(delivered);
        }
        catch (Exception)
        {
            // A repeat is best-effort: the next real frame or the next tick will do. Never let
            // it fault the capture session.
            copy?.Dispose();
        }
    }

    private void OnFrameArrived(Direct3D11CaptureFramePool sender, object args)
    {
        // Coalesce: if a frame is still being copied/encoded, drop this one (realtime — latest wins).
        if (Interlocked.Exchange(ref frameInFlight, 1) == 1)
        {
            using Direct3D11CaptureFrame? skipped = sender.TryGetNextFrame();
            Interlocked.Exchange(ref frameInFlight, 0);
            return;
        }

        _ = ProcessNextFrameAsync(sender);
    }

    private async Task ProcessNextFrameAsync(Direct3D11CaptureFramePool sender)
    {
        SoftwareBitmap? bitmap = null;
        try
        {
            using Direct3D11CaptureFrame? frame = sender.TryGetNextFrame();
            if (frame is null)
            {
                return;
            }

            // Surface (GPU texture) -> CPU BGRA SoftwareBitmap, the format the encode path consumes.
            bitmap = await SoftwareBitmap.CreateCopyFromSurfaceAsync(
                frame.Surface,
                BitmapAlphaMode.Premultiplied);

            Action<SoftwareBitmap>? callback;
            lock (sync)
            {
                callback = disposed ? null : onFrame;
            }

            if (callback is null)
            {
                bitmap.Dispose();
                bitmap = null;
                return;
            }

            // Keep our own copy so the repeat pump can re-deliver a still screen; the callback
            // owns and disposes what it is handed, so it can never be the cached one.
            SoftwareBitmap cached = SoftwareBitmap.Copy(bitmap);
            lock (sync)
            {
                lastFrame?.Dispose();
                lastFrame = cached;
                lastDeliveryAtMs = Environment.TickCount64;
            }

            // Ownership passes to the callback (it disposes after copy/encode).
            SoftwareBitmap delivered = bitmap;
            bitmap = null;
            callback(delivered);
        }
        catch (Exception ex)
        {
            bitmap?.Dispose();
            onError?.Invoke($"Кадр экрана не захвачен: {ex.Message}");
        }
        finally
        {
            Interlocked.Exchange(ref frameInFlight, 0);
        }
    }

    internal void Stop()
    {
        lock (sync)
        {
            StopLocked();
        }
    }

    private void StopLocked()
    {
        if (repeatTimer is not null)
        {
            try { repeatTimer.Dispose(); } catch { /* teardown best-effort */ }
            repeatTimer = null;
        }

        if (lastFrame is not null)
        {
            try { lastFrame.Dispose(); } catch { /* teardown best-effort */ }
            lastFrame = null;
        }

        if (session is not null)
        {
            try { session.Dispose(); } catch { /* teardown best-effort */ }
            session = null;
        }

        if (framePool is not null)
        {
            framePool.FrameArrived -= OnFrameArrived;
            try { framePool.Dispose(); } catch { /* teardown best-effort */ }
            framePool = null;
        }

        if (item is not null)
        {
            item.Closed -= OnItemClosed;
            item = null;
        }

        if (device is not null)
        {
            try { (device as IDisposable)?.Dispose(); } catch { /* teardown best-effort */ }
            device = null;
        }

        onFrame = null;
        onError = null;
        Interlocked.Exchange(ref frameInFlight, 0);
    }

    public void Dispose()
    {
        lock (sync)
        {
            if (disposed)
            {
                return;
            }

            disposed = true;
            StopLocked();
        }
    }
}

// Minimal DXGI interop to create the IDirect3DDevice that Direct3D11CaptureFramePool requires. This is
// the documented path (there is no pure-WinRT device factory): create a D3D11 device, then wrap its
// DXGI device as a WinRT IDirect3DDevice via CreateDirect3D11DeviceFromDXGIDevice.
internal static class Direct3D11Interop
{
    internal static IDirect3DDevice CreateDevice()
    {
        // BGRA_SUPPORT so the capture surfaces match the B8G8R8A8 frame pool format.
        const uint D3D11_CREATE_DEVICE_BGRA_SUPPORT = 0x20;
        const uint D3D_DRIVER_TYPE_HARDWARE = 1;
        const uint D3D_DRIVER_TYPE_WARP = 5;
        const uint D3D11_SDK_VERSION = 7;

        int hr = D3D11CreateDevice(
            IntPtr.Zero,
            D3D_DRIVER_TYPE_HARDWARE,
            IntPtr.Zero,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            IntPtr.Zero,
            0,
            D3D11_SDK_VERSION,
            out IntPtr d3dDevice,
            out _,
            out _);

        if (hr != 0)
        {
            // Fall back to the WARP software renderer (e.g. no GPU / RDP session).
            hr = D3D11CreateDevice(
                IntPtr.Zero,
                D3D_DRIVER_TYPE_WARP,
                IntPtr.Zero,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                IntPtr.Zero,
                0,
                D3D11_SDK_VERSION,
                out d3dDevice,
                out _,
                out _);
        }

        if (hr != 0 || d3dDevice == IntPtr.Zero)
        {
            throw new InvalidOperationException($"D3D11CreateDevice failed (hr=0x{hr:X8}).");
        }

        try
        {
            IntPtr dxgiDevice = QueryDxgiDevice(d3dDevice);
            try
            {
                int wrapHr = CreateDirect3D11DeviceFromDXGIDevice(dxgiDevice, out IntPtr graphicsDevice);
                if (wrapHr != 0 || graphicsDevice == IntPtr.Zero)
                {
                    throw new InvalidOperationException(
                        $"CreateDirect3D11DeviceFromDXGIDevice failed (hr=0x{wrapHr:X8}).");
                }

                try
                {
                    return MarshalInspectable<IDirect3DDevice>.FromAbi(graphicsDevice);
                }
                finally
                {
                    Marshal.Release(graphicsDevice);
                }
            }
            finally
            {
                Marshal.Release(dxgiDevice);
            }
        }
        finally
        {
            Marshal.Release(d3dDevice);
        }
    }

    private static IntPtr QueryDxgiDevice(IntPtr d3dDevice)
    {
        Guid iidDxgiDevice = new("54ec77fa-1377-44e6-8c32-88fd5f44c84c"); // IDXGIDevice
        int hr = Marshal.QueryInterface(d3dDevice, in iidDxgiDevice, out IntPtr dxgiDevice);
        if (hr != 0 || dxgiDevice == IntPtr.Zero)
        {
            throw new InvalidOperationException($"QueryInterface(IDXGIDevice) failed (hr=0x{hr:X8}).");
        }

        return dxgiDevice;
    }

    [DllImport("d3d11.dll", ExactSpelling = true)]
    private static extern int D3D11CreateDevice(
        IntPtr pAdapter,
        uint driverType,
        IntPtr software,
        uint flags,
        IntPtr pFeatureLevels,
        uint featureLevels,
        uint sdkVersion,
        out IntPtr ppDevice,
        out uint pFeatureLevel,
        out IntPtr ppImmediateContext);

    [DllImport("d3d11.dll", ExactSpelling = true)]
    private static extern int CreateDirect3D11DeviceFromDXGIDevice(
        IntPtr dxgiDevice,
        out IntPtr graphicsDevice);
}

/// A whole monitor being shared: its rectangle on the virtual desktop, in physical pixels.
internal sealed record SharedMonitor(int Left, int Top, int Width, int Height);
