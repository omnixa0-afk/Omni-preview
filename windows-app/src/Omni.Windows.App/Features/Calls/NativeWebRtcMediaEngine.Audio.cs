using System.Buffers;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Features.Calls;

// Audio session wrappers (opus-over-RTP capture + receive/playback) around the native handles.
// Partial of NativeWebRtcMediaEngine split out for size; same class, identical IL, no behaviour change.
internal static partial class NativeWebRtcMediaEngine
{
    /// <summary>
    /// Phase 2a-step4 (2026-05-23): wrapper around the opaque audio
    /// capture+send handle. Each encoded opus RTP packet goes straight to
    /// the packet sink (see <see cref="SetPacketSink"/>). The audio session
    /// is independent of <see cref="Session"/> because it does not share
    /// libwebrtc state.
    /// </summary>
    internal sealed class AudioCaptureSession : IDisposable
    {
        private readonly NativeWebRtcLibrary library;
        private IntPtr handle;

        internal AudioCaptureSession(NativeWebRtcLibrary library, IntPtr handle)
        {
            this.library = library;
            this.handle = handle;
        }

        ~AudioCaptureSession()
        {
            Dispose();
        }

        /// <summary>Where the encoded audio RTP packets go (see <see cref="Session.SetPacketSink"/>).</summary>
        internal void SetPacketSink(IntPtr sink, IntPtr context)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.AudioCaptureSetPacketSink(handle, sink, context), "audio_capture_set_packet_sink");
        }

        /// <summary>Packets handed to the core so far (0 if disposed or the call fails).</summary>
        internal ulong ReadPacketsSent()
        {
            if (handle == IntPtr.Zero)
            {
                return 0;
            }
            return library.AudioCapturePacketsSent(handle, out ulong sent) == NativeWebRtcStatus.Ok ? sent : 0;
        }

        /// <summary>
        /// Muted: capture, echo cancellation and the speaking level keep running, nothing is sent.
        /// Lives in the engine now — the shell used to drain the packets and throw them away.
        /// </summary>
        internal void SetMuted(bool muted)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.AudioCaptureSetMuted(handle, muted ? 1 : 0), "audio_capture_set_muted");
        }

        /// <summary>
        /// How long the microphone's raw input has been pure digital zero, ms (since the last
        /// sample that was not, or since capture started). 0 if disposed or the call fails.
        /// </summary>
        internal long ReadDigitalSilenceMs()
        {
            if (handle == IntPtr.Zero)
            {
                return 0;
            }
            NativeWebRtcStatus status = library.AudioCaptureDigitalSilenceMs(handle, out long silenceMs);
            return status == NativeWebRtcStatus.Ok ? silenceMs : 0;
        }

        /// <summary>
        /// Smoothed RMS (0..1) of the mic audio currently being sent — the self tile's speaking
        /// level. Returns 0 if disposed or the native call fails (a level readout must never
        /// throw into the UI poll loop).
        /// </summary>
        internal float ReadLevel()
        {
            if (handle == IntPtr.Zero)
            {
                return 0f;
            }
            NativeWebRtcStatus status = library.AudioCaptureLevel(handle, out float level);
            return status == NativeWebRtcStatus.Ok ? level : 0f;
        }

        /// <summary>
        /// One line of echo-canceller state (`aec=on reverse_blocks=… erle=… delay=…ms`, or
        /// `aec=off(apm_unavailable)`), for the call diagnostics. Never throws — a diagnostic
        /// must not be able to break the call it describes.
        /// </summary>
        internal string ReadEchoDiagnostics()
        {
            if (handle == IntPtr.Zero)
            {
                return "aec=none(no_capture)";
            }

            try
            {
                byte[] buffer = new byte[256];
                NativeWebRtcStatus status = library.AudioCaptureEchoDiagnostics(
                    handle,
                    buffer,
                    (nuint)buffer.Length,
                    out nuint written);
                if (status != NativeWebRtcStatus.Ok || written == 0)
                {
                    return "aec=unknown";
                }

                int length = (int)Math.Min(written - 1, (nuint)buffer.Length);
                return System.Text.Encoding.UTF8.GetString(buffer, 0, length);
            }
            catch (Exception ex)
            {
                return $"aec=unknown({ex.GetType().Name})";
            }
        }

        public void Dispose()
        {
            IntPtr current = Interlocked.Exchange(ref handle, IntPtr.Zero);
            if (current != IntPtr.Zero)
            {
                library.AudioCaptureDestroy(current);
                GC.SuppressFinalize(this);
            }
        }

        private void ThrowIfDisposed()
        {
            if (handle == IntPtr.Zero)
            {
                throw new ObjectDisposedException(nameof(AudioCaptureSession));
            }
        }
    }

    /// <summary>
    /// Phase 2b-step3 (2026-05-23): wrapper around the opaque audio
    /// receive+playback handle. The caller pushes every inbound
    /// datagram via <see cref="PushRtpPacket"/> and the handle internally
    /// drops non-audio packets (PT/SSRC mismatch) silently — the C#
    /// demux is intentionally trivial.
    /// </summary>
    internal sealed class AudioPlaybackSession : IDisposable
    {
        private readonly NativeWebRtcLibrary library;
        private IntPtr handle;

        internal AudioPlaybackSession(NativeWebRtcLibrary library, IntPtr handle)
        {
            this.library = library;
            this.handle = handle;
        }

        ~AudioPlaybackSession()
        {
            Dispose();
        }

        /// <summary>The native playback, for the inbound router (<see cref="MediaRouter"/>).</summary>
        internal IntPtr Handle => handle;

        /// <summary>
        /// Phase 2c (2026-05-23): read aggregate counters from the
        /// native playback. Accepted = packets that decoded to PCM and
        /// reached the render queue. Dropped = PT/SSRC mismatch +
        /// decode failures (combined; per-class breakdown isn't exposed).
        /// </summary>
        internal (ulong Accepted, ulong Dropped) ReadCounters()
        {
            ThrowIfDisposed();
            NativeWebRtcStatus status = library.AudioPlaybackReadCounters(
                handle,
                out ulong accepted,
                out ulong dropped);
            ThrowIfFailed(status, "audio_playback_read_counters");
            return (accepted, dropped);
        }

        /// <summary>The jitter buffer: holds / aims at, and concealed / accelerated / decelerated ms.</summary>
        internal NativeWebRtcMediaAudioJitterStatsNative ReadJitterStats()
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.AudioPlaybackJitterStats(handle, out NativeWebRtcMediaAudioJitterStatsNative stats), "audio_playback_jitter_stats");
            return stats;
        }

        /// <summary>
        /// Lipsync: video's playout target delay becomes the audio jitter buffer's minimum delay, so
        /// audio does not run ahead of video lips. Native clamps at 500 ms and returns the value
        /// applied. Safe to call frequently.
        /// </summary>
        internal uint SetTargetPlayoutDelayMs(uint delayMs)
        {
            ThrowIfDisposed();
            NativeWebRtcStatus status = library.AudioPlaybackSetTargetPlayoutDelayMs(
                handle,
                delayMs,
                out uint appliedDelayMs);
            ThrowIfFailed(status, "audio_playback_set_target_playout_delay_ms");
            return appliedDelayMs;
        }

        /// <summary>
        /// Smoothed RMS (0..1) of this sender's decoded audio — the remote tile's speaking level
        /// / active-speaker signal. Returns 0 if disposed or the native call fails (the level
        /// poll must never throw into the UI loop).
        /// </summary>
        internal float ReadLevel()
        {
            if (handle == IntPtr.Zero)
            {
                return 0f;
            }
            NativeWebRtcStatus status = library.AudioPlaybackLevel(handle, out float level);
            return status == NativeWebRtcStatus.Ok ? level : 0f;
        }

        public void Dispose()
        {
            IntPtr current = Interlocked.Exchange(ref handle, IntPtr.Zero);
            if (current != IntPtr.Zero)
            {
                library.AudioPlaybackDestroy(current);
                GC.SuppressFinalize(this);
            }
        }

        private void ThrowIfDisposed()
        {
            if (handle == IntPtr.Zero)
            {
                throw new ObjectDisposedException(nameof(AudioPlaybackSession));
            }
        }
    }
}
