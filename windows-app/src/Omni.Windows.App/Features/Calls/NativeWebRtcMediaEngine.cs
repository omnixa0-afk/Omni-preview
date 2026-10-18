using System.Buffers;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Features.Calls;

internal readonly record struct NativeWebRtcRtpPacketBatch(
    byte[] Buffer,
    int Length,
    uint PacketCount);

internal readonly record struct NativeWebRtcDecodedI420Frame(
    byte[] Buffer,
    int Width,
    int Height,
    uint RtpTimestamp,
    long TimestampUs);

internal readonly record struct NativeWebRtcDecodedBgraFrame(
    byte[] Buffer,
    int Width,
    int Height,
    uint RtpTimestamp,
    long TimestampUs);

internal enum NativeWebRtcMediaAbrState : uint
{
    Stable = 0,
    HoldCongestedProbe = 1,
    HoldModerateProbe = 2,
    DownLocalProcessing = 3,
    DownCongested = 4,
    DownModerate = 5,
    UpStable = 6,
}

/// The ABR tick's clock — all that is left of the sample (ABI 18). Capture/encode timings went in
/// ABI 12 (OveruseFrameDetector measures that natively), queue depth in ABI 13 with the queue, and
/// the peer's loss/jitter/RTT and the age of its feedback in ABI 18: the RR reaches the native
/// sender as real RTCP, so the session knows them itself. See the struct comment in
/// omni_webrtc_media.h.
internal readonly record struct NativeWebRtcMediaAbrSample(long NowMs);

internal readonly record struct NativeWebRtcMediaAbrTarget(
    bool Changed,
    int BitrateBps,
    int Fps,
    int Width,
    int Height,
    NativeWebRtcMediaAbrState State)
{
    internal string StateText => State switch
    {
        NativeWebRtcMediaAbrState.HoldCongestedProbe => "hold:congested_probe",
        NativeWebRtcMediaAbrState.HoldModerateProbe => "hold:moderate_probe",
        NativeWebRtcMediaAbrState.DownLocalProcessing => "down:local_processing",
        NativeWebRtcMediaAbrState.DownCongested => "down:congested",
        NativeWebRtcMediaAbrState.DownModerate => "down:moderate",
        NativeWebRtcMediaAbrState.UpStable => "up:stable",
        _ => "stable",
    };

    internal bool IsDownshift => State is NativeWebRtcMediaAbrState.DownLocalProcessing
        or NativeWebRtcMediaAbrState.DownCongested
        or NativeWebRtcMediaAbrState.DownModerate;
}

internal static partial class NativeWebRtcMediaEngine
{
    // The one place the expected ABI lives; the probe reads it too (it had its own copy, and the
    // two drifted apart on the ABI 13 bump).
    internal const uint ExpectedAbiVersion = 23;
    private const int MaxErrorBytes = 64 * 1024;
    private const int MaxStringBytes = 1_048_576;
    internal const int MaxRtpBatchBytes = 1_048_576;
    private const string LibraryFileName = "omni_webrtc_media.dll";

    private static readonly Lazy<NativeLibraryLoadResult> Library = new(LoadLibrary);

    internal static bool TryCreateH264Session(
        int width,
        int height,
        int fps,
        int bitrateBps,
        out Session? session,
        out string diagnostics)
        => TryCreateSession(CallVideoCodecKind.H264, width, height, fps, bitrateBps, out session, out diagnostics);

    internal static bool TryCreateSession(
        CallVideoCodecKind codecKind,
        int width,
        int height,
        int fps,
        int bitrateBps,
        out Session? session,
        out string diagnostics,
        int? negotiatedWidth = null,
        int? negotiatedHeight = null,
        int? negotiatedFps = null,
        int? negotiatedBitrateBps = null,
        uint encodeVendorId = 0,
        uint decodeVendorId = 0,
        bool decodeSoftware = false,
        bool screenContent = false,
        int maxDatagramBytes = 0)
    {
        session = null;
        diagnostics = string.Empty;

        NativeLibraryLoadResult loadResult = Library.Value;
        if (loadResult.Library is null)
        {
            diagnostics = loadResult.Diagnostics;
            return false;
        }

        string codec = CallVideoCodecFactory.FormatCodec(codecKind);
        int decodeSoftwareFlag = decodeSoftware ? 1 : 0;
        // Screen share is a different trade-off under a thin link and the native encode
        // orchestrator has to be told which one this session is: a shrunk face is still a face,
        // shrunk text is not text. Upstream disables quality scaling for screencast and pays with
        // frame rate instead — see the config comment in omni_webrtc_media.cpp.
        int screenContentFlag = screenContent ? 1 : 0;
        // ПРЕДЕЛ ПАКЕТА — СОГЛАСОВАННЫЙ, А НЕ НАШ.
        //
        // Ядро берёт минимум из бюджетов обеих сторон и потолка транспорта; медиа-слой про канал
        // не знает ничего. Ноль здесь значит «согласования ещё нет» — тогда ключ не ставим вовсе
        // и нативная сессия берёт своё умолчание, а не наше представление о чужом канале.
        string datagramBudget = maxDatagramBytes > 0
            ? $",\"max_datagram_bytes\":{maxDatagramBytes}"
            : string.Empty;
        string configJson = $$"""{"codec":"{{codec}}","width":{{width}},"height":{{height}},"fps":{{fps}},"bitrate_bps":{{bitrateBps}},"negotiated_width":{{negotiatedWidth ?? width}},"negotiated_height":{{negotiatedHeight ?? height}},"negotiated_fps":{{negotiatedFps ?? fps}},"negotiated_bitrate_bps":{{negotiatedBitrateBps ?? bitrateBps}},"encode_vendor_id":{{encodeVendorId}},"decode_vendor_id":{{decodeVendorId}},"decode_software":{{decodeSoftwareFlag}},"content_screen":{{screenContentFlag}}{{datagramBudget}}}""";
        IntPtr configBuffer = IntPtr.Zero;
        IntPtr errorBuffer = IntPtr.Zero;
        try
        {
            byte[] configBytes = Encoding.UTF8.GetBytes(configJson + "\0");
            configBuffer = Marshal.AllocHGlobal(configBytes.Length);
            Marshal.Copy(configBytes, 0, configBuffer, configBytes.Length);
            errorBuffer = Marshal.AllocHGlobal(MaxErrorBytes);

            NativeWebRtcStatus status = loadResult.Library.SessionCreate(
                configBuffer,
                out IntPtr sessionHandle,
                errorBuffer,
                (UIntPtr)MaxErrorBytes,
                out UIntPtr requiredErrorLen);
            if (status != NativeWebRtcStatus.Ok)
            {
                diagnostics = ReadUtf8(errorBuffer)
                    ?? $"native session_create failed: status={status}, required_error_len={requiredErrorLen.ToUInt64()}";
                return false;
            }

            if (sessionHandle == IntPtr.Zero)
            {
                diagnostics = "native session_create returned a null session handle";
                return false;
            }

            session = new Session(loadResult.Library, sessionHandle);
            return session is not null;
        }
        finally
        {
            if (configBuffer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(configBuffer);
            }

            if (errorBuffer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(errorBuffer);
            }
        }
    }

    /// <summary>
    /// Phase 2b-step3 (2026-05-23): factory for the audio receive+playback
    /// handle. Failure means "no render endpoint" or "decoder init
    /// failed" — both bubble up to the user diagnostic.
    /// </summary>
    internal static bool TryCreateAudioPlayback(
        out AudioPlaybackSession? session,
        out string diagnostics,
        bool renderEnabled = true,
        string? deviceId = null)
    {
        session = null;
        diagnostics = string.Empty;

        NativeLibraryLoadResult loadResult = Library.Value;
        if (loadResult.Library is null)
        {
            diagnostics = loadResult.Diagnostics;
            return false;
        }

        IntPtr errorBuffer = IntPtr.Zero;
        IntPtr configJson = IntPtr.Zero;
        try
        {
            errorBuffer = Marshal.AllocHGlobal(MaxErrorBytes);
            // renderEnabled=false → a decode-only level probe (no WASAPI endpoint, no AEC feed).
            // Used per group-call sender for active-speaker levels alongside the shared playback.
            string config = renderEnabled
                ? $$"""{"render":1{{DeviceIdJsonField(deviceId)}}}"""
                : """{"render":0}""";
            byte[] configBytes = Encoding.UTF8.GetBytes(config + "\0");
            configJson = Marshal.AllocHGlobal(configBytes.Length);
            Marshal.Copy(configBytes, 0, configJson, configBytes.Length);
            NativeWebRtcStatus status = loadResult.Library.AudioPlaybackCreate(
                configJson,
                out IntPtr handle,
                errorBuffer,
                (UIntPtr)MaxErrorBytes,
                out UIntPtr requiredErrorLen);
            if (status != NativeWebRtcStatus.Ok)
            {
                diagnostics = ReadUtf8(errorBuffer)
                    ?? $"native audio_playback_create failed: status={status}, required_error_len={requiredErrorLen.ToUInt64()}";
                return false;
            }

            if (handle == IntPtr.Zero)
            {
                diagnostics = "native audio_playback_create returned a null handle";
                return false;
            }

            session = new AudioPlaybackSession(loadResult.Library, handle);
            return true;
        }
        finally
        {
            if (errorBuffer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(errorBuffer);
            }
            if (configJson != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(configJson);
            }
        }
    }

    /// <summary>
    /// Phase 2a-step4 (2026-05-23): factory for the audio capture+send
    /// handle. Returns true with a non-null session on success. On
    /// failure, diagnostics carries the underlying WASAPI HRESULT or
    /// the loader error. Failure here typically means "no audio
    /// endpoint available" (headless host, denied permission) and is
    /// expected to be surfaced to the user rather than retried.
    /// </summary>
    internal static bool TryCreateAudioCapture(
        int targetBitrateBps,
        out AudioCaptureSession? session,
        out string diagnostics,
        string? deviceId = null)
    {
        session = null;
        diagnostics = string.Empty;

        NativeLibraryLoadResult loadResult = Library.Value;
        if (loadResult.Library is null)
        {
            diagnostics = loadResult.Diagnostics;
            return false;
        }

        string configJson = $$"""{"target_bitrate_bps":{{targetBitrateBps}}{{DeviceIdJsonField(deviceId)}}}""";
        IntPtr configBuffer = IntPtr.Zero;
        IntPtr errorBuffer = IntPtr.Zero;
        try
        {
            byte[] configBytes = Encoding.UTF8.GetBytes(configJson + "\0");
            configBuffer = Marshal.AllocHGlobal(configBytes.Length);
            Marshal.Copy(configBytes, 0, configBuffer, configBytes.Length);
            errorBuffer = Marshal.AllocHGlobal(MaxErrorBytes);

            NativeWebRtcStatus status = loadResult.Library.AudioCaptureCreate(
                configBuffer,
                out IntPtr handle,
                errorBuffer,
                (UIntPtr)MaxErrorBytes,
                out UIntPtr requiredErrorLen);
            if (status != NativeWebRtcStatus.Ok)
            {
                diagnostics = ReadUtf8(errorBuffer)
                    ?? $"native audio_capture_create failed: status={status}, required_error_len={requiredErrorLen.ToUInt64()}";
                return false;
            }

            if (handle == IntPtr.Zero)
            {
                diagnostics = "native audio_capture_create returned a null handle";
                return false;
            }

            session = new AudioCaptureSession(loadResult.Library, handle);
            return true;
        }
        finally
        {
            if (configBuffer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(configBuffer);
            }

            if (errorBuffer != IntPtr.Zero)
            {
                Marshal.FreeHGlobal(errorBuffer);
            }
        }
    }

    /// <summary>
    /// `,"device_id":"…"` for a chosen audio device, empty for the default one. Escaped exactly as
    /// far as the native reader unescapes (a backslash takes the next character literally).
    /// </summary>
    private static string DeviceIdJsonField(string? deviceId)
    {
        if (string.IsNullOrEmpty(deviceId))
        {
            return string.Empty;
        }

        string escaped = deviceId.Replace("\\", "\\\\").Replace("\"", "\\\"");
        return $",\"device_id\":\"{escaped}\"";
    }

    internal sealed record AudioDeviceOption(string Id, string Name, bool IsDefault);

    internal sealed record AudioDeviceList(
        IReadOnlyList<AudioDeviceOption> Microphones,
        IReadOnlyList<AudioDeviceOption> Speakers);

    /// <summary>
    /// Active microphones and speakers for the Settings pickers (ABI 20). Ids are what
    /// TryCreateAudioCapture / TryCreateAudioPlayback take as deviceId. Empty lists when the
    /// native library is missing or Windows cannot enumerate — the default device still works.
    /// </summary>
    internal static AudioDeviceList ListAudioDevices()
    {
        var empty = new AudioDeviceList([], []);
        NativeLibraryLoadResult loadResult = Library.Value;
        if (loadResult.Library is null)
        {
            return empty;
        }

        byte[] buffer = new byte[16 * 1024];
        NativeWebRtcStatus status = loadResult.Library.AudioListDevices(buffer, (nuint)buffer.Length, out nuint written);
        if (status == NativeWebRtcStatus.BufferTooSmall && written > (nuint)buffer.Length)
        {
            buffer = new byte[(int)written];
            status = loadResult.Library.AudioListDevices(buffer, (nuint)buffer.Length, out written);
        }

        if (status != NativeWebRtcStatus.Ok || written == 0)
        {
            return empty;
        }

        using System.Text.Json.JsonDocument document =
            System.Text.Json.JsonDocument.Parse(buffer.AsMemory(0, (int)written - 1));
        return new AudioDeviceList(
            ReadAudioDevices(document.RootElement, "capture"),
            ReadAudioDevices(document.RootElement, "render"));
    }

    private static IReadOnlyList<AudioDeviceOption> ReadAudioDevices(System.Text.Json.JsonElement root, string property)
    {
        if (!root.TryGetProperty(property, out System.Text.Json.JsonElement array) ||
            array.ValueKind != System.Text.Json.JsonValueKind.Array)
        {
            return [];
        }

        var devices = new List<AudioDeviceOption>();
        foreach (System.Text.Json.JsonElement item in array.EnumerateArray())
        {
            string? id = item.TryGetProperty("id", out System.Text.Json.JsonElement idElement) ? idElement.GetString() : null;
            if (string.IsNullOrEmpty(id))
            {
                continue;
            }

            string? name = item.TryGetProperty("name", out System.Text.Json.JsonElement nameElement) ? nameElement.GetString() : null;
            bool isDefault = item.TryGetProperty("default", out System.Text.Json.JsonElement defaultElement) &&
                defaultElement.ValueKind == System.Text.Json.JsonValueKind.True;
            devices.Add(new AudioDeviceOption(id, string.IsNullOrWhiteSpace(name) ? id : name, isDefault));
        }

        return devices;
    }

    private sealed record NativeLibraryLoadResult(
        NativeWebRtcLibrary? Library,
        string Diagnostics);

    internal sealed class NativeWebRtcLibrary
    {
        internal NativeWebRtcLibrary(IntPtr handle)
        {
            Handle = handle;
            AbiVersion = LoadExport<AbiVersionDelegate>("omni_webrtc_media_abi_version");
            SessionCreate = LoadExport<SessionCreateDelegate>("omni_webrtc_media_session_create");
            SessionDestroy = LoadExport<SessionDestroyDelegate>("omni_webrtc_media_session_destroy");
            PushI420Frame = LoadExport<PushI420FrameDelegate>("omni_webrtc_media_session_push_i420_frame");
            PushNv12Frame = LoadExport<PushNv12FrameDelegate>("omni_webrtc_media_session_push_nv12_frame");
            PushBgraFrame = LoadExport<PushBgraFrameDelegate>("omni_webrtc_media_session_push_bgra_frame");
            SetRates = LoadExport<SetRatesDelegate>("omni_webrtc_media_session_set_rates");
            SetMaxBitrate = LoadExport<SetMaxBitrateDelegate>("omni_webrtc_media_session_set_max_bitrate");
            RequestKeyframe = LoadExport<RequestKeyframeDelegate>("omni_webrtc_media_session_request_keyframe");
            SetPacketSink = LoadExport<SetPacketSinkDelegate>("omni_webrtc_media_session_set_packet_sink");
            RouterCreate = LoadExport<RouterCreateDelegate>("omni_webrtc_media_router_create");
            RouterDestroy = LoadExport<RouterDestroyDelegate>("omni_webrtc_media_router_destroy");
            RouterSetSender = LoadExport<RouterSetSessionDelegate>("omni_webrtc_media_router_set_sender");
            RouterSetReceiver = LoadExport<RouterSetSessionDelegate>("omni_webrtc_media_router_set_receiver");
            RouterSetAudioPlayback = LoadExport<RouterSetSessionDelegate>("omni_webrtc_media_router_set_audio_playback");
            RouterSetGroupMode = LoadExport<RouterSetGroupModeDelegate>("omni_webrtc_media_router_set_group_mode");
            RouterSetGroupSender = LoadExport<RouterSetGroupSenderDelegate>("omni_webrtc_media_router_set_group_sender");
            RouterTakeRequests = LoadExport<RouterTakeRequestsDelegate>("omni_webrtc_media_router_take_requests");
            RouterCounters = LoadExport<RouterCountersDelegate>("omni_webrtc_media_router_counters");
            RouterDeliver = NativeLibrary.GetExport(handle, "omni_webrtc_media_router_deliver");
            PushRtpPackets = LoadExport<PushRtpPacketsDelegate>("omni_webrtc_media_session_push_rtp_packets");
            // TWCC (2026-08-17): отчёт о доставке по RFC 8888. Приёмник кладёт готовый RTCP-блок
            // в свою общую очередь RTCP, оболочка узнаёт его по типу 205/формату 11 и отдаёт сюда,
            // отправителю. Байты она не разбирает.
            ApplyTransportFeedback = LoadExport<ApplyTransportFeedbackDelegate>("omni_webrtc_media_session_apply_transport_feedback");
            TransportRttUs = LoadExport<TransportRttUsDelegate>("omni_webrtc_media_session_transport_rtt_us");
            UpdateAdaptiveBitrate = LoadExport<UpdateAdaptiveBitrateDelegate>("omni_webrtc_media_session_update_abr");
            DrainI420Frame = LoadExport<DrainI420FrameDelegate>("omni_webrtc_media_session_drain_i420_frame");
            DrainBgraFrame = LoadExport<DrainBgraFrameDelegate>("omni_webrtc_media_session_drain_bgra_frame");
            // #5 increment 3b (2026-06-05): GPU video renderer (SwapChainPanel) session FFI.
            AttachVideoRenderer = LoadExport<AttachVideoRendererDelegate>("omni_webrtc_media_session_attach_video_renderer");
            RenderAttachedVideo = LoadExport<RenderAttachedVideoDelegate>("omni_webrtc_media_session_render_attached_video");
            ResizeAttachedVideo = LoadExport<ResizeAttachedVideoDelegate>("omni_webrtc_media_session_resize_attached_video");
            DetachVideoRenderer = LoadExport<DetachVideoRendererDelegate>("omni_webrtc_media_session_detach_video_renderer");
            DiagnosticsJson = LoadExport<DiagnosticsJsonDelegate>("omni_webrtc_media_session_diagnostics_json");
            // Wave 3 (2026-05-16): libyuv-backed pixel ops.
            ScaleBgra = LoadExport<ScaleBgraDelegate>("omni_webrtc_media_scale_bgra");
            ScaleNv12 = LoadExport<ScaleNv12Delegate>("omni_webrtc_media_scale_nv12");
            Nv12ToBgra = LoadExport<Nv12ToBgraDelegate>("omni_webrtc_media_nv12_to_bgra");
            // Audio Phase 2a-step4 (2026-05-23): audio capture+send pipeline.
            AudioCaptureCreate = LoadExport<AudioCaptureCreateDelegate>("omni_webrtc_media_audio_capture_create");
            AudioCaptureDestroy = LoadExport<AudioCaptureDestroyDelegate>("omni_webrtc_media_audio_capture_destroy");
            AudioCaptureSetPacketSink = LoadExport<AudioCaptureSetPacketSinkDelegate>("omni_webrtc_media_audio_capture_set_packet_sink");
            AudioCaptureSetMuted = LoadExport<AudioCaptureSetMutedDelegate>("omni_webrtc_media_audio_capture_set_muted");
            AudioCapturePacketsSent = LoadExport<AudioCapturePacketsSentDelegate>("omni_webrtc_media_audio_capture_packets_sent");
            // Audio Phase 2b-step3 (2026-05-23): audio receive+playback pipeline.
            AudioPlaybackCreate = LoadExport<AudioPlaybackCreateDelegate>("omni_webrtc_media_audio_playback_create");
            AudioPlaybackDestroy = LoadExport<AudioPlaybackDestroyDelegate>("omni_webrtc_media_audio_playback_destroy");
            // Audio Phase 2c (2026-05-23): playback counters for full-duplex diagnostics.
            AudioPlaybackReadCounters = LoadExport<AudioPlaybackReadCountersDelegate>("omni_webrtc_media_audio_playback_read_counters");
            AudioPlaybackJitterStats = LoadExport<AudioPlaybackJitterStatsDelegate>("omni_webrtc_media_audio_playback_jitter_stats");
            // Audio Phase 3 (2026-05-23): lipsync — push video's playout target into audio.
            AudioPlaybackSetTargetPlayoutDelayMs = LoadExport<AudioPlaybackSetTargetPlayoutDelayMsDelegate>("omni_webrtc_media_audio_playback_set_target_playout_delay_ms");
            // Group-call speaking indicator (2026-07-11): per-endpoint smoothed RMS for tiles.
            AudioCaptureLevel = LoadExport<AudioCaptureLevelDelegate>("omni_webrtc_media_audio_capture_level");
            AudioCaptureDigitalSilenceMs = LoadExport<AudioCaptureDigitalSilenceMsDelegate>("omni_webrtc_media_audio_capture_digital_silence_ms");
            AudioPlaybackLevel = LoadExport<AudioPlaybackLevelDelegate>("omni_webrtc_media_audio_playback_level");
            // Echo-canceller visibility (2026-08-11): an echo complaint is otherwise
            // undiagnosable — a canceller that is never fed looks like one that is failing.
            AudioCaptureEchoDiagnostics = LoadExport<AudioCaptureEchoDiagnosticsDelegate>("omni_webrtc_media_audio_capture_echo_diagnostics");
            // Microphone / speakers choice (ABI 20, 2026-10-12): Settings → «Звонки».
            AudioListDevices = LoadExport<AudioListDevicesDelegate>("omni_webrtc_media_audio_list_devices");
        }

        private IntPtr Handle { get; }

        internal AbiVersionDelegate AbiVersion { get; }

        internal SessionCreateDelegate SessionCreate { get; }

        internal SessionDestroyDelegate SessionDestroy { get; }

        internal PushI420FrameDelegate PushI420Frame { get; }

        internal PushNv12FrameDelegate PushNv12Frame { get; }

        internal PushBgraFrameDelegate PushBgraFrame { get; }

        internal SetRatesDelegate SetRates { get; }

        internal SetMaxBitrateDelegate SetMaxBitrate { get; }

        internal RequestKeyframeDelegate RequestKeyframe { get; }

        internal SetPacketSinkDelegate SetPacketSink { get; }

        internal RouterCreateDelegate RouterCreate { get; }

        internal RouterDestroyDelegate RouterDestroy { get; }

        internal RouterSetSessionDelegate RouterSetSender { get; }

        internal RouterSetSessionDelegate RouterSetReceiver { get; }

        internal RouterSetSessionDelegate RouterSetAudioPlayback { get; }

        internal RouterSetGroupModeDelegate RouterSetGroupMode { get; }

        internal RouterSetGroupSenderDelegate RouterSetGroupSender { get; }

        internal RouterTakeRequestsDelegate RouterTakeRequests { get; }

        internal RouterCountersDelegate RouterCounters { get; }

        /// <summary>Address of omni_webrtc_media_router_deliver — handed to the core, never called here.</summary>
        internal IntPtr RouterDeliver { get; }

        internal PushRtpPacketsDelegate PushRtpPackets { get; }


        internal ApplyTransportFeedbackDelegate ApplyTransportFeedback { get; }

        internal TransportRttUsDelegate TransportRttUs { get; }


        internal UpdateAdaptiveBitrateDelegate UpdateAdaptiveBitrate { get; }

        internal DrainI420FrameDelegate DrainI420Frame { get; }

        internal DrainBgraFrameDelegate DrainBgraFrame { get; }

        internal AttachVideoRendererDelegate AttachVideoRenderer { get; }

        internal RenderAttachedVideoDelegate RenderAttachedVideo { get; }

        internal ResizeAttachedVideoDelegate ResizeAttachedVideo { get; }

        internal DetachVideoRendererDelegate DetachVideoRenderer { get; }

        internal DiagnosticsJsonDelegate DiagnosticsJson { get; }

        internal ScaleBgraDelegate ScaleBgra { get; }

        internal ScaleNv12Delegate ScaleNv12 { get; }

        internal Nv12ToBgraDelegate Nv12ToBgra { get; }

        internal AudioCaptureCreateDelegate AudioCaptureCreate { get; }

        internal AudioCaptureDestroyDelegate AudioCaptureDestroy { get; }

        internal AudioCaptureSetPacketSinkDelegate AudioCaptureSetPacketSink { get; }

        internal AudioCaptureSetMutedDelegate AudioCaptureSetMuted { get; }

        internal AudioCapturePacketsSentDelegate AudioCapturePacketsSent { get; }

        internal AudioPlaybackCreateDelegate AudioPlaybackCreate { get; }

        internal AudioPlaybackDestroyDelegate AudioPlaybackDestroy { get; }

        internal AudioPlaybackReadCountersDelegate AudioPlaybackReadCounters { get; }

        internal AudioPlaybackJitterStatsDelegate AudioPlaybackJitterStats { get; }

        internal AudioPlaybackSetTargetPlayoutDelayMsDelegate AudioPlaybackSetTargetPlayoutDelayMs { get; }

        internal AudioCaptureLevelDelegate AudioCaptureLevel { get; }
        internal AudioCaptureDigitalSilenceMsDelegate AudioCaptureDigitalSilenceMs { get; }

        internal AudioPlaybackLevelDelegate AudioPlaybackLevel { get; }

        internal AudioCaptureEchoDiagnosticsDelegate AudioCaptureEchoDiagnostics { get; }

        internal AudioListDevicesDelegate AudioListDevices { get; }

        private TDelegate LoadExport<TDelegate>(string exportName)
            where TDelegate : Delegate
        {
            if (!NativeLibrary.TryGetExport(Handle, exportName, out IntPtr exportAddress))
            {
                throw new EntryPointNotFoundException(exportName);
            }

            return Marshal.GetDelegateForFunctionPointer<TDelegate>(exportAddress);
        }
    }

    private static NativeLibraryLoadResult LoadLibrary()
    {
        List<string> failures = [];

        // Ask the runtime by NAME first, and only then fall back to hand-built paths.
        //
        // Every path candidate below resolves to the directory holding the EXE, which is right for a
        // normal build and wrong for the standalone single-file one: there the native libraries are
        // self-extracted to %TEMP%\.net\<app>\<hash>\ and nothing sits next to the exe. The runtime's
        // own probing knows about that directory — it is how `omni_ffi.dll` has been loading all
        // along — so a by-name load works in both layouts and a path-only load works in one.
        //
        // Found live 2026-08-11 on the first standalone build: calls connected and stayed silent,
        // the camera button never became usable, and both peers emitted zero media datagrams,
        // because the engine never had a library at all.
        if (TryLoadByName(out NativeLibraryLoadResult? byName, out string byNameFailure))
        {
            return byName!;
        }
        failures.Add(byNameFailure);

        foreach (string candidate in EnumerateLibraryCandidates())
        {
            if (!File.Exists(candidate))
            {
                failures.Add($"{candidate}: missing");
                continue;
            }

            IntPtr handle = IntPtr.Zero;
            try
            {
                handle = NativeLibrary.Load(candidate);
                NativeWebRtcLibrary library = new(handle);
                uint abi = library.AbiVersion();
                if (abi != ExpectedAbiVersion)
                {
                    failures.Add($"{candidate}: unsupported ABI version {abi}, expected {ExpectedAbiVersion}");
                    NativeLibrary.Free(handle);
                    handle = IntPtr.Zero;
                    continue;
                }

                Core.AppTrace.Write("media/engine-load", $"ok: {candidate}");
                return new NativeLibraryLoadResult(library, $"loaded={candidate}");
            }
            catch (Exception ex)
            {
                if (handle != IntPtr.Zero)
                {
                    NativeLibrary.Free(handle);
                }

                failures.Add($"{candidate}: {ex.GetType().Name}: {ex.Message}");
            }
        }

        string diagnostics = failures.Count == 0
            ? $"{LibraryFileName}: no candidate paths"
            : string.Join("; ", failures.Take(4));
        Core.AppTrace.Write("media/engine-load", $"FAILED: {diagnostics}");
        return new NativeLibraryLoadResult(null, diagnostics);
    }

    private static bool TryLoadByName(out NativeLibraryLoadResult? result, out string failure)
    {
        result = null;
        if (!string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable("OMNI_WEBRTC_MEDIA_DLL")))
        {
            failure = $"{LibraryFileName}: skipped by-name load, OMNI_WEBRTC_MEDIA_DLL is set";
            return false;
        }

        IntPtr handle = IntPtr.Zero;
        try
        {
            handle = NativeLibrary.Load(
                LibraryFileName,
                typeof(NativeWebRtcMediaEngine).Assembly,
                searchPath: null);
            NativeWebRtcLibrary library = new(handle);
            uint abi = library.AbiVersion();
            if (abi != ExpectedAbiVersion)
            {
                NativeLibrary.Free(handle);
                failure = $"{LibraryFileName} (by name): unsupported ABI version {abi}, expected {ExpectedAbiVersion}";
                return false;
            }

            result = new NativeLibraryLoadResult(library, $"loaded={LibraryFileName} (runtime probing)");
            Core.AppTrace.Write("media/engine-load", "ok: runtime probing");
            failure = string.Empty;
            return true;
        }
        catch (Exception ex)
        {
            if (handle != IntPtr.Zero)
            {
                NativeLibrary.Free(handle);
            }

            failure = $"{LibraryFileName} (by name): {ex.GetType().Name}: {ex.Message}";
            return false;
        }
    }

    private static IEnumerable<string> EnumerateLibraryCandidates()
    {
        string? explicitPath = Environment.GetEnvironmentVariable("OMNI_WEBRTC_MEDIA_DLL");
        if (!string.IsNullOrWhiteSpace(explicitPath))
        {
            yield return explicitPath;
        }

        yield return Path.Combine(AppContext.BaseDirectory, LibraryFileName);

        string? processDirectory = Path.GetDirectoryName(Environment.ProcessPath);
        if (!string.IsNullOrWhiteSpace(processDirectory))
        {
            yield return Path.Combine(processDirectory, LibraryFileName);
        }

        yield return Path.Combine(Environment.CurrentDirectory, LibraryFileName);
    }

    private static void ValidateI420Plane(
        string planeName,
        ReadOnlySpan<byte> plane,
        int stride,
        int width,
        int height)
    {
        if (width <= 0 || height <= 0)
        {
            throw new ArgumentOutOfRangeException(nameof(width), "Frame dimensions must be positive.");
        }

        if (stride < width)
        {
            throw new ArgumentOutOfRangeException(nameof(stride), $"{planeName} stride is smaller than width.");
        }

        int required = checked(stride * height);
        if (plane.Length < required)
        {
            throw new ArgumentException($"{planeName} plane length {plane.Length} is smaller than required {required} bytes.");
        }
    }

    /// <summary>
    /// ЗАМЕР ДЛИНЫ И ЧТЕНИЕ — ДВА РАЗНЫХ ВЫЗОВА, А СИСТЕМА МЕЖДУ НИМИ ЖИВЁТ.
    ///
    /// Диагностика собирается заново на каждом вызове, и счётчики между замером и чтением успевают
    /// подрасти — достаточно одному из них прибавить разряд. Буфер, выделенный ровно по прошлой
    /// длине, тогда оказывается мал, и вызов падает с BufferTooSmall.
    ///
    /// Это не гипотеза: контрактный тест упал ровно так, как только в диагностику добавили
    /// несколько быстро растущих счётчиков (за один прогон стенда playout_wait_polls доходит до
    /// двух с лишним сотен). Дефект был здесь и раньше — просто строка росла медленнее.
    ///
    /// Поэтому читаем с запасом и с повтором: запас — та величина, на которую строка успевает
    /// подрасти за такт, повтор — страховка на случай, если и её не хватило.
    /// </summary>
    private static string ReadNativeString(NativeStringReader reader)
    {
        const int attempts = 4;
        for (int attempt = 0; attempt < attempts; attempt++)
        {
            NativeWebRtcStatus status = reader(IntPtr.Zero, UIntPtr.Zero, out UIntPtr requiredLen);
            if (status != NativeWebRtcStatus.BufferTooSmall && status != NativeWebRtcStatus.Ok)
            {
                ThrowIfFailed(status, "native string sizing");
            }

            ulong required = requiredLen.ToUInt64();
            if (required == 0 || required > MaxStringBytes)
            {
                throw new InvalidOperationException($"native string has invalid size: {required}");
            }

            ulong capacity = Math.Min((ulong)MaxStringBytes, required + (256UL * (ulong)(attempt + 1)));
            IntPtr buffer = Marshal.AllocHGlobal(checked((int)capacity));
            try
            {
                status = reader(buffer, (UIntPtr)capacity, out _);
                if (status == NativeWebRtcStatus.BufferTooSmall && attempt + 1 < attempts)
                {
                    continue;
                }

                ThrowIfFailed(status, "native string read");
                return ReadUtf8(buffer) ?? string.Empty;
            }
            finally
            {
                Marshal.FreeHGlobal(buffer);
            }
        }

        throw new InvalidOperationException(
            "native string kept outgrowing its measured length across four attempts");
    }

    private static string? ReadUtf8(IntPtr buffer)
        => buffer == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(buffer);

    private static void ThrowIfFailed(NativeWebRtcStatus status, string operation)
    {
        if (status == NativeWebRtcStatus.Ok)
        {
            return;
        }

        throw status switch
        {
            NativeWebRtcStatus.InvalidArgument => new ArgumentException($"native {operation} failed: {status}"),
            NativeWebRtcStatus.NotImplemented => new NotSupportedException($"native {operation} is not implemented"),
            NativeWebRtcStatus.BufferTooSmall => new InvalidOperationException($"native {operation} buffer is too small"),
            _ => new InvalidOperationException($"native {operation} failed: {status}"),
        };
    }

    private delegate NativeWebRtcStatus NativeStringReader(
        IntPtr buffer,
        UIntPtr bufferLen,
        out UIntPtr requiredLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate uint AbiVersionDelegate();

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus SessionCreateDelegate(
        IntPtr configJson,
        out IntPtr session,
        IntPtr errorBuffer,
        UIntPtr errorBufferLen,
        out UIntPtr requiredErrorLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void SessionDestroyDelegate(IntPtr session);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus PushI420FrameDelegate(
        IntPtr session,
        IntPtr yPlane,
        int yStride,
        IntPtr uPlane,
        int uStride,
        IntPtr vPlane,
        int vStride,
        int width,
        int height,
        long timestampUs,
        int forceKeyFrame);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus PushNv12FrameDelegate(
        IntPtr session,
        IntPtr yPlane,
        int yStride,
        IntPtr uvPlane,
        int uvStride,
        int width,
        int height,
        long timestampUs,
        int forceKeyFrame);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus PushBgraFrameDelegate(
        IntPtr session,
        IntPtr bgra,
        int bgraStride,
        int width,
        int height,
        long timestampUs,
        int forceKeyFrame);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus SetRatesDelegate(
        IntPtr session,
        int bitrateBps,
        int fps);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus SetMaxBitrateDelegate(IntPtr session, int maxBitrateBps);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RequestKeyframeDelegate(IntPtr session);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus SetPacketSinkDelegate(
        IntPtr session,
        IntPtr sink,
        IntPtr context);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RouterCreateDelegate(out IntPtr router);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void RouterDestroyDelegate(IntPtr router);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RouterSetSessionDelegate(IntPtr router, IntPtr session);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RouterSetGroupModeDelegate(IntPtr router, int groupMode);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RouterSetGroupSenderDelegate(
        IntPtr router,
        IntPtr senderTag,
        IntPtr receiver,
        IntPtr levelProbe);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal unsafe delegate NativeWebRtcStatus RouterTakeRequestsDelegate(
        IntPtr router,
        NativeWebRtcMediaRouterRequestsNative* requests);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RouterCountersDelegate(
        IntPtr router,
        out ulong delivered,
        out ulong feedback,
        out ulong dropped);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus PushRtpPacketsDelegate(
        IntPtr session,
        IntPtr buffer,
        UIntPtr bufferLen,
        uint packetCount);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus ApplyTransportFeedbackDelegate(
        IntPtr session,
        IntPtr data,
        UIntPtr dataLen);

    /// Проход туда-обратно, измеренный ЭТОЙ сессией; -1 пока не измерен ни разу.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus TransportRttUsDelegate(
        IntPtr session,
        out long rttUs);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal unsafe delegate NativeWebRtcStatus UpdateAdaptiveBitrateDelegate(
        IntPtr session,
        NativeWebRtcMediaAbrSampleNative* sample,
        NativeWebRtcMediaAbrTargetNative* target);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus DrainI420FrameDelegate(
        IntPtr session,
        IntPtr buffer,
        UIntPtr bufferLen,
        out int width,
        out int height,
        out uint rtpTimestamp,
        out long timestampUs,
        out UIntPtr requiredLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus DrainBgraFrameDelegate(
        IntPtr session,
        IntPtr buffer,
        UIntPtr bufferLen,
        out int width,
        out int height,
        out uint rtpTimestamp,
        out long timestampUs,
        out UIntPtr requiredLen);

    // #5 increment 3b (2026-06-05): GPU video renderer (SwapChainPanel) session FFI. attach
    // returns the composition swapchain's IDXGISwapChain1* (AddRef'd) via out swapChain; the C#
    // layer binds it to a WinUI SwapChainPanel through ISwapChainPanelNative.SetSwapChain, then
    // Marshal.Release()s it. render/resize/detach drive that swapchain. The caller serialises all
    // four (the session's Gate); attach/resize/detach run on the UI thread, render on the presenter
    // thread. Decoded pixels never cross FFI.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AttachVideoRendererDelegate(
        IntPtr session,
        int width,
        int height,
        out IntPtr swapChain);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus RenderAttachedVideoDelegate(
        IntPtr session,
        [MarshalAs(UnmanagedType.U1)] out bool presented);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus ResizeAttachedVideoDelegate(
        IntPtr session,
        int width,
        int height);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus DetachVideoRendererDelegate(IntPtr session);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus DiagnosticsJsonDelegate(
        IntPtr session,
        IntPtr buffer,
        UIntPtr bufferLen,
        out UIntPtr requiredLen);

    // Wave 3 (2026-05-16): libyuv-backed pixel ops.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus ScaleBgraDelegate(
        IntPtr srcBgra,
        int srcStride,
        int srcWidth,
        int srcHeight,
        IntPtr dstBgra,
        int dstStride,
        int dstWidth,
        int dstHeight);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus ScaleNv12Delegate(
        IntPtr srcY,
        int srcYStride,
        IntPtr srcUv,
        int srcUvStride,
        int srcWidth,
        int srcHeight,
        IntPtr dstY,
        int dstYStride,
        IntPtr dstUv,
        int dstUvStride,
        int dstWidth,
        int dstHeight);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus Nv12ToBgraDelegate(
        IntPtr srcY,
        int srcYStride,
        IntPtr srcUv,
        int srcUvStride,
        IntPtr dstBgra,
        int dstStride,
        int width,
        int height);

    // Audio Phase 2a-step4 (2026-05-23): audio capture+send pipeline.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureCreateDelegate(
        IntPtr configJson,
        out IntPtr handle,
        IntPtr errorBuffer,
        UIntPtr errorBufferLen,
        out UIntPtr requiredErrorLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void AudioCaptureDestroyDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureSetPacketSinkDelegate(
        IntPtr handle,
        IntPtr sink,
        IntPtr context);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureSetMutedDelegate(IntPtr handle, int muted);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCapturePacketsSentDelegate(IntPtr handle, out ulong sent);

    // Audio Phase 2b-step3 (2026-05-23): audio receive+playback pipeline.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioPlaybackCreateDelegate(
        IntPtr configJson,
        out IntPtr handle,
        IntPtr errorBuffer,
        UIntPtr errorBufferLen,
        out UIntPtr requiredErrorLen);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void AudioPlaybackDestroyDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioPlaybackReadCountersDelegate(
        IntPtr handle,
        out ulong packetsAccepted,
        out ulong packetsDropped);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioPlaybackSetTargetPlayoutDelayMsDelegate(
        IntPtr handle,
        uint delayMs,
        out uint appliedDelayMs);

    // How long the mic's raw input has been pure digital zero (ABI 21): the call screen's "the
    // microphone hears nothing".
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureDigitalSilenceMsDelegate(
        IntPtr handle,
        out long silenceMs);

    // Group-call speaking indicator (2026-07-11): smoothed RMS (0..1) of a capture (self) or
    // playback (per-sender) endpoint. Polled by the group-call tile UI.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureLevelDelegate(
        IntPtr handle,
        out float outLevel);

    // Echo-canceller state for the call diagnostics: whether the APM exists at all, how many
    // far-end blocks it has been fed, and what AEC3 says it is removing.
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioCaptureEchoDiagnosticsDelegate(
        IntPtr handle,
        byte[] buffer,
        nuint bufferLen,
        out nuint written);

    // Active microphones and speakers as JSON (`{"capture":[…],"render":[…]}`).
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioListDevicesDelegate(
        byte[] buffer,
        nuint bufferLen,
        out nuint written);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioPlaybackLevelDelegate(
        IntPtr handle,
        out float outLevel);

    /// <summary>
    /// Wave 3 entry point: BGRA bilinear scale via libyuv::ARGBScale.
    /// Throws if the native library or libyuv backend is unavailable.
    /// </summary>
    internal static unsafe void ScaleBgra(
        ReadOnlySpan<byte> srcBgra,
        int srcWidth,
        int srcHeight,
        Span<byte> dstBgra,
        int dstWidth,
        int dstHeight)
    {
        NativeWebRtcLibrary library = RequireLibrary();
        int srcStride = checked(srcWidth * 4);
        int dstStride = checked(dstWidth * 4);
        if (srcBgra.Length < checked(srcStride * srcHeight) ||
            dstBgra.Length < checked(dstStride * dstHeight))
        {
            throw new ArgumentException("BGRA buffer is shorter than width×height×4.");
        }

        fixed (byte* srcPtr = srcBgra)
        fixed (byte* dstPtr = dstBgra)
        {
            NativeWebRtcStatus status = library.ScaleBgra(
                (IntPtr)srcPtr, srcStride, srcWidth, srcHeight,
                (IntPtr)dstPtr, dstStride, dstWidth, dstHeight);
            if (status != NativeWebRtcStatus.Ok)
            {
                throw new InvalidOperationException(
                    $"omni_webrtc_media_scale_bgra failed: status={status}");
            }
        }
    }

    /// <summary>
    /// Wave 3 entry point: NV12 bilinear scale via libyuv::NV12Scale.
    /// </summary>
    internal static unsafe void ScaleNv12(
        ReadOnlySpan<byte> srcNv12,
        int srcWidth,
        int srcHeight,
        Span<byte> dstNv12,
        int dstWidth,
        int dstHeight)
    {
        if ((dstWidth & 1) != 0 || (dstHeight & 1) != 0)
        {
            throw new ArgumentException("NV12 target size must be even.");
        }

        NativeWebRtcLibrary library = RequireLibrary();
        int srcYStride = srcWidth;
        int srcUvStride = srcWidth + (srcWidth & 1);
        int dstYStride = dstWidth;
        int dstUvStride = dstWidth;
        int srcYPlaneSize = checked(srcYStride * srcHeight);
        int srcUvPlaneSize = checked(srcUvStride * (srcHeight / 2));
        int dstYPlaneSize = checked(dstYStride * dstHeight);
        int dstUvPlaneSize = checked(dstUvStride * (dstHeight / 2));
        if (srcNv12.Length < checked(srcYPlaneSize + srcUvPlaneSize) ||
            dstNv12.Length < checked(dstYPlaneSize + dstUvPlaneSize))
        {
            throw new ArgumentException("NV12 buffer is shorter than width×height×3/2.");
        }

        fixed (byte* srcPtr = srcNv12)
        fixed (byte* dstPtr = dstNv12)
        {
            NativeWebRtcStatus status = library.ScaleNv12(
                (IntPtr)srcPtr, srcYStride,
                (IntPtr)(srcPtr + srcYPlaneSize), srcUvStride,
                srcWidth, srcHeight,
                (IntPtr)dstPtr, dstYStride,
                (IntPtr)(dstPtr + dstYPlaneSize), dstUvStride,
                dstWidth, dstHeight);
            if (status != NativeWebRtcStatus.Ok)
            {
                throw new InvalidOperationException(
                    $"omni_webrtc_media_scale_nv12 failed: status={status}");
            }
        }
    }

    /// <summary>
    /// Wave 3 entry point: NV12 → BGRA color convert via libyuv::NV12ToARGB
    /// (libyuv "ARGB" on little-endian == byte order B,G,R,A == our BGRA).
    /// </summary>
    internal static unsafe void Nv12ToBgra(
        ReadOnlySpan<byte> srcNv12,
        int width,
        int height,
        Span<byte> dstBgra)
    {
        NativeWebRtcLibrary library = RequireLibrary();
        int yStride = width;
        int uvStride = width + (width & 1);
        int yPlaneSize = checked(yStride * height);
        int uvPlaneSize = checked(uvStride * (height / 2));
        int dstStride = checked(width * 4);
        if (srcNv12.Length < checked(yPlaneSize + uvPlaneSize) ||
            dstBgra.Length < checked(dstStride * height))
        {
            throw new ArgumentException("Buffer is shorter than the declared frame size.");
        }

        fixed (byte* srcPtr = srcNv12)
        fixed (byte* dstPtr = dstBgra)
        {
            NativeWebRtcStatus status = library.Nv12ToBgra(
                (IntPtr)srcPtr, yStride,
                (IntPtr)(srcPtr + yPlaneSize), uvStride,
                (IntPtr)dstPtr, dstStride,
                width, height);
            if (status != NativeWebRtcStatus.Ok)
            {
                throw new InvalidOperationException(
                    $"omni_webrtc_media_nv12_to_bgra failed: status={status}");
            }
        }
    }

    private static NativeWebRtcLibrary RequireLibrary()
    {
        NativeLibraryLoadResult loadResult = Library.Value;
        if (loadResult.Library is null)
        {
            throw new InvalidOperationException(
                $"VIDEO_TRANSPORT_V2_REQUIRED: native realtime media library is unavailable: {loadResult.Diagnostics}");
        }

        return loadResult.Library;
    }

    internal enum NativeWebRtcStatus
    {
        Ok = 0,
        InvalidArgument = 1,
        BufferTooSmall = 2,
        NotImplemented = 3,
        InternalError = 4,
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct NativeWebRtcMediaAudioJitterStatsNative
    {
        internal uint BufferMs;
        internal uint TargetMs;
        internal ulong ConcealedMs;
        internal ulong AcceleratedMs;
        internal ulong DeceleratedMs;
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate NativeWebRtcStatus AudioPlaybackJitterStatsDelegate(
        IntPtr handle,
        out NativeWebRtcMediaAudioJitterStatsNative stats);

    [StructLayout(LayoutKind.Sequential)]
    internal struct NativeWebRtcMediaAbrSampleNative
    {
        internal long NowMs;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct NativeWebRtcMediaAbrTargetNative
    {
        internal uint Changed;
        internal uint TargetBitrateBps;
        internal uint TargetFps;
        internal uint TargetWidth;
        internal uint TargetHeight;
        internal uint State;
    }
}
