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

internal sealed partial class CallVideoService : IAsyncDisposable
{
    private const int DefaultTargetFrameIntervalMs = 33;
    private const int DefaultTargetBitrateBps = 4_000_000;
    /// Where a call's first sender starts below its ceiling. The congestion controller probes up
    /// from here within seconds; starting AT the ceiling overshot a thinner path on the very first
    /// tick (live 2026-10-18: `6300 -> 5153 down:congested`).
    private const int FreshSenderStartBitrateBps = 1_500_000;
    // Self-view preview cadence. Was 250 ms (4 fps), then 100 ms (10 fps) — still visibly choppy
    // because each preview frame was a FULL capture-resolution (720p) NV12->BGRA convert + 720p
    // WriteableBitmap blit on the UI thread, ~11x more pixels than the tiny PiP needs (task #86).
    // Now that the preview is downscaled to PiP size before convert+blit (see LocalPreviewFrame.
    // ToBgraFrame / Nv12ToPreviewBgra), each frame is cheap, so we run at the capture cadence
    // (~30 fps) for a smooth self-view at a fraction of the old 10 fps cost.
    private const int LocalPreviewFrameIntervalMs = 33;
    // Longest side (px) the self-view is downscaled to before color-convert + UI blit. The PiP is
    // ~190 px on screen; 384 keeps it crisp at up to ~2x DPI while cutting ~720p work ~11x.
    private const int LocalPreviewMaxDimension = 384;
    private const int MediaNegotiationTimeoutMs = 5_000;
    // Same bound for the camera format probe, for the same reason: it guards against a camera
    // swapped between calls, which two minutes covers.
    private const int CaptureProbeFreshnessMs = 120_000;
    private const int PassiveCapabilityPublishIntervalMs = 2_000;
    /// Ceiling for the passive re-publish backoff. Without one the loop below re-published every
    /// two seconds for as long as the app was open: a live pair that could not negotiate produced
    /// 344 publish attempts on one machine and 49 on the other inside a few minutes, each one a
    /// wake-up and two trace lines, and none of them any more likely to succeed than the first.
    private const int PassiveCapabilityPublishMaxIntervalMs = 60_000;
    private const int NegotiationMonitorIntervalMs = 1_000;
    private const int DiagnosticsPublishIntervalMs = 500;
    private static readonly TimeSpan CameraStartSoftDiagnosticDelay = TimeSpan.FromSeconds(5);
    private static readonly TimeSpan CameraStartTimeout = TimeSpan.FromSeconds(30);

    [ComImport]
    [Guid("5B0D3235-4DBA-4D44-865E-8F1D0E4FD04D")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private unsafe interface IMemoryBufferByteAccess
    {
        void GetBuffer(out byte* buffer, out uint capacity);
    }

    private readonly RustBridge rustBridge;
    private readonly DispatcherQueue dispatcherQueue;
    private readonly Image localPreviewImage;
    private readonly Image remoteVideoImage;
    // #5 increment 3b (2026-06-05): GPU video path. The native D3D11 renderer presents decoded
    // frames into this panel's composition swapchain (no BGRA across FFI, no UI-thread blit).
    // remoteVideoImage stays the software fallback sink (shown when the GPU path is unavailable).
    private readonly SwapChainPanel remoteVideoSwapChainPanel;
    private readonly TextBlock diagnosticsTextBlock;
    private readonly object lifecycleSync = new();
    private readonly object diagnosticsRateSync = new();
    private readonly object nativeMediaSync = new();
    private readonly object remoteRenderSync = new();
    private readonly object localPreviewSync = new();
    private readonly SemaphoreSlim captureProbeSync = new(1, 1);

    private CancellationTokenSource? mediaCancellation;
    private CancellationTokenSource? captureCancellation;
    private Task? receiveLoopTask;
    private Task? diagnosticsLoopTask;
    private Task? controlLoopTask;
    private Task? capabilityPublishLoopTask;
    private Task? negotiationMonitorLoopTask;
    // Tracked fire-and-forget tasks: see docs/decisions/csharp-call-task-lifecycle.md
    // (P0 #1 of video pipeline audit 2026-05-15). Without tracking, StopAsync
    // could dispose nativeSenderSession while one of these is mid-encode/render,
    // causing AccessViolation on the disposed IntPtr handle.
    private Task? activeFrameProcessingTask;
    private Task? localPreviewLoopTask;
    private Task? remoteRenderLoopTask;
    // Phase B (2026-05-18): native-driven capture reconfigure. When the
    // libwebrtc ABR state machine decides a new (width, height) — either
    // because GoogCC bitrate fell below the floor for the current profile,
    // or (Phase B2) QualityScaler / OveruseFrameDetector demands a downshift
    // — it emits target.Width/Height through UpdateAdaptiveBitrate. We
    // observe the change and restart the WinRT capture under a new cap so
    // the source matches the encoder's target. This is the *only* place
    // C# changes camera resolution; there is no longer a C# fallback heuristic.
    // See docs/decisions/webrtc-quality-scaler-adoption.md.
    private Task? captureReconfigureTask;
    private MediaCapture? mediaCapture;
    private MediaFrameReader? frameReader;
    private NativeWebRtcMediaEngine.Session? nativeSenderSession;
    private NativeWebRtcMediaEngine.Session? nativeReceiverSession;
    // The receiver the shell last called into, and when that call went in (0 = it returned) —
    // watched by DetectHungVideoDecoder. Written by CallIntoReceiver.
    private NativeWebRtcMediaEngine.Session? receiverInCall;
    private long receiverCallStartedAtMs;
    // Group video (T1): a `group:<id>` dialog prefixes each inbound RTP packet with the 8-byte
    // sender_tag (Rust handle_incoming_group_media); the native router demuxes on it. Set under
    // lifecycleSync in WatchDialogAsync.
    private volatile bool activeGroupMode;

    // Group video tiles (T3b): each participant tile registers its Image here and the service
    // renders INTO it directly (same software WriteableBitmap path as the 1:1 views) — no XAML
    // binding, no INPC. Key = lowercase sender_tag hex for remote members, GroupSelfTileKey for
    // our own tile (fed by the local camera preview, not the network). Inbound group RTP is
    // demuxed by its 8-byte tag into a PER-SENDER receiver session (groupReceiverSessions,
    // guarded by nativeMediaSync like the 1:1 sessions); decoded frames render into that
    // sender's registered tile. Tiles re-register on every rebuild (Image.Loaded), replacing
    // the previous target; StopAsync disposes the sessions and clears the registry.
    internal const string GroupSelfTileKey = "self";
    private const int MaxGroupReceiverSessions = 12;
    // The core send targets of the group receivers' RTCP, one per sender; destroyed with them.
    // Guarded by nativeMediaSync.
    private readonly List<ulong> groupFeedbackTargetIds = new();
    private sealed class GroupTileTarget
    {
        public Image? Image;
        public WriteableBitmap? Bitmap;
    }
    private readonly object groupTileSync = new();
    private readonly Dictionary<string, GroupTileTarget> groupTiles = new(StringComparer.OrdinalIgnoreCase);
    // The video payload type each group receiver was built for (guarded by nativeMediaSync).
    private readonly Dictionary<string, byte> groupReceiverPayloadTypes = new(StringComparer.Ordinal);

    private readonly Dictionary<string, NativeWebRtcMediaEngine.Session> groupReceiverSessions =
        new(StringComparer.OrdinalIgnoreCase);
    // Increment 2: one decode-only audio level probe per group sender (keyed by sender tag), so the
    // active-speaker UI can read each remote's speaking level. The shared nativeAudioPlaybackSession
    // still owns render + AEC; probes only decode + measure (no WASAPI endpoint), so this never
    // doubles audio. Guarded by nativeMediaSync; disposed with groupReceiverSessions.
    private readonly Dictionary<string, NativeWebRtcMediaEngine.AudioPlaybackSession> groupAudioLevelProbes =
        new(StringComparer.OrdinalIgnoreCase);
    private RealtimeMediaSelection? lastReceiverSelection;
    // Audio Phase 2a-step4b (2026-05-23): WASAPI capture + opus + RTP
    // packetizer. Lives alongside the video sender and starts together; its packets go straight
    // to the core like the video sender's (see activePacketTargetId).
    private NativeWebRtcMediaEngine.AudioCaptureSession? nativeAudioCaptureSession;
    // The core's per-call send target (omni_realtime_media_packet_target_create): every native
    // session of this call hands its outgoing packets to the core's sink with this ID, on the
    // thread that produced them. The shell wires the two and never touches a packet. 0 = none.
    // Until 2026-09-26 a shell loop drained the engine on a ~15.6 ms timer and sent batches —
    // docs/bugs/media-rode-the-shells-timer.md.
    private ulong activePacketTargetId;
    // The call's inbound router: the core hands it every inbound datagram at once, and it routes
    // them to the sessions below — the shell only keeps it told which sessions exist
    // (RouteSessions) and builds the ones it asks for (ServiceMediaRouter). Same history as above.
    private NativeWebRtcMediaEngine.MediaRouter? activeMediaRouter;
    private string audioCaptureDiagnostics = "none";
    // Audio Phase 2b-step4 (2026-05-23): receive-side opus + WASAPI render
    // counterpart to nativeAudioCaptureSession. Created when the video
    // receiver is set up so the playback endpoint is ready as soon as
    // the remote peer starts sending audio. Disposed on stop alongside
    // the receiver. The handle internally drops mismatched-PT/SSRC
    // packets, so we can fire every inbound RTP packet at it without
    // demuxing first.
    private NativeWebRtcMediaEngine.AudioPlaybackSession? nativeAudioPlaybackSession;
    private string audioPlaybackDiagnostics = "none";
    // Phase 3 (2026-05-23): last lipsync delay the audio playback
    // confirmed (clamped at the native side). 0 until the video
    // receiver has reported a JitterEstimator-driven target.
    private long audioPlayoutTargetDelayMs;
    private WriteableBitmap? localPreviewBitmap;
    private WriteableBitmap? remoteVideoBitmap;
    private string? activeDialogKey;
    // Wave 2: pinned UTF-8 buffer for activeDialogKey. Lifecycle pinned to
    // (StartAsync, StopAsync) — created with the key, disposed in cleanup.
    // Used to create the call's send target and wire its inbound sink, both by dialog key.
    // See DialogKeyPin.cs.
    private DialogKeyPin? activeDialogKeyPin;
    private string activeMediaSessionId = string.Empty;
    private string? activeSelectionSignature;
    private RealtimeMediaSelection? activeLocalSelection;
    private string? activeReceiverSelectionSignature;
    private CallVideoCodecKind activeNativeCodecKind = CallVideoCodecKind.H264;
    private IntPtr nativeHandle;
    private bool localCameraActive;
    // Group-call local mic mute (increment 2). Distinct from the 1:1 call-state mute
    // (rustBridge.SetLocalMicMuted, keyed by a 1:1 CallId that group calls don't have). Muting
    // gates the audio SEND inside the engine: capture keeps running for the local speaking level,
    // nothing is handed to the core.
    private volatile bool localAudioMuted;
    // Audio-first call model: a call starts audio-only; the local camera is an explicit
    // opt-in toggled at runtime (StartLocalCameraAsync). Default false = no webcam on start.
    private bool localCameraRequested;
    // Is a CALL running on the watched dialog? Watching a dialog (WatchDialogAsync, which
    // MainWindow does on every chat open) sets up receive + negotiation; only StartAsync /
    // StartGroupMediaAsync — an actual placed or accepted call — sets this.
    //
    // It exists because the two used to be the same thing. The negotiation monitor started
    // the local sender for ANY watched dialog, so merely opening a chat opened the
    // MICROPHONE and streamed audio to the peer: both sides could hear each other before
    // anyone placed a call, an open chat was a hot mic, and the "call" appeared to connect
    // instantly with nothing accepted. Live test 2026-08-11: audio datagrams flowing at
    // 10:55:02, CallInvite at 10:55:11.
    private volatile bool callMediaRequested;
    // Screen share (Zoom/Telemost "share screen"): when requested, the video track carries the
    // captured screen INSTEAD of the camera (source swap). The native video sender already exists
    // for audio+video; StartScreenCapture just feeds it screen frames (ScreenShareCaptureSource)
    // in place of the webcam. `screenShareCameraWasRequested` remembers the pre-share camera state so
    // stopping the share returns to it. See docs/decisions/screen-share.md.
    private bool localScreenShareRequested;
    private bool screenShareCameraWasRequested;
    private global::Windows.Graphics.Capture.GraphicsCaptureItem? screenShareItem;
    /// The size the screen is shared in, picked on the share button; kept after the share stops so
    /// the menu marks it next time.
    private CallVideoResolutionOption? screenShareResolution;

    /// The size the last share went in, for the menu's mark (null before the first share).
    internal CallVideoResolutionOption? LastScreenShareResolution => screenShareResolution;
    // The running screen-share source: the real capture, or the synthetic pattern (test channel).
    private IDisposable? screenShareSource;
    // `pattern.on`: share the synthetic pattern instead of a captured screen (SyntheticPatternSource).
    private bool syntheticPatternRequested;
    // Per-window camera override chosen in Settings (a MediaFrameSourceGroup.Id); null = system default.
    private volatile string? preferredCameraId;
    // Task #87: per-window encode/decode device choice from Settings (a CallVideoDeviceSelection key:
    // "auto" / "software" / a GPU description). Resolved against the live capability probe into a
    // concrete vendor id + software flag when each native session is created.
    private volatile string encodeDevicePreferenceKey = CallVideoDeviceSelection.AutoKey;
    private volatile string decodeDevicePreferenceKey = CallVideoDeviceSelection.AutoKey;
    private bool remoteVideoSeen;
    // #5 increment 3b (2026-06-05): GPU video render path state.
    //  - gpuVideoEnabled (intent): try the GPU path. Read on the media thread
    //    (DrainDecodedNativeFrames) + UI thread; flipped false PERMANENTLY for the call once an
    //    attach fails / the build can't bring up D3D, so we drop back to the WriteableBitmap path.
    //  - gpuVideoPath (active): a swapchain is attached + bound to the panel. Read on the diag
    //    thread (BuildNativeSessionDiagnostics) + UI thread.
    //  - The remaining fields are touched only on the UI thread (attach / resize / teardown all
    //    marshal there). Presentation does NOT — see the presenter fields below.
    private volatile bool gpuVideoEnabled = true;
    private volatile bool gpuVideoPath;
    private NativeWebRtcMediaEngine.Session? gpuRendererAttachedReceiver;
    private ISwapChainPanelNative? remoteSwapChainPanelNative;
    private bool gpuPanelHandlersWired;
    // The presenter: one dedicated thread per receiver session, presenting decoded frames on the
    // display's clock instead of on the UI thread and instead of on packet arrival. See
    // CallVideoService.Render.cs (EnsureRemoteGpuPresenter) and
    // docs/decisions/video-presentation-thread.md. Written by the start/stop path, read elsewhere.
    private NativeWebRtcMediaEngine.Session? gpuPresenterReceiver;
    private Thread? gpuPresenterThread;
    private CancellationTokenSource? gpuPresenterCancellation;
    private int gpuPresenterStartRunning;
    // Long enough to cover a present in flight, short enough that a stuck GPU driver cannot hold
    // "hang up" or "close the app" hostage: the thread is a background thread and touches only the
    // receiver, which is disposed after this.
    private const int PresenterJoinTimeoutMs = 1_000;
    // A decode + present tick takes milliseconds; one stuck this long never returns
    // (DetectHungVideoDecoder, docs/bugs/hung-decoder-froze-the-app.md).
    private const int HungDecoderMs = 3_000;
    // How long the UI thread may wait for a receiver's gate before it skips the call instead.
    private const int UiReceiverGateTimeoutMs = 100;
    private volatile bool rtcpFeedbackEnabled;
    // Frames taken out of the source, in "source frames" of credit. See ShouldEncodeFrame.
    private long frameSelectionCredit;
    /// Когда взят прошлый кадр источника без объявленной частоты. См. ShouldEncodeFrame.
    private long frameSelectionLastSelectedAtMs;
    private long nextLocalPreviewAtMs;
    private int captureFrameInFlight;
    private int localPreviewLoopScheduled;
    private int remoteRenderLoopScheduled;
    private long framesSent;
    private long framesReceived;
    private long framesRendered;
    private long decodeStalls;
    private long renderStalls;
    private long encodeFrameProcessingMs;
    private long decodeFrameProcessingMs;
    private long renderFrameProcessingMs;
    private long captureFramesArrived;
    private long captureFramesSkippedThrottle;
    private long captureFramesSkippedBusy;
    private long captureFramesProcessed;
    private long captureFrameProcessingMs;
    private long activeVideoWidth;
    private long activeVideoHeight;
    private long decodedVideoWidth;
    private long decodedVideoHeight;
    private long diagnosticsSampleAtMs;
    private long diagnosticsSampleFramesSent;
    private long diagnosticsSampleFramesRendered;
    private long diagnosticsSampleEncodedBytesSent;
    private long diagnosticsSampleCaptureFramesArrived;
    private long diagnosticsCurrentSentFps;
    private long diagnosticsCurrentRenderedFps;
    private long diagnosticsCurrentSentKbps;
    private long diagnosticsCurrentCaptureLoopFps;
    private long nextDiagnosticsPublishAtMs;
    private int diagnosticsRefreshRequested;
    // Longest gap between two encoded frames inside the current diagnostics window, and the last
    // encode's timestamp it is measured from. Average frame rate hides a hitch completely: 30 fps
    // with one 400 ms stall still reads as 30 fps. This is the number that matches what the user
    // actually sees. Reset every window by the diagnostics builder.
    private long lastEncodeAtMs;
    private long encodeGapMaxMs;
    // Worst gap since media started. The window figure above is reset every time the string is
    // built, so anything sampled seconds apart would miss a rare hitch entirely — and a rare
    // hitch is exactly the complaint.
    private long encodeGapWorstMs;
    // Same idea on the receive side: worst interval between two frames actually presented — in
    // this diagnostics window, and since media started. The window figure is what tells a one-off
    // at video start (decoder build, first keyframe) apart from a picture that keeps stuttering;
    // with only the all-time figure a single reading cannot distinguish them, and that is exactly
    // the question every "дёргается" report asks.
    private long renderGapMaxMs;
    private long renderGapWorstMs;
    // Потери у собеседника (Q8) по его последнему RR — из нативной диагностики отправителя.
    private long diagnosticsRemoteLossFraction;
    // Проход туда-обратно, ИЗМЕРЕННЫЙ НАМИ по входящему RTCP, а не пересказанный собеседником.
    // -1 = ещё не измеряли. Живой двухмашинный прогон 2026-08-29 показал, зачем он в диагностике:
    // на канале с настоящими 50 мс обе печатаемые величины RTT были нулём, потому что обе брались
    // из отчёта пира, а пир без камеры ничего не измеряет. На одномашинном стенде ноль — ПРАВИЛЬНЫЙ
    // ответ, поэтому отличить «быстро» от «никто не мерил» там нельзя по построению.
    private long diagnosticsMeasuredPathRttUs = -1;
    private long lastRemoteFrameRenderedAtMs;
    private long captureStartAttemptId;
    private long captureStartedAtMs;
    private string captureDiagnostics = "none";
    private string captureProbeDiagnostics = "camera_probe=not_run";
    private string codecDiagnostics = "none";
    private string nativeMediaDiagnostics = "none";
    private string negotiationDiagnostics = "none";
    private string lastError = string.Empty;
    private int targetCaptureWidth = CallVideoPreferences.DefaultPreferredCaptureWidth;
    private int targetCaptureHeight = CallVideoPreferences.DefaultPreferredCaptureHeight;
    private int targetFps = CallVideoPreferences.DefaultTargetFps;
    private int targetFrameIntervalMs = DefaultTargetFrameIntervalMs;
    private int targetBitrateBps = DefaultTargetBitrateBps;
    private int negotiatedCaptureWidth = CallVideoPreferences.DefaultPreferredCaptureWidth;
    private int negotiatedCaptureHeight = CallVideoPreferences.DefaultPreferredCaptureHeight;
    private int negotiatedFps = CallVideoPreferences.DefaultTargetFps;
    private int negotiatedBitrateBps = DefaultTargetBitrateBps;
    /// The last ABR target of this call's sender: a sender rebuilt mid-call starts where the old one
    /// was, not at the ceiling. 0 = no sender ran in this call yet.
    private int lastSenderAbrBitrateBps;
    /// Согласованный предел одной датаграммы, байт. 0 = согласования ещё не было.
    private int negotiatedMaxDatagramBytes;
    private int adaptiveBitrateBps = DefaultTargetBitrateBps;
    private int adaptiveFps = CallVideoPreferences.DefaultTargetFps;
    private string adaptiveDiagnostics = "stable";
    private int captureProfileMaxWidth;
    private int captureProfileMaxHeight;
    private int activeCaptureProfileWidth;
    private int activeCaptureProfileHeight;
    private int activeCaptureProfileFps;
    private string selectedCapturePixelFormat = CallVideoPreferences.CapturePixelFormatNv12;
    private CallVideoCaptureProbeResult? captureProbeResult;
    // When captureProbeResult was produced, and whether a ring-time warm-up is already running.
    // Both exist so the camera format probe stops landing on the answer path — see
    // WarmCaptureProbe.
    private long captureProbeCompletedAtMs;
    private int captureProbeWarmupRunning;
    // Guards the lazy video-decoder build so a burst of inbound video packets starts exactly one.
    private int nativeVideoReceiverBuildRunning;
    private LocalPreviewFrame? pendingLocalPreviewFrame;
    private BgraFrame? pendingRemoteRenderFrame;
    // Serialises local-sender reconfigurations (camera on/off, screen share start/stop,
    // preference change). Every request must actually RUN — see the note on
    // ReconfigureLocalSenderForSelectionAsync for what dropping one did.
    private readonly SemaphoreSlim localSenderReconfigureSync = new(1, 1);
    private int captureReconfigureInFlight;
    private CallVideoCodecPreference codecPreference = CallVideoCodecPreference.Auto;
    // The two directions of the current call (per-pair-video-codec-selection.md): what each carries
    // and why, for Diagnostics → Media.
    private RealtimeVideoStream? activeSendStream;
    private RealtimeVideoStream? activeReceiveStream;
    // Codecs that broke in this call and left the passport until it ends (§6). Under lifecycleSync.
    private readonly CallVideoCodecHealth codecHealth = new();
    // What our last passport said about us (content, processor, power) — Diagnostics → Media.
    private string lastPublishedPassport = string.Empty;
    // Разрешение, которое выбрал отправитель. Не Auto: авторазрешение снято 2026-08-17,
    // и значение здесь меняется только по явному выбору из меню камеры/экрана.
    private CallVideoResolutionOption selectedResolution = CallVideoResolutionOption.Even(
        CallVideoPreferences.DefaultPreferredCaptureWidth,
        CallVideoPreferences.DefaultPreferredCaptureHeight);
    // Пока false, разрешение следует за возможностями источника (AdoptProbeDefaultResolutionUnsafe).
    // Явный выбор из меню камеры/экрана его фиксирует, и подстройка больше не вмешивается.
    private bool resolutionChosenExplicitly;
    private CallVideoFrameRatePreference frameRatePreference = CallVideoFrameRatePreference.Auto;
    private CallVideoBitratePreference bitratePreference = CallVideoBitratePreference.Auto;

    public CallVideoService(
        RustBridge rustBridge,
        DispatcherQueue dispatcherQueue,
        Image localPreviewImage,
        Image remoteVideoImage,
        SwapChainPanel remoteVideoSwapChainPanel,
        TextBlock diagnosticsTextBlock)
    {
        this.rustBridge = rustBridge;
        this.dispatcherQueue = dispatcherQueue;
        this.localPreviewImage = localPreviewImage;
        this.remoteVideoImage = remoteVideoImage;
        this.remoteVideoSwapChainPanel = remoteVideoSwapChainPanel;
        this.diagnosticsTextBlock = diagnosticsTextBlock;
        // The first call must not pay for the hardware proof either (own thread, off startup's way).
        NativeWebRtcMediaProbe.RefreshInBackground();
    }

    internal event EventHandler? StateChanged;

    /// <summary>
    /// Is the CAMERA the live local video source? Screen share also runs a local video source
    /// (it swaps the camera out), and `localCameraActive` is set for both — so the camera
    /// button used to light up and offer "Выключить камеру" the moment a share started, with
    /// the camera off. Reported live 2026-08-11.
    /// </summary>
    internal bool IsLocalCameraActive => localCameraActive && !localScreenShareRequested;

    /// True while ANY local video source is running (camera or screen share).
    internal bool IsLocalVideoSourceActive => localCameraActive;

    /// True while we are broadcasting our screen on the video track (instead of the camera).
    internal bool IsScreenSharing => localScreenShareRequested;

    // The monitor being shared, when the share is a whole monitor (not a window, not the synthetic
    // pattern): the only thing remote screen control may drive. See ScreenControlInjector.
    private volatile SharedMonitor? sharedMonitor;

    internal SharedMonitor? SharedMonitor => localScreenShareRequested ? sharedMonitor : null;

    /// Whether screen capture is available on this machine (Windows.Graphics.Capture support).
    internal static bool IsScreenShareSupported => ScreenShareCaptureSource.IsSupported;

    /// The dialog whose media session is live, or null. Lets the shell notice that the
    /// call behind a running media session is over and tear it down.
    internal string? ActiveDialogKey => activeDialogKey;

    // A group call's media is running. Its lifetime belongs to the group conversation (join/leave),
    // not to the 1:1 call state — the 1:1 paths must leave it alone.
    internal bool IsGroupMediaActive => activeGroupMode && callMediaRequested && mediaCancellation is not null;

    /// Set before StartAsync for a remote-control session (docs/decisions/remote-access-for-contacts.md):
    /// the session is screen and input only, so the microphone is never opened.
    internal bool MicrophoneDisabled { get; set; }

    /// A remote-control session uses no camera on either side (the owner shares its screen, the
    /// controller only watches). The camera probe then is pure delay on the answer path — about a
    /// second, measured live 2026-10-06 — and lights the camera LED for nothing.
    internal bool CameraNotNeeded { get; set; }

    /// Local mic mute state (self). Toggling it gates the audio send loop.
    internal bool IsLocalAudioMuted => localAudioMuted;

    /// <summary>
    /// How long a microphone may deliver nothing but digital zeros before the call says so. A real
    /// microphone always has a noise floor, so this is a device that hears nothing at all: a PC whose
    /// only "microphone" is a virtual one (Steam Streaming Microphone — live 2026-10-14, the PC sent
    /// silence for whole calls and nobody could tell), or a hardware mute switch.
    /// </summary>
    internal static readonly TimeSpan SilentMicrophoneAfter = TimeSpan.FromSeconds(6);

    /// <summary>
    /// The name of the microphone of the running call when it has heard nothing for
    /// <see cref="SilentMicrophoneAfter"/>; <c>null</c> when it hears, when we are muted, or when
    /// there is no capture. Never throws — polled from the call refresh.
    /// </summary>
    internal string? ReadSilentMicrophone()
    {
        NativeWebRtcMediaEngine.AudioCaptureSession? capture = nativeAudioCaptureSession;
        if (capture is null || localAudioMuted)
        {
            return null;
        }

        try
        {
            if (capture.ReadDigitalSilenceMs() < (long)SilentMicrophoneAfter.TotalMilliseconds)
            {
                return null;
            }

            System.Text.RegularExpressions.Match name = System.Text.RegularExpressions.Regex.Match(
                capture.ReadEchoDiagnostics(), "mic=\"((?:[^\"\\\\]|\\\\.)*)\"");
            return name.Success && name.Groups[1].Value.Length > 0
                ? System.Text.RegularExpressions.Regex.Unescape(name.Groups[1].Value)
                : "Микрофон";
        }
        catch (ObjectDisposedException)
        {
            return null; // the call ended between the read and the check
        }
    }

    internal void SetLocalAudioMuted(bool muted)
    {
        localAudioMuted = muted;
        lock (lifecycleSync)
        {
            nativeAudioCaptureSession?.SetMuted(muted);
        }
    }

    // Camera the local capture should prefer (Settings → "Камера", persisted per window).
    // StartCaptureAsync orders the matching MediaFrameSourceGroup first and falls back to any
    // other camera if it can't start, so a stale/unplugged id never blocks the call.
    internal string? PreferredCameraId
    {
        get => preferredCameraId;
        set => preferredCameraId = value;
    }

    // Microphone and speakers (Settings → «Звонки», persisted per window): IMMDevice ids from
    // NativeWebRtcMediaEngine.ListAudioDevices; null = Windows' default. Read when the capture /
    // playback is built, so a change applies to the next call. A gone device falls back to the
    // default natively, so a stale id never leaves a call silent.
    internal string? PreferredMicrophoneId { get; set; }

    internal string? PreferredSpeakerId { get; set; }

    // Task #87: Settings pushes the chosen encode/decode device keys here (CallVideoDeviceSelection
    // keys). They take effect on the next session create (call start / renegotiation), like the
    // codec/quality/fps preferences.
    internal void SetDevicePreferences(string? encodeDeviceKey, string? decodeDeviceKey)
    {
        encodeDevicePreferenceKey = string.IsNullOrWhiteSpace(encodeDeviceKey)
            ? CallVideoDeviceSelection.AutoKey
            : encodeDeviceKey;
        decodeDevicePreferenceKey = string.IsNullOrWhiteSpace(decodeDeviceKey)
            ? CallVideoDeviceSelection.AutoKey
            : decodeDeviceKey;
    }

    // Resolve the stored encode/decode preference keys against the live capability probe into the
    // concrete vendor id / software flag the native session config consumes. Auto -> energy-first.
    private ResolvedVideoDevice ResolveEncodeDevice()
        => CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode,
            encodeDevicePreferenceKey,
            NativeWebRtcMediaProbe.GetProbeResult().VideoDevices);

    private ResolvedVideoDevice ResolveDecodeDevice()
        => CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode,
            decodeDevicePreferenceKey,
            NativeWebRtcMediaProbe.GetProbeResult().VideoDevices);

    // A remote frame arrived recently. Once a sender stops (camera off / stream stall) the last
    // frame stays on screen frozen; aging it out lets the call overlay fall back to the peer
    // avatar. The timestamp is bumped on every rendered frame (software + GPU paths), so active
    // video always reads live and only a genuine stop crosses the threshold.
    private const long RemoteVideoStaleMs = 2000;
    internal bool HasRemoteVideo =>
        remoteVideoSeen &&
        Environment.TickCount64 - Interlocked.Read(ref lastRemoteFrameRenderedAtMs) < RemoteVideoStaleMs;

    /// <summary>
    /// Is the CALL media session live on this dialog? Requires an actual call
    /// (<see cref="callMediaRequested"/>) — a merely watched chat negotiates and can
    /// receive, but it is not a call and must not light up the call overlay or the
    /// camera button.
    /// </summary>
    internal bool IsActiveForDialog(string? dialogKey)
        => callMediaRequested &&
           !string.IsNullOrWhiteSpace(dialogKey) &&
           string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal) &&
           (localCameraActive ||
            remoteVideoSeen ||
            nativeReceiverSession is not null ||
            !string.IsNullOrWhiteSpace(activeReceiverSelectionSignature));

    /// <summary>
    /// Whether this machine can be a video source right now: a live media session on
    /// this dialog AND a camera that can serve the chosen profile. The camera toggle is
    /// gated on this, so a machine with no camera shows a disabled button (with a reason)
    /// instead of one that throws when pressed.
    /// </summary>
    internal bool CanStartLocalCameraForDialog(string? dialogKey)
        => IsActiveForDialog(dialogKey) && LocalCameraIsUsable;

    /// <summary>Why the camera toggle is unavailable, or null when it is.</summary>
    internal string? LocalCameraUnavailableReason
    {
        get
        {
            CallVideoCaptureProbeResult? probe;
            lock (lifecycleSync)
            {
                probe = captureProbeResult;
            }

            if (probe is null)
            {
                return "Камера ещё не опрошена.";
            }

            return probe.Formats.Count == 0
                ? $"Камера недоступна: {probe.BuildShortDiagnostics()}"
                : null;
        }
    }

    private bool LocalCameraIsUsable
    {
        get
        {
            CallVideoCaptureProbeResult? probe;
            CallVideoResolutionOption quality;
            CallVideoFrameRatePreference frameRate;
            lock (lifecycleSync)
            {
                probe = captureProbeResult;
                quality = selectedResolution;
                frameRate = frameRatePreference;
            }

            return probe is not null &&
                   CallVideoPreferences.CanCaptureVideo(quality, frameRate, probe);
        }
    }

    internal Task WatchDialogAsync(string dialogKey) => WatchDialogAsync(dialogKey, groupMode: false);

    // groupMode (G10): a `group:<id>` call has no 1:1 pairwise session, so the
    // capability-publish / negotiation-monitor / control (NACK/PLI) loops — all of
    // which use the 1:1 media-negotiation channel — are skipped. Media itself
    // (receive/send loops) rides the group peer-SFU path via the core's dialog
    // routing. Media format is self-negotiated (see StartGroupMediaAsync).
    internal async Task WatchDialogAsync(string dialogKey, bool groupMode)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            await StopAsync();
            return;
        }

        lock (lifecycleSync)
        {
            if (string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal) &&
                mediaCancellation is not null)
            {
                return;
            }
        }

        await StopAsync();

        // Architecture: "If the V2 QUIC DATAGRAM carrier is unavailable, calls
        // fail visibly with VIDEO_TRANSPORT_V2_REQUIRED instead of falling
        // back to legacy media." Surface this BEFORE starting receive/send
        // loops that would otherwise silently no-op on a null handle.
        // See docs/decisions/h264-runtime-proof.md.
        IntPtr resolvedHandle = rustBridge.GetInitializedHandleForRealtimeMedia();
        if (resolvedHandle == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                "VIDEO_TRANSPORT_V2_REQUIRED: native realtime media handle is unavailable; QUIC DATAGRAM carrier is required for video calls.");
        }

        CancellationTokenSource cancellation = new();
        DialogKeyPin dialogKeyPin = new(dialogKey);
        ulong packetTargetId = CreatePacketTarget(resolvedHandle, dialogKeyPin);
        NativeWebRtcMediaEngine.MediaRouter mediaRouter;
        try
        {
            mediaRouter = CreateWiredMediaRouter(resolvedHandle, dialogKeyPin, groupMode);
        }
        catch
        {
            Core.NativeMethods.omni_realtime_media_packet_target_destroy(packetTargetId);
            dialogKeyPin.Dispose();
            throw;
        }

        lock (lifecycleSync)
        {
            activeDialogKey = dialogKey;
            activeDialogKeyPin = dialogKeyPin;
            activePacketTargetId = packetTargetId;
            activeMediaRouter = mediaRouter;
            nativeHandle = resolvedHandle;
            mediaCancellation = cancellation;
            activeGroupMode = groupMode;
            ResetMediaCounters();
            receiveLoopTask = Task.Run(() => ReceiveLoopAsync(dialogKey, cancellation.Token), cancellation.Token);
            diagnosticsLoopTask = Task.Run(() => DiagnosticsLoopAsync(cancellation.Token), cancellation.Token);
            // The control (NACK/PLI), capability-publish and negotiation-monitor
            // loops all use the 1:1 pairwise negotiation channel — skip them for a
            // group call (self-negotiated, no pairwise session). See groupMode.
            if (!groupMode)
            {
                controlLoopTask = Task.Run(() => AbrLoopAsync(cancellation.Token), cancellation.Token);
                capabilityPublishLoopTask = Task.Run(
                    () => MaintainPublishedLocalCapabilitiesAsync(dialogKey, cancellation.Token),
                    cancellation.Token);
                negotiationMonitorLoopTask = Task.Run(
                    () => MonitorNegotiatedMediaAsync(dialogKey, cancellation.Token),
                    cancellation.Token);
            }
            else
            {
                // A group's format is self-picked, but the core re-picks our one codec for everybody
                // as people join and announce what they decode (phase 4a): follow it.
                negotiationMonitorLoopTask = Task.Run(
                    () => MonitorGroupMediaAsync(dialogKey, cancellation.Token),
                    cancellation.Token);
            }
        }

        PublishDiagnostics();
    }

    /// <summary>
    /// Is the local sender already running exactly this selection? The audio capture handle is
    /// part of the answer: the signature alone is set at the very end of a sender start, but a
    /// torn-down session leaves it stale until the next one finishes.
    /// </summary>
    private bool IsLocalSenderUpForSelection(RealtimeMediaSelection selection)
    {
        string selectionSignature = BuildSelectionSignature(selection);
        lock (lifecycleSync)
        {
            return nativeAudioCaptureSession is not null &&
                   string.Equals(activeSelectionSignature, selectionSignature, StringComparison.Ordinal);
        }
    }

    /// <summary>
    /// A call exists but nobody has answered it yet — do the expensive warm-ups NOW, while the
    /// user is listening to it ring, instead of after the answer where they are waiting to talk.
    ///
    /// Only the native hardware probe: it encodes and decodes synthetic frames on the GPU, opens
    /// no capture device and sends nothing, so running it before pickup costs the user nothing.
    /// The camera probe is warmed too. It opens each capture device in
    /// <c>MediaCaptureSharingMode.SharedReadOnly</c> and reads the format list WITHOUT ever
    /// starting a frame reader or a preview — nothing streams, so the capture indicator has
    /// nothing to light up for. It also already runs on every answered call; the only case this
    /// adds it to is a call nobody picks up. If that ever turns out to be visible to the user,
    /// this is the one line to drop. See docs/bugs/slow-media-start.md.
    /// </summary>
    /// <summary>
    /// A call starts audio-only and unmuted, always. Both flags say what the USER asked for in a
    /// call, and a call that has ended takes its answers with it.
    ///
    /// Neither was reset: a call where the camera was switched on left <c>localCameraRequested</c>
    /// true, so the NEXT call opened the webcam by itself the moment media came up — the user
    /// never asked, and the audio-first model says they must. Mute had the same shape (a call
    /// ended muted came back muted while the button showed "unmuted"); the group path had always
    /// reset it, the 1:1 path had not.
    /// </summary>
    private void ResetPerCallMediaIntent(bool startMuted = false)
    {
        // The one exception: a mic switched off while the call was still ringing (the core keeps it
        // in the call summary) — it opens muted, with no window in which audio already left.
        localAudioMuted = startMuted;
        lock (lifecycleSync)
        {
            localCameraRequested = false;
            localScreenShareRequested = false;
            syntheticPatternRequested = false;
            screenShareCameraWasRequested = false;
            screenShareItem = null;
            codecHealth.Reset();
        }
    }

    internal void WarmMediaProbesForPendingCall(bool cameraNeeded)
    {
        // The native proof is refreshed at startup and after each call, never on the call path
        // (NativeWebRtcMediaProbe.RefreshInBackground). Only a process that never had one yet
        // gets its first here, so the ringing hides as much of it as it can.
        if (NativeWebRtcMediaProbe.ProbeAgeMs() < 0)
        {
            NativeWebRtcMediaProbe.RefreshInBackground();
        }

        if (cameraNeeded)
        {
            WarmCaptureProbe();
        }
    }


    /// <summary>
    /// Re-probe the capture devices in the background if the cached answer is stale. Same bargain
    /// as the native probe: the answer path wants a FRESH list (a camera swapped between calls
    /// must not leave us publishing a profile it cannot serve), but it does not want to pay for
    /// it with the user waiting to be heard.
    /// </summary>
    private void WarmCaptureProbe()
    {
        if (!InvalidateCaptureProbeIfStale())
        {
            return;
        }

        if (Interlocked.Exchange(ref captureProbeWarmupRunning, 1) == 1)
        {
            return;
        }

        _ = Task.Run(async () =>
        {
            try
            {
                await EnsureCaptureProbeAsync(CancellationToken.None);
            }
            catch
            {
                // Best effort: if the warm-up failed, the answer path probes for itself and
                // reports the failure there, where it can actually be shown to the user.
            }
            finally
            {
                Interlocked.Exchange(ref captureProbeWarmupRunning, 0);
            }
        });
    }

    /// <summary>
    /// Drop the cached capture probe if it is older than <see cref="CaptureProbeFreshnessMs"/>;
    /// returns whether a re-probe is now needed. Replaces an unconditional clear at every call
    /// start — that guaranteed the probe ran on the answer path every single time.
    /// </summary>
    private bool InvalidateCaptureProbeIfStale()
    {
        lock (lifecycleSync)
        {
            if (captureProbeResult is not null &&
                Environment.TickCount64 - captureProbeCompletedAtMs < CaptureProbeFreshnessMs)
            {
                return false;
            }

            captureProbeResult = null;
            return true;
        }
    }

    internal async Task StartAsync(string dialogKey, bool startMuted = false)
    {
        ResetPerCallMediaIntent(startMuted);
        // Same bargain for the C#-side capture probe: a USB camera swapped between calls must not
        // leave us publishing a profile it cannot serve (Audit P1), but the answer path shouldn't
        // pay for the re-probe when the ring already did it.
        InvalidateCaptureProbeIfStale();

        // Everything between "the call was answered" and "the peer hears us" is on the user's
        // clock, so every stage of it is timed. See docs/bugs/slow-media-start.md.
        long startedAtMs = Environment.TickCount64;

        // WatchDialogAsync stops whatever was running and clears this, so claim the call
        // AFTER it: from here on the negotiation monitor is allowed to open the mic.
        await WatchDialogAsync(dialogKey);
        callMediaRequested = true;
        long watchDoneAtMs = Environment.TickCount64;

        CancellationToken token;
        lock (lifecycleSync)
        {
            if (localCameraActive)
            {
                return;
            }

            token = mediaCancellation?.Token
                ?? throw new InvalidOperationException("Video receiver is not initialized.");
        }

        RealtimeMediaSelection selection = await EnsureNegotiatedMediaAsync(dialogKey, token);
        long negotiatedAtMs = Environment.TickCount64;
        // Through the same gate every other sender (re)configuration goes through. Without it the
        // start path and the negotiation monitor — which sees the very selection this path just
        // committed — built a sender each, at the same time: two decoders, two microphone
        // handles, one of each thrown away, and the answer path paying for both. Live trace
        // showed the doubled work as two `audio_capture=` stages in one call start.
        await localSenderReconfigureSync.WaitAsync(token);
        try
        {
            // The negotiation monitor watches for exactly the selection this path just committed
            // and reconfigures the sender when it sees one. Both used to run at once, each
            // building a decoder and a microphone handle and one of the two being thrown away —
            // the live trace showed it as two `audio_capture=` stages inside one call start. The
            // gate serializes them; this check means the loser does nothing at all instead of
            // rebuilding what the winner already stood up.
            if (!IsLocalSenderUpForSelection(selection))
            {
                await StartLocalSenderForSelectionAsync(dialogKey, selection, token);
            }
        }
        finally
        {
            localSenderReconfigureSync.Release();
        }

        long senderUpAtMs = Environment.TickCount64;
        Core.AppTrace.Write(
            "media/start-stage",
            $"TOTAL={senderUpAtMs - startedAtMs}ms watch={watchDoneAtMs - startedAtMs}ms " +
            $"negotiate={negotiatedAtMs - watchDoneAtMs}ms sender={senderUpAtMs - negotiatedAtMs}ms");
    }

    /// Start media for a group call (G10). `groupDialogKey` is `group:<id>`; the
    /// core routes send/receive on it through the peer-SFU path. There is no 1:1
    /// negotiation — we self-select a media format from our own capabilities (all
    /// members converge on a shared default), then start the sender. The native
    /// receiver demuxes co-members' streams by RTP SSRC (v1: one mixed remote
    /// view; per-sender tiles / active-speaker are a later UI increment).
    internal async Task StartGroupMediaAsync(string groupDialogKey)
    {
        ResetPerCallMediaIntent();
        lock (lifecycleSync)
        {
            captureProbeResult = null;
        }

        await WatchDialogAsync(groupDialogKey, groupMode: true);
        callMediaRequested = true;

        CancellationToken token;
        lock (lifecycleSync)
        {
            if (localCameraActive)
            {
                return;
            }

            token = mediaCancellation?.Token
                ?? throw new InvalidOperationException("Video receiver is not initialized.");
        }

        RealtimeMediaSelection selection = await EnsureGroupSelfNegotiatedMediaAsync(groupDialogKey, token);
        // Same gate as the 1:1 start, for the same reason.
        await localSenderReconfigureSync.WaitAsync(token);
        try
        {
            await StartLocalSenderForSelectionAsync(groupDialogKey, selection, token);
        }
        finally
        {
            localSenderReconfigureSync.Release();
        }
    }

    // Self-negotiate a group media format from our own capabilities (no pairwise
    // exchange): the core self-selects and returns the committed selection.
    private async Task<RealtimeMediaSelection> EnsureGroupSelfNegotiatedMediaAsync(
        string dialogKey,
        CancellationToken token)
    {
        CallVideoCaptureProbeResult captureProbe = await EnsureCaptureProbeAsync(token);
        RealtimeMediaCapabilities capabilities = BuildLocalCapabilities(dialogKey, captureProbe);
        RealtimeMediaNegotiationSnapshot snapshot = await Task.Run(
            () => rustBridge.PublishGroupMediaCapabilities(capabilities),
            token);
        return snapshot.LocalSelection
            ?? throw new InvalidOperationException(
                snapshot.Diagnostic ?? "No local media format is available for the group call.");
    }

    internal async Task StopAsync()
    {
        CancellationTokenSource? cancellation;
        CancellationTokenSource? captureCancel;
        Task? receiveTask;
        Task? controlTask;
        Task? capabilityPublishTask;
        Task? negotiationMonitorTask;
        DialogKeyPin? dialogKeyPinForCleanup;
        ulong packetTargetIdForCleanup;
        NativeWebRtcMediaEngine.MediaRouter? mediaRouterForCleanup;
        IntPtr handleForCleanup;
        Task? trackedFrameTaskForCleanup;
        Task? trackedPreviewTaskForCleanup;
        Task? trackedRemoteRenderTaskForCleanup;
        Task? trackedReconfigureTaskForCleanup;
        MediaFrameReader? reader;
        MediaCapture? capture;
        IDisposable? screenSource;
        NativeWebRtcMediaEngine.Session? currentNativeSender;
        NativeWebRtcMediaEngine.Session? currentNativeReceiver;
        NativeWebRtcMediaEngine.AudioCaptureSession? currentAudioCapture;
        NativeWebRtcMediaEngine.AudioPlaybackSession? currentAudioPlayback;
        string? dialogKeyForCleanup;

        // Whatever else this teardown does, the call is over: drop the claim first so a
        // negotiation monitor still finishing its pass cannot re-open the microphone.
        if (callMediaRequested)
        {
            Core.AppTrace.Write("media/sender-down", $"dialog={activeDialogKey}");
        }

        callMediaRequested = false;

        lock (lifecycleSync)
        {
            cancellation = mediaCancellation;
            captureCancel = captureCancellation;
            receiveTask = receiveLoopTask;
            controlTask = controlLoopTask;
            capabilityPublishTask = capabilityPublishLoopTask;
            negotiationMonitorTask = negotiationMonitorLoopTask;
            reader = frameReader;
            capture = mediaCapture;
            screenSource = screenShareSource;
            currentNativeSender = nativeSenderSession;
            currentNativeReceiver = nativeReceiverSession;
            currentAudioCapture = nativeAudioCaptureSession;
            currentAudioPlayback = nativeAudioPlaybackSession;
            dialogKeyForCleanup = activeDialogKey;

            mediaCancellation = null;
            captureCancellation = null;
            receiveLoopTask = null;
            // The diagnostics loop only reads state and writes a string; it stops with the
            // cancellation token and is not awaited — nothing downstream depends on its exit.
            diagnosticsLoopTask = null;
            controlLoopTask = null;
            capabilityPublishLoopTask = null;
            negotiationMonitorLoopTask = null;
            // Capture + null the tracked fire-and-forget tasks inside the lock
            // so any concurrent OnFrameArrived / QueueLocalPreviewFrame /
            // profile-adapt path sees a fresh empty slot and doesn't race
            // with our await below. We await the captured locals outside the
            // lock. See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
            trackedFrameTaskForCleanup = activeFrameProcessingTask;
            trackedPreviewTaskForCleanup = localPreviewLoopTask;
            trackedRemoteRenderTaskForCleanup = remoteRenderLoopTask;
            trackedReconfigureTaskForCleanup = captureReconfigureTask;
            activeFrameProcessingTask = null;
            localPreviewLoopTask = null;
            remoteRenderLoopTask = null;
            captureReconfigureTask = null;
            frameReader = null;
            mediaCapture = null;
            screenShareSource = null;
            localScreenShareRequested = false;
            syntheticPatternRequested = false;
            screenShareItem = null;
            sharedMonitor = null;
            nativeSenderSession = null;
            nativeReceiverSession = null;
            nativeAudioCaptureSession = null;
            nativeAudioPlaybackSession = null;
            audioCaptureDiagnostics = "none";
            audioPlaybackDiagnostics = "none";
            Interlocked.Exchange(ref audioPlayoutTargetDelayMs, 0);
            activeMediaSessionId = string.Empty;
            activeDialogKey = null;
            // Wave 2a fix (2026-05-16): capture pin reference under lock and
            // null the field, but defer Dispose() until AFTER send/receive
            // loops have been awaited below. Otherwise a Send/Drain in flight
            // can race: it reads activeDialogKeyPin into a local before the
            // null-store, then deref's pin.Pointer AFTER Dispose() unpinned
            // the byte[] — BEX64 STATUS_STACK_BUFFER_OVERRUN in omni_ffi.dll.
            dialogKeyPinForCleanup = activeDialogKeyPin;
            activeDialogKeyPin = null;
            packetTargetIdForCleanup = activePacketTargetId;
            activePacketTargetId = 0;
            mediaRouterForCleanup = activeMediaRouter;
            activeMediaRouter = null;
            handleForCleanup = nativeHandle;
            activeSelectionSignature = null;
            activeReceiverSelectionSignature = null;
            nativeHandle = IntPtr.Zero;
            captureDiagnostics = "none";
            codecDiagnostics = "none";
            nativeMediaDiagnostics = "none";
            negotiationDiagnostics = "none";
            localCameraActive = false;
            remoteVideoSeen = false;
            SetRtcpFeedbackEnabled(false, "media_stopped");
            Interlocked.Exchange(ref frameSelectionCredit, 0);
            Interlocked.Exchange(ref frameSelectionLastSelectedAtMs, 0);
            nextLocalPreviewAtMs = 0;
            captureStartedAtMs = 0;
            targetCaptureWidth = CallVideoPreferences.DefaultPreferredCaptureWidth;
            targetCaptureHeight = CallVideoPreferences.DefaultPreferredCaptureHeight;
            targetFps = CallVideoPreferences.DefaultTargetFps;
            targetFrameIntervalMs = DefaultTargetFrameIntervalMs;
            targetBitrateBps = DefaultTargetBitrateBps;
            negotiatedCaptureWidth = CallVideoPreferences.DefaultPreferredCaptureWidth;
            negotiatedCaptureHeight = CallVideoPreferences.DefaultPreferredCaptureHeight;
            negotiatedFps = CallVideoPreferences.DefaultTargetFps;
            negotiatedBitrateBps = DefaultTargetBitrateBps;
            lastSenderAbrBitrateBps = 0;
            adaptiveBitrateBps = DefaultTargetBitrateBps;
            adaptiveFps = CallVideoPreferences.DefaultTargetFps;
            adaptiveDiagnostics = "stable";
            captureProfileMaxWidth = 0;
            captureProfileMaxHeight = 0;
            activeCaptureProfileWidth = 0;
            activeCaptureProfileHeight = 0;
            activeCaptureProfileFps = 0;
            Interlocked.Exchange(ref captureReconfigureInFlight, 0);
            Interlocked.Exchange(ref localPreviewLoopScheduled, 0);
            Interlocked.Exchange(ref remoteRenderLoopScheduled, 0);
            ResetMediaCounters();
        }

        ClearPendingLocalPreviewFrame();
        ClearPendingRemoteRenderFrame();
        // Inbound media stops FIRST: the core's unwire returns only when no delivery is in flight,
        // so nothing reaches the router — or a session behind it — while the rest is torn down.
        UnwireInboundMedia(handleForCleanup, dialogKeyPinForCleanup);
        cancellation?.Cancel();
        captureCancel?.Cancel();

        if (reader is not null)
        {
            reader.FrameArrived -= OnFrameArrived;
            try
            {
                await reader.StopAsync().AsTask();
            }
            catch
            {
                // Reader shutdown is best-effort during app close or profile switch.
            }

            reader.Dispose();
        }

        capture?.Dispose();
        screenSource?.Dispose();

        if (receiveTask is not null)
        {
            try
            {
                await receiveTask;
            }
            catch (OperationCanceledException)
            {
            }
            catch
            {
                // Stop must remain reliable even if the receive pump was already failing.
            }
        }

        if (capabilityPublishTask is not null)
        {
            try
            {
                await capabilityPublishTask;
            }
            catch (OperationCanceledException)
            {
            }
            catch
            {
                // Passive negotiation maintenance must not block stopping media.
            }
        }

        if (controlTask is not null)
        {
            try
            {
                await controlTask;
            }
            catch (OperationCanceledException)
            {
            }
            catch
            {
                // RTCP/control pump must not block media shutdown.
            }
        }

        if (negotiationMonitorTask is not null)
        {
            try
            {
                await negotiationMonitorTask;
            }
            catch (OperationCanceledException)
            {
            }
            catch
            {
                // Negotiation monitoring is best-effort during shutdown.
            }
        }

        // Await the previously-untracked fire-and-forget tasks BEFORE
        // disposing native sender/receiver. Without this, an in-flight encode
        // pushes a frame to a disposed IntPtr → AccessViolation. Locals were
        // captured atomically inside the lifecycleSync lock above.
        // See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
        await AwaitTrackedTaskWithGraceAsync(trackedFrameTaskForCleanup);
        await AwaitTrackedTaskWithGraceAsync(trackedPreviewTaskForCleanup);
        await AwaitTrackedTaskWithGraceAsync(trackedRemoteRenderTaskForCleanup);
        await AwaitTrackedTaskWithGraceAsync(trackedReconfigureTaskForCleanup);
        // Same rule as the tasks above, for the presenter thread: it calls into the receiver on
        // every tick, so it has to be stopped and joined before that receiver is disposed.
        // A presenter that does not stop is stuck in the receiver's decoder; RetireReceiverAsync
        // then finds the receiver's gate taken and abandons it rather than destroying it.
        _ = await StopRemoteGpuPresenterAsync();

        await WaitForCaptureFrameInFlightAsync();
        // The receive loop (the router's only other caller) has been awaited above.
        mediaRouterForCleanup?.Dispose();
        currentNativeSender?.Dispose();
        await RetireReceiverAsync(currentNativeReceiver);
        // T3b: per-sender group receiver sessions + tile targets. Safe here — the receive loop
        // (their only feeder) has been awaited above, so nothing recreates a session after Clear.
        List<NativeWebRtcMediaEngine.Session> groupSessionsForCleanup;
        List<NativeWebRtcMediaEngine.AudioPlaybackSession> groupProbesForCleanup;
        List<ulong> groupFeedbackTargetsForCleanup;
        lock (nativeMediaSync)
        {
            groupSessionsForCleanup = new(groupReceiverSessions.Values);
            groupReceiverSessions.Clear();
            groupReceiverPayloadTypes.Clear();
            groupFeedbackTargetsForCleanup = new(groupFeedbackTargetIds);
            groupFeedbackTargetIds.Clear();
            groupProbesForCleanup = new(groupAudioLevelProbes.Values);
            groupAudioLevelProbes.Clear();
            lastReceiverSelection = null;
        }
        lock (groupTileSync)
        {
            groupTiles.Clear();
        }
        foreach (NativeWebRtcMediaEngine.Session groupSession in groupSessionsForCleanup)
        {
            await RetireReceiverAsync(groupSession);
        }
        foreach (NativeWebRtcMediaEngine.AudioPlaybackSession groupProbe in groupProbesForCleanup)
        {
            groupProbe.Dispose();
        }
        foreach (ulong groupFeedbackTarget in groupFeedbackTargetsForCleanup)
        {
            Core.NativeMethods.omni_realtime_media_packet_target_destroy(groupFeedbackTarget);
        }
        currentAudioCapture?.Dispose();
        currentAudioPlayback?.Dispose();
        // Every engine object of this call is gone, so nothing hands packets to the target any
        // more; a straggler (a receiver still being built) would hit a dead ID and be ignored.
        if (packetTargetIdForCleanup != 0)
        {
            Core.NativeMethods.omni_realtime_media_packet_target_destroy(packetTargetIdForCleanup);
        }
        captureCancel?.Dispose();
        cancellation?.Dispose();
        // Wave 2a fix: safe to release the pinned dialogKey buffer now —
        // every send/drain/control loop has drained and every tracked
        // fire-and-forget task has completed above. No one holds the
        // pin's Pointer past this point.
        dialogKeyPinForCleanup?.Dispose();
        if (!string.IsNullOrWhiteSpace(dialogKeyForCleanup))
        {
            try
            {
                rustBridge.ClearRealtimeMediaNegotiation(dialogKeyForCleanup);
            }
            catch
            {
                // Negotiation cleanup is best-effort after the local media pump has stopped.
            }
        }

        await RunOnUiAsync(() =>
        {
            // #5 increment 3b: unbind the panel from the (now-disposed) swapchain and restore the
            // software fallback sink. The receiver session was disposed above, releasing the native
            // renderer's swapchain ref; dropping the panel's ref here frees the swapchain object.
            ResetRemoteGpuRenderer();
            localPreviewBitmap = null;
            remoteVideoBitmap = null;
            localPreviewImage.Source = null;
            remoteVideoImage.Source = null;
            diagnosticsTextBlock.Text = "Media: off";
        });
        if (!string.IsNullOrWhiteSpace(dialogKeyForCleanup))
        {
            // Re-prove the hardware now that nothing is encoding, for the next call.
            NativeWebRtcMediaProbe.RefreshInBackground();
        }

        PublishStateChanged();
    }

    public async ValueTask DisposeAsync()
        => await StopAsync();

    /// <summary>
    /// Пока отправитель не выбрал разрешение сам, оно следует за КАМЕРОЙ, а не за константой.
    ///
    /// Иначе камера с максимумом 424x240 получала бы запрошенные по умолчанию 720p, не могла их
    /// отдать и публиковалась как decode-only — то есть кнопка камеры оказывалась выключена на
    /// ровном месте. Раньше от этого спасал режим Auto; Auto снят вместе с авторазрешением
    /// (2026-08-17), и подстройку под источник надо было унести сюда, а не потерять.
    /// </summary>
    private void AdoptProbeDefaultResolutionUnsafe(CallVideoCaptureProbeResult probe)
    {
        if (resolutionChosenExplicitly || probe.Formats.Count == 0)
        {
            return;
        }

        int maxWidth = 0;
        int maxHeight = 0;
        foreach (CallVideoCaptureFormat format in probe.Formats)
        {
            if ((long)format.Width * format.Height <= (long)maxWidth * maxHeight)
            {
                continue;
            }

            maxWidth = (int)format.Width;
            maxHeight = (int)format.Height;
        }

        selectedResolution = CallVideoPreferences.DefaultResolution(
            CallVideoPreferences.BuildResolutionOptions(maxWidth, maxHeight));
    }

    /// <summary>
    /// Что отправитель может выбрать для КАМЕРЫ на этой машине. Максимум берётся из пробы
    /// захвата — то есть из того, что камера реально умеет, а не из наших представлений о ней.
    /// Пустой список означает «камеры нет»: кнопка и так недоступна, меню показывать нечего.
    /// </summary>
    internal async Task<IReadOnlyList<CallVideoResolutionOption>> GetCameraResolutionOptionsAsync(
        CancellationToken token = default)
    {
        CallVideoCaptureProbeResult probe = await EnsureCaptureProbeAsync(token);
        if (probe.Formats.Count == 0)
        {
            return [];
        }

        int maxWidth = 0;
        int maxHeight = 0;
        foreach (CallVideoCaptureFormat format in probe.Formats)
        {
            // Сравниваем по площади: у камеры бывают профили и 1280x720, и 1600x1200, и «шире»
            // не значит «больше пикселей».
            if ((long)format.Width * format.Height <= (long)maxWidth * maxHeight)
            {
                continue;
            }

            maxWidth = (int)format.Width;
            maxHeight = (int)format.Height;
        }

        return CallVideoPreferences.BuildResolutionOptions(maxWidth, maxHeight);
    }

    internal CallVideoBitratePreference BitratePreference
    {
        get
        {
            lock (lifecycleSync)
            {
                return bitratePreference;
            }
        }
    }

    /// <summary>
    /// Settings → Calls → «Битрейт». Goes out in our passport, so a running call is renegotiated
    /// and both directions take the new ceiling — the peer's sender too.
    /// </summary>
    internal async Task ApplyBitratePreferenceAsync(CallVideoBitratePreference next)
    {
        string? dialogKey;
        CancellationToken token;
        lock (lifecycleSync)
        {
            if (bitratePreference == next)
            {
                return;
            }

            bitratePreference = next;
            activeSelectionSignature = null;
            nativeMediaDiagnostics = "pending_renegotiation";
            dialogKey = activeDialogKey;
            token = mediaCancellation?.Token ?? CancellationToken.None;
        }

        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return;
        }

        CallVideoCaptureProbeResult captureProbe = await EnsureCaptureProbeAsync(CancellationToken.None);
        await RepublishLocalCapabilitiesAsync(dialogKey, token, localSenderActive: true, captureProbe);
    }

    internal CallVideoPreferenceSnapshot GetPreferences()
    {
        lock (lifecycleSync)
        {
            return new CallVideoPreferenceSnapshot(codecPreference, selectedResolution, frameRatePreference);
        }
    }

    internal async Task ApplyPreferencesAsync(
        CallVideoCodecPreference nextCodecPreference,
        CallVideoResolutionOption nextResolution,
        CallVideoFrameRatePreference nextFrameRatePreference)
    {
        CallVideoCaptureProbeResult captureProbe = await EnsureCaptureProbeAsync(CancellationToken.None);
        // Reject a profile this camera cannot serve, HERE, where the user picked it and
        // can see the message. Building the capabilities no longer throws for it (that
        // used to take the whole call's audio down as a side effect), so the check has
        // to be explicit — otherwise picking 1080p on a 720p webcam would silently
        // degrade the call to audio with no explanation.
        // With no camera at all there is nothing to reject — the user is choosing a
        // preference for a camera they may plug in later, not asking this call for video.
        if (captureProbe.Formats.Count > 0 &&
            !CallVideoPreferences.CanCaptureVideo(
                nextResolution,
                nextFrameRatePreference,
                captureProbe))
        {
            throw new InvalidOperationException(
                $"Камера не поддерживает выбранный профиль: {CallVideoPreferences.FormatPreferences(new CallVideoPreferenceSnapshot(nextCodecPreference, nextResolution, nextFrameRatePreference))}; {captureProbe.BuildShortDiagnostics()}");
        }

        string? dialogKey;
        CancellationToken token;
        bool localSenderActive;
        lock (lifecycleSync)
        {
            if (codecPreference == nextCodecPreference &&
                selectedResolution == nextResolution &&
                frameRatePreference == nextFrameRatePreference)
            {
                return;
            }

            codecPreference = nextCodecPreference;
            selectedResolution = nextResolution;
            resolutionChosenExplicitly = true;
            frameRatePreference = nextFrameRatePreference;
            activeSelectionSignature = null;
            negotiationDiagnostics = CallVideoPreferences.FormatPreferences(
                new CallVideoPreferenceSnapshot(codecPreference, selectedResolution, frameRatePreference));
            nativeMediaDiagnostics = "pending_renegotiation";
            dialogKey = activeDialogKey;
            token = mediaCancellation?.Token ?? CancellationToken.None;
            // Audio-first: reconfigure the local sender (audio always, camera optional) for ANY
            // active call on this dialog, not only when the camera is on — same fix as the
            // negotiation monitor. Past the dialogKey guard below the call is active, so the
            // updated preferences reach the audio sender instead of waiting for the camera.
            localSenderActive = !string.IsNullOrWhiteSpace(activeDialogKey);
        }

        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            PublishDiagnostics();
            return;
        }

        // ЗДЕСЬ СТОЯЛ `ClearRealtimeMediaNegotiation(dialogKey)`, И ОН СНОСИЛ ЧУЖОЕ.
        //
        // Менялись НАШИ настройки (разрешение, кодек, частота), а очистка стирает всю запись
        // согласования целиком — вместе с возможностями СОБЕСЕДНИКА и выбранной конфигурацией.
        // Дальше на входящие датаграммы закрывается ворота `dialog_has_active_media_negotiation`
        // (она смотрит ровно на наличие выбора), и всё, что шлёт пир, молча выбрасывается.
        //
        // Живой звонок 2026-08-30: собеседник согласован на 790957, человек нажал камеру и выбрал
        // разрешение — на 795167 запись уже `waiting_for_remote_capabilities remote_caps=none`, а
        // с 795175 идёт `media/datagram-drop reason=no-active-negotiation`, 1500+ штук до конца
        // звонка. Ровно столько датаграмм в это время отправил ПК: его голос не потерялся в сети,
        // его выбросили у нас на пороге.
        //
        // Публикация новых своих возможностей и так пересобирает выбор из запомненных чужих
        // (store_local_realtime_media_capabilities -> refresh_realtime_media_selection), то есть
        // делает ровно то, ради чего очистку сюда и поставили. Сходимость сторон при этом
        // прежняя, как в начале звонка: предлагающий (lex-меньший account_id) увидит наши новые
        // возможности и пришлёт выбор — см. docs/decisions/media-negotiation-role.md.
        await RepublishLocalCapabilitiesAsync(dialogKey, token, localSenderActive, captureProbe);
    }

    /// Our changed passport out, the selection rebuilt from it, and — in a call — our receiver and
    /// sender moved onto the new selection.
    private async Task RepublishLocalCapabilitiesAsync(
        string dialogKey,
        CancellationToken token,
        bool localSenderActive,
        CallVideoCaptureProbeResult captureProbe)
    {
        RealtimeMediaNegotiationSnapshot snapshot = TryPublishLocalCapabilities(
            dialogKey,
            throwOnFailure: true,
            captureProbe);
        if (!localSenderActive)
        {
            PublishDiagnostics();
            return;
        }

        RealtimeMediaSelection selection = snapshot.LocalSelection ?? await EnsureNegotiatedMediaAsync(dialogKey, token);
        AdoptRepublishedSelection(selection);
        await ReconfigureLocalSenderForSelectionAsync(dialogKey, selection, token);
    }

    /// <summary>
    /// Our own new passport mid-call (a codec ceiling, a share that started) can change what we
    /// RECEIVE as well — the selection covers both directions. The sender is rebuilt by the caller;
    /// the decoder here, as the negotiation monitor does for a selection that arrives from the peer.
    /// Without it the receiver stayed on the old codec and dropped every packet of the new one
    /// (live 2026-10-13: ceiling H.264 on the laptop, 942 packets in, 0 frames).
    /// </summary>
    private void AdoptRepublishedSelection(RealtimeMediaSelection selection)
    {
        activeSendStream = selection.SendStream;
        activeReceiveStream = selection.ReceiveStream;
        try
        {
            EnsureNativeReceiverForSelection(selection);
        }
        catch (Exception ex)
        {
            RecordError($"Native video receiver setup failed: {FormatExceptionForDiagnostics(ex)}");
        }
    }

    private async Task MaintainPublishedLocalCapabilitiesAsync(string dialogKey, CancellationToken token)
    {
        // Backs off. The peer's capabilities arrive when the peer is ready, and hammering the
        // publish every two seconds neither hurries that nor reports anything new -- it just keeps
        // the machine awake and drowns the trace. Retry quickly at first, because the common case is
        // "the other side is a second behind", then stretch out.
        int delayMs = PassiveCapabilityPublishIntervalMs;
        while (!token.IsCancellationRequested)
        {
            RealtimeMediaNegotiationSnapshot snapshot = await PublishLocalCapabilitiesAsync(
                dialogKey,
                throwOnFailure: false,
                token);
            if (snapshot.Selected is not null ||
                string.Equals(snapshot.Status, "negotiated", StringComparison.OrdinalIgnoreCase))
            {
                return;
            }

            await Task.Delay(delayMs, token);
            delayMs = Math.Min(delayMs * 2, PassiveCapabilityPublishMaxIntervalMs);
        }
    }

    /// <summary>
    /// Group call: the core re-picks the one codec we send everybody whenever someone joins, leaves or
    /// says what they decode (per-pair-video-codec-selection.md §9, phase 4a). Follow its selection —
    /// and when our own codec breaks, publish the passport again without it.
    /// </summary>
    private async Task MonitorGroupMediaAsync(string dialogKey, CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            await Task.Delay(NegotiationMonitorIntervalMs, token);
            try
            {
                RealtimeMediaSelection? selection;
                if (DetectBrokenVideoCodec())
                {
                    lock (lifecycleSync)
                    {
                        activeSelectionSignature = null;
                    }

                    selection = await EnsureGroupSelfNegotiatedMediaAsync(dialogKey, token);
                }
                else
                {
                    selection = (await Task.Run(() => rustBridge.GetRealtimeMediaNegotiation(dialogKey), token)).LocalSelection;
                }

                bool reconfigure;
                lock (lifecycleSync)
                {
                    reconfigure = selection is not null &&
                        callMediaRequested &&
                        string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal) &&
                        !string.Equals(activeSelectionSignature, BuildSelectionSignature(selection), StringComparison.Ordinal);
                }

                if (reconfigure && selection is not null)
                {
                    Core.AppTrace.Write("media/group-codec", $"dialog={dialogKey} send={selection.SendStream?.Codec ?? "none"}: sender follows");
                    await ReconfigureLocalSenderForSelectionAsync(dialogKey, selection, token, skipIfSelectionAlreadyLive: true);
                }
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                RecordError($"Group media monitor failed: {FormatExceptionForDiagnostics(ex)}");
            }
        }
    }

    private async Task MonitorNegotiatedMediaAsync(string dialogKey, CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            await Task.Delay(NegotiationMonitorIntervalMs, token);
            if (DetectBrokenVideoCodec())
            {
                try
                {
                    await ReannounceAfterCodecFailureAsync(dialogKey, token);
                }
                catch (OperationCanceledException)
                {
                    throw;
                }
                catch (Exception ex)
                {
                    RecordError($"Re-announcing after a codec failure failed: {FormatExceptionForDiagnostics(ex)}");
                }
            }

            RealtimeMediaNegotiationSnapshot snapshot;
            try
            {
                snapshot = await Task.Run(() => rustBridge.GetRealtimeMediaNegotiation(dialogKey), token);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                RecordError($"Video negotiation monitor failed: {FormatExceptionForDiagnostics(ex)}");
                continue;
            }

            negotiationDiagnostics = BuildNegotiationDiagnostics(snapshot);
            if (snapshot.LocalSelection is not { } selection)
            {
                PublishDiagnostics();
                continue;
            }

            string selectionSignature = BuildSelectionSignature(selection);
            bool localSenderActive;
            bool shouldReconfigure;
            lock (lifecycleSync)
            {
                // Audio-first: the local sender (audio always, camera optional) must
                // reconfigure for ANY active call on this dialog, not only when the camera is
                // on — the old `localCameraActive` gate skipped audio-only calls and left them
                // half-duplex. См. docs/notes/calls-full-duplex-CONTINUE.md.
                //
                // This is also the backstop when a selection lands after StartAsync gave up
                // waiting for it. That used to be the NORMAL path for the caller, whose media
                // started while the call was still ringing and whose 5-second negotiation wait
                // therefore expired whenever the peer took longer than that to pick up. Media
                // now starts on `active`, so the peer's capabilities are already published and
                // the wait resolves at once; this stays as the recovery path.
                //
                // …but "active call", not "open chat". Without the callMediaRequested
                // half, this started the microphone for any watched dialog — see the
                // field's own comment.
                localSenderActive =
                    callMediaRequested &&
                    string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal);
                shouldReconfigure =
                    localSenderActive &&
                    !string.Equals(activeSelectionSignature, selectionSignature, StringComparison.Ordinal);
            }

            try
            {
                if (!localSenderActive)
                {
                    ApplyNegotiatedSelection(selection);
                }

                EnsureNativeReceiverForSelection(selection);
            }
            catch (Exception ex)
            {
                RecordError($"Native video receiver setup failed: {FormatExceptionForDiagnostics(ex)}");
                continue;
            }

            if (!shouldReconfigure)
            {
                PublishDiagnostics();
                continue;
            }

            try
            {
                await ReconfigureLocalSenderForSelectionAsync(
                    dialogKey,
                    selection,
                    token,
                    skipIfSelectionAlreadyLive: true);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                RecordError($"Video reconfiguration failed: {FormatExceptionForDiagnostics(ex)}");
            }
        }
    }

    private async Task<RealtimeMediaSelection> EnsureNegotiatedMediaAsync(
        string dialogKey,
        CancellationToken token)
    {
        long publishStartedAtMs = Environment.TickCount64;
        RealtimeMediaNegotiationSnapshot snapshot = await PublishLocalCapabilitiesAsync(
            dialogKey,
            throwOnFailure: true,
            token);
        long publishDoneAtMs = Environment.TickCount64;
        Core.AppTrace.Write(
            "media/start-stage",
            $"publish_capabilities={publishDoneAtMs - publishStartedAtMs}ms selected={snapshot.Selected is not null}");
        if (snapshot.LocalSelection is { } published)
        {
            return published;
        }

        long deadlineMs = Environment.TickCount64 + MediaNegotiationTimeoutMs;
        while (Environment.TickCount64 < deadlineMs)
        {
            token.ThrowIfCancellationRequested();
            await Task.Delay(100, token);
            snapshot = await Task.Run(() => rustBridge.GetRealtimeMediaNegotiation(dialogKey), token);
            if (snapshot.LocalSelection is { } selected)
            {
                Core.AppTrace.Write(
                    "media/start-stage",
                    $"await_peer_capabilities={Environment.TickCount64 - publishDoneAtMs}ms");
                return selected;
            }

            // There is no "unsupported" outcome any more: once the peer's capabilities
            // arrive there is always a session, audio-only at worst, and the snapshot's
            // Diagnostic says why it carries no video. The only way out of this loop is
            // the peer never announcing itself at all.
        }

        throw new InvalidOperationException("Media negotiation timed out while waiting for remote capabilities.");
    }

    private async Task<CallVideoCaptureProbeResult> EnsureCaptureProbeAsync(CancellationToken token)
    {
        CallVideoCaptureProbeResult? cached;
        lock (lifecycleSync)
        {
            cached = captureProbeResult;
        }

        if (cached is not null)
        {
            return cached;
        }

        await captureProbeSync.WaitAsync(token);
        try
        {
            lock (lifecycleSync)
            {
                cached = captureProbeResult;
            }

            if (cached is not null)
            {
                return cached;
            }

            long probeStartedAtMs = Environment.TickCount64;
            CallVideoCaptureProbeResult probed = await CallVideoCaptureProbe.ProbeAsync(token);
            Core.AppTrace.Write(
                "media/start-stage",
                $"capture_probe={Environment.TickCount64 - probeStartedAtMs}ms formats={probed.Formats.Count}");
            lock (lifecycleSync)
            {
                captureProbeResult = probed;
                captureProbeCompletedAtMs = Environment.TickCount64;
                captureProbeDiagnostics = probed.BuildShortDiagnostics();
                AdoptProbeDefaultResolutionUnsafe(probed);
            }

            PublishDiagnostics(force: true);
            return probed;
        }
        finally
        {
            captureProbeSync.Release();
        }
    }

    private async Task<RealtimeMediaNegotiationSnapshot> PublishLocalCapabilitiesAsync(
        string dialogKey,
        bool throwOnFailure,
        CancellationToken token)
    {
        long publishEnteredAtMs = Environment.TickCount64;
        try
        {
            CallVideoCaptureProbeResult captureProbe = CameraNotNeeded
                ? CallVideoCaptureProbeResult.Unavailable("not_needed")
                : await EnsureCaptureProbeAsync(token);
            long captureProbeReadyAtMs = Environment.TickCount64;
            // The gate everything downstream hangs on: no capabilities means no negotiation, no
            // selection, no audio, no video and a camera button that never enables — and the catch
            // below turns any failure here into a snapshot nobody reads. Live standalone test
            // 2026-08-11: calls connected and were silent with no clue anywhere.
            Core.AppTrace.Write(
                "media/capture-probe",
                $"dialog={dialogKey} formats={captureProbe.Formats.Count} {captureProbe.Diagnostics}");
            long tracedAtMs = Environment.TickCount64;
            RealtimeMediaNegotiationSnapshot published =
                TryPublishLocalCapabilities(dialogKey, throwOnFailure, captureProbe);
            Core.AppTrace.Write(
                "media/start-stage",
                $"await_capture_probe={captureProbeReadyAtMs - publishEnteredAtMs}ms " +
                $"trace_probe={tracedAtMs - captureProbeReadyAtMs}ms " +
                $"build_and_publish={Environment.TickCount64 - tracedAtMs}ms");
            return published;
        }
        catch (Exception ex) when (!throwOnFailure)
        {
            Core.AppTrace.WriteFailure("media/capture-probe", $"dialog={dialogKey} probe/publish", ex);
            return new RealtimeMediaNegotiationSnapshot(
                dialogKey,
                "idle",
                null,
                null,
                null,
                "local capabilities were not published",
                DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
        }
    }

    private RealtimeMediaNegotiationSnapshot TryPublishLocalCapabilities(
        string dialogKey,
        bool throwOnFailure,
        CallVideoCaptureProbeResult captureProbe)
    {
        try
        {
            RealtimeMediaCapabilities capabilities = BuildLocalCapabilities(dialogKey, captureProbe);
            lastPublishedPassport = $"content:{capabilities.Content} cpu:{capabilities.CpuLoad} power:{capabilities.Power}" +
                (capabilities.CodecCeiling is null ? string.Empty : $" ceiling:{capabilities.CodecCeiling}") +
                (capabilities.BitrateLevel == "auto" ? string.Empty : $" bitrate:{capabilities.BitrateLevel}");
            RealtimeMediaNegotiationSnapshot snapshot = rustBridge.PublishRealtimeMediaCapabilities(capabilities);
            negotiationDiagnostics = BuildNegotiationDiagnostics(snapshot);
            Core.AppTrace.Write(
                "media/capabilities",
                $"dialog={dialogKey} published codecs={capabilities.Codecs?.Count ?? 0} " +
                $"-> status={snapshot.Status} selected={(snapshot.Selected is null ? "none" : "yes")} " +
                $"remote_caps={(snapshot.RemoteCapabilities is null ? "none" : "yes")} note={snapshot.Diagnostic}");
            PublishDiagnostics();
            return snapshot;
        }
        catch (Exception ex) when (!throwOnFailure)
        {
            Core.AppTrace.WriteFailure("media/capabilities", $"dialog={dialogKey} publish", ex);
            return new RealtimeMediaNegotiationSnapshot(
                dialogKey,
                "idle",
                null,
                null,
                null,
                "local capabilities were not published",
                DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
        }
    }

    private RealtimeMediaCapabilities BuildLocalCapabilities(
        string dialogKey,
        CallVideoCaptureProbeResult captureProbe)
    {
        CallVideoCodecPreference currentCodecPreference;
        CallVideoResolutionOption currentQualityPreference;
        CallVideoFrameRatePreference currentFrameRatePreference;
        CallVideoBitratePreference currentBitratePreference;
        lock (lifecycleSync)
        {
            currentCodecPreference = codecPreference;
            currentQualityPreference = selectedResolution;
            currentFrameRatePreference = frameRatePreference;
            currentBitratePreference = bitratePreference;
        }

        return CallVideoPreferences.BuildCapabilities(
            dialogKey,
            currentCodecPreference,
            currentQualityPreference,
            currentFrameRatePreference,
            captureProbe,
            BuildNativeRealtimeCodecs(),
            canShareScreen: IsScreenShareSupported,
            onBattery: IsOnBattery(),
            cpuLoad: ProcessorLoad.Read(),
            screenContent: localScreenShareRequested,
            screenResolution: screenShareResolution,
            bitrate: currentBitratePreference);
    }

    /// From a battery right now (the passport's power line: it makes software decoding expensive).
    private static bool IsOnBattery()
    {
        try
        {
            return global::Windows.System.Power.PowerManager.BatteryStatus != global::Windows.System.Power.BatteryStatus.NotPresent &&
                   global::Windows.System.Power.PowerManager.PowerSupplyStatus == global::Windows.System.Power.PowerSupplyStatus.NotPresent;
        }
        catch (Exception)
        {
            // No power information (a VM, a policy): count it as mains, the neutral default.
            return false;
        }
    }

    private IReadOnlyList<CallVideoCodecSupport> BuildNativeRealtimeCodecs()
    {
        // The latest proof (refreshed at startup and after each call). Blocks only in a process
        // whose first proof is still running — then this stage shows up in the trace.
        long probeStartedAtMs = Environment.TickCount64;
        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        long probeMs = Environment.TickCount64 - probeStartedAtMs;
        if (probeMs >= 20)
        {
            Core.AppTrace.Write("media/start-stage", $"native_probe={probeMs}ms hw={string.Join(',', probe.HardwareCodecs)}");
        }

        if (!probe.NativeSessionScaffold)
        {
            return [];
        }

        IReadOnlyList<CallVideoCodecSupport> codecs = CallVideoCodecFactory.RestrictToDevices(
            CallVideoCodecFactory.GetSupportedCodecs(probe),
            encodeSoftwareOnly: ResolveEncodeDevice().UseSoftware,
            decodeSoftwareOnly: ResolveDecodeDevice().UseSoftware);
        lock (lifecycleSync)
        {
            return codecHealth.Apply(codecs);
        }
    }

    /// <summary>
    /// Once a second, from the negotiation monitor: did the codec of either direction break?
    /// True when one just did — the caller announces the passport again without it
    /// (docs/decisions/per-pair-video-codec-selection.md §6).
    /// </summary>
    private bool DetectBrokenVideoCodec()
    {
        // First: a receiver stuck in its decoder is not read at all, it is given up.
        if (DetectHungVideoDecoder(out bool struckHungDecoder))
        {
            return struckHungDecoder;
        }

        NativeWebRtcMediaEngine.Session? sender;
        NativeWebRtcMediaEngine.Session? receiver;
        lock (nativeMediaSync)
        {
            sender = nativeSenderSession;
            receiver = nativeReceiverSession;
        }

        CallVideoCodecKind? sendCodec = CallVideoCodecFactory.ParseCodec(activeSendStream?.Codec);
        CallVideoCodecKind? receiveCodec = CallVideoCodecFactory.ParseCodec(activeReceiveStream?.Codec);
        // Frames the encoder owed us: what came in, less what the bandwidth held back (a congested
        // network pauses video and that is not the encoder's fault). Frames dropped because the
        // encoder was still busy DO count — a stuck encoder is exactly a busy one.
        uint[] encoder = sender is null || sendCodec is null
            ? [0, 0, 0, 0]
            : ReadNativeCounters(sender, "mft_encode_fallback_requests", "encoder_incoming_frames", "encoder_dropped_by_rate", "encoder_encoded_frames");
        uint fallbackRequests = encoder[0];
        uint framesIn = encoder[1] - Math.Min(encoder[1], encoder[2]);
        uint framesOut = encoder[3];
        (uint decoded, uint failures) = receiver is null || receiveCodec is null
            ? (0u, 0u)
            : (ReadNativeCounter(receiver, "frames_decoded") + ReadNativeCounter(receiver, "hw_frames_decoded"),
               ReadNativeCounter(receiver, "decode_failures"));
        bool encoderBroke;
        bool decoderBroke;
        lock (lifecycleSync)
        {
            encoderBroke = sendCodec is { } send &&
                (codecHealth.ObserveEncoder(send, fallbackRequests) | codecHealth.ObserveEncoderProgress(send, framesIn, framesOut));
            decoderBroke = receiveCodec is { } receive && codecHealth.ObserveDecoder(receive, decoded, failures);
        }

        if (encoderBroke)
        {
            RecordError($"Video encoder for {CallVideoCodecFactory.FormatCodec(sendCodec!.Value)} failed mid-call ({framesIn} frames in, {framesOut} out, {fallbackRequests} fallback requests); announcing the passport without it.");
        }

        if (decoderBroke)
        {
            RecordError($"Video decoder for {CallVideoCodecFactory.FormatCodec(receiveCodec!.Value)} failed {failures} frames without one decoded; announcing the passport without it.");
        }

        return encoderBroke || decoderBroke;
    }

    private static uint[] ReadNativeCounters(NativeWebRtcMediaEngine.Session session, params string[] names)
    {
        try
        {
            using JsonDocument document = JsonDocument.Parse(session.GetDiagnosticsJson());
            return [.. names.Select(name => ReadNativeDiagnosticsUInt32(document.RootElement, name))];
        }
        catch (Exception)
        {
            return new uint[names.Length]; // a session torn down mid-read: nothing to judge
        }
    }

    private static uint ReadNativeCounter(NativeWebRtcMediaEngine.Session session, string name)
    {
        try
        {
            using JsonDocument document = JsonDocument.Parse(session.GetDiagnosticsJson());
            return ReadNativeDiagnosticsUInt32(document.RootElement, name);
        }
        catch (Exception)
        {
            return 0; // a session torn down mid-read: nothing to judge
        }
    }

    /// <summary>
    /// Test channel: what <see cref="DetectBrokenVideoCodec"/> does when a driver drops the session
    /// of the codec running in one direction. A real driver failure cannot be ordered, so this is
    /// the live check of the half after the detection: the new passport and the switch.
    /// </summary>
    internal async Task<string> SimulateVideoCodecFailureAsync(bool encoder)
    {
        string? dialogKey;
        CancellationToken token;
        CallVideoCodecKind? codec;
        bool struck;
        lock (lifecycleSync)
        {
            dialogKey = activeDialogKey;
            token = mediaCancellation?.Token ?? CancellationToken.None;
            codec = CallVideoCodecFactory.ParseCodec(encoder ? activeSendStream?.Codec : activeReceiveStream?.Codec);
            struck = codec is { } kind &&
                     (encoder ? codecHealth.ObserveEncoder(kind, fallbackRequests: 1) : codecHealth.StrikeDecoder(kind));
        }

        if (string.IsNullOrWhiteSpace(dialogKey) || !struck)
        {
            return $"not struck: dialog={dialogKey ?? "none"} codec={(codec is null ? "none" : CallVideoCodecFactory.FormatCodec(codec.Value))} (H.264 is never struck)";
        }

        RecordError($"Simulated {(encoder ? "encoder" : "decoder")} failure of {CallVideoCodecFactory.FormatCodec(codec!.Value)}; announcing the passport without it.");
        await ReannounceAfterCodecFailureAsync(dialogKey, token);
        return $"struck {(encoder ? "encode" : "decode")} {CallVideoCodecFactory.FormatCodec(codec.Value)}";
    }

    /// <summary>
    /// A new passport mid-call, as for a changed codec ceiling: the selection is recomputed on
    /// both sides from it, and both directions are rebuilt for what comes back.
    /// </summary>
    private async Task ReannounceAfterCodecFailureAsync(string dialogKey, CancellationToken token)
    {
        lock (lifecycleSync)
        {
            activeSelectionSignature = null;
        }

        RealtimeMediaNegotiationSnapshot snapshot = await PublishLocalCapabilitiesAsync(
            dialogKey,
            throwOnFailure: false,
            token);
        if (snapshot.LocalSelection is not { } selection)
        {
            return; // the monitor picks the peer's answer up when it lands
        }

        AdoptRepublishedSelection(selection);
        await ReconfigureLocalSenderForSelectionAsync(dialogKey, selection, token);
    }

    /// One receiver session from a committed selection — shared by the 1:1 single receiver and
    /// the per-sender group tile sessions (T3b), so their decode configs can never diverge.
    private NativeWebRtcMediaEngine.Session CreateReceiverSessionForSelection(
        RealtimeVideoSelection video,
        RealtimeVideoStream stream,
        out string diagnostics)
    {
        CallVideoCodecKind codecKind = ParseNegotiatedCodec(stream.Codec);
        ResolvedVideoDevice decodeDevice = ResolveDecodeDevice();
        // The decoder takes the format of the direction it decodes, which is the sender's.
        return CreateNativeSession(
            codecKind,
            ClampUIntToInt(stream.Width, 2, CallVideoPreferences.MaxVideoSide),
            ClampUIntToInt(stream.Height, 2, CallVideoPreferences.MaxVideoSide),
            ClampUIntToInt(stream.Fps, 1, 60),
            ClampUIntToInt(stream.TargetBitrateBps, 256_000, 20_000_000),
            ClampUIntToInt(stream.Width, 2, CallVideoPreferences.MaxVideoSide),
            ClampUIntToInt(stream.Height, 2, CallVideoPreferences.MaxVideoSide),
            ClampUIntToInt(stream.Fps, 1, 60),
            ClampUIntToInt(stream.TargetBitrateBps, 256_000, 20_000_000),
            "receiver",
            out diagnostics,
            decodeVendorId: decodeDevice.VendorId,
            decodeSoftware: decodeDevice.UseSoftware,
            maxDatagramBytes: negotiatedMaxDatagramBytes);
    }

    /// <summary>
    /// Bring up the decoder + audio playback for a committed selection.
    ///
    /// Decide under the lock, BUILD outside it, swap back under it, dispose after. Creating a
    /// decoder session measures ~875 ms and creating the WASAPI render endpoint is not free
    /// either, while <c>nativeMediaSync</c> is the lock taken by every encode, every inbound
    /// packet and every render tick — building inside it froze the entire media path for that
    /// long at each (re)negotiation, and the freeze landed exactly on the answer path. Same
    /// grab-under-lock / work-outside shape the task lifecycle already uses; see
    /// docs/decisions/csharp-call-task-lifecycle.md and docs/bugs/slow-media-start.md.
    /// </summary>
    private void EnsureNativeReceiverForSelection(RealtimeMediaSelection selection)
    {
        // The FORMAT, not the bitrates: a decoder decodes whatever rate arrives, and rebuilding it
        // for a new ceiling cost ~0.9 s plus a keyframe (Settings → «Битрейт», live 2026-10-18).
        string selectionSignature = BuildFormatSignature(selection);
        bool buildPlayback;
        NativeWebRtcMediaEngine.Session? staleReceiver = null;

        lock (nativeMediaSync)
        {
            // T3b: remember the committed selection so receiver sessions — the 1:1 one below and
            // the per-sender GROUP ones — can be created from it when a sender's first packet
            // arrives. Needed on every path, so it is set before anything else.
            lastReceiverSelection = selection;
            buildPlayback = nativeAudioPlaybackSession is null;

            if (!string.Equals(activeReceiverSelectionSignature, selectionSignature, StringComparison.Ordinal))
            {
                // The agreed format changed: whatever decoder exists decodes the old one. Drop it
                // and let the next inbound video packet build a matching one.
                staleReceiver = nativeReceiverSession;
                nativeReceiverSession = null;
                activeMediaRouter?.SetReceiver(null);
                activeReceiverSelectionSignature = selectionSignature;
            }

            if (selection.Video is null)
            {
                nativeMediaDiagnostics = "native_receiver=audio_only";
            }
            else if (nativeReceiverSession is null)
            {
                nativeMediaDiagnostics = "native_receiver=awaiting_video";
            }
        }

        // Not here and not now: this may be the UI thread, and a presenter may still be inside it.
        RetireReceiver(staleReceiver);

        if (!buildPlayback)
        {
            return;
        }

        // Playback comes up EAGERLY — it is what makes the call audible, and the peer's audio
        // starts the moment they answer. Soft failure: a call without a usable render endpoint
        // still proceeds and the diagnostic reads `audio_recv=unavailable`.
        _ = NativeWebRtcMediaEngine.TryCreateAudioPlayback(
            out NativeWebRtcMediaEngine.AudioPlaybackSession? nextPlayback,
            out string playbackDiagnostics,
            deviceId: PreferredSpeakerId);

        NativeWebRtcMediaEngine.AudioPlaybackSession? discardedPlayback = null;
        lock (nativeMediaSync)
        {
            if (nextPlayback is null)
            {
                audioPlaybackDiagnostics = $"audio_recv=unavailable: {playbackDiagnostics}";
            }
            else if (nativeAudioPlaybackSession is null)
            {
                nativeAudioPlaybackSession = nextPlayback;
                activeMediaRouter?.SetAudioPlayback(nextPlayback);
                audioPlaybackDiagnostics = "audio_recv=ready";
            }
            else
            {
                discardedPlayback = nextPlayback;
            }
        }

        discardedPlayback?.Dispose();
    }

    /// <summary>
    /// The peer just sent video and there is no decoder yet — build one, off the receive thread.
    ///
    /// Building it costs ~750 ms and it used to happen at call start, where it sat between "the
    /// call was answered" and "the peer can hear us" — for a decoder that on an audio-first call
    /// has nothing to decode until the other side chooses to turn a camera on. So it is built
    /// when video actually shows up. The price is the handful of packets that arrive while it is
    /// being built: they are dropped, and the picture starts at the next keyframe — which this
    /// decoder asks the peer for as soon as it sees its first packet (kReasonDecoderStarted →
    /// PLI). It used to be covered by the encoder's IDR every two seconds; that periodic IDR is
    /// gone, so the request is now explicit. See docs/decisions/keyframes-on-demand.md.
    ///
    /// Group calls have always worked this way — per-sender sessions built on a sender's first
    /// packet — so this is the 1:1 path catching up with them.
    /// </summary>
    private void EnsureNativeVideoReceiverForInboundVideo()
    {
        RealtimeMediaSelection? selection;
        string selectionSignature;
        lock (nativeMediaSync)
        {
            if (nativeReceiverSession is not null || activeGroupMode)
            {
                return;
            }

            selection = lastReceiverSelection;
            selectionSignature = activeReceiverSelectionSignature ?? string.Empty;
        }

        // No direction towards us (the peer cannot send, or sends nothing we decode): video
        // packets here would be stray; there is no decoder to build.
        if (selection?.Video is null || selection.ReceiveStream is not RealtimeVideoStream receiveStream)
        {
            return;
        }

        if (Interlocked.Exchange(ref nativeVideoReceiverBuildRunning, 1) == 1)
        {
            return;
        }

        _ = Task.Run(() =>
        {
            NativeWebRtcMediaEngine.Session? built = null;
            try
            {
                built = CreateReceiverSessionForSelection(selection.Video, receiveStream, out string diagnostics);
                NativeWebRtcMediaEngine.Session? discarded = null;
                lock (nativeMediaSync)
                {
                    // Anything may have moved while this was building: a new format agreed, the
                    // call torn down, or another builder finishing first. Install only into the
                    // exact state this was built for.
                    if (nativeReceiverSession is null &&
                        string.Equals(activeReceiverSelectionSignature, selectionSignature, StringComparison.Ordinal))
                    {
                        // Its RTCP (delivery reports, NACK, keyframe requests) goes out the same
                        // way the sender's RTP does.
                        AttachPacketSink(built);
                        nativeReceiverSession = built;
                        activeMediaRouter?.SetReceiver(built);
                        nativeMediaDiagnostics = $"native_receiver=ready; {diagnostics}";
                        built = null;
                    }
                    else
                    {
                        discarded = built;
                        built = null;
                    }
                }

                discarded?.Dispose();
            }
            catch (Exception ex)
            {
                built?.Dispose();
                RecordError($"Native video receiver setup failed: {FormatExceptionForDiagnostics(ex)}");
            }
            finally
            {
                Interlocked.Exchange(ref nativeVideoReceiverBuildRunning, 0);
            }
        });
    }

    private static NativeWebRtcMediaEngine.Session CreateNativeSession(
        CallVideoCodecKind codecKind,
        int width,
        int height,
        int fps,
        int bitrateBps,
        int negotiatedWidth,
        int negotiatedHeight,
        int negotiatedFrameRate,
        int negotiatedBitrate,
        string role,
        out string diagnostics,
        uint encodeVendorId = 0,
        uint decodeVendorId = 0,
        bool decodeSoftware = false,
        bool screenContent = false,
        int maxDatagramBytes = 0)
    {
        if (!NativeWebRtcMediaEngine.TryCreateSession(
                codecKind,
                width,
                height,
                fps,
                bitrateBps,
                out NativeWebRtcMediaEngine.Session? session,
                out diagnostics,
                negotiatedWidth,
                negotiatedHeight,
                negotiatedFrameRate,
                negotiatedBitrate,
                encodeVendorId,
                decodeVendorId,
                decodeSoftware,
                screenContent,
                maxDatagramBytes) ||
            session is null)
        {
            throw new InvalidOperationException($"Native WebRTC {FormatCodecForDiagnostics(codecKind)} {role} session is unavailable: {diagnostics}");
        }

        diagnostics = $"native/libwebrtc {FormatCodecForDiagnostics(codecKind)} {role} {width}x{height}@{fps} {bitrateBps / 1000}kbps negotiated={negotiatedWidth}x{negotiatedHeight}@{negotiatedFrameRate} {negotiatedBitrate / 1000}kbps; {diagnostics}";
        return session;
    }

    private static string FormatCodecForDiagnostics(CallVideoCodecKind codecKind)
        => CallVideoCodecFactory.FormatCodec(codecKind);

    private async Task StartLocalSenderForSelectionAsync(
        string dialogKey,
        RealtimeMediaSelection selection,
        CancellationToken token)
    {
        long senderStageStartedAtMs = Environment.TickCount64;
        ApplyNegotiatedSelection(selection);

        // Audio Phase 2a-step4b (2026-05-23): spin up the audio capture+send pipeline. Failure is
        // soft — the call still proceeds with video-only and the audio diagnostic explains why.
        // Headless / muted / locked endpoint shows up here as an INTERNAL_ERROR with an HRESULT.
        //
        // FIRST, ahead of the decoder and the encoder: this is the microphone, and on a call that
        // starts audio-only it is the whole call. Building it after the decoder made "we can be
        // heard" wait on ~700 ms of decoder setup that only matters for being able to SEE the
        // peer, which arrives a fraction of a second later anyway.
        NativeWebRtcMediaEngine.AudioCaptureSession? nextAudioCapture = null;
        string audioDiagnostics;
        if (MicrophoneDisabled)
        {
            // A remote-control session: no microphone is opened at all (not "muted" — closed).
            audioDiagnostics = "audio=off: remote-control session";
        }
        else if (NativeWebRtcMediaEngine.TryCreateAudioCapture(
                targetBitrateBps: 32000,
                out NativeWebRtcMediaEngine.AudioCaptureSession? createdAudio,
                out string audioCreateDiagnostics,
                deviceId: PreferredMicrophoneId) &&
            createdAudio is not null)
        {
            nextAudioCapture = createdAudio;
            audioDiagnostics = "audio=capturing:32kbps";
        }
        else
        {
            audioDiagnostics = $"audio=unavailable: {audioCreateDiagnostics}";
        }

        long audioCaptureUpAtMs = Environment.TickCount64;
        EnsureNativeReceiverForSelection(selection);
        long receiverUpAtMs = Environment.TickCount64;
        // No video in the session (neither peer can source it, or no common format):
        // there is no encoder to build. The audio pipeline below is the whole call and
        // it comes up exactly the same way. `DrainNativeOutgoingPacketsForSend` already
        // treats a null sender as "nothing to send".
        CallVideoCodecKind negotiatedCodec = selection.SendStream is null
            ? activeNativeCodecKind
            : ParseNegotiatedCodec(selection.SendStream.Codec);
        string nextCodecDiagnostics = "video=none (audio-only session)";
        NativeWebRtcMediaEngine.Session? nextNativeSender = null;
        // No direction from us (no camera/screen source, or no codec we can encode that the peer
        // decodes): nothing to encode, the receive side and audio come up all the same.
        if (selection.Video is not null && selection.SendStream is not null)
        {
            ResolvedVideoDevice encodeDevice = ResolveEncodeDevice();
            nextNativeSender = CreateNativeSession(
                negotiatedCodec,
                targetCaptureWidth,
                targetCaptureHeight,
                targetFps,
                targetBitrateBps,
                negotiatedCaptureWidth,
                negotiatedCaptureHeight,
                negotiatedFps,
                negotiatedBitrateBps,
                "sender",
                out nextCodecDiagnostics,
                encodeVendorId: encodeDevice.VendorId,
                screenContent: localScreenShareRequested,
                maxDatagramBytes: negotiatedMaxDatagramBytes);
            nextNativeSender.RequestKeyFrame();
        }

        Core.AppTrace.Write(
            "media/start-stage",
            $"audio_capture={audioCaptureUpAtMs - senderStageStartedAtMs}ms " +
            $"receiver={receiverUpAtMs - audioCaptureUpAtMs}ms " +
            $"encoder={Environment.TickCount64 - receiverUpAtMs}ms");
        // The microphone opening is the single most important thing to be able to prove
        // AFTER the fact: it must happen inside a call and nowhere else.
        //
        // `source` matters just as much once screen share is in play: every camera toggle,
        // share start/stop and preference change lands here, and without it the trace shows
        // a row of identical reconfigurations with no way to tell which was which.
        string videoSource = selection.Video is null
            ? "none"
            : localScreenShareRequested ? "screen"
            : localCameraRequested ? "camera"
            : "off";
        Core.AppTrace.Write(
            "media/sender-up",
            $"dialog={dialogKey} call={callMediaRequested} video={(selection.Video is null ? "none" : "yes")} " +
            $"source={videoSource} profile={targetCaptureWidth}x{targetCaptureHeight}@{targetFps} {audioDiagnostics}");
        bool ownershipTransferred = false;
        try
        {
            // Grabbed under the lock, disposed AFTER it. Disposing the previous handles is not
            // bookkeeping: the audio capture stops and joins its worker thread and the encoder
            // session tears down its MFT, and doing that while holding lifecycleSync stalled
            // every other media path that needs the lock — measured as multi-second waits on the
            // answer path. Same shape StopAsync/StopLocalSenderAsync already use.
            NativeWebRtcMediaEngine.Session? previousNativeSender = null;
            NativeWebRtcMediaEngine.AudioCaptureSession? previousAudioCapture = null;
            lock (lifecycleSync)
            {
                if (!string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal) ||
                    mediaCancellation is null)
                {
                    throw new OperationCanceledException();
                }

                previousNativeSender = nativeSenderSession;
                previousAudioCapture = nativeAudioCaptureSession;
                AttachPacketSink(nextNativeSender);
                if (nextAudioCapture is not null)
                {
                    AttachPacketSink(nextAudioCapture);
                    nextAudioCapture.SetMuted(localAudioMuted);
                }
                nativeSenderSession = nextNativeSender;
                // Delivery reports are routed to whichever sender is current; the one being
                // replaced is disposed only after this.
                activeMediaRouter?.SetSender(nextNativeSender);
                nativeAudioCaptureSession = nextAudioCapture;
                activeNativeCodecKind = negotiatedCodec;
                codecDiagnostics = nextCodecDiagnostics;
                nativeMediaDiagnostics = "native_sender=starting";
                audioCaptureDiagnostics = audioDiagnostics;
                ownershipTransferred = true;
            }

            previousNativeSender?.Dispose();
            previousAudioCapture?.Dispose();

            // Audio-first: the camera is an explicit opt-in. On call start
            // localCameraRequested is false, so we never open the webcam — the audio
            // capture created above + the standalone send loop carry the call full-duplex
            // (audio-only). The user turns video on later via StartLocalCameraAsync, which
            // reconfigures the sender with the flag set. See docs/decisions/call-audio-only-degrade.md.
            bool cameraStarted = false;
            string? cameraDegradeError = null;
            if (nextNativeSender is null)
            {
                // Audio-only session: nothing to feed frames into. Opening the camera
                // here would light the privacy LED for a stream nobody can receive.
                localScreenShareRequested = false;
                syntheticPatternRequested = false;
            }
            else if (localScreenShareRequested)
            {
                // Screen share replaces the camera as the video source: feed the (already-created)
                // native video sender screen frames instead of opening the webcam.
                try
                {
                    // Windows.Graphics.Capture binds to the UI thread; the negotiation monitor
                    // rebuilds the sender from a pool thread when the PEER changes the selection
                    // mid-share (a codec ceiling, a frame rate), and starting the capture there
                    // threw RPC_E_WRONG_THREAD — the share was dropped "audio only" (live
                    // 2026-10-13). On the UI thread already, the call is direct.
                    if (dispatcherQueue.HasThreadAccess)
                    {
                        StartScreenCapture(dialogKey);
                    }
                    else
                    {
                        await RunOnUiAsync(() => StartScreenCapture(dialogKey));
                    }
                    cameraStarted = true;
                }
                catch (OperationCanceledException)
                {
                    throw;
                }
                catch (Exception ex) when (nextAudioCapture is not null && !token.IsCancellationRequested)
                {
                    localScreenShareRequested = false;
                    syntheticPatternRequested = false;
                    cameraDegradeError = "Демонстрация экрана недоступна — звонок продолжается со звуком.";
                    captureDiagnostics = $"screen_share_unavailable: {ex.Message}";
                }
            }
            else if (localCameraRequested)
            {
                try
                {
                    await StartCaptureAsync(token);
                    cameraStarted = true;
                }
                catch (OperationCanceledException)
                {
                    throw;
                }
                catch (Exception ex) when (nextAudioCapture is not null && !token.IsCancellationRequested)
                {
                    // The webcam couldn't open (busy / unplugged / access denied) while the
                    // user was enabling video. Degrade to audio-only instead of dropping the
                    // call: keep the audio capture + sender, just don't run the camera.
                    localCameraRequested = false;
                    cameraDegradeError = "Камера недоступна — звонок продолжается со звуком.";
                    captureDiagnostics = $"camera_unavailable: {ex.Message}";
                }
            }

            ResetMediaCounters();
            lock (lifecycleSync)
            {
                if (!string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal) ||
                    mediaCancellation is null)
                {
                    throw new OperationCanceledException();
                }

                localCameraActive = cameraStarted;
                lastError = cameraDegradeError ?? string.Empty;
                nativeMediaDiagnostics = cameraStarted
                    ? "native_sender=ready"
                    : cameraDegradeError is null
                        ? "native_sender=audio_only"
                        : "native_sender=audio_only(camera_unavailable)";
                activeSelectionSignature = BuildSelectionSignature(selection);
                activeLocalSelection = selection;
            }

            PublishDiagnostics();
        }
        catch
        {
            if (ownershipTransferred)
            {
                lock (lifecycleSync)
                {
                    if (nextNativeSender is not null &&
                        ReferenceEquals(nativeSenderSession, nextNativeSender))
                    {
                        nativeSenderSession = null;
                        activeMediaRouter?.SetSender(null);
                        nativeMediaDiagnostics = nativeReceiverSession is null
                            ? "none"
                            : "native_receiver=ready";
                    }
                    if (ReferenceEquals(nativeAudioCaptureSession, nextAudioCapture))
                    {
                        nativeAudioCaptureSession = null;
                        audioCaptureDiagnostics = "none";
                    }
                }
            }

            nextNativeSender?.Dispose();
            nextAudioCapture?.Dispose();
            throw;
        }
    }

    // Opt-in camera control during an active call. StartLocalCameraAsync flips
    // localCameraRequested and restarts the local sender so the webcam opens (and the
    // negotiated video sender starts pushing frames); StopLocalCameraAsync turns it back
    // off → audio-only. Safe to call when no call is active (no-op).
    internal Task StartLocalCameraAsync() => SetLocalCameraRequestedAsync(true);

    internal Task StopLocalCameraAsync() => SetLocalCameraRequestedAsync(false);

    private async Task SetLocalCameraRequestedAsync(bool requested)
    {
        // Turning the camera on while screen-sharing implicitly stops the share (one video source).
        if (requested && localScreenShareRequested)
        {
            localScreenShareRequested = false;
            syntheticPatternRequested = false;
            screenShareItem = null;
        }

        localCameraRequested = requested;
        await ReconfigureAfterVideoSourceChangeAsync();
    }

    // Screen share control (Zoom/Telemost "share screen"). StartScreenShareAsync swaps the video
    // source from the camera to the picked screen/window: it remembers the current camera state,
    // drops the camera, and restarts the local sender so it pushes screen frames instead of webcam
    // frames. StopScreenShareAsync restores the pre-share camera state. Safe when no call is active.
    internal async Task StartScreenShareAsync(
        global::Windows.Graphics.Capture.GraphicsCaptureItem item,
        CallVideoResolutionOption? resolution = null)
    {
        ArgumentNullException.ThrowIfNull(item);
        // The screen's own size, not the camera's: it goes into the passport while the share runs.
        // Unpicked — the source's own size, up to 1440p.
        screenShareResolution = resolution ?? CallVideoPreferences.DefaultScreenResolution(
            CallVideoPreferences.BuildResolutionOptions(item.Size.Width, item.Size.Height));
        // Remember the camera state only when transitioning from not-sharing, so a re-pick mid-share
        // doesn't clobber it with "camera off".
        if (!localScreenShareRequested)
        {
            screenShareCameraWasRequested = localCameraRequested;
        }

        screenShareItem = item;
        sharedMonitor = ScreenShareCaptureSource.FindMonitorOf(item);
        localScreenShareRequested = true;
        syntheticPatternRequested = false;
        localCameraRequested = false; // screen replaces the camera as the video source
        await ReconfigureAfterVideoSourceChangeAsync();
    }

    /// <summary>
    /// Share the synthetic pattern (see <see cref="SyntheticPatternSource"/>) as if it were a screen:
    /// same flags, same frame path, no display needed. Test channel only (`pattern.on`).
    /// </summary>
    internal async Task StartSyntheticPatternShareAsync()
    {
        if (!localScreenShareRequested)
        {
            screenShareCameraWasRequested = localCameraRequested;
        }

        screenShareItem = null;
        sharedMonitor = null;
        screenShareResolution = CallVideoResolutionOption.Even(SyntheticPatternSource.Width, SyntheticPatternSource.Height);
        syntheticPatternRequested = true;
        localScreenShareRequested = true;
        localCameraRequested = false;
        await ReconfigureAfterVideoSourceChangeAsync();
    }

    internal async Task StopScreenShareAsync()
    {
        if (!localScreenShareRequested)
        {
            return;
        }

        localScreenShareRequested = false;
        syntheticPatternRequested = false;
        screenShareItem = null;
        sharedMonitor = null;
        localCameraRequested = screenShareCameraWasRequested; // return to the pre-share camera state
        await ReconfigureAfterVideoSourceChangeAsync();
    }

    private async Task ReconfigureAfterVideoSourceChangeAsync()
    {
        string? dialogKey;
        RealtimeMediaSelection? selection;
        CancellationToken token;
        lock (lifecycleSync)
        {
            dialogKey = activeDialogKey;
            selection = activeLocalSelection;
            token = mediaCancellation?.Token ?? CancellationToken.None;
        }

        if (string.IsNullOrWhiteSpace(dialogKey) || selection is null)
        {
            return;
        }

        // 1:1: what we show changed (camera <-> screen), so does the best codec for our direction
        // (per-pair-video-codec-selection.md §3: AV1's screen tools). Announce again; the selection
        // comes back for both directions, the peer rebuilds its decoder on the new signature.
        if (!dialogKey.StartsWith("group:", StringComparison.Ordinal))
        {
            try
            {
                CallVideoCaptureProbeResult captureProbe = await EnsureCaptureProbeAsync(token);
                RealtimeMediaNegotiationSnapshot snapshot = TryPublishLocalCapabilities(
                    dialogKey,
                    throwOnFailure: false,
                    captureProbe);
                selection = snapshot.LocalSelection ?? selection;
                AdoptRepublishedSelection(selection);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                // The previous selection still carries the call; the codec simply stays as it was.
                RecordError($"re-announce for the new video source failed: {FormatExceptionForDiagnostics(ex)}");
            }
        }

        try
        {
            await ReconfigureLocalSenderForSelectionAsync(dialogKey, selection, token);
        }
        catch (OperationCanceledException)
        {
        }

        PublishStateChanged();
    }

    // Start feeding the (already-created) native video sender frames from the picked screen/window.
    // Called from the sender-start path in place of the camera reader when a share is requested.
    // Runs synchronously to first-frame setup; frames then arrive on the capture source's own thread.
    private void StartScreenCapture(string dialogKey)
    {
        if (syntheticPatternRequested)
        {
            screenShareSource?.Dispose();
            SyntheticPatternSource pattern = new();
            screenShareSource = pattern;
            pattern.Start(bitmap => ProcessScreenFrame(bitmap, dialogKey));
            captureDiagnostics = "screen_share=synthetic";
            captureStartedAtMs = Environment.TickCount64;
            return;
        }

        global::Windows.Graphics.Capture.GraphicsCaptureItem item =
            screenShareItem ?? throw new InvalidOperationException("No screen-share target selected.");

        screenShareSource?.Dispose();
        ScreenShareCaptureSource source = new();
        screenShareSource = source;
        Interlocked.Exchange(ref screenFramesSinceShareStart, 0);
        screenShareCaptureStartedAtMs = Environment.TickCount64;
        source.Start(
            item,
            frameCallback: bitmap => ProcessScreenFrame(bitmap, dialogKey),
            errorCallback: message =>
            {
                RecordError(message);
                // The shared window closed (or capture faulted) — stop the share back to camera state.
                _ = StopScreenShareAsync();
            });
        captureDiagnostics = "screen_share=capturing";
        captureStartedAtMs = Environment.TickCount64;
    }

    // Screen-frame counterpart of OnFrameArrived: copy the BGRA screen bitmap into an encode frame and
    // push it down the same EncodeAndSend + local-preview path the camera uses. Owns + disposes the
    // bitmap. Best-effort per frame (a fault records + drops, never crashes the share).
    /// Screen frames since the current share started — zero for long means nothing to show (the
    /// owner's monitor is really off, or capture itself is broken — the 2026-10-05 "monitor off"
    /// reading was the latter, see ScreenShareCaptureSource.Start).
    private long screenFramesSinceShareStart;
    private long screenShareCaptureStartedAtMs;

    /// The share runs but has produced no picture for a while.
    internal bool ScreenShareHasNoPicture =>
        localScreenShareRequested &&
        screenShareCaptureStartedAtMs != 0 &&
        Interlocked.Read(ref screenFramesSinceShareStart) == 0 &&
        Environment.TickCount64 - screenShareCaptureStartedAtMs > 4_000;

    private void ProcessScreenFrame(SoftwareBitmap bitmap, string dialogKey)
    {
        Interlocked.Increment(ref screenFramesSinceShareStart);
        CancellationToken token = mediaCancellation?.Token ?? CancellationToken.None;
        CapturedVideoFrame? capturedFrame = null;
        long startedAtMs = Environment.TickCount64;
        try
        {
            if (token.IsCancellationRequested ||
                !localScreenShareRequested ||
                !string.Equals(activeDialogKey, dialogKey, StringComparison.Ordinal))
            {
                return;
            }

            if (!ShouldEncodeFrame())
            {
                return; // honour the adaptive frame-interval cap (same throttle the camera uses)
            }

            Interlocked.Increment(ref captureFramesArrived);
            bool queueLocalPreview = ShouldRenderLocalPreview();
            capturedFrame = CopyCapturedFrameFromBitmap(bitmap, includePreview: false);
            // У демонстрации экрана нет камеры, а значит нет и профиля захвата, который можно
            // перезапустить под ограничением ABR — тем путём разрешение снижается только у камеры.
            // Поэтому масштабируем сам кадр. Без этого ABR у демонстрации мог сбавить битрейт и
            // fps, но не разрешение: экран продолжал кодироваться целиком, и ключевой кадр
            // оставался неподъёмным ровно там, где у пользователя и умирало видео.
            // См. docs/bugs/video-dies-on-packet-loss.md.
            capturedFrame = DownscaleToAdaptiveTarget(capturedFrame);
            if (token.IsCancellationRequested)
            {
                return;
            }

            EncodeAndSend(capturedFrame);
            if (queueLocalPreview)
            {
                QueueLocalPreviewFrame(capturedFrame.DetachForLocalPreview());
            }
        }
        catch (Exception ex)
        {
            RecordError(FormatExceptionForDiagnostics(ex));
        }
        finally
        {
            Interlocked.Increment(ref captureFramesProcessed);
            Interlocked.Add(ref captureFrameProcessingMs, Math.Max(0, Environment.TickCount64 - startedAtMs));
            capturedFrame?.Dispose();
            bitmap.Dispose();
        }
    }

    private async Task ReconfigureLocalSenderForSelectionAsync(
        string dialogKey,
        RealtimeMediaSelection selection,
        CancellationToken token,
        bool skipIfSelectionAlreadyLive = false)
    {
        // Queue behind any reconfiguration already running instead of dropping this one.
        //
        // It used to `return` when one was in flight. Each request carries a DIFFERENT
        // intent — which source to run — read inside StartLocalSenderForSelectionAsync from
        // localCameraRequested / localScreenShareRequested, so a dropped request is not a
        // duplicate of the running one, it is a lost instruction with nothing to retry it.
        // Turning the camera on takes seconds (WinRT MediaCapture init); pressing "share
        // screen" during that window was silently discarded, leaving the flags saying
        // "sharing" while the pipeline stayed on the other source. The UI showed the share
        // as active and the picture stopped. Live test 2026-08-11: "включил экран не
        // выключая камеры — зависло".
        await localSenderReconfigureSync.WaitAsync(token);
        try
        {
            // The negotiation monitor decides to reconfigure from a snapshot it read BEFORE
            // queueing here, so by the time it gets the gate the answer path may already have
            // stood up exactly this selection — then rebuilding it would tear down a working
            // sender to build an identical one. Only the monitor passes this: every other caller
            // (camera on/off, screen share, preference change) keeps the SAME selection and
            // changes the SOURCE, so for them "already live" is precisely when work is needed.
            if (skipIfSelectionAlreadyLive && IsLocalSenderUpForSelection(selection))
            {
                return;
            }

            if (TryChangeSendCeilingInPlace(selection))
            {
                return;
            }

            await StopLocalSenderAsync(clearLocalPreview: false);
            token.ThrowIfCancellationRequested();
            await StartLocalSenderForSelectionAsync(dialogKey, selection, token);
        }
        finally
        {
            localSenderReconfigureSync.Release();
        }
    }

    /// <summary>
    /// Only our direction's bitrate CEILING changed (Settings → «Битрейт» on either side): the live
    /// sender takes the new ceiling as it is — no new capture, encoder or audio, so no gap and no
    /// keyframe. Rebuilding it for this cost ~1 s without frames on both sides (live 2026-10-18).
    /// A selection whose ceiling is the same is not ours to handle: the caller then wants a new
    /// SOURCE (camera, screen) on the same selection.
    /// </summary>
    private bool TryChangeSendCeilingInPlace(RealtimeMediaSelection selection)
    {
        NativeWebRtcMediaEngine.Session? sender;
        RealtimeMediaSelection? live;
        lock (lifecycleSync)
        {
            sender = nativeSenderSession;
            live = activeLocalSelection;
        }

        if (sender is null ||
            live?.SendStream is not { } current ||
            selection.SendStream is not { } next ||
            next.TargetBitrateBps == current.TargetBitrateBps ||
            !string.Equals(BuildFormatSignature(live), BuildFormatSignature(selection), StringComparison.Ordinal))
        {
            return false;
        }

        int ceiling = ClampUIntToInt(next.TargetBitrateBps, 256_000, 20_000_000);
        lock (sender.Gate)
        {
            sender.SetMaxBitrate(ceiling);
        }

        lock (lifecycleSync)
        {
            negotiatedBitrateBps = ceiling;
            // The native target dropped under a lowered ceiling at once; the shell's copy follows,
            // or it reads the old rate until ABR next changes (12 s in the live pass).
            targetBitrateBps = Math.Min(targetBitrateBps, ceiling);
            adaptiveBitrateBps = Math.Min(adaptiveBitrateBps, ceiling);
            activeSendStream = selection.SendStream;
            activeReceiveStream = selection.ReceiveStream;
            activeSelectionSignature = BuildSelectionSignature(selection);
            activeLocalSelection = selection;
        }

        Core.AppTrace.Write(
            "media/ceiling",
            $"in place {current.TargetBitrateBps / 1000} -> {next.TargetBitrateBps / 1000}kbps abr={targetBitrateBps / 1000}kbps");
        PublishDiagnostics();
        return true;
    }

    private async Task StopLocalSenderAsync(bool clearLocalPreview)
    {
        CancellationTokenSource? captureCancel;
        Task? inflightFrameTask;
        Task? inflightReconfigureTask;
        MediaFrameReader? reader;
        MediaCapture? capture;
        IDisposable? screenSource;
        NativeWebRtcMediaEngine.Session? currentNativeSender;
        NativeWebRtcMediaEngine.AudioCaptureSession? currentAudioCapture;

        lock (lifecycleSync)
        {
            captureCancel = captureCancellation;
            // Capture under lock to atomically grab + clear, matching the
            // pattern used by StopAsync. See docs/decisions/csharp-call-task-lifecycle.md.
            inflightFrameTask = activeFrameProcessingTask;
            inflightReconfigureTask = captureReconfigureTask;
            reader = frameReader;
            capture = mediaCapture;
            // Stop any screen-share capture source too; the flags/item that decide whether to restart
            // it stay untouched (StartLocalSenderForSelectionAsync reads them). Only the source object
            // is torn down here so the old capture stops before a new one starts.
            screenSource = screenShareSource;
            currentNativeSender = nativeSenderSession;
            currentAudioCapture = nativeAudioCaptureSession;

            activeFrameProcessingTask = null;
            captureReconfigureTask = null;
            captureCancellation = null;
            frameReader = null;
            mediaCapture = null;
            screenShareSource = null;
            nativeSenderSession = null;
            activeMediaRouter?.SetSender(null);
            nativeAudioCaptureSession = null;
            audioCaptureDiagnostics = "none";
            nativeMediaDiagnostics = nativeReceiverSession is null ? "none" : "native_receiver=ready";
            selectedCapturePixelFormat = CallVideoPreferences.CapturePixelFormatNv12;
            localCameraActive = false;
            captureDiagnostics = "none";
            captureStartedAtMs = 0;
            activeCaptureProfileWidth = 0;
            activeCaptureProfileHeight = 0;
            activeCaptureProfileFps = 0;
            Interlocked.Exchange(ref frameSelectionCredit, 0);
            Interlocked.Exchange(ref frameSelectionLastSelectedAtMs, 0);
            nextLocalPreviewAtMs = 0;
            Interlocked.Exchange(ref localPreviewLoopScheduled, 0);
            Interlocked.Exchange(ref activeVideoWidth, 0);
            Interlocked.Exchange(ref activeVideoHeight, 0);
        }

        ClearPendingLocalPreviewFrame();
        captureCancel?.Cancel();

        if (reader is not null)
        {
            reader.FrameArrived -= OnFrameArrived;
            try
            {
                await reader.StopAsync().AsTask();
            }
            catch
            {
                // Sender restart must not fail because the camera reader was already stopping.
            }

            reader.Dispose();
        }

        capture?.Dispose();
        screenSource?.Dispose();

        // Await in-flight OnFrameArrived + profile-adapt before disposing
        // native sender. activeFrameProcessingTask uses mediaCancellation
        // (not captureCancellation), so it won't observe the cancel above —
        // but EncodeAndSend bails when nativeSenderSession is null (already
        // nulled in the lock), so the in-flight body will exit on its own
        // path. The await is bounded by AwaitTrackedTaskWithGraceAsync.
        // WaitForCaptureFrameInFlightAsync below is the legacy backup.
        // See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
        await AwaitTrackedTaskWithGraceAsync(inflightFrameTask);
        await AwaitTrackedTaskWithGraceAsync(inflightReconfigureTask);
        await WaitForCaptureFrameInFlightAsync();
        currentNativeSender?.Dispose();
        // Audio capture worker has its own thread; Dispose stops + joins it.
        currentAudioCapture?.Dispose();
        captureCancel?.Dispose();

        if (clearLocalPreview)
        {
            await RunOnUiAsync(() =>
            {
                localPreviewBitmap = null;
                localPreviewImage.Source = null;
            });
        }
    }

    /// Обратная связь RTCP включается ВЫБОРОМ. Пока флаг снят, цикл управления не забирает
    /// входящие отчёты ВООБЩЕ (см. RunControlLoopAsync), то есть регулятор скорости остаётся без
    /// единственного источника правды о доставке — и это выглядит снаружи как «пир молчит».
    ///
    /// Печатаем каждое ИЗМЕНЕНИЕ флага: 2026-08-30 звонок прошёл целиком с
    /// `feedbackStale=0ms` и ни одним отчётом, и по логу нельзя было отличить «отчётов не
    /// слали» от «мы перестали их забирать». Теперь можно.
    private void SetRtcpFeedbackEnabled(bool enabled, string reason)
    {
        if (rtcpFeedbackEnabled == enabled)
        {
            return;
        }

        rtcpFeedbackEnabled = enabled;
        Core.AppTrace.Write(
            "media/rtcp-feedback",
            $"dialog={activeDialogKey ?? "none"} enabled={enabled.ToString().ToLowerInvariant()} reason={reason}");
    }

    private void ApplyNegotiatedSelection(RealtimeMediaSelection selection)
    {
        activeMediaSessionId = selection.SessionId;
        activeSendStream = selection.SendStream;
        activeReceiveStream = selection.ReceiveStream;
        // Бюджет датаграммы — свойство СЕССИИ, а не видео: он согласован и для звонка без камеры.
        // Поэтому берётся до ветки «аудио без видео» ниже.
        negotiatedMaxDatagramBytes = selection.MaxDatagramBytes > 0
            ? ClampUIntToInt(selection.MaxDatagramBytes, 300, 1200)
            : 0;
        if (selection.Video is not { } video)
        {
            // Audio-only session. Leave the video targets at their defaults — nothing
            // reads them while there is no encoder — and say plainly what was agreed,
            // because "negotiated=..." with stale dimensions would read as a video call
            // that silently produces no picture.
            SetRtcpFeedbackEnabled(false, "audio_only");
            negotiationDiagnostics = "negotiated=audio_only";
            return;
        }

        // Our direction's own format and target (docs/decisions/per-direction-video-format.md); a
        // receive-only side keeps the receive direction's numbers for its diagnostics.
        RealtimeVideoStream? ownDirection = selection.SendStream ?? selection.ReceiveStream;
        targetCaptureWidth = ClampUIntToInt(ownDirection?.Width ?? 1280, 2, CallVideoPreferences.MaxVideoSide);
        targetCaptureHeight = ClampUIntToInt(ownDirection?.Height ?? 720, 2, CallVideoPreferences.MaxVideoSide);
        targetFps = ClampUIntToInt(ownDirection?.Fps ?? 30, 1, 60);
        targetFrameIntervalMs = Math.Max(1, 1000 / targetFps);
        // The selection's bitrate is the CEILING; the sender starts below it — where this call's
        // previous sender was, else at a modest start — and the congestion controller goes up.
        negotiatedBitrateBps = ClampUIntToInt(ownDirection?.TargetBitrateBps ?? 1_200_000, 256_000, 20_000_000);
        targetBitrateBps = Math.Min(
            negotiatedBitrateBps,
            lastSenderAbrBitrateBps > 0 ? lastSenderAbrBitrateBps : FreshSenderStartBitrateBps);
        negotiatedCaptureWidth = targetCaptureWidth;
        negotiatedCaptureHeight = targetCaptureHeight;
        negotiatedFps = targetFps;
        adaptiveBitrateBps = targetBitrateBps;
        adaptiveFps = targetFps;
        adaptiveDiagnostics = "stable";
        captureProfileMaxWidth = 0;
        captureProfileMaxHeight = 0;
        activeCaptureProfileWidth = 0;
        activeCaptureProfileHeight = 0;
        activeCaptureProfileFps = 0;
        Interlocked.Exchange(ref captureReconfigureInFlight, 0);
        SetRtcpFeedbackEnabled(video.RtcpFeedbackEnabled, "selection");
        selectedCapturePixelFormat = NormalizeCapturePixelFormat(video.CapturePixelFormat);
        negotiationDiagnostics = $"negotiated={FormatDirections(selection.SendStream, selection.ReceiveStream)} format={selectedCapturePixelFormat} rtcp={video.RtcpFeedbackEnabled.ToString().ToLowerInvariant()}";
    }

    private static string NormalizeCapturePixelFormat(string? pixelFormat)
        => string.Equals(pixelFormat, CallVideoPreferences.CapturePixelFormatBgra, StringComparison.OrdinalIgnoreCase)
            ? CallVideoPreferences.CapturePixelFormatBgra
            : CallVideoPreferences.CapturePixelFormatNv12;

    private static int ClampUIntToInt(uint value, int min, int max)
        => checked((int)Math.Clamp(value, (uint)min, (uint)max));

    private static string BuildNegotiationDiagnostics(RealtimeMediaNegotiationSnapshot snapshot)
    {
        if (snapshot.Selected is { } selected)
        {
            if (selected.Video is not { } video)
            {
                // Negotiated, audio only. The Diagnostic is not an error here — it is the
                // reason there is no video, and it is the only place the user can find out.
                return string.IsNullOrWhiteSpace(snapshot.Diagnostic)
                    ? "negotiated=audio_only"
                    : $"negotiated=audio_only:{snapshot.Diagnostic}";
            }

            return $"negotiated={FormatDirections(snapshot.SendVideo, snapshot.ReceiveVideo)} format={NormalizeCapturePixelFormat(video.CapturePixelFormat)} rtcp={video.RtcpFeedbackEnabled.ToString().ToLowerInvariant()}";
        }

        return string.IsNullOrWhiteSpace(snapshot.Diagnostic)
            ? $"negotiation={snapshot.Status}"
            : $"negotiation={snapshot.Status}:{snapshot.Diagnostic}";
    }

    private static CallVideoCodecKind ParseNegotiatedCodec(string codec)
        => CallVideoCodecFactory.ParseCodec(codec)
           ?? throw new InvalidOperationException($"Unsupported negotiated video codec '{codec}'.");

    /// <summary>"send=av1 1920x1080@30 2400kbps recv=h265 1280x720@30 2800kbps" — each direction its own
    /// codec and format (Diagnostics → Media).</summary>
    internal static string FormatDirections(RealtimeVideoStream? send, RealtimeVideoStream? receive)
        => $"send={FormatDirection(send)} recv={FormatDirection(receive)}";

    private static string FormatDirection(RealtimeVideoStream? stream)
        => stream is null
            ? "none"
            : $"{stream.Codec} {stream.Width}x{stream.Height}@{stream.Fps} {stream.TargetBitrateBps / 1000}kbps";

    /// <see cref="BuildSelectionSignature"/> without the bitrates: what a decoder, or an encoder
    /// that can take a new ceiling live, would have to be rebuilt for.
    private static string BuildFormatSignature(RealtimeMediaSelection selection)
        => selection.Video is not { } video
            ? $"audio_only:{selection.MaxDatagramBytes}"
            : string.Join(
                ':',
                FormatShape(selection.SendStream),
                FormatShape(selection.ReceiveStream),
                selection.MaxDatagramBytes,
                video.CapturePixelFormat,
                video.RtcpFeedbackEnabled);

    private static string FormatShape(RealtimeVideoStream? stream)
        => stream is null ? "none" : $"{stream.Codec} {stream.Width}x{stream.Height}@{stream.Fps}";

    private static string BuildSelectionSignature(RealtimeMediaSelection selection)
        => selection.Video is not { } video
            ? $"audio_only:{selection.MaxDatagramBytes}"
            : string.Join(
                ':',
                FormatDirections(selection.SendStream, selection.ReceiveStream),
                selection.MaxDatagramBytes,
                video.CapturePixelFormat,
                video.RtcpFeedbackEnabled);

    private bool IsCurrentCaptureStartAttempt(long attemptId)
        => Interlocked.Read(ref captureStartAttemptId) == attemptId;

    internal sealed record CameraOption(string Id, string DisplayName);

    // Enumerate selectable cameras for the Settings picker. Uses MediaFrameSourceGroup (the same
    // enumeration StartCaptureAsync consumes) so each returned Id matches what the capture path
    // selects on — only groups exposing a colour video source (the ones we can actually open) are
    // returned. Best-effort: returns empty on any enumeration failure.
    internal static async Task<IReadOnlyList<CameraOption>> ListCamerasAsync()
    {
        try
        {
            IReadOnlyList<MediaFrameSourceGroup> groups = await MediaFrameSourceGroup.FindAllAsync();
            return groups
                .Where(group => group.SourceInfos.Any(info =>
                    info.SourceKind == MediaFrameSourceKind.Color &&
                    info.MediaStreamType is MediaStreamType.VideoRecord or MediaStreamType.VideoPreview))
                .Select(group => new CameraOption(
                    group.Id,
                    string.IsNullOrWhiteSpace(group.DisplayName) ? "Камера" : group.DisplayName))
                .ToArray();
        }
        catch
        {
            return Array.Empty<CameraOption>();
        }
    }

    private async Task StartCaptureAsync(CancellationToken token)
    {
        CancellationTokenSource captureCts = CancellationTokenSource.CreateLinkedTokenSource(token);
        lock (lifecycleSync)
        {
            captureCancellation = captureCts;
        }

        try
        {
            CancellationToken captureToken = captureCts.Token;
            try
            {
                await StartWinRtCaptureWithTimeoutAsync(captureToken, captureFailurePrefix: "capture_unavailable");
                return;
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                string winRtFailure = captureDiagnostics;
                if (string.IsNullOrWhiteSpace(winRtFailure))
                {
                    winRtFailure = $"capture_unavailable: {FormatExceptionForDiagnostics(ex)}";
                }

                // No fallback path — WinRT MediaCapture is the only supported
                // camera source. OpenCV fallback removed in Wave 1 (2026-05-16).
                captureDiagnostics = winRtFailure;
                throw new InvalidOperationException(winRtFailure, ex);
            }
        }
        catch
        {
            lock (lifecycleSync)
            {
                if (ReferenceEquals(captureCancellation, captureCts))
                {
                    captureCancellation = null;
                }
            }

            captureCts.Dispose();
            throw;
        }
    }

    private async Task StartWinRtCaptureWithTimeoutAsync(
        CancellationToken captureToken,
        string captureFailurePrefix)
    {
        using CancellationTokenSource timeoutCts =
            CancellationTokenSource.CreateLinkedTokenSource(captureToken);

        long attemptId = Interlocked.Increment(ref captureStartAttemptId);
        Task startTask = StartWinRtCaptureAsync(timeoutCts.Token, attemptId);
        Task timeoutTask = Task.Delay(CameraStartTimeout, captureToken);
        Task softDiagnosticTask = Task.Delay(CameraStartSoftDiagnosticDelay, captureToken);
        while (true)
        {
            Task completedTask = await Task.WhenAny(startTask, timeoutTask, softDiagnosticTask);
            if (ReferenceEquals(completedTask, startTask))
            {
                try
                {
                    await startTask;
                    return;
                }
                catch (OperationCanceledException) when (!captureToken.IsCancellationRequested)
                {
                    throw new InvalidOperationException(
                        $"{captureFailurePrefix}: Camera start was canceled before the camera became available.");
                }
            }

            if (ReferenceEquals(completedTask, softDiagnosticTask))
            {
                if (captureToken.IsCancellationRequested)
                {
                    throw new OperationCanceledException(captureToken);
                }

                if (IsCurrentCaptureStartAttempt(attemptId))
                {
                    captureDiagnostics =
                        $"camera_starting: WinRT camera is still opening after {CameraStartSoftDiagnosticDelay.TotalSeconds:0}s; if another app or window owns the camera this will become camera_contended.";
                    PublishDiagnostics();
                }

                softDiagnosticTask = Task.Delay(Timeout.InfiniteTimeSpan);
                continue;
            }

            timeoutCts.Cancel();
            if (captureToken.IsCancellationRequested)
            {
                throw new OperationCanceledException(captureToken);
            }

            Interlocked.CompareExchange(ref captureStartAttemptId, attemptId + 1, attemptId);
            _ = startTask.ContinueWith(
                static task =>
                {
                    _ = task.Exception;
                },
                TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously);
            throw new InvalidOperationException(
                $"camera_contended: Camera start timed out after {CameraStartTimeout.TotalSeconds:0}s.");
        }
    }

    private async Task StartWinRtCaptureAsync(CancellationToken captureToken, long attemptId)
    {
        MediaCapture? capture = null;
        MediaFrameReader? reader = null;
        bool ownershipTransferred = false;
        try
        {
            IReadOnlyList<MediaFrameSourceGroup> groups = await MediaFrameSourceGroup.FindAllAsync().AsTask(captureToken);
            string? preferredGroupId = preferredCameraId;
            var candidateSources = groups
                .SelectMany(group => group.SourceInfos
                    .Where(info =>
                        info.SourceKind == MediaFrameSourceKind.Color &&
                        info.MediaStreamType is MediaStreamType.VideoRecord or MediaStreamType.VideoPreview)
                    .Select(info => new { Group = group, Info = info }))
                // The user-chosen camera (Settings) wins; preview-first within each group; falls
                // through to any other camera if the preferred one can't activate the profile.
                .OrderBy(source => !string.IsNullOrEmpty(preferredGroupId) &&
                    string.Equals(source.Group.Id, preferredGroupId, StringComparison.OrdinalIgnoreCase) ? 0 : 1)
                .ThenBy(source => source.Info.MediaStreamType == MediaStreamType.VideoPreview ? 0 : 1)
                .ToArray();

            if (candidateSources.Length == 0)
            {
                throw new InvalidOperationException("Camera was not found.");
            }

            string lastSourceFailure = string.Empty;
            foreach (var candidateSource in candidateSources)
            {
                capture?.Dispose();
                capture = null;
                reader = null;

                MediaFrameSourceGroup selectedGroup = candidateSource.Group;
                MediaFrameSourceInfo selectedInfo = candidateSource.Info;
                MediaCaptureInitializationSettings settings = new()
                {
                    SourceGroup = selectedGroup,
                    StreamingCaptureMode = StreamingCaptureMode.Video,
                    SharingMode = MediaCaptureSharingMode.ExclusiveControl,
                    MemoryPreference = MediaCaptureMemoryPreference.Cpu,
                    MediaCategory = MediaCategory.Communications,
                };

                try
                {
                    capture = new();
                    await capture.InitializeAsync(settings).AsTask(captureToken);
                    ConfigureVideoDeviceForRealtime(capture);

                    if (!capture.FrameSources.TryGetValue(selectedInfo.Id, out MediaFrameSource? source))
                    {
                        throw new InvalidOperationException("Camera source is not available.");
                    }

                    await ApplyPreferredVideoFormatAsync(capture, source, captureToken);

                    string readerSubtype = ResolveSelectedReaderSubtype(source);
                    reader = await capture.CreateFrameReaderAsync(source, readerSubtype).AsTask(captureToken);
                    reader.AcquisitionMode = MediaFrameReaderAcquisitionMode.Realtime;
                    reader.FrameArrived += OnFrameArrived;
                    MediaFrameReaderStartStatus startStatus = await reader.StartAsync().AsTask(captureToken);
                    if (startStatus != MediaFrameReaderStartStatus.Success)
                    {
                        throw new InvalidOperationException($"camera_contended: Camera reader did not start: {startStatus}.");
                    }

                    if (captureToken.IsCancellationRequested ||
                        Interlocked.Read(ref captureStartAttemptId) != attemptId)
                    {
                        throw new OperationCanceledException(captureToken);
                    }

                    lock (lifecycleSync)
                    {
                        if (Interlocked.Read(ref captureStartAttemptId) != attemptId)
                        {
                            throw new OperationCanceledException(captureToken);
                        }

                        mediaCapture = capture;
                        frameReader = reader;
                        captureDiagnostics = BuildCaptureDiagnostics(
                            source.CurrentFormat,
                            readerSubtype,
                            selectedInfo.MediaStreamType,
                            settings.SharingMode);
                        captureStartedAtMs = Environment.TickCount64;
                        activeCaptureProfileWidth = checked((int)source.CurrentFormat.VideoFormat.Width);
                        activeCaptureProfileHeight = checked((int)source.CurrentFormat.VideoFormat.Height);
                        activeCaptureProfileFps = checked((int)Math.Round(GetFrameRate(source.CurrentFormat)));
                        ownershipTransferred = true;
                    }

                    return;
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    lastSourceFailure =
                        $"{selectedGroup.DisplayName}/{selectedInfo.MediaStreamType}: {FormatExceptionForDiagnostics(ex)}";
                    if (reader is not null)
                    {
                        reader.FrameArrived -= OnFrameArrived;
                        reader.Dispose();
                        reader = null;
                    }

                    capture?.Dispose();
                    capture = null;
                }
            }

            throw new InvalidOperationException(
                string.IsNullOrWhiteSpace(lastSourceFailure)
                    ? "capture_unavailable: no camera source could activate the negotiated profile."
                    : $"capture_unavailable: no camera source could activate the negotiated profile. Last failure: {lastSourceFailure}");
        }
        catch (Exception ex)
        {
            captureDiagnostics = IsLikelyCameraContentionFailure(ex)
                ? BuildCameraContentionFailure(ex)
                : $"capture_unavailable: {FormatExceptionForDiagnostics(ex)}";
            if (!ownershipTransferred)
            {
                if (reader is not null)
                {
                    reader.FrameArrived -= OnFrameArrived;
                    reader.Dispose();
                }

                capture?.Dispose();
            }

            throw;
        }
    }

    private static void ConfigureVideoDeviceForRealtime(MediaCapture capture)
    {
        try
        {
            capture.VideoDeviceController.DesiredOptimization = MediaCaptureOptimization.Latency;
        }
        catch
        {
            // Some drivers reject low-latency optimization after shared initialization.
        }

        try
        {
            if (capture.VideoDeviceController.ExposurePriorityVideoControl.Supported)
            {
                capture.VideoDeviceController.ExposurePriorityVideoControl.Enabled = false;
            }
        }
        catch
        {
            // Optional camera control; failure must not prevent calls.
        }
    }

    private async Task ApplyPreferredVideoFormatAsync(
        MediaCapture capture,
        MediaFrameSource source,
        CancellationToken captureToken)
    {
        IReadOnlyList<MediaFrameFormat> candidates = SelectPreferredVideoFormats(source);
        string lastFailure = string.Empty;
        foreach (MediaFrameFormat candidate in candidates)
        {
            try
            {
                await source.SetFormatAsync(candidate).AsTask(captureToken);
                ConfigureVideoDeviceForRealtime(capture);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                lastFailure = $"{FormatMediaFrameFormat(candidate)}: {FormatExceptionForDiagnostics(ex)}";
                continue;
            }

            if (CurrentFormatMatchesActiveSelection(source.CurrentFormat, out string activeFailure))
            {
                return;
            }

            lastFailure = $"{FormatMediaFrameFormat(candidate)} activated as {FormatMediaFrameFormat(source.CurrentFormat)}; {activeFailure}";
        }

        string suffix = string.IsNullOrWhiteSpace(lastFailure) ? string.Empty : $" Last failure: {lastFailure}";
        throw new InvalidOperationException(
            $"capture_unavailable: selected camera source could not activate negotiated {targetCaptureWidth}x{targetCaptureHeight}@{targetFps} {selectedCapturePixelFormat} profile.{suffix}");
    }


    private void OnFrameArrived(MediaFrameReader sender, MediaFrameArrivedEventArgs args)
    {
        if (!localCameraActive)
        {
            return;
        }

        Interlocked.Increment(ref captureFramesArrived);
        if (Interlocked.Exchange(ref captureFrameInFlight, 1) == 1)
        {
            Interlocked.Increment(ref captureFramesSkippedBusy);
            return;
        }

        MediaFrameReference? reference = null;
        try
        {
            if (!ShouldEncodeFrame())
            {
                Interlocked.Increment(ref captureFramesSkippedThrottle);
                Interlocked.Exchange(ref captureFrameInFlight, 0);
                return;
            }

            reference = sender.TryAcquireLatestFrame();
            VideoMediaFrame? videoFrame = reference?.VideoMediaFrame;
            if (videoFrame is null)
            {
                Interlocked.Exchange(ref captureFrameInFlight, 0);
                return;
            }

            MediaFrameReference capturedReference = reference ?? throw new InvalidOperationException("Camera reader returned no frame reference.");
            reference = null;
            // Track this Task and capture cancellation token so StopAsync can
            // await completion BEFORE disposing nativeSenderSession. Without
            // this, EncodeAndSend can push frames to a disposed handle.
            // See docs/decisions/csharp-call-task-lifecycle.md (P0 #1).
            CancellationToken processingToken = mediaCancellation?.Token ?? CancellationToken.None;
            Task processingTask = Task.Run(async () =>
            {
                long processingStartedAtMs = Environment.TickCount64;
                CapturedVideoFrame? capturedFrame = null;
                try
                {
                    if (processingToken.IsCancellationRequested)
                    {
                        return;
                    }

                    VideoMediaFrame capturedVideoFrame = capturedReference.VideoMediaFrame
                        ?? throw new InvalidOperationException("Camera frame lost its video payload.");
                    bool queueLocalPreview = ShouldRenderLocalPreview();
                    capturedFrame = await CopyCapturedFrameAsync(capturedVideoFrame, includePreview: false);
                    if (processingToken.IsCancellationRequested)
                    {
                        return;
                    }

                    EncodeAndSend(capturedFrame);
                    if (queueLocalPreview)
                    {
                        QueueLocalPreviewFrame(capturedFrame.DetachForLocalPreview());
                    }
                }
                catch (Exception ex)
                {
                    RecordError(FormatExceptionForDiagnostics(ex));
                }
                finally
                {
                    Interlocked.Increment(ref captureFramesProcessed);
                    Interlocked.Add(ref captureFrameProcessingMs, Math.Max(0, Environment.TickCount64 - processingStartedAtMs));
                    capturedFrame?.Dispose();
                    capturedReference.Dispose();
                    Interlocked.Exchange(ref captureFrameInFlight, 0);
                }
            });
            // captureFrameInFlight flag guarantees at most one OnFrameArrived
            // body runs concurrently, so this assignment is not racy.
            Volatile.Write(ref activeFrameProcessingTask, processingTask);
        }
        catch (Exception ex)
        {
            reference?.Dispose();
            Interlocked.Exchange(ref captureFrameInFlight, 0);
            RecordError(FormatExceptionForDiagnostics(ex));
        }
    }

    private void EncodeAndSend(CapturedVideoFrame frame)
    {
        if (nativeSenderSession is null)
        {
            throw new InvalidOperationException("Native WebRTC sender session is required for video.");
        }

        string? dialogKey = activeDialogKey;
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return;
        }

        CapturedVideoFrame? adaptiveEncodeFrame = null;
        CapturedVideoFrame encodeFrame = frame;
        long encodeStartedAtMs = Environment.TickCount64;
        try
        {
            adaptiveEncodeFrame = CreateAdaptiveEncodeFrame(frame);
            encodeFrame = adaptiveEncodeFrame ?? frame;
            NativeWebRtcMediaEngine.Session? currentSender = nativeSenderSession;
            if (currentSender is null)
            {
                throw new InvalidOperationException("Native WebRTC sender session is not initialized.");
            }

            // The sender's own gate, not nativeMediaSync: the decoder of the other direction must
            // never be able to stop our camera (docs/bugs/hung-decoder-froze-the-app.md).
            lock (currentSender.Gate)
            {
                // ПЕТЛЯ АДАПТАЦИИ НЕ ДОЛЖНА УНИЧТОЖАТЬ ТО, ЧЕМ ОНА УПРАВЛЯЕТ.
                //
                // Здесь стоял EnsureNativeSenderForEncodeFrame: он сравнивал размер кадра с
                // предыдущим и на любом расхождении СНОСИЛ нативную сессию и поднимал новую.
                // Ветка была обходом ограничения ядра — оно отвергало кадр не того размера, — а
                // ограничения давно нет: push_*_frame перенастраивает кодировщик на месте
                // (AdaptEncoderToFrameSizeUnsafe в omni_webrtc_media.cpp, там же и комментарий,
                // что снос сессии больше не нужен). Обход пережил своё оправдание и стал дефектом.
                //
                // Цена была ровно та, ради которой всё делалось. Размер кадра меняется ТОЛЬКО
                // когда оркестратор попросил кадр меньше — то есть в момент успеха адаптации.
                // Снос сессии уносил VideoStreamEncoder со всем состоянием: пожеланиями к
                // источнику, накопленными шагами, счётчиками сканера полосы. Пожелания
                // возвращались во всю ширину, и всё начиналось заново.
                //
                // Живой звонок 2026-08-27 (трасса стороны A из call-local-pair.ps1, 221 решение
                // ABR): сужение до 990x556 приходило ШЕСТНАДЦАТЬ раз, каждый раз держалось ОДИН
                // отсчёт (~50 мс) и откатывалось на 1280x720, а через 5.4-6.0 с — период
                // пятисекундного цикла BandwidthQualityScalerResource — сканер приходил к тому же
                // решению заново. Пила вместо лестницы 1280 -> 990 -> 766 -> 592, которую тот же
                // код даёт на стенде, где кадры подаются в одну сессию.
                PushNativeFrame(currentSender, encodeFrame);
            }

            Interlocked.Exchange(ref activeVideoWidth, encodeFrame.Width);
            Interlocked.Exchange(ref activeVideoHeight, encodeFrame.Height);
            Interlocked.Increment(ref framesSent);
            TrackEncodeGap(Environment.TickCount64);
            PublishDiagnostics();
        }
        catch (Exception ex)
        {
            throw new InvalidOperationException(
                $"native/libwebrtc {FormatCodecForDiagnostics(activeNativeCodecKind)} encode failed for {encodeFrame.Width}x{encodeFrame.Height} {encodeFrame.EncodePixelFormat} len={encodeFrame.EncodeLength}: {ex.Message}",
                ex);
        }
        finally
        {
            adaptiveEncodeFrame?.Dispose();
            Interlocked.Add(ref encodeFrameProcessingMs, Math.Max(0, Environment.TickCount64 - encodeStartedAtMs));
        }
    }

    private static void PushNativeFrame(NativeWebRtcMediaEngine.Session session, CapturedVideoFrame frame)
    {
        ReadOnlySpan<byte> buffer = frame.EncodeBuffer.AsSpan(0, frame.EncodeLength);
        if (frame.EncodePixelFormat == CallVideoPixelFormat.NV12)
        {
            int yLength = checked(frame.Width * frame.Height);
            if (buffer.Length < yLength)
            {
                throw new InvalidOperationException("NV12 frame is shorter than its Y plane.");
            }

            session.PushNv12Frame(
                buffer[..yLength],
                frame.Width,
                buffer[yLength..],
                frame.Width,
                frame.Width,
                frame.Height,
                DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() * 1000,
                forceKeyFrame: false);
            return;
        }

        if (frame.EncodePixelFormat == CallVideoPixelFormat.Bgra)
        {
            session.PushBgraFrame(
                buffer,
                checked(frame.Width * 4),
                frame.Width,
                frame.Height,
                DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() * 1000,
                forceKeyFrame: false);
            return;
        }

        throw new InvalidOperationException($"Native WebRTC sender does not support {frame.EncodePixelFormat} input.");
    }


    private CapturedVideoFrame? CreateAdaptiveEncodeFrame(CapturedVideoFrame frame)
    {
        if (!CallVideoFrameScaler.TrySelectScaledSize(
                frame.Width,
                frame.Height,
                targetCaptureWidth,
                targetCaptureHeight,
                out int scaledWidth,
                out int scaledHeight))
        {
            return null;
        }

        CallVideoFrameScaler.BgraBuffer scaledFrame;
        if (frame.EncodePixelFormat == CallVideoPixelFormat.Bgra)
        {
            scaledFrame = CallVideoFrameScaler.ScaleBgra(
                frame.EncodeBuffer,
                frame.EncodeLength,
                frame.Width,
                frame.Height,
                scaledWidth,
                scaledHeight);
        }
        else if (frame.EncodePixelFormat == CallVideoPixelFormat.NV12)
        {
            using CallVideoFrameScaler.Nv12Buffer scaledNv12Frame = CallVideoFrameScaler.ScaleNv12(
                frame.EncodeBuffer,
                frame.EncodeLength,
                frame.Width,
                frame.Height,
                scaledWidth,
                scaledHeight);
            return new CapturedVideoFrame(
                scaledNv12Frame.Width,
                scaledNv12Frame.Height,
                scaledNv12Frame.DetachBuffer(),
                scaledNv12Frame.Length,
                CallVideoPixelFormat.NV12,
                previewFrame: null);
        }
        else
        {
            return null;
        }

        return new CapturedVideoFrame(
            scaledFrame.Width,
            scaledFrame.Height,
            scaledFrame.DetachBuffer(),
            scaledFrame.Length,
            CallVideoPixelFormat.Bgra,
            previewFrame: null);
    }

    private async Task ReceiveLoopAsync(string dialogKey, CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            try
            {
                // Nothing to receive here any more — the core hands datagrams to the router. What is
                // left for the shell: build what the router asked for, and render.
                ServiceMediaRouter();
                DrainDecodedNativeFrames();
                if (activeGroupMode)
                {
                    // Nothing else refreshes the diagnostics of a member who only listens: in a 1:1
                    // call the remote render loop does it, in a group a silent member has no such
                    // loop and its counters froze at join (live pass 2026-10-04). Only a flag — the
                    // text is still built on the diagnostics loop's 500 ms cadence.
                    PublishDiagnostics();
                }
                await Task.Delay(10, token);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                RecordError(FormatExceptionForDiagnostics(ex));
                await Task.Delay(100, token);
            }
        }
    }



    private long ReadNativeSenderDiagnosticLong(string propertyName)
    {
        NativeWebRtcMediaEngine.Session? sender;
        lock (nativeMediaSync)
        {
            sender = nativeSenderSession;
        }

        if (sender is null)
        {
            return 0;
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(sender.GetDiagnosticsJson());
            ulong value = ReadNativeDiagnosticsUInt64(document.RootElement, propertyName);
            return (long)Math.Min(value, long.MaxValue);
        }
        catch
        {
            return 0;
        }
    }


    /// Register (or replace) the Image a participant tile renders into. Called from the tile's
    /// Loaded handler on every tiles rebuild — replacing drops the stale bitmap so a fresh one
    /// binds to the new Image on the next frame.
    internal void RegisterGroupTileImage(string tileKey, Image image)
    {
        if (string.IsNullOrEmpty(tileKey))
        {
            return;
        }

        lock (groupTileSync)
        {
            groupTiles[tileKey] = new GroupTileTarget { Image = image };
        }
    }

    /// Increment 2 — group-call speaking indicator. Returns the smoothed audio RMS (0..1) of the
    /// endpoint behind a tile key. Self (<see cref="GroupSelfTileKey"/>) reads the local mic
    /// capture level; a remote key reads that sender's decode-only level probe. Returns 0 for an
    /// unknown key or before the sender's first audio packet. Never throws — safe on the UI poll.
    internal float ReadGroupTileSpeakingLevel(string tileKey)
    {
        if (string.IsNullOrEmpty(tileKey))
        {
            return 0f;
        }

        if (string.Equals(tileKey, GroupSelfTileKey, StringComparison.OrdinalIgnoreCase))
        {
            return nativeAudioCaptureSession?.ReadLevel() ?? 0f;
        }

        // UI poll: the shared lock is held while a group receiver is being built (~750 ms); a
        // level missed this poll is read on the next one.
        if (!Monitor.TryEnter(nativeMediaSync))
        {
            return 0f;
        }

        try
        {
            return groupAudioLevelProbes.TryGetValue(tileKey, out NativeWebRtcMediaEngine.AudioPlaybackSession? probe)
                ? probe.ReadLevel()
                : 0f;
        }
        finally
        {
            Monitor.Exit(nativeMediaSync);
        }
    }

    /// <summary>
    /// Build the sessions the router asked for since the last pass: the 1:1 video receiver once
    /// the peer's first video arrives, and a receiver + level probe per group sender whose media
    /// arrived with nobody to take it. Packets before that are lost, as they always were.
    /// </summary>
    private void ServiceMediaRouter()
    {
        NativeWebRtcMediaEngine.MediaRouter? router = activeMediaRouter;
        if (router is null)
        {
            return;
        }

        (bool videoReceiverWanted, IReadOnlyList<(byte[] Tag, byte VideoPayloadType)> groupSenders) = router.TakeRequests();
        if (videoReceiverWanted)
        {
            EnsureNativeVideoReceiverForInboundVideo();
        }

        if (groupSenders.Count == 0)
        {
            return;
        }

        lock (nativeMediaSync)
        {
            foreach ((byte[] tag, byte videoPayloadType) in groupSenders)
            {
                // Our own format isn't agreed yet (a joiner hears the others ~3 s before its media
                // is up): registering now would wire this sender WITHOUT a video receiver, and the
                // router never asks about a registered sender again — the joiner saw no video for
                // the whole call (live pass 2026-10-04). Leave it unregistered; its next packet asks
                // again. Its audio plays meanwhile through the shared playback.
                if (!GroupSenderCanBeWired(lastReceiverSelection))
                {
                    continue;
                }

                string senderTagHex = Convert.ToHexString(tag).ToLowerInvariant();
                NativeWebRtcMediaEngine.Session? replaced = TakeGroupReceiverBuiltForOtherCodec(senderTagHex, videoPayloadType);
                router.SetGroupSender(
                    tag,
                    GetOrCreateGroupReceiverSession(senderTagHex, videoPayloadType),
                    GetOrCreateGroupAudioProbe(senderTagHex));
                // Detached by the call above (it returns with no delivery in flight).
                replaced?.Dispose();
            }
        }
    }

    /// <summary>
    /// The call's inbound router, wired to the core: from here on every datagram of the dialog is
    /// handed to it at once, on the core's thread.
    /// </summary>
    private static NativeWebRtcMediaEngine.MediaRouter CreateWiredMediaRouter(
        IntPtr handle,
        DialogKeyPin pin,
        bool groupMode)
    {
        if (!NativeWebRtcMediaEngine.MediaRouter.TryCreate(out NativeWebRtcMediaEngine.MediaRouter? router, out string diagnostics) ||
            router is null)
        {
            throw new InvalidOperationException($"native media router unavailable: {diagnostics}");
        }

        try
        {
            router.SetGroupMode(groupMode);
            IntPtr error = Core.NativeMethods.omni_realtime_media_set_inbound_sink(
                handle,
                pin.Pointer,
                pin.Length,
                router.DeliverFunction,
                router.Handle);
            if (error != IntPtr.Zero)
            {
                throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
            }
        }
        catch
        {
            router.Dispose();
            throw;
        }

        return router;
    }

    private static void UnwireInboundMedia(IntPtr handle, DialogKeyPin? pin)
    {
        if (handle == IntPtr.Zero || pin is null)
        {
            return;
        }

        IntPtr error = Core.NativeMethods.omni_realtime_media_set_inbound_sink(
            handle,
            pin.Pointer,
            pin.Length,
            IntPtr.Zero,
            IntPtr.Zero);
        if (error != IntPtr.Zero)
        {
            NativeErrorReader.ReadAndFree(error);
        }
    }

    /// The core's dialog-key mark for a group receiver's RTCP target (`group:<id>#fb:<sender tag>`,
    /// see `group_feedback_from_dialog_key` in omni-app).
    internal const string GroupFeedbackKeyMark = "#fb:";

    /// Send a group receiver's RTCP back to the sender it decodes — the core decides what of it
    /// goes (the sender's next hop sends everything, the rest only repair requests). Without it the
    /// sender heard nothing: its encoder sat on the 150 kbps floor and no keyframe request ever
    /// reached it (docs/bugs/group-call-joiner-saw-no-video.md). Caller holds nativeMediaSync.
    private void AttachGroupFeedbackSink(NativeWebRtcMediaEngine.Session receiver, string senderTagHex)
    {
        IntPtr handle = nativeHandle;
        string? dialogKey = activeDialogKey;
        if (handle == IntPtr.Zero || dialogKey is null)
        {
            return;
        }

        // The core copies the key into its route, so the pin only has to live through the call.
        using DialogKeyPin pin = new($"{dialogKey}{GroupFeedbackKeyMark}{senderTagHex}");
        ulong targetId = CreatePacketTarget(handle, pin);
        groupFeedbackTargetIds.Add(targetId);
        receiver.SetPacketSink(Core.NativeMethods.RealtimeMediaPacketSinkFunction, (IntPtr)(long)targetId);
    }

    /// Whether a group sender the router asked about can be wired now: only once our own media
    /// format is agreed, since its video receiver is built from that selection.
    internal static bool GroupSenderCanBeWired(RealtimeMediaSelection? agreedSelection) => agreedSelection is not null;

    /// One receiver session per group sender (caller holds nativeMediaSync). Sessions are built
    /// from the group's self-negotiated selection (stored by EnsureNativeReceiverForSelection);
    /// packets arriving before the selection is committed are dropped — the sender retransmits
    /// via keyframes anyway. Bounded to keep a hostile/huge call from exhausting decoders.
    private NativeWebRtcMediaEngine.Session? GetOrCreateGroupReceiverSession(string senderTagHex, byte videoPayloadType)
    {
        if (groupReceiverSessions.TryGetValue(senderTagHex, out NativeWebRtcMediaEngine.Session? existing))
        {
            return existing;
        }

        // An audio-only group selection has no video to decode, so there is no receiver
        // to build — the shared audio playback handle already carries this sender.
        if (lastReceiverSelection?.Video is not RealtimeVideoSelection video ||
            lastReceiverSelection.ReceiveStream is not RealtimeVideoStream stream ||
            groupReceiverSessions.Count >= MaxGroupReceiverSessions)
        {
            return null;
        }

        // Each group sender picks its own codec (per-pair-video-codec-selection.md §9, phase 4a): the
        // receiver is built for what its stream carries, H.264 before any video has come.
        RealtimeVideoStream senderStream = stream with
        {
            Codec = GroupStreamCodecFromPayloadType(videoPayloadType),
            PayloadType = videoPayloadType != 0 ? videoPayloadType : (byte)CallVideoRtpConstants.H264PayloadType,
        };
        NativeWebRtcMediaEngine.Session created =
            CreateReceiverSessionForSelection(video, senderStream, out string diagnostics);
        AttachGroupFeedbackSink(created, senderTagHex);
        groupReceiverSessions[senderTagHex] = created;
        groupReceiverPayloadTypes[senderTagHex] = senderStream.PayloadType;
        nativeMediaDiagnostics = $"group_receiver[{senderTagHex}]=ready; {diagnostics}";
        return created;
    }

    /// <summary>
    /// The group receiver of <paramref name="senderTagHex"/> when it was built for another codec than
    /// the sender's video now carries — taken out of the map so a new one is built; the caller
    /// disposes it once the router no longer holds it. <c>null</c> when it fits or there is none.
    /// Caller holds nativeMediaSync.
    /// </summary>
    private NativeWebRtcMediaEngine.Session? TakeGroupReceiverBuiltForOtherCodec(string senderTagHex, byte videoPayloadType)
    {
        if (videoPayloadType == 0 ||
            !groupReceiverSessions.TryGetValue(senderTagHex, out NativeWebRtcMediaEngine.Session? existing) ||
            groupReceiverPayloadTypes.GetValueOrDefault(senderTagHex) == videoPayloadType)
        {
            return null;
        }

        groupReceiverSessions.Remove(senderTagHex);
        groupReceiverPayloadTypes.Remove(senderTagHex);
        Core.AppTrace.Write(
            "media/group-receiver",
            $"sender={senderTagHex} codec -> {GroupStreamCodecFromPayloadType(videoPayloadType)}: receiver rebuilt");
        return existing;
    }

    /// <summary>The codec name of a group sender's video payload type (H.264 when unknown).</summary>
    internal static string GroupStreamCodecFromPayloadType(byte payloadType) => payloadType switch
    {
        CallVideoRtpConstants.Av1PayloadType => "av1",
        CallVideoRtpConstants.H265PayloadType => "h265",
        _ => "h264",
    };

    /// One decode-only audio level probe per group sender (caller holds nativeMediaSync). Bounded
    /// like the video receivers. A failed create (e.g. no decoder) returns null and is simply not
    /// retried this packet — the next packet tries again; audio still renders via the shared handle.
    private NativeWebRtcMediaEngine.AudioPlaybackSession? GetOrCreateGroupAudioProbe(string senderTagHex)
    {
        if (groupAudioLevelProbes.TryGetValue(senderTagHex, out NativeWebRtcMediaEngine.AudioPlaybackSession? existing))
        {
            return existing;
        }

        if (groupAudioLevelProbes.Count >= MaxGroupReceiverSessions)
        {
            return null;
        }

        if (!NativeWebRtcMediaEngine.TryCreateAudioPlayback(
                out NativeWebRtcMediaEngine.AudioPlaybackSession? probe,
                out _,
                renderEnabled: false) ||
            probe is null)
        {
            return null;
        }

        groupAudioLevelProbes[senderTagHex] = probe;
        return probe;
    }

    /// Drain each sender's decoded frames and render the newest into that sender's tile — the group
    /// twin of DrainDecodedNativeFrames (software path only; tiles are small, no GPU swapchain).
    private void DrainGroupDecodedFrames()
    {
        List<(string TileKey, NativeWebRtcDecodedBgraFrame Frame)>? newestPerSender = null;
        List<(string TileKey, NativeWebRtcMediaEngine.Session Session)> sessions;
        lock (nativeMediaSync)
        {
            sessions = groupReceiverSessions.Select(pair => (pair.Key, pair.Value)).ToList();
        }

        // Each sender's decode under its own gate, outside the shared lock: one sender's decoder
        // stuck in a driver costs that sender's tile, not every tile and the encoder with them.
        foreach ((string tileKey, NativeWebRtcMediaEngine.Session session) in sessions)
        {
            if (!Monitor.TryEnter(session.Gate))
            {
                continue;
            }

            NativeWebRtcDecodedBgraFrame? newest = null;
            try
            {
                while (session.TryDrainBgraFrame(out NativeWebRtcDecodedBgraFrame decoded) &&
                       decoded.Buffer.Length > 0)
                {
                    newest = decoded;
                }
            }
            catch (ObjectDisposedException)
            {
                // Torn down between the snapshot and the drain.
            }
            finally
            {
                Monitor.Exit(session.Gate);
            }

            if (newest is { } frame)
            {
                (newestPerSender ??= new()).Add((tileKey, frame));
            }
        }

        if (newestPerSender is null)
        {
            return;
        }

        foreach ((string tileKey, NativeWebRtcDecodedBgraFrame decoded) in newestPerSender)
        {
            remoteVideoSeen = true;
            Interlocked.Increment(ref framesReceived);
            BgraFrame frame = new(
                decoded.Width,
                decoded.Height,
                decoded.Buffer,
                decoded.Buffer.Length,
                returnToPool: false);
            _ = RenderGroupTileOwnedFrameAsync(tileKey, frame);
        }
    }

    /// Render an OWNED frame into a tile and dispose it (fire-and-forget from the drain).
    private async Task RenderGroupTileOwnedFrameAsync(string tileKey, BgraFrame frame)
    {
        try
        {
            await RenderIntoGroupTileAsync(tileKey, frame, remote: true);
        }
        catch (Exception ex)
        {
            RecordError($"Group tile render failed: {FormatExceptionForDiagnostics(ex)}");
        }
        finally
        {
            frame.Dispose();
        }
    }

    /// Blit a frame into a registered tile's Image on the UI thread (borrowed frame — the caller
    /// keeps ownership). No-op when the tile isn't registered (call UI not open / member left).
    /// A remote member's frame counts as rendered — the group twin of the 1:1 render counter, so
    /// `rendered=` means the same thing in both kinds of call.
    private Task RenderIntoGroupTileAsync(string tileKey, BgraFrame frame, bool remote = false)
        => RunOnUiAsync(() =>
        {
            GroupTileTarget? target;
            lock (groupTileSync)
            {
                groupTiles.TryGetValue(tileKey, out target);
            }

            if (target?.Image is not Image image)
            {
                return;
            }

            if (target.Bitmap is null ||
                target.Bitmap.PixelWidth != frame.Width ||
                target.Bitmap.PixelHeight != frame.Height)
            {
                target.Bitmap = new WriteableBitmap(frame.Width, frame.Height);
                image.Source = target.Bitmap;
            }

            using Stream stream = target.Bitmap.PixelBuffer.AsStream();
            stream.Seek(0, SeekOrigin.Begin);
            stream.Write(frame.Buffer, 0, frame.Length);
            target.Bitmap.Invalidate();
            if (remote)
            {
                Interlocked.Increment(ref framesRendered);
                PublishDiagnostics();
            }
        });

    private void DrainDecodedNativeFrames()
    {
        // Group call (T3b): decoded frames live in the per-sender sessions, each rendering into
        // its own tile — the single-receiver / GPU paths below are 1:1-only.
        if (activeGroupMode)
        {
            DrainGroupDecodedFrames();
            return;
        }

        // GPU path (#5 increment 3b): don't pull BGRA across FFI. The native D3D11 renderer presents
        // the newest decoded frame straight into the panel's swapchain — on the presenter thread,
        // on the display's clock. This call only makes sure that thread exists for the current
        // decoder; it does NOT hand it this batch. Presentation stopped being driven by packet
        // arrival on purpose: see docs/decisions/video-presentation-thread.md. Falls back to the
        // BGRA-drain loop below if the GPU renderer can't attach (gpuVideoEnabled is flipped false
        // in StartRemoteGpuPresenterAsync).
        if (gpuVideoEnabled)
        {
            EnsureRemoteGpuPresenter();
            return;
        }

        while (true)
        {
            NativeWebRtcMediaEngine.Session? receiver = nativeReceiverSession;
            if (receiver is null || receiver.IsAbandoned)
            {
                return;
            }

            // TryEnter, not lock: this thread also serves the router. Busy means the presenter or
            // a stuck decode is inside — the next pass tries again.
            if (!Monitor.TryEnter(receiver.Gate))
            {
                return;
            }

            NativeWebRtcDecodedBgraFrame decoded = default;
            bool drained;
            try
            {
                drained = ReferenceEquals(nativeReceiverSession, receiver) &&
                          CallIntoReceiver(receiver, () => receiver.TryDrainBgraFrame(out decoded));
            }
            finally
            {
                Monitor.Exit(receiver.Gate);
            }

            if (!drained)
            {
                return;
            }

            if (decoded.Buffer.Length == 0)
            {
                return;
            }

            remoteVideoSeen = true;
            Interlocked.Increment(ref framesReceived);
            Interlocked.Exchange(ref decodedVideoWidth, decoded.Width);
            Interlocked.Exchange(ref decodedVideoHeight, decoded.Height);
            QueueRemoteRenderFrame(new BgraFrame(
                decoded.Width,
                decoded.Height,
                decoded.Buffer,
                decoded.Buffer.Length,
                returnToPool: false));
        }
    }

    private Task RenderFrameAsync(Image image, bool isLocalPreview, BgraFrame frame)
        => RunOnUiAsync(() =>
        {
            if (isLocalPreview)
            {
                RenderFrameToImage(image, ref localPreviewBitmap, frame);
            }
            else
            {
                RenderFrameToImage(image, ref remoteVideoBitmap, frame);
            }
        });

    private static void RenderFrameToImage(Image image, ref WriteableBitmap? bitmap, BgraFrame frame)
    {
        if (bitmap is null ||
            bitmap.PixelWidth != frame.Width ||
            bitmap.PixelHeight != frame.Height)
        {
            bitmap = new WriteableBitmap(frame.Width, frame.Height);
            image.Source = bitmap;
        }

        using Stream stream = bitmap.PixelBuffer.AsStream();
        stream.Seek(0, SeekOrigin.Begin);
        stream.Write(frame.Buffer, 0, frame.Length);
        bitmap.Invalidate();
    }

    /// <summary>
    /// Marshal onto the UI thread. Everything left here is genuinely UI work — binding the
    /// swapchain to the panel, resizing it, blitting a WriteableBitmap, updating the diagnostics
    /// text. Presenting remote video used to be on this queue too, at raised priority to get it
    /// past the shell's refresh work; it now runs on its own thread instead, which is what the
    /// priority was papering over. See docs/decisions/video-presentation-thread.md.
    /// </summary>
    private Task RunOnUiAsync(Action action)
    {
        TaskCompletionSource completion = new(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!dispatcherQueue.TryEnqueue(() =>
        {
            try
            {
                action();
                completion.SetResult();
            }
            catch (Exception ex)
            {
                completion.SetException(ex);
            }
        }))
        {
            // The dispatcher is gone — the window is closing. There is no UI left to update, so
            // this counts as done rather than as a failure: throwing here would abort a media
            // teardown that has real native handles left to release.
            completion.TrySetResult();
        }

        return completion.Task;
    }



    private static string ReadNativeDiagnosticsString(JsonElement root, string propertyName, string fallback)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.String
            ? element.GetString() ?? fallback
            : fallback;

    private static string ReadNativeDiagnosticsStringArray(JsonElement root, string propertyName)
    {
        if (!root.TryGetProperty(propertyName, out JsonElement element) ||
            element.ValueKind != JsonValueKind.Array)
        {
            return string.Empty;
        }

        List<string> values = [];
        foreach (JsonElement item in element.EnumerateArray())
        {
            if (item.ValueKind == JsonValueKind.String &&
                item.GetString() is { Length: > 0 } value)
            {
                values.Add(value);
            }
        }

        return string.Join(',', values);
    }

    private static uint ReadNativeDiagnosticsUInt32(JsonElement root, string propertyName)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.Number &&
           element.TryGetUInt32(out uint value)
            ? value
            : 0;

    private static int ReadNativeDiagnosticsInt32(JsonElement root, string propertyName)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.Number &&
           element.TryGetInt32(out int value)
            ? value
            : 0;

    private static ulong ReadNativeDiagnosticsUInt64(JsonElement root, string propertyName)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.Number &&
           element.TryGetUInt64(out ulong value)
            ? value
            : 0;

    // Phase A removal (2026-05-18): MaybeScheduleCaptureProfileFallback /
    // MaybeScheduleCaptureProfileRecovery + CallVideoCaptureProfileAdaptation
    // were deleted. Capture FPS is a camera-sensor signal (low light raises
    // exposure → fewer frames/sec) and is the wrong trigger for resolution
    // change. Resolution adaptation is now native-driven (Phase B: libwebrtc
    // QualityScaler + OveruseFrameDetector → OmniAbrTarget.width/height).
    // See docs/decisions/webrtc-quality-scaler-adoption.md.

    // Phase B (2026-05-18): react to a native resolution decision.
    // Contract: native always returns target.Width/Height ≤ negotiated. When
    // target == negotiated, we clear the cap so the camera reverts to its
    // full profile. When target < negotiated, we restart capture under that
    // cap. The single-flight gate avoids overlapping restarts when native
    // toggles back and forth across an UpdateAdaptiveBitrate tick.
    private void MaybeScheduleCaptureReconfigure(int targetWidth, int targetHeight)
    {
        if (targetWidth <= 0 || targetHeight <= 0)
        {
            return;
        }

        int negotiatedWidth = Volatile.Read(ref negotiatedCaptureWidth);
        int negotiatedHeight = Volatile.Read(ref negotiatedCaptureHeight);
        int activeWidth = Volatile.Read(ref activeCaptureProfileWidth);
        int activeHeight = Volatile.Read(ref activeCaptureProfileHeight);
        if (activeWidth <= 0 || activeHeight <= 0 || negotiatedWidth <= 0 || negotiatedHeight <= 0)
        {
            return;
        }

        // Clamp target to negotiated bounds — native should already do this
        // but defense in depth is cheap.
        int boundedWidth = Math.Min(targetWidth, negotiatedWidth);
        int boundedHeight = Math.Min(targetHeight, negotiatedHeight);
        bool wantsCap = boundedWidth < negotiatedWidth || boundedHeight < negotiatedHeight;
        int desiredCapWidth = wantsCap ? boundedWidth : 0;
        int desiredCapHeight = wantsCap ? boundedHeight : 0;

        int currentCapWidth = Volatile.Read(ref captureProfileMaxWidth);
        int currentCapHeight = Volatile.Read(ref captureProfileMaxHeight);
        if (desiredCapWidth == currentCapWidth && desiredCapHeight == currentCapHeight)
        {
            // Cap already matches native intent. No action.
            return;
        }

        // Coalesce concurrent reconfigure requests behind a single-flight
        // gate so we don't pile two RestartWinRtCaptureAsync onto the queue.
        if (Interlocked.Exchange(ref captureReconfigureInFlight, 1) == 1)
        {
            return;
        }

        CancellationToken token = mediaCancellation?.Token ?? CancellationToken.None;
        Volatile.Write(ref captureReconfigureTask, Task.Run(async () =>
        {
            try
            {
                if (token.IsCancellationRequested)
                {
                    return;
                }

                await RestartWinRtCaptureUnderCapAsync(desiredCapWidth, desiredCapHeight);
            }
            catch (OperationCanceledException)
            {
            }
            catch (Exception ex)
            {
                RecordError($"Native-driven capture reconfigure failed: {FormatExceptionForDiagnostics(ex)}");
            }
            finally
            {
                Interlocked.Exchange(ref captureReconfigureInFlight, 0);
            }
        }));
    }

    // Phase B (2026-05-18): replaces the deleted RestartWinRtCaptureWithProfileCapAsync.
    // maxWidth == 0 && maxHeight == 0 means "clear the cap" (revert to full
    // negotiated profile). Otherwise the WinRT pipeline restarts under the
    // given upper bound. Caller MUST own the captureReconfigureInFlight gate.
    private async Task RestartWinRtCaptureUnderCapAsync(int maxWidth, int maxHeight)
    {
        CancellationToken token;
        lock (lifecycleSync)
        {
            if (!localCameraActive ||
                mediaCapture is null ||
                captureCancellation is null)
            {
                return;
            }

            token = captureCancellation.Token;
        }

        MediaFrameReader? reader;
        MediaCapture? capture;
        lock (lifecycleSync)
        {
            reader = frameReader;
            capture = mediaCapture;
            frameReader = null;
            mediaCapture = null;
            captureStartedAtMs = 0;
            activeCaptureProfileWidth = 0;
            activeCaptureProfileHeight = 0;
            activeCaptureProfileFps = 0;
        }

        if (reader is not null)
        {
            reader.FrameArrived -= OnFrameArrived;
            try
            {
                await reader.StopAsync().AsTask();
            }
            catch
            {
                // Reconfigure must still proceed even if the old reader is mid-stop.
            }

            reader.Dispose();
        }

        capture?.Dispose();
        await WaitForCaptureFrameInFlightAsync();

        Volatile.Write(ref captureProfileMaxWidth, maxWidth);
        Volatile.Write(ref captureProfileMaxHeight, maxHeight);
        try
        {
            await StartWinRtCaptureWithTimeoutAsync(token, captureFailurePrefix: "capture_unavailable");
            ResetCaptureRateCounters();
            ForceKeyFrame();
            PublishDiagnostics(force: true);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            string failure = FormatExceptionForDiagnostics(ex);
            Volatile.Write(ref captureProfileMaxWidth, 0);
            Volatile.Write(ref captureProfileMaxHeight, 0);
            try
            {
                await StartWinRtCaptureWithTimeoutAsync(token, captureFailurePrefix: "capture_unavailable");
                ResetCaptureRateCounters();
                PublishDiagnostics(force: true);
            }
            catch (Exception restoreEx) when (restoreEx is not OperationCanceledException)
            {
                RecordError(
                    $"Native-driven capture reconfigure failed ({failure}) and original profile did not restore: " +
                    $"{FormatExceptionForDiagnostics(restoreEx)}");
                throw;
            }
        }
    }

    // -1 держим отдельным ответом, а не нулём: «не измеряли» и «ноль миллисекунд» — разные
    // утверждения, и подмена одного другим уже стоила этому проекту разбора (счётчик L2).
    private string FormatMeasuredPathRtt()
    {
        long us = Interlocked.Read(ref diagnosticsMeasuredPathRttUs);
        return us < 0 ? "n/a" : $"{us / 1000}ms";
    }

    private string BuildControlDiagnostics()
        => $" | rtcp pathRtt={FormatMeasuredPathRtt()} twccIn={ReadRouterCounters().Feedback}{BuildPacketTargetDiagnostics()}";

    /// <summary>
    /// What the core did with this call's outgoing packets: took, refused (no negotiation yet, no
    /// link), dropped on purpose by the stand knob OMNI_MEDIA_TEST_PACKET_LOSS_PERCENT.
    /// </summary>
    /// <summary>What the router took from the core: all / delivery reports / dropped as malformed.</summary>
    private (ulong Delivered, ulong Feedback, ulong Dropped) ReadRouterCounters()
        => activeMediaRouter?.ReadCounters() ?? (0, 0, 0);

    private string BuildPacketTargetDiagnostics()
    {
        ulong targetId = activePacketTargetId;
        if (targetId == 0)
        {
            return " out=none";
        }

        Core.NativeMethods.omni_realtime_media_packet_target_counters(
            targetId,
            out ulong accepted,
            out ulong refused,
            out ulong testDropped);
        return $" out={accepted}p/{refused}refused testDrop={testDropped}";
    }

    /// <summary>
    /// The core's send target for this call. Created before any engine session, so each one can
    /// be wired to it the moment it exists (<see cref="AttachPacketSink(NativeWebRtcMediaEngine.Session?)"/>).
    /// </summary>
    private static ulong CreatePacketTarget(IntPtr handle, DialogKeyPin pin)
    {
        IntPtr error = Core.NativeMethods.omni_realtime_media_packet_target_create(
            handle,
            pin.Pointer,
            pin.Length,
            out ulong targetId);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }

        return targetId;
    }

    private void AttachPacketSink(NativeWebRtcMediaEngine.Session? session)
        => session?.SetPacketSink(
            Core.NativeMethods.RealtimeMediaPacketSinkFunction,
            (IntPtr)(long)activePacketTargetId);

    private void AttachPacketSink(NativeWebRtcMediaEngine.AudioCaptureSession capture)
        => capture.SetPacketSink(
            Core.NativeMethods.RealtimeMediaPacketSinkFunction,
            (IntPtr)(long)activePacketTargetId);

    private string BuildProcessingDiagnostics()
    {
        long received = Interlocked.Read(ref framesReceived);
        long rendered = Interlocked.Read(ref framesRendered);
        long encodeMs = CurrentAverageEncodeMs();
        long decodeMs = received == 0 ? 0 : Interlocked.Read(ref decodeFrameProcessingMs) / received;
        long renderMs = rendered == 0 ? 0 : Interlocked.Read(ref renderFrameProcessingMs) / rendered;
        long captureMs = CurrentAverageCaptureProcessingMs();
        // presentGap belongs here, not next to `remote=`: that section is skipped entirely on the
        // GPU path (it needs decoded dimensions, which only the software drain records), so the
        // one number that says whether the picture on screen is smooth was invisible exactly
        // where the picture actually is. See docs/bugs/media-hitching.md.
        //
        // Read-and-reset for the window figure, same as capture's gapMax: <this window>/<the whole
        // call>. Only the all-time number was published at first, and it cannot answer the question
        // being asked — a 200 ms figure that never moves again is video start-up, a 200 ms figure
        // that keeps coming back is a stutter, and both read identically.
        return $" | proc enc={encodeMs}ms cap={captureMs}ms dec={decodeMs}ms render={renderMs}ms " +
               $"presentGap={Interlocked.Exchange(ref renderGapMaxMs, 0)}ms/{Interlocked.Read(ref renderGapWorstMs)}ms " +
               $"decodeStall={Interlocked.Read(ref decodeStalls)} renderStall={Interlocked.Read(ref renderStalls)}";
    }

    private int CurrentAverageEncodeMs()
    {
        long sent = Interlocked.Read(ref framesSent);
        return sent == 0
            ? 0
            : (int)Math.Clamp(Interlocked.Read(ref encodeFrameProcessingMs) / sent, 0, int.MaxValue);
    }

    private int CurrentAverageCaptureProcessingMs()
    {
        long processed = Interlocked.Read(ref captureFramesProcessed);
        return processed == 0
            ? 0
            : (int)Math.Clamp(Interlocked.Read(ref captureFrameProcessingMs) / processed, 0, int.MaxValue);
    }

    private DiagnosticsRateSnapshot GetDiagnosticsRateSnapshot(
        long nowMs,
        long sentFrames,
        long renderedFrames,
        long encodedBytes,
        long captureFrames)
    {
        lock (diagnosticsRateSync)
        {
            long sampleAtMs = diagnosticsSampleAtMs;
            if (sampleAtMs <= 0)
            {
                diagnosticsSampleAtMs = nowMs;
                diagnosticsSampleFramesSent = sentFrames;
                diagnosticsSampleFramesRendered = renderedFrames;
                diagnosticsSampleEncodedBytesSent = encodedBytes;
                diagnosticsSampleCaptureFramesArrived = captureFrames;
            }
            else
            {
                long elapsedMs = nowMs - sampleAtMs;
                if (elapsedMs >= 1_000)
                {
                    diagnosticsCurrentSentFps =
                        Math.Max(0, sentFrames - diagnosticsSampleFramesSent) * 1000 / elapsedMs;
                    diagnosticsCurrentRenderedFps =
                        Math.Max(0, renderedFrames - diagnosticsSampleFramesRendered) * 1000 / elapsedMs;
                    diagnosticsCurrentSentKbps =
                        Math.Max(0, encodedBytes - diagnosticsSampleEncodedBytesSent) * 8 / elapsedMs;
                    diagnosticsCurrentCaptureLoopFps =
                        Math.Max(0, captureFrames - diagnosticsSampleCaptureFramesArrived) * 1000 / elapsedMs;

                    diagnosticsSampleAtMs = nowMs;
                    diagnosticsSampleFramesSent = sentFrames;
                    diagnosticsSampleFramesRendered = renderedFrames;
                    diagnosticsSampleEncodedBytesSent = encodedBytes;
                    diagnosticsSampleCaptureFramesArrived = captureFrames;
                }
            }

            return new DiagnosticsRateSnapshot(
                diagnosticsCurrentSentFps,
                diagnosticsCurrentRenderedFps,
                diagnosticsCurrentSentKbps,
                diagnosticsCurrentCaptureLoopFps);
        }
    }

    /// Remember the longest gap between two encodes in this diagnostics window.
    private void TrackEncodeGap(long nowMs)
    {
        long previousMs = Interlocked.Exchange(ref lastEncodeAtMs, nowMs);
        if (previousMs <= 0)
        {
            return;
        }

        long gapMs = nowMs - previousMs;
        RaiseMaxTo(ref encodeGapMaxMs, gapMs);
        RaiseMaxTo(ref encodeGapWorstMs, gapMs);
    }

    private static void RaiseMaxTo(ref long target, long candidateMs)
    {
        long observedMs = Interlocked.Read(ref target);
        while (candidateMs > observedMs)
        {
            long raced = Interlocked.CompareExchange(ref target, candidateMs, observedMs);
            if (raced == observedMs)
            {
                return;
            }

            observedMs = raced;
        }
    }

    private string BuildCaptureLoopDiagnostics(long captureLoopFps)
    {
        long avgMs = CurrentAverageCaptureProcessingMs();
        return $" loop={captureLoopFps}fps skipT={Interlocked.Read(ref captureFramesSkippedThrottle)} skipB={Interlocked.Read(ref captureFramesSkippedBusy)} proc={avgMs}ms";
    }

    private static string BuildVideoDiagnostics(string label, long width, long height, long fps, long? kbps)
    {
        if (width <= 0 || height <= 0)
        {
            return string.Empty;
        }

        string bitrate = kbps.HasValue ? $" {kbps.Value}kbps" : string.Empty;
        return $" | {label}={width}x{height}@{fps}fps{bitrate}";
    }

    private readonly record struct DiagnosticsRateSnapshot(
        long SentFps,
        long RenderedFps,
        long SentKbps,
        long CaptureLoopFps);

    private readonly record struct CaptureProfileCap(int MaxWidth, int MaxHeight, double Aspect)
    {
        internal bool IsActive => MaxWidth > 0 && MaxHeight > 0;
    }

    private void ResetCaptureRateCounters()
    {
        long nowMs = Environment.TickCount64;
        Interlocked.Exchange(ref captureFramesArrived, 0);
        Interlocked.Exchange(ref captureFramesSkippedThrottle, 0);
        Interlocked.Exchange(ref captureFramesSkippedBusy, 0);
        Interlocked.Exchange(ref captureFramesProcessed, 0);
        Interlocked.Exchange(ref captureFrameProcessingMs, 0);
        lock (diagnosticsRateSync)
        {
            diagnosticsSampleAtMs = nowMs;
            diagnosticsSampleFramesSent = Interlocked.Read(ref framesSent);
            diagnosticsSampleFramesRendered = Interlocked.Read(ref framesRendered);
            diagnosticsSampleEncodedBytesSent = ReadNativeSenderDiagnosticLong("rtp_octets_sent");
            diagnosticsSampleCaptureFramesArrived = 0;
            diagnosticsCurrentCaptureLoopFps = 0;
        }
    }

    private void ResetMediaCounters()
    {
        long nowMs = Environment.TickCount64;
        Interlocked.Exchange(ref framesSent, 0);
        Interlocked.Exchange(ref lastEncodeAtMs, 0);
        Interlocked.Exchange(ref encodeGapMaxMs, 0);
        Interlocked.Exchange(ref encodeGapWorstMs, 0);
        Interlocked.Exchange(ref renderGapMaxMs, 0);
        Interlocked.Exchange(ref renderGapWorstMs, 0);
        Interlocked.Exchange(ref framesReceived, 0);
        Interlocked.Exchange(ref framesRendered, 0);
        Interlocked.Exchange(ref decodeStalls, 0);
        Interlocked.Exchange(ref renderStalls, 0);
        Interlocked.Exchange(ref encodeFrameProcessingMs, 0);
        Interlocked.Exchange(ref decodeFrameProcessingMs, 0);
        Interlocked.Exchange(ref renderFrameProcessingMs, 0);
        Interlocked.Exchange(ref captureFramesArrived, 0);
        Interlocked.Exchange(ref captureFramesSkippedThrottle, 0);
        Interlocked.Exchange(ref captureFramesSkippedBusy, 0);
        Interlocked.Exchange(ref captureFramesProcessed, 0);
        Interlocked.Exchange(ref captureFrameProcessingMs, 0);
        Interlocked.Exchange(ref activeVideoWidth, 0);
        Interlocked.Exchange(ref activeVideoHeight, 0);
        Interlocked.Exchange(ref decodedVideoWidth, 0);
        Interlocked.Exchange(ref decodedVideoHeight, 0);
        Interlocked.Exchange(ref diagnosticsRemoteLossFraction, 0);
        Interlocked.Exchange(ref diagnosticsMeasuredPathRttUs, -1);
        Interlocked.Exchange(ref nextDiagnosticsPublishAtMs, 0);
        Interlocked.Exchange(ref lastRemoteFrameRenderedAtMs, 0);
        lock (diagnosticsRateSync)
        {
            diagnosticsSampleAtMs = nowMs;
            diagnosticsSampleFramesSent = 0;
            diagnosticsSampleFramesRendered = 0;
            diagnosticsSampleEncodedBytesSent = 0;
            diagnosticsSampleCaptureFramesArrived = 0;
            diagnosticsCurrentSentFps = 0;
            diagnosticsCurrentRenderedFps = 0;
            diagnosticsCurrentSentKbps = 0;
            diagnosticsCurrentCaptureLoopFps = 0;
        }
    }

    private bool ShouldRenderLocalPreview()
    {
        long nowMs = Environment.TickCount64;
        long nextMs = Interlocked.Read(ref nextLocalPreviewAtMs);
        if (nowMs < nextMs)
        {
            return false;
        }

        Interlocked.Exchange(ref nextLocalPreviewAtMs, nowMs + LocalPreviewFrameIntervalMs);
        return true;
    }

    // ПРАВИЛО ВЫБИРАЕТСЯ ПО ПРИРОДЕ ИСТОЧНИКА, А НЕ ОДНО НА ВСЕХ.
    //
    // Камера объявляет частоту и держит шаг — её прореживают СЧЁТОМ. Экран частоты не объявляет
    // вовсе (кадр приходит, когда что-то изменилось) — его прореживают ДЕДЛАЙНОМ. Оба правила и
    // причины, по которым их нельзя менять местами, живут в CallVideoFrameSelector.
    //
    // До 2026-08-27 правило было одно, и для экрана оно не работало вовсе: неизвестная частота
    // источника подменялась согласованной, условие «хотим не меньше, чем даёт источник»
    // срабатывало на равенстве, и в кодировщик уходил КАЖДЫЙ кадр. Живой замер: профиль 30 к/с,
    // `sentFps` на демонстрации экрана 33-38.
    //
    // Тела обеих ветвей выполняются по одному кадру за раз: OnFrameArrived сериализован флагом
    // captureFrameInFlight, поэтому состояние выбора — обычные поля, а не Interlocked-арифметика.
    private bool ShouldEncodeFrame()
    {
        int wantedFps = Math.Max(1, targetFps);
        int declaredSourceFps = activeCaptureProfileFps;
        if (declaredSourceFps <= 0)
        {
            return CallVideoFrameSelector.SelectFromEventDrivenSource(
                wantedFps, Environment.TickCount64, ref frameSelectionLastSelectedAtMs);
        }

        return CallVideoFrameSelector.SelectFromDeclaredRate(
            wantedFps, declaredSourceFps, ref frameSelectionCredit);
    }

    private void PublishStateChanged()
        => StateChanged?.Invoke(this, EventArgs.Empty);

    private void RecordError(string message)
    {
        lastError = message;
        PublishDiagnostics(force: true);
    }

    private static string FormatExceptionForDiagnostics(Exception ex)
        => $"{ex.GetType().Name}: {ex.Message}";

    private static int ReadClampedIntEnvironment(string name, int fallback, int min, int max)
    {
        string? value = Environment.GetEnvironmentVariable(name);
        return int.TryParse(value, out int parsed)
            ? Math.Clamp(parsed, min, max)
            : fallback;
    }

    private static bool IsLikelyCameraContentionFailure(Exception ex)
        => ex.Message.Contains("camera_contended", StringComparison.OrdinalIgnoreCase) ||
           ex is COMException;

    private static string BuildCameraContentionFailure(Exception ex)
        => ex.Message.Contains("camera_contended", StringComparison.OrdinalIgnoreCase)
            ? ex.Message
            : $"camera_contended: {FormatExceptionForDiagnostics(ex)}";

    private async Task WaitForCaptureFrameInFlightAsync()
    {
        for (int i = 0; i < 20; i++)
        {
            if (Interlocked.CompareExchange(ref captureFrameInFlight, 0, 0) == 0)
            {
                return;
            }

            await Task.Delay(25);
        }
    }

    /// <summary>
    /// Bounded await of a tracked fire-and-forget Task. Used in teardown to
    /// ensure the Task observes cancellation and finishes BEFORE we dispose
    /// the native sender/receiver session it might still be touching.
    /// Swallows OperationCanceledException (expected on cancel) and other
    /// exceptions (best-effort cleanup, error already surfaced by the loop).
    /// 3 seconds is generous for typical encode/render iteration; cancellation
    /// is checked at well-defined points so worst-case is one in-flight frame.
    /// See docs/decisions/csharp-call-task-lifecycle.md.
    /// </summary>
    private static async Task AwaitTrackedTaskWithGraceAsync(Task? task)
    {
        if (task is null || task.IsCompleted)
        {
            return;
        }

        try
        {
            Task completed = await Task.WhenAny(task, Task.Delay(3000));
            if (completed != task)
            {
                // 3-second grace exceeded — the task either didn't observe
                // cancellation or is stuck in a non-cancellable native call.
                // We give up waiting and let StopAsync proceed; remaining
                // exception will be observed by GC when the Task is collected.
                return;
            }

            await task;
        }
        catch (OperationCanceledException)
        {
        }
        catch
        {
            // Best-effort cleanup; errors already recorded by the loop body.
        }
    }

}
