namespace Omni.Windows.App.Features.Calls;

internal enum CallVideoCodecKind
{
    H264,
    H265,
    Av1,
}

internal enum CallVideoPixelFormat
{
    Bgra,
    NV12,
}

/// <summary>
/// What this machine does with one codec: <c>EncodeHardware</c>/<c>DecodeHardware</c> null = cannot,
/// true = in hardware, false = in software.
/// </summary>
internal readonly record struct CallVideoCodecSupport(
    CallVideoCodecKind Kind,
    int PayloadType,
    bool? EncodeHardware,
    bool? DecodeHardware,
    string DiagnosticName);

internal static class CallVideoRtpConstants
{
    // The same table as the core's default_video_payload_type.
    internal const int H264PayloadType = 102;
    internal const int H265PayloadType = 49;
    internal const int Av1PayloadType = 45;

    internal static int PayloadTypeFor(CallVideoCodecKind kind) => kind switch
    {
        CallVideoCodecKind.H265 => H265PayloadType,
        CallVideoCodecKind.Av1 => Av1PayloadType,
        _ => H264PayloadType,
    };
    internal const int MaxDatagramBytes = 1000;

    // Признак отчёта о доставке (RTCP 205/11) узнаёт теперь нативный разборщик входящих
    // (OmniWebRtcMediaRouter::IsCongestionControlFeedback) — оболочка пакетов не видит.
}

internal static class CallVideoCodecFactory
{
    internal static NativeWebRtcMediaProbeResult GetNativeWebRtcProbeResult()
        => NativeWebRtcMediaProbe.GetProbeResult();

    internal static IReadOnlyList<CallVideoCodecSupport> GetSupportedCodecs()
        => GetSupportedCodecs(NativeWebRtcMediaProbe.GetProbeResult());

    /// <summary>
    /// The passport's codec lines from the native proof: per codec whether the native session can
    /// run it at all, and then encode/decode, hardware/software. H.264 and HEVC are hardware-only
    /// (no software H.264 in the build, docs/decisions/per-pair-video-codec-selection.md); a machine
    /// without them sends and watches AV1 in software.
    /// </summary>
    internal static IReadOnlyList<CallVideoCodecSupport> GetSupportedCodecs(NativeWebRtcMediaProbeResult probe)
    {
        if (!probe.NativeSessionScaffold)
        {
            return [];
        }

        List<CallVideoCodecSupport> codecs = [];
        foreach (CallVideoCodecKind kind in new[] { CallVideoCodecKind.Av1, CallVideoCodecKind.H265, CallVideoCodecKind.H264 })
        {
            if (!probe.SupportsNativeSessionCodec(kind))
            {
                continue;
            }

            string name = FormatCodec(kind);
            bool? encode = probe.HardwareEncodes(name) ? true
                : probe.SoftwareEncodes(name) ? false
                : null;
            bool? decode = probe.HardwareDecodes(name) ? true
                : probe.SoftwareDecodes(name) ? false
                : null;
            if (encode is null && decode is null)
            {
                continue;
            }

            codecs.Add(new CallVideoCodecSupport(
                kind,
                CallVideoRtpConstants.PayloadTypeFor(kind),
                encode,
                decode,
                $"native/libwebrtc {name} enc={FormatSide(encode)} dec={FormatSide(decode)}"));
        }

        return codecs;
    }

    /// <summary>
    /// Settings → Calls can send encoding or decoding to the processor. Then the passport must not
    /// promise what only the video card does: the native session skips the hardware path, and a
    /// codec without a software one (HEVC both ways, hardware AV1 encode) would leave the call
    /// without a picture.
    /// </summary>
    internal static IReadOnlyList<CallVideoCodecSupport> RestrictToDevices(
        IReadOnlyList<CallVideoCodecSupport> codecs,
        bool encodeSoftwareOnly,
        bool decodeSoftwareOnly)
    {
        if (!encodeSoftwareOnly && !decodeSoftwareOnly)
        {
            return codecs;
        }

        List<CallVideoCodecSupport> restricted = [];
        foreach (CallVideoCodecSupport codec in codecs)
        {
            bool? encode = codec.EncodeHardware;
            bool? decode = codec.DecodeHardware;
            // Software here means libwebrtc's built-in set: AV1 alone (libaom/dav1d). H.264 and HEVC
            // have no software path, so on the processor they drop out and the pair falls to AV1.
            bool software = codec.Kind == CallVideoCodecKind.Av1;
            if (encodeSoftwareOnly && encode == true)
            {
                encode = software ? false : null;
            }

            if (decodeSoftwareOnly && decode == true)
            {
                decode = software ? false : null;
            }

            if (encode is null && decode is null)
            {
                continue;
            }

            restricted.Add(codec with { EncodeHardware = encode, DecodeHardware = decode });
        }

        return restricted;
    }

    private static string FormatSide(bool? hardware)
        => hardware switch { true => "hw", false => "sw", null => "none" };

    internal static string FormatCodec(CallVideoCodecKind codecKind)
        => codecKind switch
        {
            CallVideoCodecKind.H265 => "h265",
            CallVideoCodecKind.Av1 => "av1",
            _ => "h264",
        };

    internal static CallVideoCodecKind? ParseCodec(string? codec)
        => (codec ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "h264" => CallVideoCodecKind.H264,
            "h265" => CallVideoCodecKind.H265,
            "av1" => CallVideoCodecKind.Av1,
            _ => null,
        };
}
