using System.Runtime.InteropServices;
using System.Text.Json;

namespace Omni.Windows.App.Features.Calls;

internal sealed record NativeWebRtcMediaProbeResult(
    bool DllLoaded,
    bool Available,
    string Backend,
    bool SupportsRtpOverOmniQuic,
    bool SupportsRtcpFeedback,
    bool NativeSessionScaffold,
    string NativeSessionScaffoldCodec,
    IReadOnlyList<string> NativeSessionScaffoldCodecs,
    bool NativeSessionNv12Input,
    bool NativeSessionBgraInput,
    bool NativeSessionBgraOutput,
    bool CodecRoundtripSmoke,
    ulong CodecRoundtripEncodedBytes,
    IReadOnlyList<string> HardwareCodecs,
    NativeWebRtcHardwareH264Probe HardwareH264Probe,
    IReadOnlyList<NativeVideoDevice> VideoDevices,
    IReadOnlyList<string> SoftwareCodecs,
    string Diagnostics,
    IReadOnlyList<string>? HardwareDecodeCodecs = null)
{
    internal string Summary
    {
        get
        {
            string hardware = HardwareCodecs.Count == 0 ? "none" : string.Join(",", HardwareCodecs);
            string software = SoftwareCodecs.Count == 0 ? "none" : string.Join(",", SoftwareCodecs);
            string nativeSessionCodecs = NativeSessionScaffoldCodecs.Count == 0 ? NativeSessionScaffoldCodec : string.Join(",", NativeSessionScaffoldCodecs);
            string hardwareH264 = HardwareH264Probe.MediaFoundationAvailable
                ? $"mf_h264_hw=enc:{HardwareH264Probe.EncoderCount}/dec:{HardwareH264Probe.DecoderCount}/prod:{HardwareH264Probe.ProductionSession.ToString().ToLowerInvariant()}"
                : "mf_h264_hw=unavailable";
            return $"loaded={DllLoaded.ToString().ToLowerInvariant()} available={Available.ToString().ToLowerInvariant()} backend={Backend} native_scaffold={NativeSessionScaffold.ToString().ToLowerInvariant()} scaffold_codecs={nativeSessionCodecs} native_nv12={NativeSessionNv12Input.ToString().ToLowerInvariant()} native_bgra_in={NativeSessionBgraInput.ToString().ToLowerInvariant()} native_bgra_out={NativeSessionBgraOutput.ToString().ToLowerInvariant()} codec_smoke={CodecRoundtripSmoke.ToString().ToLowerInvariant()} encoded={CodecRoundtripEncodedBytes}B hw={hardware} {hardwareH264} sw={software}; {Diagnostics}";
        }
    }

    // Hardware encode is what the native proof calls "hw" (it really opened the encoder and ran
    // frames); hardware decode is its own list; software encode/decode is libwebrtc's built-in set
    // ("sw": libaom/dav1d for AV1, OpenH264/FFmpeg for H.264).
    internal bool HardwareEncodes(string codec) => Contains(HardwareCodecs, codec);

    internal bool SoftwareEncodes(string codec) => Contains(SoftwareCodecs, codec);

    internal bool HardwareDecodes(string codec) => Contains(HardwareDecodeCodecs ?? [], codec);

    internal bool SoftwareDecodes(string codec) => Contains(SoftwareCodecs, codec);

    private static bool Contains(IReadOnlyList<string> list, string codec)
        => list.Any(entry => string.Equals(entry, codec, StringComparison.OrdinalIgnoreCase));

    internal bool SupportsNativeSessionCodec(CallVideoCodecKind codecKind)
    {
        string codecName = CallVideoCodecFactory.FormatCodec(codecKind);

        return NativeSessionScaffold &&
               (NativeSessionScaffoldCodecs.Any(codec => string.Equals(codec, codecName, StringComparison.OrdinalIgnoreCase)) ||
                string.Equals(NativeSessionScaffoldCodec, codecName, StringComparison.OrdinalIgnoreCase));
    }
}

internal sealed record NativeWebRtcHardwareH264Probe(
    bool MediaFoundationAvailable,
    uint EncoderCount,
    uint DecoderCount,
    bool ProductionSession,
    IReadOnlyList<string> EncoderNames,
    IReadOnlyList<string> DecoderNames,
    string Reason);

/// <summary>
/// One real GPU and what it can actually do for H.264, as reported by the native
/// per-adapter capability probe (decode = D3D11 video-device profile check, encode =
/// functional test-encode). Drives the Settings encode/decode device picker so it only
/// ever offers hardware that genuinely works.
/// </summary>
internal sealed record NativeVideoDevice(
    string Description,
    uint VendorId,
    bool Integrated,
    bool CanEncodeH264,
    bool CanDecodeH264);

internal static class NativeWebRtcMediaProbe
{
    private const uint ExpectedAbiVersion = NativeWebRtcMediaEngine.ExpectedAbiVersion;
    private const int MaxProbeJsonBytes = 1_048_576;
    private const string LibraryFileName = "omni_webrtc_media.dll";

    private static readonly object ProbeSync = new();
    private static NativeWebRtcMediaProbeResult? cachedProbeResult;
    private static long cachedProbeCompletedAtMs;
    private static Task? probeRefreshTask;

    /// <summary>
    /// The latest proof, at once. Blocks only when there has never been one — then it joins a
    /// refresh already running (the startup warm-up) instead of starting a second smoke.
    /// </summary>
    internal static NativeWebRtcMediaProbeResult GetProbeResult()
    {
        Task? running;
        lock (ProbeSync)
        {
            if (cachedProbeResult is not null)
            {
                return cachedProbeResult;
            }

            running = probeRefreshTask is { IsCompleted: false } ? probeRefreshTask : null;
        }

        running?.Wait();
        lock (ProbeSync)
        {
            if (cachedProbeResult is null)
            {
                cachedProbeResult = ProbeUncached();
                cachedProbeCompletedAtMs = Environment.TickCount64;
            }

            return cachedProbeResult;
        }
    }

    /// <summary>
    /// Re-run the real native hardware encode/decode smoke on a background thread, keeping the
    /// previous answer usable until the new one is in. Architecture wants the advertised H.264
    /// hardware to be PROVEN, not assumed (docs/decisions/h264-runtime-proof.md) — so the proof is
    /// refreshed at startup and after every call, while nothing else is using the encoder.
    ///
    /// It used to run at call start (first unconditionally, then "if older than 2 min"), which put
    /// the smoke — 2.5–3.5 s measured live 2026-10-06 — between "answered" and "the peer sees
    /// us" on nearly every call. An unattended remote-control session has no ringing to hide it
    /// in, so it paid the whole of it. A GPU that breaks between calls now shows up as a visible
    /// encode failure in that call, and in what we advertise from the next one on.
    /// </summary>
    internal static void RefreshInBackground()
    {
        lock (ProbeSync)
        {
            if (probeRefreshTask is { IsCompleted: false })
            {
                return;
            }

            // Its own thread, not a pool one: the smoke is ~3 s of BLOCKING native work, and
            // parking a thread-pool thread on it starves everything else queued there.
            probeRefreshTask = Task.Factory.StartNew(
                () =>
                {
                    NativeWebRtcMediaProbeResult fresh = ProbeUncached();
                    lock (ProbeSync)
                    {
                        cachedProbeResult = fresh;
                        cachedProbeCompletedAtMs = Environment.TickCount64;
                    }
                },
                CancellationToken.None,
                TaskCreationOptions.LongRunning | TaskCreationOptions.DenyChildAttach,
                TaskScheduler.Default);
        }
    }

    /// How old the proof in use is — for the diagnostics line, so a stale answer is visible.
    internal static long ProbeAgeMs()
    {
        lock (ProbeSync)
        {
            return cachedProbeResult is null ? -1 : Environment.TickCount64 - cachedProbeCompletedAtMs;
        }
    }

    internal static NativeWebRtcMediaProbeResult ParseProbeJsonForTests(string json)
        => ParseProbeJson(json, dllLoaded: true, diagnosticsPrefix: "test");

    // The library stays loaded after the first probe that found it. The probe proves WHICH hardware
    // encoder works (the native side remembers it, so a call picks that MFT first); unloading the
    // library threw that away whenever the call engine had not loaded it yet — and the call then
    // took the Intel HEVC MFT that stalls, not the NVIDIA one the probe had proven (stand,
    // 2026-10-14; docs/bugs/a-stuck-encoder-was-never-struck.md). The engine loads it anyway.
    private static nint keptLibraryHandle;

    private static NativeWebRtcMediaProbeResult ProbeUncached()
    {
        List<string> failures = [];
        foreach (string candidate in EnumerateLibraryCandidates())
        {
            if (!File.Exists(candidate))
            {
                failures.Add($"{candidate}: missing");
                continue;
            }

            nint libraryHandle = 0;
            try
            {
                libraryHandle = NativeLibrary.Load(candidate);
                NativeWebRtcMediaProbeResult result = ProbeLoadedLibrary(libraryHandle, candidate);
                if (result.DllLoaded && Interlocked.CompareExchange(ref keptLibraryHandle, libraryHandle, 0) == 0)
                {
                    libraryHandle = 0;
                }

                return result;
            }
            catch (Exception ex)
            {
                failures.Add($"{candidate}: {ex.GetType().Name}: {ex.Message}");
            }
            finally
            {
                if (libraryHandle != 0)
                {
                    NativeLibrary.Free(libraryHandle);
                }
            }
        }

        string diagnostics = failures.Count == 0
            ? $"{LibraryFileName}: no candidate paths"
            : string.Join("; ", failures.Take(4));
        return Unavailable(dllLoaded: false, backend: "none", diagnostics);
    }

    private static NativeWebRtcMediaProbeResult ProbeLoadedLibrary(nint libraryHandle, string loadedPath)
    {
        AbiVersionDelegate abiVersion = LoadExport<AbiVersionDelegate>(libraryHandle, "omni_webrtc_media_abi_version");
        uint abi = abiVersion();
        if (abi != ExpectedAbiVersion)
        {
            return Unavailable(
                dllLoaded: true,
                backend: "unknown",
                $"loaded={loadedPath}; unsupported ABI version {abi}, expected {ExpectedAbiVersion}");
        }

        ProbeJsonDelegate probeJson = LoadExport<ProbeJsonDelegate>(libraryHandle, "omni_webrtc_media_probe_json");
        string json = ReadProbeJson(probeJson);
        return ParseProbeJson(json, dllLoaded: true, diagnosticsPrefix: $"loaded={loadedPath}");
    }

    private static string ReadProbeJson(ProbeJsonDelegate probeJson)
    {
        OmniWebRtcMediaStatus status = probeJson(IntPtr.Zero, UIntPtr.Zero, out UIntPtr requiredLen);
        if (requiredLen == UIntPtr.Zero)
        {
            throw new InvalidOperationException($"native probe returned zero length with status={status}");
        }

        ulong requiredBytes = requiredLen.ToUInt64();
        if (requiredBytes > MaxProbeJsonBytes)
        {
            throw new InvalidOperationException($"native probe JSON is too large: {requiredBytes} bytes");
        }

        IntPtr buffer = Marshal.AllocHGlobal(checked((int)requiredBytes));
        try
        {
            status = probeJson(buffer, requiredLen, out UIntPtr writtenLen);
            if (status != OmniWebRtcMediaStatus.Ok && status != OmniWebRtcMediaStatus.BufferTooSmall)
            {
                throw new InvalidOperationException($"native probe failed with status={status}");
            }

            string? json = Marshal.PtrToStringUTF8(buffer);
            if (string.IsNullOrWhiteSpace(json))
            {
                throw new InvalidOperationException($"native probe returned empty JSON, required={writtenLen.ToUInt64()}");
            }

            return json;
        }
        finally
        {
            Marshal.FreeHGlobal(buffer);
        }
    }

    private static NativeWebRtcMediaProbeResult ParseProbeJson(
        string json,
        bool dllLoaded,
        string diagnosticsPrefix)
    {
        using JsonDocument document = JsonDocument.Parse(json);
        JsonElement root = document.RootElement;
        bool available = ReadBool(root, "available");
        string backend = ReadString(root, "backend", "unknown");
        bool supportsRtp = ReadBool(root, "supports_rtp_over_omni_quic");
        bool supportsRtcp = ReadBool(root, "supports_rtcp_feedback");
        bool nativeSessionScaffold = ReadBool(root, "native_session_scaffold");
        string nativeSessionScaffoldCodec = ReadString(root, "native_session_scaffold_codec", "none");
        IReadOnlyList<string> nativeSessionScaffoldCodecs = ReadNativeSessionScaffoldCodecs(
            root,
            nativeSessionScaffoldCodec);
        bool nativeSessionNv12Input = ReadBool(root, "native_session_nv12_input");
        bool nativeSessionBgraInput = ReadBool(root, "native_session_bgra_input");
        bool nativeSessionBgraOutput = ReadBool(root, "native_session_bgra_output");
        bool codecRoundtripSmoke = ReadBool(root, "codec_roundtrip_smoke");
        ulong codecRoundtripEncodedBytes = ReadUInt64(root, "codec_roundtrip_encoded_bytes");
        string codecRoundtripDiagnostics = ReadString(root, "codec_roundtrip_diagnostics", string.Empty);
        NativeWebRtcHardwareH264Probe hardwareH264Probe = ReadHardwareH264Probe(root);
        string reason = ReadString(root, "reason", available ? "ok" : "unavailable");
        string hardwareH264Diagnostics = hardwareH264Probe.MediaFoundationAvailable
            ? $"mf_h264_hw enc={hardwareH264Probe.EncoderCount} dec={hardwareH264Probe.DecoderCount} prod={hardwareH264Probe.ProductionSession.ToString().ToLowerInvariant()}: {hardwareH264Probe.Reason}"
            : hardwareH264Probe.Reason;
        string diagnostics = $"{diagnosticsPrefix}; {reason}; {hardwareH264Diagnostics}";
        if (!string.IsNullOrWhiteSpace(codecRoundtripDiagnostics))
        {
            diagnostics = $"{diagnostics}; codec_roundtrip={codecRoundtripDiagnostics}";
        }

        return new NativeWebRtcMediaProbeResult(
            dllLoaded,
            available,
            backend,
            supportsRtp,
            supportsRtcp,
            nativeSessionScaffold,
            nativeSessionScaffoldCodec,
            nativeSessionScaffoldCodecs,
            nativeSessionNv12Input,
            nativeSessionBgraInput,
            nativeSessionBgraOutput,
            codecRoundtripSmoke,
            codecRoundtripEncodedBytes,
            ReadStringArray(root, "hardware_codecs"),
            hardwareH264Probe,
            ReadVideoDevices(root),
            ReadStringArray(root, "software_codecs"),
            diagnostics,
            ReadStringArray(root, "hardware_decode_codecs"));
    }

    private static TDelegate LoadExport<TDelegate>(nint libraryHandle, string exportName)
        where TDelegate : Delegate
    {
        if (!NativeLibrary.TryGetExport(libraryHandle, exportName, out IntPtr exportAddress))
        {
            throw new EntryPointNotFoundException(exportName);
        }

        return Marshal.GetDelegateForFunctionPointer<TDelegate>(exportAddress);
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

    private static NativeWebRtcMediaProbeResult Unavailable(bool dllLoaded, string backend, string diagnostics)
        => new(
            dllLoaded,
            Available: false,
            backend,
            SupportsRtpOverOmniQuic: false,
            SupportsRtcpFeedback: false,
            NativeSessionScaffold: false,
            NativeSessionScaffoldCodec: "none",
            NativeSessionScaffoldCodecs: [],
            NativeSessionNv12Input: false,
            NativeSessionBgraInput: false,
            NativeSessionBgraOutput: false,
            CodecRoundtripSmoke: false,
            CodecRoundtripEncodedBytes: 0,
            HardwareCodecs: [],
            HardwareH264Probe: new NativeWebRtcHardwareH264Probe(
                MediaFoundationAvailable: false,
                EncoderCount: 0,
                DecoderCount: 0,
                ProductionSession: false,
                EncoderNames: [],
                DecoderNames: [],
                Reason: diagnostics),
            VideoDevices: [],
            SoftwareCodecs: [],
            diagnostics);

    private static bool ReadBool(JsonElement root, string propertyName)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.True;

    private static string ReadString(JsonElement root, string propertyName, string fallback)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.String
            ? element.GetString() ?? fallback
            : fallback;

    private static ulong ReadUInt64(JsonElement root, string propertyName)
        => root.TryGetProperty(propertyName, out JsonElement element) &&
           element.ValueKind == JsonValueKind.Number &&
           element.TryGetUInt64(out ulong value)
            ? value
            : 0;

    private static IReadOnlyList<string> ReadStringArray(JsonElement root, string propertyName)
    {
        if (!root.TryGetProperty(propertyName, out JsonElement element) ||
            element.ValueKind != JsonValueKind.Array)
        {
            return [];
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

        return values;
    }

    private static IReadOnlyList<string> ReadNativeSessionScaffoldCodecs(
        JsonElement root,
        string fallbackCodec)
    {
        IReadOnlyList<string> codecs = ReadStringArray(root, "native_session_scaffold_codecs");
        if (codecs.Count > 0)
        {
            return codecs;
        }

        return string.IsNullOrWhiteSpace(fallbackCodec) ||
               string.Equals(fallbackCodec, "none", StringComparison.OrdinalIgnoreCase)
            ? []
            : [fallbackCodec];
    }

    private static IReadOnlyList<NativeVideoDevice> ReadVideoDevices(JsonElement root)
    {
        if (!root.TryGetProperty("video_devices", out JsonElement element) ||
            element.ValueKind != JsonValueKind.Array)
        {
            return [];
        }

        List<NativeVideoDevice> devices = [];
        foreach (JsonElement item in element.EnumerateArray())
        {
            if (item.ValueKind != JsonValueKind.Object)
            {
                continue;
            }

            string description = ReadString(item, "description", string.Empty);
            if (string.IsNullOrWhiteSpace(description))
            {
                continue;
            }

            devices.Add(new NativeVideoDevice(
                description,
                checked((uint)Math.Min(ReadUInt64(item, "vendor_id"), uint.MaxValue)),
                ReadBool(item, "integrated"),
                ReadBool(item, "can_encode_h264"),
                ReadBool(item, "can_decode_h264")));
        }

        return devices;
    }

    private static NativeWebRtcHardwareH264Probe ReadHardwareH264Probe(JsonElement root)
    {
        if (!root.TryGetProperty("hardware_h264_probe", out JsonElement probe) ||
            probe.ValueKind != JsonValueKind.Object)
        {
            return new NativeWebRtcHardwareH264Probe(
                MediaFoundationAvailable: false,
                EncoderCount: 0,
                DecoderCount: 0,
                ProductionSession: false,
                EncoderNames: [],
                DecoderNames: [],
                Reason: "hardware_h264_probe is not present in native probe");
        }

        return new NativeWebRtcHardwareH264Probe(
            ReadBool(probe, "media_foundation_available"),
            checked((uint)Math.Min(ReadUInt64(probe, "encoder_count"), uint.MaxValue)),
            checked((uint)Math.Min(ReadUInt64(probe, "decoder_count"), uint.MaxValue)),
            ReadBool(probe, "production_session"),
            ReadStringArray(probe, "encoder_names"),
            ReadStringArray(probe, "decoder_names"),
            ReadString(probe, "reason", string.Empty));
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate uint AbiVersionDelegate();

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate OmniWebRtcMediaStatus ProbeJsonDelegate(
        IntPtr buffer,
        UIntPtr bufferLen,
        out UIntPtr requiredLen);

    private enum OmniWebRtcMediaStatus
    {
        Ok = 0,
        InvalidArgument = 1,
        BufferTooSmall = 2,
        NotImplemented = 3,
        InternalError = 4
    }
}
