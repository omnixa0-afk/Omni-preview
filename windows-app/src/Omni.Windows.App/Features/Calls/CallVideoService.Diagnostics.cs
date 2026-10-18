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

// Diagnostics text builders: assembles the human-readable call/media diagnostics overlay from
// local capture/encode counters and the native sender/receiver session JSON. Split out of
// CallVideoService.cs as a partial; same class, identical IL, no behaviour change.
internal sealed partial class CallVideoService
{
    /// <summary>
    /// Ask for a diagnostics refresh. Costs an interlocked write — the string is built by
    /// <see cref="DiagnosticsLoopAsync"/>, on its own thread.
    ///
    /// It used to build the text right here, and the callers are the hot paths: every encoded
    /// frame, every render tick, every drained packet. Twice a second one of them paid for two
    /// FFI calls that serialize the native session state to JSON **while holding that session's
    /// mutex**, two JsonDocument parses and a three-kilobyte string — on the thread that was in
    /// the middle of encoding a frame. That is a periodic hitch on the video path, and it is
    /// exactly the kind the user feels but no counter shows. See docs/bugs/media-hitching.md.
    /// </summary>
    private void PublishDiagnostics(bool force = false)
    {
        Interlocked.Exchange(ref diagnosticsRefreshRequested, 1);
        if (force)
        {
            // A forced refresh is a state change (an error, a source switch): let the UI re-read
            // the service now rather than at the next diagnostics tick. Raising the event is
            // cheap; building the text is not, and that still waits for the loop.
            PublishStateChanged();
        }
    }

    /// <summary>
    /// The one place the diagnostics string is built. Also the place the receiver's decoded-frame
    /// counters and the lipsync playout target are refreshed from — <see cref="BuildDiagnosticsText"/>
    /// does that as a side effect, at the same 500 ms cadence it always did.
    /// </summary>
    private async Task DiagnosticsLoopAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            try
            {
                await Task.Delay(DiagnosticsPublishIntervalMs, token);
                if (Interlocked.Exchange(ref diagnosticsRefreshRequested, 0) == 0)
                {
                    continue;
                }

                string text = BuildDiagnosticsText();
                _ = RunOnUiAsync(() => diagnosticsTextBlock.Text = text);
                PublishStateChanged();
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch
            {
                // Diagnostics must never take the call down with them.
            }
        }
    }

    /// <summary>
    /// The codec of each direction in words, as rows of Diagnostics → Media (ADR §8): «Ваше видео:
    /// AV1 — лучший из возможных», «Видео собеседника: HEVC — отправитель не кодирует AV1». Empty
    /// without video. Ends with a newline so the technical line stays a row of its own.
    /// </summary>
    internal static string FormatVideoDirectionsForPeople(RealtimeVideoStream? send, RealtimeVideoStream? receive)
    {
        if (send is null && receive is null)
        {
            return string.Empty;
        }

        static string Line(string label, RealtimeVideoStream? stream)
            => stream is null
                ? $"{label}: не идёт\n"
                : $"{label}: {CodecForPeople(stream.Codec)}, {stream.Width}×{stream.Height}, {stream.Fps} к/с" +
                  (string.IsNullOrWhiteSpace(stream.Reason) ? string.Empty : $" — {stream.Reason}") + "\n";

        return Line("Ваше видео", send) + Line("Видео собеседника", receive);
    }

    private static string CodecForPeople(string codec)
        => CallVideoCodecFactory.ParseCodec(codec) switch
        {
            CallVideoCodecKind.Av1 => "AV1",
            CallVideoCodecKind.H265 => "HEVC",
            CallVideoCodecKind.H264 => "H.264",
            _ => codec,
        };

    private string BuildDiagnosticsText()
    {
        string state = localCameraActive
            ? "sending"
            : remoteVideoSeen
                ? "receiving"
                : "ready";
        long nowMs = Environment.TickCount64;
        long sentFrames = Interlocked.Read(ref framesSent);
        long renderedFrames = Interlocked.Read(ref framesRendered);
        // What the sender handed to the core — the engine counts it at the moment of sending.
        long encodedBytes = ReadNativeSenderDiagnosticLong("rtp_octets_sent");
        long captureFrames = Interlocked.Read(ref captureFramesArrived);
        DiagnosticsRateSnapshot rates = GetDiagnosticsRateSnapshot(
            nowMs,
            sentFrames,
            renderedFrames,
            encodedBytes,
            captureFrames);
        string localVideo = BuildVideoDiagnostics(
            "local",
            Interlocked.Read(ref activeVideoWidth),
            Interlocked.Read(ref activeVideoHeight),
            rates.SentFps,
            rates.SentKbps);
        string remoteVideo = BuildVideoDiagnostics(
            "remote",
            Interlocked.Read(ref decodedVideoWidth),
            Interlocked.Read(ref decodedVideoHeight),
            rates.RenderedFps,
            null);
        string codec = string.IsNullOrWhiteSpace(codecDiagnostics) ? string.Empty : $" | codec={codecDiagnostics}";
        NativeWebRtcMediaProbeResult nativeWebRtcProbe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        string nativeWebRtc = nativeWebRtcProbe.DllLoaded ? $" | native_webrtc={nativeWebRtcProbe.Summary}" : string.Empty;
        string nativeMedia = string.IsNullOrWhiteSpace(nativeMediaDiagnostics) || string.Equals(nativeMediaDiagnostics, "none", StringComparison.Ordinal)
            ? string.Empty
            : $" | native_media={nativeMediaDiagnostics}";
        string nativeSession = BuildNativeSessionDiagnostics();
        string transport = " | active_media_transport=realtime_v2_quic_datagram video_v1_packets_sent=0";
        string negotiation = string.IsNullOrWhiteSpace(negotiationDiagnostics) || string.Equals(negotiationDiagnostics, "none", StringComparison.Ordinal)
            ? string.Empty
            : $" | {negotiationDiagnostics}";
        // Read-and-reset: the number is the worst gap of THIS window, not of all time. An average
        // frame rate cannot show a hitch — 30 fps with a 400 ms stall in it is still 30 fps.
        long encodeGapMax = Interlocked.Exchange(ref encodeGapMaxMs, 0);
        string capture = string.IsNullOrWhiteSpace(captureDiagnostics) || string.Equals(captureDiagnostics, "none", StringComparison.Ordinal)
            ? string.Empty
            : $" | capture={captureDiagnostics}{BuildCaptureLoopDiagnostics(rates.CaptureLoopFps)} " +
              $"gapMax={encodeGapMax}ms/{Interlocked.Read(ref encodeGapWorstMs)}ms";
        string captureProbe = string.IsNullOrWhiteSpace(captureProbeDiagnostics)
            ? string.Empty
            : $" | {captureProbeDiagnostics}";
        CallVideoHealthSample healthSample = new(
            captureDiagnostics,
            Interlocked.Read(ref diagnosticsRemoteLossFraction),
            Interlocked.Read(ref decodeStalls),
            Interlocked.Read(ref renderStalls),
            rates.CaptureLoopFps,
            targetFps);
        string health = $" | health={CallVideoHealthClassifier.Classify(healthSample)}";
        string control = BuildControlDiagnostics();
        string processing = BuildProcessingDiagnostics();
        string abr = $" | abr={adaptiveDiagnostics} target={targetCaptureWidth}x{targetCaptureHeight}@{adaptiveFps}fps {adaptiveBitrateBps / 1000}kbps";
        // Audio Phase 2b-4: capture (send-side) is concatenated onto
        // the video datagram batch. Playback (receive-side) consumes
        // every inbound datagram and drops non-audio by PT/SSRC. The
        // diagnostic surfaces sent counters from capture, and the
        // playback line shows whether the render endpoint is up so a
        // live tester can tell "no audio" from "remote side hasn't
        // sent yet".
        // Increment 2: live mic RMS (0..1) driving the self tile's speaking ring — lets a tester
        // confirm the voice-activity signal and calibrate the threshold.
        float selfSpeakingLevel = nativeAudioCaptureSession?.ReadLevel() ?? 0f;
        // Echo canceller: whether it exists, whether anything is feeding it the far-end
        // reference, and what it is actually removing. "It howls" is not actionable without
        // this — see docs/bugs/echo-was-invisible.md.
        string echo = nativeAudioCaptureSession is null
            ? string.Empty
            : $" {nativeAudioCaptureSession.ReadEchoDiagnostics()}";
        string audioCapture = string.IsNullOrEmpty(audioCaptureDiagnostics) || string.Equals(audioCaptureDiagnostics, "none", StringComparison.Ordinal)
            ? string.Empty
            : $" | {audioCaptureDiagnostics} sent={nativeAudioCaptureSession?.ReadPacketsSent() ?? 0}p mute={(localAudioMuted ? 1 : 0)} spk={selfSpeakingLevel:F3}{echo}";
        string audioPlayback;
        if (string.IsNullOrEmpty(audioPlaybackDiagnostics) || string.Equals(audioPlaybackDiagnostics, "none", StringComparison.Ordinal))
        {
            audioPlayback = string.Empty;
        }
        else
        {
            // Audio packets taken into the jitter buffer / packets that were not this audio (video and
            // RTCP — expected), and what the jitter buffer (NetEq) holds and has done: jb=now/target,
            // concealed / accelerated away / decelerated in, cumulative. Where audio latency sits is
            // read here, not guessed.
            NativeWebRtcMediaEngine.AudioPlaybackSession? playback = nativeAudioPlaybackSession;
            string countersText = string.Empty;
            if (playback is not null)
            {
                try
                {
                    (ulong accepted, ulong dropped) = playback.ReadCounters();
                    NativeWebRtcMediaEngine.NativeWebRtcMediaAudioJitterStatsNative jitter = playback.ReadJitterStats();
                    countersText = $" audio_in={accepted}p other={dropped}p jb={jitter.BufferMs}/{jitter.TargetMs}ms " +
                                   $"concealed={jitter.ConcealedMs}ms accel={jitter.AcceleratedMs}ms decel={jitter.DeceleratedMs}ms";
                }
                catch (ObjectDisposedException)
                {
                    // Race with concurrent stop; show base diagnostic only.
                }
            }
            // Phase 3 (2026-05-23): show the lipsync target so live
            // tests can confirm audio is being held to match video.
            long lipsync = Interlocked.Read(ref audioPlayoutTargetDelayMs);
            string lipsyncText = lipsync > 0 ? $" lipsync={lipsync}ms" : string.Empty;
            audioPlayback = $" | {audioPlaybackDiagnostics}{countersText}{lipsyncText}";
        }
        string audio = audioCapture + audioPlayback;
        string error = string.IsNullOrWhiteSpace(lastError) ? string.Empty : $" | error={lastError}";
        string passport = string.IsNullOrEmpty(lastPublishedPassport) ? string.Empty : $" | passport={lastPublishedPassport}";
        return $"{FormatVideoDirectionsForPeople(activeSendStream, activeReceiveStream)}Media: {state}{passport}{localVideo}{remoteVideo}{codec}{nativeWebRtc}{nativeMedia}{nativeSession}{transport}{negotiation}{captureProbe}{capture}{health}{abr}{audio}{control}{processing} | sent={sentFrames}f/{ReadNativeSenderDiagnosticLong("rtp_packets_sent")}p | recv={Interlocked.Read(ref framesReceived)}f/{ReadRouterCounters().Delivered}p | rendered={renderedFrames}f{error}";
    }

    private string BuildNativeSessionDiagnostics()
    {
        NativeWebRtcMediaEngine.Session? sender;
        NativeWebRtcMediaEngine.Session? receiver;
        NativeWebRtcMediaEngine.AudioPlaybackSession? audioPlayback;
        List<(string Tag, NativeWebRtcMediaEngine.Session Session)> groupReceivers;
        lock (nativeMediaSync)
        {
            sender = nativeSenderSession;
            receiver = nativeReceiverSession;
            audioPlayback = nativeAudioPlaybackSession;
            groupReceivers = groupReceiverSessions.Select(pair => (pair.Key, pair.Value)).ToList();
        }

        (string senderDiagnostics, _, _, uint remoteLossQ8) = BuildNativeSessionDiagnostics("send", sender);
        (string receiverDiagnostics, uint receiverPlayoutDelayMs, uint receiverFramesDecoded, _) = BuildNativeSessionDiagnostics("recv", receiver);
        remotePresentedSize = ReadPresentedSize(receiver);
        // Потери у собеседника по его последнему RR — их теперь знает нативный отправитель
        // (ABI 18), а не пересказ управляющего канала. Нужны классификатору здоровья.
        Interlocked.Exchange(ref diagnosticsRemoteLossFraction, remoteLossQ8);

        // #5 increment 3b: the GPU path bypasses the BGRA-drain + WriteableBitmap loop, so the C#
        // frame counters never move there. Source them — and remoteVideoSeen — from the native
        // decoder, which presents the newest decoded frame into the swapchain each render tick.
        // Honest and consistent with the native_session=…decoded=N field; runs at the ~500 ms diag
        // cadence which is plenty for a placeholder→video reveal. (Exact per-present counts aren't
        // tracked: the renderer intentionally drops all-but-newest for latency, so rendered≈decoded.)
        if (gpuVideoPath && receiver is not null)
        {
            Interlocked.Exchange(ref framesReceived, receiverFramesDecoded);
            Interlocked.Exchange(ref framesRendered, receiverFramesDecoded);
            if (receiverFramesDecoded > 0)
            {
                remoteVideoSeen = true;
            }
        }

        // Phase 3 (2026-05-23): forward video's JitterEstimator-driven
        // playout target into the audio render so lips don't outrun
        // sound. The diagnostic strip rebuild runs every ~500 ms — that
        // cadence is plenty for an audio-render gate that itself drains
        // at WASAPI's natural rate. Race with concurrent stop is
        // swallowed.
        if (receiverPlayoutDelayMs > 0 && audioPlayback is not null)
        {
            try
            {
                uint applied = audioPlayback.SetTargetPlayoutDelayMs(receiverPlayoutDelayMs);
                Interlocked.Exchange(ref audioPlayoutTargetDelayMs, applied);
            }
            catch (ObjectDisposedException)
            {
                // Race with concurrent stop — ignore.
            }
        }

        // A group call decodes each sender in its own session; without these the strip shows only
        // the (absent) 1:1 receiver and a group that decodes nothing looks idle, not broken.
        string groupDiagnostics = string.Join(
            ' ',
            groupReceivers.Select(group => $"group_recv[{group.Tag[..Math.Min(8, group.Tag.Length)]}]:{BuildNativeSessionDiagnostics("recv", group.Session).Text}"));

        if (string.IsNullOrEmpty(senderDiagnostics) &&
            string.IsNullOrEmpty(receiverDiagnostics) &&
            string.IsNullOrEmpty(groupDiagnostics))
        {
            return string.Empty;
        }

        return $" | native_session={string.Join(' ', new[] { senderDiagnostics, receiverDiagnostics, groupDiagnostics }.Where(value => !string.IsNullOrEmpty(value)))}";
    }

    // The shape of the peer's picture on the swapchain: the renderer fits it whole into the panel
    // with bars (FitViewport in d3d11_video_renderer.h), so a remote-control viewer's pointer maps
    // through the picture's own rectangle, not the panel's. Refreshed with the diagnostics.
    private volatile Tuple<int, int>? remotePresentedSize;

    internal (int Width, int Height)? RemotePresentedSize =>
        remotePresentedSize is { } size ? (size.Item1, size.Item2) : null;

    private static Tuple<int, int>? ReadPresentedSize(NativeWebRtcMediaEngine.Session? session)
    {
        if (session is null)
        {
            return null;
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(session.GetDiagnosticsJson());
            int width = (int)ReadNativeDiagnosticsUInt32(document.RootElement, "presented_width");
            int height = (int)ReadNativeDiagnosticsUInt32(document.RootElement, "presented_height");
            return width > 0 && height > 0 ? Tuple.Create(width, height) : null;
        }
        catch (Exception)
        {
            return null; // a session torn down mid-read: no picture to map to
        }
    }

    private static (string Text, uint PlayoutTargetDelayMs, uint FramesDecoded, uint RemoteLossQ8) BuildNativeSessionDiagnostics(string role, NativeWebRtcMediaEngine.Session? session)
    {
        if (session is null)
        {
            return (string.Empty, 0, 0, 0);
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(session.GetDiagnosticsJson());
            JsonElement root = document.RootElement;
            string codec = ReadNativeDiagnosticsString(root, "codec", "unknown");
            string abrState = ReadNativeDiagnosticsString(root, "abr_state", "stable");
            string abrReason = ReadNativeDiagnosticsString(root, "abr_reason", "stable");
            uint bitrate = ReadNativeDiagnosticsUInt32(root, "abr_target_bitrate_bps");
            uint fps = ReadNativeDiagnosticsUInt32(root, "abr_target_fps");
            // Handed to the core / released with no sink wired (a non-zero second number mid-call
            // is a sink someone forgot to attach). There is no send queue to report any more.
            uint handed = ReadNativeDiagnosticsUInt32(root, "packets_handed_to_core");
            uint unrouted = ReadNativeDiagnosticsUInt32(root, "packets_unrouted");
            // Both ends of the RTP wire, per session: what the sender actually handed over and
            // what the receiver actually took in. Without them a silent picture can't be split
            // into "nothing was sent", "nothing arrived", "arrived but didn't assemble" and
            // "assembled but didn't decode" — the counters below cover only the last two, which
            // is why the 2026-08-11 screen-share hunt had to guess.
            uint rtpSent = ReadNativeDiagnosticsUInt32(root, "rtp_packets_sent");
            uint rtpReceived = ReadNativeDiagnosticsUInt32(root, "rtp_packets_received");
            uint playout = ReadNativeDiagnosticsUInt32(root, "playout_pending_frames");
            uint assembled = ReadNativeDiagnosticsUInt32(root, "rtp_frames_assembled");
            uint decoded = ReadNativeDiagnosticsUInt32(root, "frames_decoded");
            // Task #84: HW H.264 decode (DXVA → NV12 texture). hwDecoded counts frames the HW
            // decoder produced; hw=active means the receiver is on the HW path (else SW OpenH264).
            uint hwDecoded = ReadNativeDiagnosticsUInt32(root, "hw_frames_decoded");
            uint decodeFailures = ReadNativeDiagnosticsUInt32(root, "decode_failures");
            uint h264ParameterSetPrepends = ReadNativeDiagnosticsUInt32(root, "h264_parameter_set_prepends");
            uint h264EncoderIdrFrames = ReadNativeDiagnosticsUInt32(root, "h264_encoder_idr_frames");
            uint h264ReceiverIdrFrames = ReadNativeDiagnosticsUInt32(root, "h264_receiver_idr_frames");
            uint h264ReceiverSpsFrames = ReadNativeDiagnosticsUInt32(root, "h264_receiver_sps_frames");
            uint h264ReceiverPpsFrames = ReadNativeDiagnosticsUInt32(root, "h264_receiver_pps_frames");
            uint h264TrackerRequests = ReadNativeDiagnosticsUInt32(root, "h264_sps_pps_tracker_keyframe_requests");
            uint h264TrackerDrops = ReadNativeDiagnosticsUInt32(root, "h264_sps_pps_tracker_drops");
            uint h264AnnexBRebuilds = ReadNativeDiagnosticsUInt32(root, "h264_annexb_rebuilds");
            uint h264AnnexBRebuildFailures = ReadNativeDiagnosticsUInt32(root, "h264_annexb_rebuild_failures");
            uint h264DecoderStatusFailures = ReadNativeDiagnosticsUInt32(root, "h264_decoder_status_failures");
            int lastDecodeStatus = ReadNativeDiagnosticsInt32(root, "last_decode_status");
            uint rtt = ReadNativeDiagnosticsUInt32(root, "abr_rtt_ewma_ms");
            uint loss = ReadNativeDiagnosticsUInt32(root, "abr_loss_ewma_q8");
            string history = ReadNativeDiagnosticsStringArray(root, "abr_reason_history");
            string historySuffix = string.IsNullOrWhiteSpace(history) ? string.Empty : $" history={history}";
            string h264Suffix = codec == "h264"
                ? $" h264EncIdr={h264EncoderIdrFrames} h264RecvIdr={h264ReceiverIdrFrames}/{h264ReceiverSpsFrames}/{h264ReceiverPpsFrames} h264AnnexB={h264AnnexBRebuilds}/{h264AnnexBRebuildFailures} h264Tracker={h264TrackerRequests}/{h264TrackerDrops} h264DecStatus={h264DecoderStatusFailures}:{lastDecodeStatus} h264SpsPps={h264ParameterSetPrepends}"
                : $" decStatus={h264DecoderStatusFailures}:{lastDecodeStatus} failHead={ReadNativeDiagnosticsString(root, "last_failed_frame_head", "-")}";
            // ADAPTATION, AS THE ENCODE ORCHESTRATOR COUNTS IT (#61, step D3).
            //
            // This used to print our own detector counters (adapt_cpu_up/down, adapt_qp_low/high).
            // Those detectors are gone — VideoStreamEncoder owns both now — and after the flip the
            // four numbers were frozen at zero, which read as "the detectors are quiet" while it
            // actually meant "we no longer see them".
            //
            // What the orchestrator counts is the thing that matters anyway: how many adaptation
            // steps it took, split by AXIS OF THE DECISION — cpu (our machine cannot keep up) vs
            // quality (the link cannot carry this picture). Those are different repairs. Alongside
            // them is what it is asking the source for right now, which is exactly what comes back
            // as the ABR target, plus which trade-off this session is on (balanced for a camera,
            // maintain_resolution for a screen share).
            uint cpuRes = ReadNativeDiagnosticsUInt32(root, "adapt_cpu_resolution_steps");
            uint cpuFps = ReadNativeDiagnosticsUInt32(root, "adapt_cpu_framerate_steps");
            uint qualityRes = ReadNativeDiagnosticsUInt32(root, "adapt_quality_resolution_steps");
            uint qualityFps = ReadNativeDiagnosticsUInt32(root, "adapt_quality_framerate_steps");
            uint overuseTicks = ReadNativeDiagnosticsUInt32(root, "adapt_overuse_ticks");
            uint wantsPixels = ReadNativeDiagnosticsUInt32(root, "encoder_wants_max_pixels");
            uint wantsFps = ReadNativeDiagnosticsUInt32(root, "encoder_wants_max_fps");
            // Counted where the change happens, not inferred from polling: the ABR tick samples
            // the wants 3-4 times a second, and from snapshots alone "the orchestrator changed
            // its mind" is indistinguishable from "we caught a moment".
            uint wantsNarrowed = ReadNativeDiagnosticsUInt32(root, "encoder_wants_narrowed");
            uint wantsWidened = ReadNativeDiagnosticsUInt32(root, "encoder_wants_widened");
            // Which adaptation axes the orchestrator itself considers live. Without this, "zero
            // adaptation steps" reads the same as "the mechanism is running and sees no need" and
            // "the mechanism was never armed" — two different repairs.
            string scaling = ReadNativeDiagnosticsString(root, "adapt_scaling_enabled", "?");
            // How often those settings are re-applied. A resource that is stopped and restarted
            // loses its check timer, and upstream's bandwidth scaler needs five uninterrupted
            // seconds — so a high count here means the mechanism never gets to finish a cycle.
            uint scalingUpdates = ReadNativeDiagnosticsUInt32(root, "adapt_settings_updates");
            // What the encoder really gives: the encoded frame's dimensions, the output over five
            // seconds and the QP the quality scaler judges by. Printed so "it never asked to shrink"
            // can be told apart from "it asked and was right".
            uint encW = ReadNativeDiagnosticsUInt32(root, "encoded_frame_width");
            uint encH = ReadNativeDiagnosticsUInt32(root, "encoded_frame_height");
            uint encBps = ReadNativeDiagnosticsUInt32(root, "encoded_window_bps");
            // The QP the quality scaler judges by (-1: the encoder gave none and none was parsed).
            int encQp = ReadNativeDiagnosticsInt32(root, "encoded_frame_qp");
            // Did the controller's target reach the encoder, and where do the frames go. Frames in
            // with nothing out looks the same whether the orchestrator lost its target and drops
            // silently or the frames never reach it — and those are different repairs.
            uint encTgtUpdates = ReadNativeDiagnosticsUInt32(root, "encoder_target_updates");
            int encTgtBps = ReadNativeDiagnosticsInt32(root, "encoder_last_target_bps");
            uint encIn = ReadNativeDiagnosticsUInt32(root, "encoder_incoming_frames");
            uint encOutFrames = ReadNativeDiagnosticsUInt32(root, "encoder_encoded_frames");
            uint encDropQ = ReadNativeDiagnosticsUInt32(root, "encoder_dropped_by_queue");
            uint encDropE = ReadNativeDiagnosticsUInt32(root, "encoder_dropped_by_encoder");
            uint encDropR = ReadNativeDiagnosticsUInt32(root, "encoder_dropped_by_rate");
            // Inside the hardware encoder: how many times it was called, how many times it came
            // back, and the longest single call. Encode runs ON the orchestrator's encode queue,
            // so a long call IS the busy queue that rejects every following frame.
            uint mftCalls = ReadNativeDiagnosticsUInt32(root, "mft_encode_calls");
            uint mftReturns = ReadNativeDiagnosticsUInt32(root, "mft_encode_returns");
            uint mftEmpty = ReadNativeDiagnosticsUInt32(root, "mft_encode_empty");
            uint mftErrors = ReadNativeDiagnosticsUInt32(root, "mft_encode_errors");
            uint mftOk = ReadNativeDiagnosticsUInt32(root, "mft_encode_ok");
            uint mftMaxMs = ReadNativeDiagnosticsUInt32(root, "mft_encode_max_ms");
            // Average per call = total time / calls. NOT calls divided by wall time: the
            // orchestrator decides when to call, so that arithmetic once told me the encoder
            // spends 300 ms per frame when it does nothing of the sort.
            uint mftAvgMs = ReadNativeDiagnosticsUInt32(root, "mft_encode_avg_ms");
            // How many times the hardware encoder asked upstream to take over. The switch to the
            // software encoder is silent by construction, so without this number "did we fall
            // back" is invisible from the outside — and it stayed invisible through a live call
            // where the hardware encoder failed 41 times in a row and nothing switched
            // (docs/bugs/software-fallback-never-fired-mid-call.md).
            uint mftFallbacks = ReadNativeDiagnosticsUInt32(root, "mft_encode_fallback_requests");
            string mftError = ReadNativeDiagnosticsString(root, "mft_last_error", string.Empty);
            string degradation = ReadNativeDiagnosticsString(root, "degradation_preference", "balanced");
            // WHICH ENCODER IS ACTUALLY RUNNING — not which one the probe found at startup.
            // The two diverge, and the difference decides which adaptation mechanism is even
            // eligible: a hardware encoder reports no QP (is_qp_trusted=false -> the bandwidth
            // scaler judges by the real output), a software one does (-> the QP scaler). Chasing
            // "why did the picture not shrink" without this is chasing two mechanisms at once.
            string encoderBackend = ReadNativeDiagnosticsString(root, "encoder_backend", "?");
            string adaptSuffix =
                $" adapt={degradation}/{encoderBackend} cpu({cpuRes}res/{cpuFps}fps) quality({qualityRes}res/{qualityFps}fps)" +
                $" wants={wantsPixels}px/{wantsFps}fps({wantsNarrowed}v/{wantsWidened}^)" +
                $" scaling=[{scaling}]x{scalingUpdates} encOut={encW}x{encH}@{encBps / 1000}kbps qp={encQp}" +
                $" encTgt={encTgtBps / 1000}kbps x{encTgtUpdates}" +
                $" frames={encIn}in/{encOutFrames}out drop={encDropQ}q/{encDropE}e/{encDropR}r" +
                $" mft={mftCalls}call/{mftReturns}ret={mftOk}ok+{mftEmpty}empty+{mftErrors}err" +
                $" avg={mftAvgMs}ms max={mftMaxMs}ms" +
                (mftFallbacks > 0 ? $" mftFallback={mftFallbacks}" : string.Empty) +
                (string.IsNullOrEmpty(mftError) ? string.Empty : $" mftErr=\"{mftError}\"") +
                $" overuse={overuseTicks}t";
            // Wave 5d follow-up (2026-05-18): surface the Wave 5c adaptive
            // playout fields into the user-visible diagnostic string. The
            // JSON had them since Phase 5c (commit 34e7350) but they
            // weren't being rendered — the live verifier and the smoke
            // automation couldn't see whether JitterEstimator was
            // adapting, only that FrameBuffer was holding frames. With
            // these surfaced, `playoutDelay=NNms` should track network
            // jitter (was hardcoded 60 ms before Wave 5c) and
            // `playoutDrops=N` flags FrameBuffer-internal capacity drops
            // plus our DropNextDecodableTemporalUnit calls.
            uint playoutTargetDelayMs = ReadNativeDiagnosticsUInt32(root, "playout_target_delay_ms");
            uint playoutFramesDropped = ReadNativeDiagnosticsUInt32(root, "playout_frames_dropped");
            string playoutSuffix = $" playoutDelay={playoutTargetDelayMs}ms playoutDrops={playoutFramesDropped}";
            // ПУТЬ ОТЧЁТА О ДОСТАВКЕ (RFC 8888) ЦЕЛИКОМ, А НЕ ОДИН ЕГО КОНЕЦ.
            //
            // Снаружи до сих пор было видно только `twcc=X/Y` — счётчики оболочки на самых концах
            // цепочки. Ноль на обоих концах одинаково означает «генератор молчит», «построил, но
            // не забрали», «забрали, но не доехало» и «доехало, но не подошло под разбор» — четыре
            // разные починки. Попытка перевести отчёт на быстрый путь 2026-08-31 встала ровно на
            // этой неразличимости и была откачена без диагноза
            // (docs/bugs/datagrams-we-counted-as-sent-never-left.md).
            //
            // Теперь печатается вся цепочка: сколько генератор ПОСТРОИЛ и сколько отправитель
            // ПРИМЕНИЛ, а между ними — очередь RTCP приёмника, которой отчёт с тех пор и едет:
            // положено / забрано оболочкой / вытеснено. Отдельного буфера отчётов больше нет,
            // поэтому и отдельных его счётчиков тоже: они описывали снятый путь.
            uint fbBuilt = ReadNativeDiagnosticsUInt32(root, "transport_feedback_reports_built");
            uint fbApplied = ReadNativeDiagnosticsUInt32(root, "transport_feedback_reports_applied");
            uint fbGenReady = ReadNativeDiagnosticsUInt32(root, "feedback_generator_ready");
            uint rtcpSent = ReadNativeDiagnosticsUInt32(root, "rtcp_packets_sent");
            // Приём по счётчикам апстрима (ReceiveStatistics): принято / потеряно / пришло не по
            // порядку. Последнее — не потеря; прежний самодельный счёт путал одно с другим и на
            // реальном UDP рисовал 12–33 % потерь при ~1 % на проводе.
            uint rxPackets = ReadNativeDiagnosticsUInt32(root, "receiver_packets");
            // Со знаком: по RFC 3550 дубликаты уводят счёт потерь в минус.
            int rxLost = ReadNativeDiagnosticsInt32(root, "receiver_cumulative_lost");
            uint rxOutOfOrder = ReadNativeDiagnosticsUInt32(root, "receiver_out_of_order_packets");
            // Решения регулятора из его журнала событий (OmniBweEventLog в нативе): оценка по задержке
            // и состояние детектора (0 норма, 1 недогруз, 2 перегруз), оценка по потерям, пробы.
            // Нужны, чтобы видеть, ЧТО держит цель, а не только её значение.
            string bweSuffix =
                $" bwe=delay:{ReadNativeDiagnosticsInt32(root, "bwe_delay_bps") / 1000}k/s{ReadNativeDiagnosticsInt32(root, "bwe_delay_state")}" +
                $"/ovr{ReadNativeDiagnosticsUInt32(root, "bwe_delay_overuse_updates")}of{ReadNativeDiagnosticsUInt32(root, "bwe_delay_updates")}" +
                $" loss:{ReadNativeDiagnosticsInt32(root, "bwe_loss_bps") / 1000}k/{ReadNativeDiagnosticsInt32(root, "bwe_loss_fraction")}" +
                $" probe:{ReadNativeDiagnosticsUInt32(root, "bwe_probes_succeeded")}ok" +
                $"/{ReadNativeDiagnosticsUInt32(root, "bwe_probes_created")}made" +
                $"/{ReadNativeDiagnosticsUInt32(root, "bwe_probes_failed")}fail" +
                $"(r{ReadNativeDiagnosticsInt32(root, "bwe_probe_last_failure_reason")})" +
                $"@{ReadNativeDiagnosticsInt32(root, "bwe_probe_last_success_bps") / 1000}k";
            // Просьбы приёмника — настоящий RTCP с ABI 16, посчитанный самими модулями: номера в
            // NACK ушло/пришло, PLI ушло/пришло (+ почему последний раз просили кадр), повторов
            // реально ушло на провод. У приёмной сессии смысл имеет «ушло», у отправляющей —
            // «пришло» и повторы.
            // Последний RR собеседника о НАШЕМ потоке (у отправляющей сессии): потери за интервал,
            // джиттер, RTT по LSR/DLSR (-1 — ещё не вычислен) и сколько отчётов пришло.
            uint remoteLossQ8 = ReadNativeDiagnosticsUInt32(root, "remote_fraction_lost_q8");
            string remoteSuffix =
                $" remote=loss{remoteLossQ8}/256 jitter{ReadNativeDiagnosticsUInt32(root, "remote_jitter")}" +
                $" rtt{ReadNativeDiagnosticsInt32(root, "remote_rtt_ms")}ms rr{ReadNativeDiagnosticsUInt32(root, "remote_reports")}" +
                $" rxRtt{ReadNativeDiagnosticsInt32(root, "receiver_rtt_ms")}ms";
            string requestSuffix =
                $" nack={ReadNativeDiagnosticsUInt32(root, "rtcp_nack_seq_sent")}out/{ReadNativeDiagnosticsUInt32(root, "rtcp_nack_seq_received")}in" +
                $" pli={ReadNativeDiagnosticsUInt32(root, "rtcp_pli_sent")}out/{ReadNativeDiagnosticsUInt32(root, "rtcp_pli_received")}in" +
                $"({ReadNativeDiagnosticsString(root, "last_keyframe_request_reason", "none")})" +
                $" rtx={ReadNativeDiagnosticsUInt32(root, "module_retransmissions_sent")}";
            string feedbackSuffix =
                $" fb=gen{fbGenReady}/{fbBuilt}built/{fbApplied}appl" +
                $" rtcp={rtcpSent}out" +
                requestSuffix +
                remoteSuffix +
                $" rx={rxPackets}p/{rxLost}lost/{rxOutOfOrder}ooo" +
                bweSuffix;
            string text = $"{role}:{codec} abr={abrState}/{abrReason}{historySuffix} target={bitrate / 1000}kbps@{fps} core={handed}/{unrouted}nosink rtp={rtpSent}out/{rtpReceived}in playout={playout}{playoutSuffix} assembled={assembled} decoded={decoded} decodeFail={decodeFailures} hwDecoded={hwDecoded}{h264Suffix} ewmaLoss={loss} ewmaRtt={rtt}ms{feedbackSuffix}{adaptSuffix}";
            return (text, playoutTargetDelayMs, decoded, remoteLossQ8);
        }
        catch
        {
            return ($"{role}:diagnostics_unavailable", 0, 0, 0);
        }
    }
}
