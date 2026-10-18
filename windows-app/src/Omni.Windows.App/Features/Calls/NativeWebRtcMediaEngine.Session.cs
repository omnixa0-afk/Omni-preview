using System.Buffers;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Features.Calls;

// The video send/receive Session: wraps the opaque native libwebrtc handle (push frames, drain
// RTP, decode, attach GPU renderer, diagnostics). Partial of NativeWebRtcMediaEngine split out
// for size; same class, identical IL, no behaviour change.
internal static partial class NativeWebRtcMediaEngine
{
    internal sealed class Session : IDisposable
    {
        private readonly NativeWebRtcLibrary library;
        private IntPtr handle;

        internal Session(NativeWebRtcLibrary library, IntPtr handle)
        {
            this.library = library;
            this.handle = handle;
        }

        /// <summary>
        /// Serialises the shell's calls into THIS session — encode and ABR for a sender; decode,
        /// render, attach, resize and drain for a receiver. One per session, not one per call:
        /// a codec stuck inside a driver (the Media Foundation HEVC decoder never returning from
        /// ProcessOutput, live 2026-10-17) then blocks only its own session, not the encoder of
        /// the other direction and the UI with it. See docs/bugs/hung-decoder-froze-the-app.md.
        /// </summary>
        internal object Gate { get; } = new();

        /// <summary>The native session, for the inbound router (<see cref="MediaRouter"/>).</summary>
        internal IntPtr Handle => handle;

        ~Session()
        {
            Dispose();
        }

        internal unsafe void PushI420Frame(
            ReadOnlySpan<byte> yPlane,
            int yStride,
            ReadOnlySpan<byte> uPlane,
            int uStride,
            ReadOnlySpan<byte> vPlane,
            int vStride,
            int width,
            int height,
            long timestampUs,
            bool forceKeyFrame)
        {
            ThrowIfDisposed();
            ValidateI420Plane("Y", yPlane, yStride, width, height);
            int chromaWidth = (width + 1) / 2;
            int chromaHeight = (height + 1) / 2;
            ValidateI420Plane("U", uPlane, uStride, chromaWidth, chromaHeight);
            ValidateI420Plane("V", vPlane, vStride, chromaWidth, chromaHeight);

            fixed (byte* y = yPlane)
            fixed (byte* u = uPlane)
            fixed (byte* v = vPlane)
            {
                ThrowIfFailed(
                    library.PushI420Frame(
                        handle,
                        (IntPtr)y,
                        yStride,
                        (IntPtr)u,
                        uStride,
                        (IntPtr)v,
                        vStride,
                        width,
                        height,
                        timestampUs,
                        forceKeyFrame ? 1 : 0),
                    "push_i420_frame");
            }
        }

        internal unsafe void PushNv12Frame(
            ReadOnlySpan<byte> yPlane,
            int yStride,
            ReadOnlySpan<byte> uvPlane,
            int uvStride,
            int width,
            int height,
            long timestampUs,
            bool forceKeyFrame)
        {
            ThrowIfDisposed();
            ValidateI420Plane("Y", yPlane, yStride, width, height);
            int chromaWidth = (width + 1) / 2;
            int chromaHeight = (height + 1) / 2;
            if (uvStride < chromaWidth * 2)
            {
                throw new ArgumentOutOfRangeException(nameof(uvStride), "NV12 UV stride is smaller than chroma width.");
            }

            int requiredUv = checked(uvStride * chromaHeight);
            if (uvPlane.Length < requiredUv)
            {
                throw new ArgumentException($"NV12 UV plane length {uvPlane.Length} is smaller than required {requiredUv} bytes.");
            }

            fixed (byte* y = yPlane)
            fixed (byte* uv = uvPlane)
            {
                ThrowIfFailed(
                    library.PushNv12Frame(
                        handle,
                        (IntPtr)y,
                        yStride,
                        (IntPtr)uv,
                        uvStride,
                        width,
                        height,
                        timestampUs,
                        forceKeyFrame ? 1 : 0),
                    "push_nv12_frame");
            }
        }

        internal unsafe void PushBgraFrame(
            ReadOnlySpan<byte> bgra,
            int bgraStride,
            int width,
            int height,
            long timestampUs,
            bool forceKeyFrame)
        {
            ThrowIfDisposed();
            if (width <= 0 || height <= 0)
            {
                throw new ArgumentOutOfRangeException(nameof(width), "Frame dimensions must be positive.");
            }

            if (bgraStride < checked(width * 4))
            {
                throw new ArgumentOutOfRangeException(nameof(bgraStride), "BGRA stride is smaller than frame width.");
            }

            int required = checked(bgraStride * height);
            if (bgra.Length < required)
            {
                throw new ArgumentException($"BGRA frame length {bgra.Length} is smaller than required {required} bytes.");
            }

            fixed (byte* source = bgra)
            {
                ThrowIfFailed(
                    library.PushBgraFrame(
                        handle,
                        (IntPtr)source,
                        bgraStride,
                        width,
                        height,
                        timestampUs,
                        forceKeyFrame ? 1 : 0),
                    "push_bgra_frame");
            }
        }

        /// <summary>
        /// Where this session's outgoing packets go: every RTP packet the pacer releases and every
        /// RTCP packet the receive half builds is handed to <paramref name="sink"/> with
        /// <paramref name="context"/> at once, on the thread that produced it. The shell passes
        /// the core's sink and a call's target ID and never sees a packet. Zero/zero detaches.
        /// </summary>
        internal void SetPacketSink(IntPtr sink, IntPtr context)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.SetPacketSink(handle, sink, context), "set_packet_sink");
        }

        internal void SetRates(int bitrateBps, int fps)
        {
            ThrowIfDisposed();
            if (bitrateBps <= 0)
            {
                throw new ArgumentOutOfRangeException(nameof(bitrateBps), "Bitrate must be positive.");
            }

            if (fps <= 0)
            {
                throw new ArgumentOutOfRangeException(nameof(fps), "Frame rate must be positive.");
            }

            ThrowIfFailed(library.SetRates(handle, bitrateBps, fps), "set_rates");
        }

        /// A new bitrate ceiling on this live sender: no new encoder, no keyframe (ABI 23).
        internal void SetMaxBitrate(int maxBitrateBps)
        {
            ThrowIfDisposed();
            if (maxBitrateBps <= 0)
            {
                throw new ArgumentOutOfRangeException(nameof(maxBitrateBps), "Bitrate must be positive.");
            }

            ThrowIfFailed(library.SetMaxBitrate(handle, maxBitrateBps), "set_max_bitrate");
        }

        internal void RequestKeyFrame()
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RequestKeyframe(handle), "request_keyframe");
        }

        internal unsafe NativeWebRtcMediaAbrTarget UpdateAdaptiveBitrate(NativeWebRtcMediaAbrSample sample)
        {
            ThrowIfDisposed();
            NativeWebRtcMediaAbrSampleNative nativeSample = new()
            {
                NowMs = sample.NowMs,
            };
            NativeWebRtcMediaAbrTargetNative nativeTarget = default;
            ThrowIfFailed(
                library.UpdateAdaptiveBitrate(handle, &nativeSample, &nativeTarget),
                "update_abr");

            NativeWebRtcMediaAbrState state = Enum.IsDefined(
                typeof(NativeWebRtcMediaAbrState),
                nativeTarget.State)
                ? (NativeWebRtcMediaAbrState)nativeTarget.State
                : NativeWebRtcMediaAbrState.Stable;
            return new NativeWebRtcMediaAbrTarget(
                nativeTarget.Changed != 0,
                checked((int)nativeTarget.TargetBitrateBps),
                checked((int)nativeTarget.TargetFps),
                checked((int)nativeTarget.TargetWidth),
                checked((int)nativeTarget.TargetHeight),
                state);
        }

        /// Отдать отчёт о доставке регулятору скорости отправляющей стороны.
        internal unsafe void ApplyTransportFeedback(ReadOnlySpan<byte> report)
        {
            ThrowIfDisposed();
            if (report.IsEmpty)
            {
                return;
            }

            fixed (byte* data = report)
            {
                ThrowIfFailed(
                    library.ApplyTransportFeedback(handle, (IntPtr)data, (UIntPtr)report.Length),
                    "apply_transport_feedback");
            }
        }

        /// Проход туда-обратно, измеренный этой сессией. -1 — ещё не измеряли.
        internal long TransportRttUs()
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.TransportRttUs(handle, out long rttUs), "transport_rtt_us");
            return rttUs;
        }

        internal unsafe void PushRtpPackets(ReadOnlySpan<byte> batch, uint packetCount)
        {
            ThrowIfDisposed();
            if (batch.IsEmpty || packetCount == 0)
            {
                throw new ArgumentException("RTP batch and packet count must be non-empty.", nameof(batch));
            }

            fixed (byte* buffer = batch)
            {
                ThrowIfFailed(
                    library.PushRtpPackets(handle, (IntPtr)buffer, (UIntPtr)batch.Length, packetCount),
                    "push_rtp_packets");
            }
        }

        internal unsafe bool TryDrainI420Frame(out NativeWebRtcDecodedI420Frame frame)
        {
            ThrowIfDisposed();
            frame = default;

            NativeWebRtcStatus sizingStatus = library.DrainI420Frame(
                handle,
                IntPtr.Zero,
                UIntPtr.Zero,
                out int width,
                out int height,
                out uint rtpTimestamp,
                out long timestampUs,
                out UIntPtr requiredLen);
            if (sizingStatus == NativeWebRtcStatus.Ok && requiredLen == UIntPtr.Zero)
            {
                return false;
            }

            if (sizingStatus != NativeWebRtcStatus.BufferTooSmall)
            {
                ThrowIfFailed(sizingStatus, "drain_i420_frame(size)");
            }

            int length = checked((int)requiredLen.ToUInt64());
            if (length <= 0)
            {
                return false;
            }

            byte[] buffer = new byte[length];
            fixed (byte* frameBuffer = buffer)
            {
                ThrowIfFailed(
                    library.DrainI420Frame(
                        handle,
                        (IntPtr)frameBuffer,
                        (UIntPtr)buffer.Length,
                        out width,
                        out height,
                        out rtpTimestamp,
                        out timestampUs,
                        out requiredLen),
                    "drain_i420_frame");
            }

            frame = new NativeWebRtcDecodedI420Frame(buffer, width, height, rtpTimestamp, timestampUs);
            return frame.Buffer.Length > 0;
        }

        internal unsafe bool TryDrainBgraFrame(out NativeWebRtcDecodedBgraFrame frame)
        {
            ThrowIfDisposed();
            frame = default;

            NativeWebRtcStatus sizingStatus = library.DrainBgraFrame(
                handle,
                IntPtr.Zero,
                UIntPtr.Zero,
                out int width,
                out int height,
                out uint rtpTimestamp,
                out long timestampUs,
                out UIntPtr requiredLen);
            if (sizingStatus == NativeWebRtcStatus.Ok && requiredLen == UIntPtr.Zero)
            {
                return false;
            }

            if (sizingStatus != NativeWebRtcStatus.BufferTooSmall)
            {
                ThrowIfFailed(sizingStatus, "drain_bgra_frame(size)");
            }

            int length = checked((int)requiredLen.ToUInt64());
            if (length <= 0)
            {
                return false;
            }

            byte[] buffer = new byte[length];
            fixed (byte* frameBuffer = buffer)
            {
                ThrowIfFailed(
                    library.DrainBgraFrame(
                        handle,
                        (IntPtr)frameBuffer,
                        (UIntPtr)buffer.Length,
                        out width,
                        out height,
                        out rtpTimestamp,
                        out timestampUs,
                        out requiredLen),
                    "drain_bgra_frame");
            }

            frame = new NativeWebRtcDecodedBgraFrame(buffer, width, height, rtpTimestamp, timestampUs);
            return frame.Buffer.Length > 0;
        }

        // #5 increment 3b (2026-06-05): GPU video renderer (SwapChainPanel). attach creates a
        // composition swapchain (width x height px) on the native D3D11 renderer and returns its
        // IDXGISwapChain1* (AddRef'd) via out swapChain. The caller binds it to a WinUI
        // SwapChainPanel via ISwapChainPanelNative.SetSwapChain on the UI thread, then
        // Marshal.Release()s it (the panel + native renderer keep their own refs).
        // render/resize/detach drive that swapchain. They must be SERIALISED against each other
        // (CallVideoService does that with the session's Gate) but are not pinned to one thread: only
        // SetSwapChain above needs the UI thread, and RenderAttachedVideo deliberately runs on the
        // presenter thread instead. Decoded pixels never cross FFI: only the swapchain handle does
        // (restores the "media doesn't cross FFI" invariant; replaces the BGRA-over-FFI
        // DrainBgraFrame path when the GPU path is active).
        // Returns false (not throw) when the GPU path is unavailable (non-Windows / no-libwebrtc
        // build, or D3D11 device/swapchain init failed) so the caller can fall back to the
        // WriteableBitmap path without surfacing it as an error.
        internal bool TryAttachVideoRenderer(int width, int height, out IntPtr swapChain)
        {
            ThrowIfDisposed();
            swapChain = IntPtr.Zero;
            NativeWebRtcStatus status = library.AttachVideoRenderer(handle, width, height, out swapChain);
            if (status == NativeWebRtcStatus.Ok)
            {
                return swapChain != IntPtr.Zero;
            }

            if (status is NativeWebRtcStatus.NotImplemented or NativeWebRtcStatus.InternalError)
            {
                // Expected on a build/GPU that can't bring up the D3D11 renderer — soft fallback.
                return false;
            }

            ThrowIfFailed(status, "attach_video_renderer");
            return false;
        }

        /// <summary>
        /// Render tick. Returns whether a NEW decoded frame actually reached the swapchain —
        /// "nothing to draw" is a perfectly normal tick and must not be mistaken for live video:
        /// the tick is a display-paced clock that keeps beating whether or not the peer sends.
        /// </summary>
        internal bool RenderAttachedVideo()
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RenderAttachedVideo(handle, out bool presented), "render_attached_video");
            return presented;
        }

        internal void ResizeAttachedVideo(int width, int height)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.ResizeAttachedVideo(handle, width, height), "resize_attached_video");
        }

        internal void DetachVideoRenderer()
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.DetachVideoRenderer(handle), "detach_video_renderer");
        }

        internal string GetDiagnosticsJson()
        {
            ThrowIfDisposed();
            return ReadNativeString((IntPtr buffer, UIntPtr bufferLen, out UIntPtr requiredLen) =>
                library.DiagnosticsJson(handle, buffer, bufferLen, out requiredLen));
        }

        /// <summary>
        /// Give the session up without destroying it: a thread is stuck inside its codec, and
        /// destroying the codec under that thread would crash the process. The native session
        /// leaks; Dispose and the finalizer leave it alone from here on.
        /// </summary>
        internal void Abandon()
        {
            Volatile.Write(ref abandoned, true);
            GC.SuppressFinalize(this);
        }

        internal bool IsAbandoned => Volatile.Read(ref abandoned);

        private bool abandoned;

        public void Dispose()
        {
            if (Volatile.Read(ref abandoned))
            {
                return;
            }

            IntPtr current = Interlocked.Exchange(ref handle, IntPtr.Zero);
            if (current != IntPtr.Zero)
            {
                library.SessionDestroy(current);
                GC.SuppressFinalize(this);
            }
        }

        private void ThrowIfDisposed()
        {
            if (handle == IntPtr.Zero)
            {
                throw new ObjectDisposedException(nameof(Session));
            }
        }
    }
}
