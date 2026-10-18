using System.Buffers;
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Security.Cryptography;
using System.Text.Json;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Core;
using Windows.Graphics.Imaging;
using Windows.Foundation;
using Windows.Media;
using Windows.Media.Capture;
using Windows.Media.Capture.Frames;
using Windows.Media.Devices;
using Windows.Media.MediaProperties;
using WinRT;

namespace Omni.Windows.App.Features.Calls;

// Frame render pipeline: local self-view preview and remote video presentation, including the
// GPU SwapChainPanel renderer (attach / resize / reset) and the software WriteableBitmap fallback.
// Split out of CallVideoService.cs as a partial; same class, identical IL, no behaviour change.
internal sealed partial class CallVideoService
{
    private void QueueLocalPreviewFrame(LocalPreviewFrame? frame)
    {
        if (frame is null)
        {
            return;
        }

        bool shouldStartLoop = false;
        lock (localPreviewSync)
        {
            pendingLocalPreviewFrame?.Dispose();
            pendingLocalPreviewFrame = frame;
            if (localPreviewLoopScheduled == 0)
            {
                localPreviewLoopScheduled = 1;
                shouldStartLoop = true;
            }
        }

        if (shouldStartLoop)
        {
            // Track + cancel so StopAsync awaits clean exit before disposing
            // localPreviewBitmap / Image control.
            // See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
            CancellationToken loopToken = mediaCancellation?.Token ?? CancellationToken.None;
            Volatile.Write(ref localPreviewLoopTask, Task.Run(() => ProcessLocalPreviewLoopAsync(loopToken)));
        }
    }

    private async Task ProcessLocalPreviewLoopAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            LocalPreviewFrame? frame;
            lock (localPreviewSync)
            {
                frame = pendingLocalPreviewFrame;
                pendingLocalPreviewFrame = null;
                if (frame is null)
                {
                    localPreviewLoopScheduled = 0;
                    return;
                }
            }

            BgraFrame? preview = null;
            try
            {
                if (!localCameraActive)
                {
                    continue;
                }

                preview = frame.ToBgraFrame();
                await RenderFrameAsync(localPreviewImage, true, preview);
                // Group call (T3b): the SAME preview frame also renders into our own participant
                // tile (registered under GroupSelfTileKey) — the self tile shows the local camera
                // directly, never round-tripping the network. Borrowed frame; disposed below.
                if (activeGroupMode)
                {
                    await RenderIntoGroupTileAsync(GroupSelfTileKey, preview);
                }
            }
            catch (Exception ex)
            {
                RecordError($"Local preview failed: {FormatExceptionForDiagnostics(ex)}");
            }
            finally
            {
                preview?.Dispose();
                frame.Dispose();
            }
        }

        // Cancellation observed — drop any remaining pending frame and clear
        // the scheduled flag so a future call can re-arm the loop.
        lock (localPreviewSync)
        {
            pendingLocalPreviewFrame?.Dispose();
            pendingLocalPreviewFrame = null;
            localPreviewLoopScheduled = 0;
        }
    }

    private void ClearPendingLocalPreviewFrame()
    {
        lock (localPreviewSync)
        {
            pendingLocalPreviewFrame?.Dispose();
            pendingLocalPreviewFrame = null;
        }
    }

    private void QueueRemoteRenderFrame(BgraFrame frame)
    {
        bool shouldStartLoop = false;
        lock (remoteRenderSync)
        {
            pendingRemoteRenderFrame?.Dispose();
            pendingRemoteRenderFrame = frame;
            if (remoteRenderLoopScheduled == 0)
            {
                remoteRenderLoopScheduled = 1;
                shouldStartLoop = true;
            }
        }

        if (shouldStartLoop)
        {
            // Track + cancel so StopAsync awaits clean exit before disposing
            // remoteVideoBitmap / native receiver.
            // See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
            CancellationToken loopToken = mediaCancellation?.Token ?? CancellationToken.None;
            Volatile.Write(ref remoteRenderLoopTask, Task.Run(() => ProcessRemoteRenderLoopAsync(loopToken)));
        }
    }

    private async Task ProcessRemoteRenderLoopAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            BgraFrame? frame;
            lock (remoteRenderSync)
            {
                frame = pendingRemoteRenderFrame;
                pendingRemoteRenderFrame = null;
                if (frame is null)
                {
                    remoteRenderLoopScheduled = 0;
                    return;
                }
            }

            long renderStartedAtMs = Environment.TickCount64;
            try
            {
                await RenderFrameAsync(remoteVideoImage, false, frame);
                Interlocked.Increment(ref framesRendered);
                Interlocked.Exchange(ref lastRemoteFrameRenderedAtMs, Environment.TickCount64);
                PublishDiagnostics();
                PublishStateChanged();
            }
            catch (Exception ex)
            {
                Interlocked.Increment(ref renderStalls);
                RecordError(FormatExceptionForDiagnostics(ex));
            }
            finally
            {
                Interlocked.Add(ref renderFrameProcessingMs, Math.Max(0, Environment.TickCount64 - renderStartedAtMs));
                frame?.Dispose();
            }
        }

        // Cancellation observed — drop pending frame, clear scheduled flag.
        lock (remoteRenderSync)
        {
            pendingRemoteRenderFrame?.Dispose();
            pendingRemoteRenderFrame = null;
            remoteRenderLoopScheduled = 0;
        }
    }

    // Remote video is presented on its OWN thread, clocked by the display — not on the UI thread,
    // and not by the network. Both of those were design errors, and raising the dispatcher priority
    // only softened the first one.
    //
    // Neither Telegram nor Discord paints video from the general UI loop: the frame goes to a
    // surface the compositor owns, on the display's clock, so shell work cannot stall the picture
    // and a burst of packets cannot make it lurch. We had the opposite on both counts. The present
    // sat in the same dispatcher queue as the whole shell — measured on the receiving machine, gaps
    // between presented frames reached 578 ms while the decoder was producing 30 fps — and it was
    // driven from the inbound datagram batch, so frames reached the swapchain in bursts shaped by
    // packet arrival.
    //
    // Nothing in the native layer ever asked for the UI thread. Its D3D11 device is created
    // multithread-protected (the decoder MFT already decodes on its own thread), and the one call
    // that genuinely needs the UI thread is ISwapChainPanelNative::SetSwapChain — the attach, once
    // per receiver session. Present does not.
    //
    // What still runs on the UI thread: attach (below), resize / DPI change, teardown. Those and
    // the present are serialised against each other by the receiver session's own Gate — and the
    // UI thread only ever TRIES that gate, with a timeout: the present goes into a third-party
    // decoder, and a decoder that never returns (Media Foundation HEVC, live 2026-10-17) must not
    // take the window down with it. A tick stuck longer than HungDecoderMs is caught by
    // DetectHungVideoDecoder, which gives that receiver up and lets a fresh one be built.
    //
    // See docs/decisions/video-presentation-thread.md, docs/bugs/hung-decoder-froze-the-app.md.

    // Media thread, every inbound batch. The common case is one reference comparison: the presenter
    // for the current decoder is already running. Work happens only when the decoder is new — which
    // means the swapchain (it lives inside the session's renderer) is new too.
    private void EnsureRemoteGpuPresenter()
    {
        NativeWebRtcMediaEngine.Session? receiver;
        lock (nativeMediaSync)
        {
            receiver = nativeReceiverSession;
        }

        if (receiver is null || ReferenceEquals(Volatile.Read(ref gpuPresenterReceiver), receiver))
        {
            return;
        }

        if (Interlocked.CompareExchange(ref gpuPresenterStartRunning, 1, 0) != 0)
        {
            return;
        }

        _ = StartRemoteGpuPresenterAsync(receiver);
    }

    // Attach the panel to this receiver's swapchain (UI thread — the only part that needs it), then
    // hand the receiver to a presenter thread. An attach failure flips gpuVideoEnabled false for the
    // rest of the call so DrainDecodedNativeFrames falls back to the BGRA + WriteableBitmap path:
    // the GPU path never regresses a working software path.
    private async Task StartRemoteGpuPresenterAsync(NativeWebRtcMediaEngine.Session receiver)
    {
        try
        {
            // A previous decoder's presenter, if any, owns a swapchain that is about to be replaced
            // on the panel. Stop it before attaching the new one.
            await StopRemoteGpuPresenterAsync();

            bool attached = false;
            await RunOnUiAsync(() => attached = TryAttachRemoteGpuRenderer(receiver));
            if (!attached)
            {
                // The decoded frames are untouched in the native deque (RenderAttachedVideo was
                // never called), so the software drain picks them up on the next receive batch.
                gpuVideoEnabled = false;
                gpuVideoPath = false;
                return;
            }

            CancellationTokenSource stop = CancellationTokenSource.CreateLinkedTokenSource(
                mediaCancellation?.Token ?? CancellationToken.None);
            Thread presenter = new(() => RunRemoteGpuPresentLoop(receiver, stop.Token))
            {
                IsBackground = true,
                Name = "omni-video-present",
                // Above the thread pool, below the audio pump: a late frame is visible, and this
                // thread does nothing but wait and present.
                Priority = ThreadPriority.AboveNormal,
            };

            Volatile.Write(ref gpuPresenterReceiver, receiver);
            Volatile.Write(ref gpuPresenterCancellation, stop);
            Volatile.Write(ref gpuPresenterThread, presenter);
            presenter.Start();
        }
        catch (Exception ex)
        {
            RecordError($"GPU presenter start failed: {FormatExceptionForDiagnostics(ex)}");
            gpuVideoEnabled = false;
            gpuVideoPath = false;
        }
        finally
        {
            Interlocked.Exchange(ref gpuPresenterStartRunning, 0);
        }
    }

    // Stop and join the running presenter. Must complete before the receiver session it presents is
    // disposed — the thread calls into that session on every tick. False when the thread did not
    // stop: it is stuck inside the receiver's decoder, and that receiver must be abandoned, never
    // disposed (destroying a codec under a thread inside it crashes the process).
    private async Task<bool> StopRemoteGpuPresenterAsync()
    {
        CancellationTokenSource? stop = Interlocked.Exchange(ref gpuPresenterCancellation, null);
        Thread? presenter = Interlocked.Exchange(ref gpuPresenterThread, null);
        Volatile.Write(ref gpuPresenterReceiver, null);
        if (stop is null)
        {
            return true;
        }

        stop.Cancel();
        bool stopped = true;
        if (presenter is not null)
        {
            // Off the caller's thread: the join is short (the loop's longest wait is one tick), but
            // StopAsync is called from the UI thread on window close and must not block on it.
            stopped = await Task.Run(() => presenter.Join(PresenterJoinTimeoutMs));
        }

        if (stopped)
        {
            // A thread still running may yet read the token; it is left to the GC then.
            stop.Dispose();
        }

        return stopped;
    }

    // Every shell call into the 1:1 receiver's decoder goes through here: the moment it went in is
    // what DetectHungVideoDecoder watches.
    private T CallIntoReceiver<T>(NativeWebRtcMediaEngine.Session receiver, Func<T> call)
    {
        Volatile.Write(ref receiverInCall, receiver);
        Interlocked.Exchange(ref receiverCallStartedAtMs, Environment.TickCount64);
        try
        {
            return call();
        }
        finally
        {
            Interlocked.Exchange(ref receiverCallStartedAtMs, 0);
        }
    }

    /// <summary>
    /// Once a second, from the negotiation monitor: is the 1:1 receiver stuck inside its decoder?
    /// A decode + present tick takes milliseconds; one that has not returned for HungDecoderMs
    /// never will (the Media Foundation HEVC decoder waited in ProcessOutput for good, live
    /// 2026-10-17). The receiver is given up — abandoned, not disposed — and the next inbound
    /// video packet builds a fresh one, which asks the peer for a key frame as it starts.
    /// <paramref name="struck"/> = the codec was struck for the rest of the call, so the caller
    /// announces the passport without it (H.264 is never struck: it rebuilds on H.264).
    /// </summary>
    private bool DetectHungVideoDecoder(out bool struck)
    {
        struck = false;
        long startedAtMs = Interlocked.Read(ref receiverCallStartedAtMs);
        if (startedAtMs == 0)
        {
            return false;
        }

        long stuckMs = Environment.TickCount64 - startedAtMs;
        NativeWebRtcMediaEngine.Session? hung = Volatile.Read(ref receiverInCall);
        if (stuckMs < HungDecoderMs || hung is null || hung.IsAbandoned)
        {
            return false;
        }

        hung.Abandon();
        Interlocked.CompareExchange(ref receiverCallStartedAtMs, 0, startedAtMs);
        lock (nativeMediaSync)
        {
            if (ReferenceEquals(nativeReceiverSession, hung))
            {
                // activeReceiverSelectionSignature stays: the next inbound video packet builds a
                // receiver for the same agreed format (EnsureNativeVideoReceiverForInboundVideo).
                nativeReceiverSession = null;
                activeMediaRouter?.SetReceiver(null);
                nativeMediaDiagnostics = "native_receiver=awaiting_video(previous decoder hung)";
            }
        }

        // The presenter thread is stuck inside that receiver for good: free its slot so the next
        // receiver attaches its own swapchain and starts its own presenter. If the call ever does
        // return, the thread sees the receiver replaced and exits.
        if (ReferenceEquals(Volatile.Read(ref gpuPresenterReceiver), hung))
        {
            Interlocked.Exchange(ref gpuPresenterCancellation, null)?.Cancel();
            Interlocked.Exchange(ref gpuPresenterThread, null);
            Interlocked.CompareExchange(ref gpuPresenterReceiver, null, hung);
        }

        if (ReferenceEquals(gpuRendererAttachedReceiver, hung))
        {
            gpuRendererAttachedReceiver = null;
        }

        CallVideoCodecKind? codec = CallVideoCodecFactory.ParseCodec(activeReceiveStream?.Codec);
        lock (lifecycleSync)
        {
            struck = codec is { } kind && codecHealth.StrikeDecoder(kind);
        }

        string codecName = codec is null ? "unknown" : CallVideoCodecFactory.FormatCodec(codec.Value);
        Core.AppTrace.Write("media/decoder-hung", $"dialog={activeDialogKey} codec={codecName} stuck_ms={stuckMs} struck={struck}");
        RecordError($"Video decoder for {codecName} did not return for {stuckMs} ms; it was given up and a new one is built{(struck ? ", the passport is announced without it" : string.Empty)}.");
        return true;
    }

    // Dispose a receiver that is no longer current — off the caller's thread, and only once its
    // gate is free: a presenter or drain may still be inside it. A receiver whose gate stays taken
    // is stuck in its decoder and is abandoned instead.
    private static void RetireReceiver(NativeWebRtcMediaEngine.Session? receiver)
    {
        if (receiver is null)
        {
            return;
        }

        _ = Task.Run(() => RetireReceiverNow(receiver));
    }

    // The same, awaited — for teardown, which must not return with sessions still alive.
    private static Task RetireReceiverAsync(NativeWebRtcMediaEngine.Session? receiver)
        => receiver is null ? Task.CompletedTask : Task.Run(() => RetireReceiverNow(receiver));

    private static void RetireReceiverNow(NativeWebRtcMediaEngine.Session receiver)
    {
        if (!Monitor.TryEnter(receiver.Gate, HungDecoderMs))
        {
            receiver.Abandon();
            return;
        }

        try
        {
            receiver.Dispose();
        }
        finally
        {
            Monitor.Exit(receiver.Gate);
        }
    }

    // The presenter thread. Waits for the display tick, presents the frame the native jitter buffer
    // says is due, and touches nothing else.
    private void RunRemoteGpuPresentLoop(NativeWebRtcMediaEngine.Session receiver, CancellationToken token)
    {
        try
        {
            using VideoPresentClock clock = new(token);
            while (clock.WaitForNextTick())
            {
                long tickStartedAtMs = Environment.TickCount64;
                bool presented;
                try
                {
                    // The receiver's own gate orders the decode + present against attach /
                    // resize (UI thread) and the software drain. Not nativeMediaSync: this call
                    // goes into a third-party decoder, and when that decoder hung (live
                    // 2026-10-17) everything waiting on the shared lock hung with it — our
                    // camera encoder, the diagnostics, the UI thread.
                    lock (receiver.Gate)
                    {
                        if (!ReferenceEquals(nativeReceiverSession, receiver))
                        {
                            // The decoder was replaced (agreed format changed), given up as hung,
                            // or the call is tearing down. The swapchain belongs to THAT session's
                            // renderer, so this presenter is finished: the next receiver attaches
                            // its own and starts its own presenter.
                            return;
                        }

                        presented = CallIntoReceiver(receiver, receiver.RenderAttachedVideo);
                    }
                }
                catch (ObjectDisposedException)
                {
                    // Raced with teardown disposing the receiver — nothing left to present.
                    return;
                }
                catch (Exception ex)
                {
                    // One bad frame must not end the picture for the rest of the call.
                    Interlocked.Increment(ref renderStalls);
                    RecordError(FormatExceptionForDiagnostics(ex));
                    continue;
                }

                Interlocked.Add(
                    ref renderFrameProcessingMs,
                    Math.Max(0, Environment.TickCount64 - tickStartedAtMs));

                // ONLY when a new frame actually reached the swapchain. The clock keeps ticking
                // whether or not the peer is still sending, so stamping unconditionally would mean
                // HasRemoteVideo never goes stale, the placeholder never appears, and the peer's
                // last picture stays frozen for the rest of the call — with a screen share started
                // afterwards invisible behind it (live test 2026-08-11).
                if (!presented)
                {
                    continue;
                }

                Interlocked.Increment(ref framesRendered);
                long presentedAtMs = Environment.TickCount64;
                long previousMs = Interlocked.Exchange(ref lastRemoteFrameRenderedAtMs, presentedAtMs);
                // The mirror of the encode-side gap, on the path that produces what the user
                // actually watches: an average frame rate cannot show a stutter, the worst interval
                // between two presented frames can.
                if (previousMs > 0)
                {
                    long gapMs = presentedAtMs - previousMs;
                    RaiseMaxTo(ref renderGapMaxMs, gapMs);
                    RaiseMaxTo(ref renderGapWorstMs, gapMs);
                }
            }
        }
        catch (Exception ex)
        {
            RecordError($"GPU presenter stopped: {FormatExceptionForDiagnostics(ex)}");
        }
        finally
        {
            // Only clear the slot if it is still ours — a newer presenter may already own it.
            Interlocked.CompareExchange<NativeWebRtcMediaEngine.Session?>(
                ref gpuPresenterReceiver, null, receiver);
        }
    }

    // UI thread. Creates the native composition swapchain on the receiver's D3D11 renderer and
    // binds it to the panel via ISwapChainPanelNative. Returns false (soft) when the GPU path is
    // unavailable so the caller falls back to software rendering.
    private bool TryAttachRemoteGpuRenderer(NativeWebRtcMediaEngine.Session receiver)
    {
        (int pixelWidth, int pixelHeight) = ResolveRemoteSwapChainPixelSize();

        IntPtr swapChain = IntPtr.Zero;
        bool attachedNative = false;
        try
        {
            // UI thread: never wait on a receiver that may be stuck in its decoder. Busy past the
            // timeout means exactly that — attach fails soft and the software path takes over.
            if (!Monitor.TryEnter(receiver.Gate, UiReceiverGateTimeoutMs))
            {
                RecordError($"GPU renderer attach skipped: the receiver did not answer in {UiReceiverGateTimeoutMs} ms.");
                return false;
            }

            try
            {
                if (!ReferenceEquals(nativeReceiverSession, receiver))
                {
                    return false;
                }

                if (!receiver.TryAttachVideoRenderer(pixelWidth, pixelHeight, out swapChain))
                {
                    return false;
                }

                attachedNative = true;
            }
            finally
            {
                Monitor.Exit(receiver.Gate);
            }

            ISwapChainPanelNative panelNative =
                remoteSwapChainPanelNative ??= remoteVideoSwapChainPanel.As<ISwapChainPanelNative>();
            int hr = panelNative.SetSwapChain(swapChain);
            if (hr < 0)
            {
                Marshal.ThrowExceptionForHR(hr);
            }
        }
        catch (Exception ex)
        {
            RecordError($"GPU renderer attach failed: {FormatExceptionForDiagnostics(ex)}");
            if (attachedNative)
            {
                // Release the native renderer's swapchain ref we just created.
                try
                {
                    if (Monitor.TryEnter(receiver.Gate, UiReceiverGateTimeoutMs))
                    {
                        try
                        {
                            if (ReferenceEquals(nativeReceiverSession, receiver))
                            {
                                receiver.DetachVideoRenderer();
                            }
                        }
                        finally
                        {
                            Monitor.Exit(receiver.Gate);
                        }
                    }
                }
                catch
                {
                    // Best-effort cleanup on the failure path; the receiver's disposal releases
                    // the swapchain ref anyway.
                }
            }

            return false;
        }
        finally
        {
            // The native attach returned the swapchain AddRef'd and SetSwapChain took its own ref,
            // so drop ours regardless of success.
            if (swapChain != IntPtr.Zero)
            {
                Marshal.Release(swapChain);
            }
        }

        WireRemoteGpuPanelHandlersOnce();
        remoteVideoSwapChainPanel.Visibility = Visibility.Visible;
        remoteVideoImage.Visibility = Visibility.Collapsed;
        gpuRendererAttachedReceiver = receiver;
        gpuVideoPath = true;
        return true;
    }

    private void WireRemoteGpuPanelHandlersOnce()
    {
        if (gpuPanelHandlersWired)
        {
            return;
        }

        gpuPanelHandlersWired = true;
        remoteVideoSwapChainPanel.SizeChanged += OnRemoteSwapChainPanelSizeChanged;
        remoteVideoSwapChainPanel.CompositionScaleChanged += OnRemoteSwapChainPanelCompositionScaleChanged;
    }

    private void OnRemoteSwapChainPanelSizeChanged(object sender, SizeChangedEventArgs e)
        => ResizeRemoteGpuRenderer();

    private void OnRemoteSwapChainPanelCompositionScaleChanged(SwapChainPanel sender, object args)
        => ResizeRemoteGpuRenderer();

    // UI thread. Resizes the swapchain backbuffer to the panel's pixel size so the GPU render
    // stays crisp as the window resizes / DPI changes.
    private void ResizeRemoteGpuRenderer()
    {
        if (!gpuVideoPath)
        {
            return;
        }

        NativeWebRtcMediaEngine.Session? receiver = gpuRendererAttachedReceiver;
        if (receiver is null)
        {
            return;
        }

        (int pixelWidth, int pixelHeight) = ResolveRemoteSwapChainPixelSize();
        try
        {
            // UI thread: skip rather than wait. A resize missed while the decoder is busy is
            // redone by the next SizeChanged; a UI thread waiting on a hung decoder is not undone.
            if (!Monitor.TryEnter(receiver.Gate, UiReceiverGateTimeoutMs))
            {
                return;
            }

            try
            {
                if (!ReferenceEquals(nativeReceiverSession, receiver))
                {
                    return;
                }

                receiver.ResizeAttachedVideo(pixelWidth, pixelHeight);
            }
            finally
            {
                Monitor.Exit(receiver.Gate);
            }
        }
        catch (ObjectDisposedException)
        {
            // Raced with stop — ignore.
        }
        catch (Exception ex)
        {
            RecordError($"GPU renderer resize failed: {FormatExceptionForDiagnostics(ex)}");
        }
    }

    // UI thread. The swapchain backbuffer is sized to the panel's pixel dimensions (the native
    // renderer stretches the decoded frame to fill it). Falls back to 1280x720 before the panel is
    // laid out; the first SizeChanged after it becomes visible corrects the size.
    private (int Width, int Height) ResolveRemoteSwapChainPixelSize()
    {
        double logicalWidth = remoteVideoSwapChainPanel.ActualWidth;
        double logicalHeight = remoteVideoSwapChainPanel.ActualHeight;
        float scaleX = remoteVideoSwapChainPanel.CompositionScaleX;
        float scaleY = remoteVideoSwapChainPanel.CompositionScaleY;
        int pixelWidth = (int)Math.Round(logicalWidth * (scaleX > 0f ? scaleX : 1f));
        int pixelHeight = (int)Math.Round(logicalHeight * (scaleY > 0f ? scaleY : 1f));
        if (pixelWidth < 16 || pixelHeight < 16)
        {
            pixelWidth = 1280;
            pixelHeight = 720;
        }

        pixelWidth = Math.Clamp(pixelWidth, 16, 7680);
        pixelHeight = Math.Clamp(pixelHeight, 16, 4320);
        return (pixelWidth, pixelHeight);
    }

    // UI thread. Unbinds the panel from the swapchain and resets the GPU path to the software
    // fallback. Called on call teardown. The native renderer's swapchain ref is released when the
    // receiver session is disposed (SessionDestroy), so the swapchain object is freed once the
    // panel drops its ref here.
    private void ResetRemoteGpuRenderer()
    {
        try
        {
            remoteSwapChainPanelNative?.SetSwapChain(IntPtr.Zero);
        }
        catch (Exception ex)
        {
            RecordError($"GPU renderer unbind failed: {FormatExceptionForDiagnostics(ex)}");
        }

        gpuRendererAttachedReceiver = null;
        gpuVideoPath = false;
        gpuVideoEnabled = true;
        remoteVideoSwapChainPanel.Visibility = Visibility.Collapsed;
        remoteVideoImage.Visibility = Visibility.Visible;
    }

    private void ClearPendingRemoteRenderFrame()
    {
        lock (remoteRenderSync)
        {
            pendingRemoteRenderFrame?.Dispose();
            pendingRemoteRenderFrame = null;
        }
    }
}
