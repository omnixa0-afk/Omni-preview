using System.Text;
using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// Settings → Calls → «Кодек»: a CEILING, not an order (docs/decisions/per-pair-video-codec-selection.md
/// §3) — codecs above it are not used for what this machine sends or receives; below it the core
/// still picks per direction.
/// </summary>
internal enum CallVideoCodecPreference
{
    Auto,
    UpToH265,
    H264,
}

/// <summary>
/// Одна строка меню разрешения у отправителя. Обе стороны чётные — этого требует H.264, и
/// нечётная сторона роняет создание сессии, а не отдельный кадр.
/// </summary>
internal readonly record struct CallVideoResolutionOption(int Width, int Height)
{
    internal static CallVideoResolutionOption Even(int width, int height)
        => new(width - (width % 2), height - (height % 2));

    internal string DisplayName => $"{Height}p ({Width}x{Height})";
}

/// <summary>
/// Settings → Calls → «Битрейт»: scales the bitrate CEILING of both directions of a call — the
/// congestion controller still sets the rate below it. Travels as <c>bitrate_level</c>; Economy on
/// either end wins, Max on either end asks for more (RealtimeVideoBitrateLevel::percent_for_pair).
/// Persisted per device in device-preferences.json (key "callVideoBitrate").
/// </summary>
internal enum CallVideoBitratePreference
{
    Economy,
    Auto,
    Max,
}

internal enum CallVideoFrameRatePreference
{
    Auto,
    Fps30,
    Fps60,
}

internal readonly record struct CallVideoPreferenceSnapshot(
    CallVideoCodecPreference Codec,
    CallVideoResolutionOption Resolution,
    CallVideoFrameRatePreference FrameRate);

internal static class CallVideoPreferences
{
    private const int MaxCodecImplementationUtf8Bytes = 128;

    internal const string CapturePixelFormatNv12 = "nv12";
    internal const string CapturePixelFormatBgra = "bgra";
    internal static readonly IReadOnlyList<string> SupportedCapturePixelFormats =
    [
        CapturePixelFormatNv12,
        CapturePixelFormatBgra,
    ];

    internal const int DefaultTargetFps = 30;
    internal const int DefaultMaxFps = 60;
    // Разрешение по умолчанию — 720p, и это ПРЕДЛОЖЕНИЕ, а не потолок: отправитель выбирает сам,
    // вплоть до максимума своей камеры или экрана (см. BuildResolutionOptions).
    internal const int DefaultPreferredCaptureWidth = 1280;
    internal const int DefaultPreferredCaptureHeight = 720;
    /// The largest side a negotiated format may have (hardware encoders top out at 4096).
    internal const int MaxVideoSide = 4096;

    /// A shared screen goes in its own size by default, but not above this height: past 1440p text
    /// stays sharp while the bits per frame double (docs/decisions/per-direction-video-format.md).
    internal const int DefaultMaxScreenHeight = 1440;

    /// A shared screen's frame rate: text needs bits per frame, not frames.
    internal const int ScreenFps = 30;

    /// Высота ступени, которую предлагаем по умолчанию, если источник её вытягивает.
    internal const int DefaultResolutionHeight = 720;

    /// Ступени, которые показываем в меню. Это ПРЕДЛОЖЕНИЯ, а не ограничения: всё, что выше
    /// максимума источника, отбрасывается, а сам максимум добавляется отдельной верхней строкой,
    /// даже если он не совпадает ни с одной ступенью (нестандартные матрицы и мониторы).
    private static readonly int[] OfferedResolutionHeights = [2160, 1440, 1080, 720, 540, 480, 360, 240];

    /// <summary>
    /// Что отправитель может выбрать для ЭТОГО источника. Список строится от реальных
    /// возможностей: у камеры — от её профилей, у экрана — от размера монитора.
    ///
    /// Пропорции берутся у источника, обе стороны остаются чётными (H.264 их требует, а
    /// нечётная сторона роняет кодировщик на этапе создания сессии, а не на кадре).
    /// </summary>
    internal static IReadOnlyList<CallVideoResolutionOption> BuildResolutionOptions(
        int sourceWidth,
        int sourceHeight)
    {
        if (sourceWidth <= 0 || sourceHeight <= 0)
        {
            return [];
        }

        List<CallVideoResolutionOption> options = [CallVideoResolutionOption.Even(sourceWidth, sourceHeight)];
        foreach (int height in OfferedResolutionHeights)
        {
            if (height >= sourceHeight)
            {
                continue;
            }

            int width = (int)Math.Round((double)sourceWidth * height / sourceHeight);
            CallVideoResolutionOption option = CallVideoResolutionOption.Even(width, height);
            if (option.Width <= 0 || option.Height <= 0 || options.Contains(option))
            {
                continue;
            }

            options.Add(option);
        }

        return options;
    }

    /// <summary>
    /// Выбор по умолчанию: 720p, а если источник столько не даёт — его максимум. Именно так это
    /// и просили: «базово 720p, если максимальное конечно не меньше».
    /// </summary>
    internal static CallVideoResolutionOption DefaultResolution(
        IReadOnlyList<CallVideoResolutionOption> options)
    {
        if (options.Count == 0)
        {
            return CallVideoResolutionOption.Even(
                DefaultPreferredCaptureWidth,
                DefaultPreferredCaptureHeight);
        }

        CallVideoResolutionOption? best = null;
        foreach (CallVideoResolutionOption option in options)
        {
            if (option.Height > DefaultResolutionHeight)
            {
                continue;
            }

            if (best is null || option.Height > best.Value.Height)
            {
                best = option;
            }
        }

        // Ни одна ступень не влезла в 720p — значит источник мельче любой из них, и его максимум
        // (первая строка списка) и есть лучшее, что мы можем предложить.
        return best ?? options[0];
    }

    /// <summary>
    /// Размер демонстрации экрана по умолчанию: родной размер источника, но не выше 1440p.
    /// <paramref name="options"/> — от <see cref="BuildResolutionOptions"/>, первая строка — родной размер.
    /// </summary>
    internal static CallVideoResolutionOption DefaultScreenResolution(
        IReadOnlyList<CallVideoResolutionOption> options)
    {
        foreach (CallVideoResolutionOption option in options)
        {
            if (option.Height <= DefaultMaxScreenHeight)
            {
                return option;
            }
        }

        return options.Count > 0
            ? options[^1]
            : CallVideoResolutionOption.Even(DefaultPreferredCaptureWidth, DefaultPreferredCaptureHeight);
    }

    /// <summary>
    /// Describe this machine to the peer. Deliberately CANNOT fail: no camera, no
    /// hardware H.264, a capture probe that came back empty — each of those narrows
    /// what we offer, none of them stops us announcing ourselves.
    ///
    /// It used to throw on every one of those, and the caller turned the exception
    /// into a published-nothing. That took the call's AUDIO down with the video:
    /// no capabilities → no negotiated session → neither peer ever started its
    /// audio pipeline, and the peer that DID have a camera found its camera button
    /// permanently disabled. Live standalone test 2026-08-11, twice.
    /// See docs/decisions/call-audio-without-video.md.
    /// </summary>
    internal static RealtimeMediaCapabilities BuildCapabilities(
        string dialogKey,
        CallVideoCodecPreference codecPreference,
        CallVideoResolutionOption resolution,
        CallVideoFrameRatePreference frameRatePreference,
        CallVideoCaptureProbeResult captureProbe,
        IReadOnlyList<CallVideoCodecSupport>? supportedCodecs = null,
        bool supportsRtcpFeedback = true,
        bool canShareScreen = false,
        bool onBattery = false,
        string cpuLoad = "medium",
        bool screenContent = false,
        CallVideoResolutionOption? screenResolution = null,
        CallVideoBitratePreference bitrate = CallVideoBitratePreference.Auto)
    {
        IReadOnlyList<CallVideoCodecSupport> codecs = SelectCodecs(
            codecPreference,
            supportedCodecs ?? CallVideoCodecFactory.GetSupportedCodecs());
        // The profile says what this machine SENDS now: the shared screen while a share runs, else
        // the camera (docs/decisions/per-direction-video-format.md).
        RealtimeVideoProfileCapability? video = codecs.Count == 0
            ? null
            : screenContent && screenResolution is { } screen
                ? BuildScreenProfile(screen)
                : BuildVideoProfileCapability(resolution, frameRatePreference, captureProbe, canShareScreen);

        return new RealtimeMediaCapabilities(
            dialogKey,
            codecs.Select(codec => new RealtimeVideoCodecCapability(
                    CallVideoCodecFactory.FormatCodec(codec.Kind),
                    checked((byte)codec.PayloadType),
                    codec.EncodeHardware is bool encode ? new RealtimeVideoCodecSide(encode) : null,
                    codec.DecodeHardware is bool decode ? new RealtimeVideoCodecSide(decode) : null,
                    NormalizeCodecImplementation(codec.DiagnosticName)))
                .ToArray(),
            video,
            (uint)CallVideoRtpConstants.MaxDatagramBytes,
            supportsRtcpFeedback,
            SentAtUnixMs: DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
            Power: onBattery ? "battery" : "mains",
            CpuLoad: cpuLoad,
            Content: screenContent ? "screen" : "camera",
            CodecCeiling: codecPreference switch
            {
                CallVideoCodecPreference.H264 => "h264",
                CallVideoCodecPreference.UpToH265 => "h265",
                _ => null,
            },
            BitrateLevel: FormatBitratePreference(bitrate));
    }

    internal static string FormatBitratePreference(CallVideoBitratePreference preference)
        => preference switch
        {
            CallVideoBitratePreference.Economy => "economy",
            CallVideoBitratePreference.Auto => "auto",
            CallVideoBitratePreference.Max => "max",
            _ => throw new ArgumentOutOfRangeException(nameof(preference), preference, null),
        };

    internal static CallVideoBitratePreference ParseBitratePreference(string? value)
        => value?.Trim().ToLowerInvariant() switch
        {
            "economy" => CallVideoBitratePreference.Economy,
            "max" => CallVideoBitratePreference.Max,
            _ => CallVideoBitratePreference.Auto,
        };

    /// <summary>
    /// Whether this machine can be a video SOURCE for the requested profile. The
    /// camera button is gated on this, so a machine that cannot capture shows a
    /// disabled button with a reason instead of one that fails when pressed.
    /// </summary>
    internal static bool CanCaptureVideo(
        CallVideoResolutionOption resolution,
        CallVideoFrameRatePreference frameRatePreference,
        CallVideoCaptureProbeResult captureProbe)
        => BuildCaptureProfileOrNull(resolution, frameRatePreference, captureProbe) is not null;

    internal static string FormatPreferences(CallVideoPreferenceSnapshot preferences)
        => $"preferences={FormatCodecPreference(preferences.Codec)}/{FormatQualityPreference(preferences.Resolution)}/{FormatFrameRatePreference(preferences.FrameRate)}";

    /// <summary>
    /// The passport's codec lines. A ceiling ("H.264" in Settings) also travels as
    /// <c>codec_ceiling</c>, so the peer does not send us anything above it either. Software H.264
    /// encoding is never offered (CallVideoCodecFactory, docs/decisions/h264-runtime-proof.md).
    /// </summary>
    private static IReadOnlyList<CallVideoCodecSupport> SelectCodecs(
        CallVideoCodecPreference preference,
        IReadOnlyList<CallVideoCodecSupport> supportedCodecs)
    {
        CallVideoCodecKind? forcedCodec = preference switch
        {
            CallVideoCodecPreference.Auto => null,
            CallVideoCodecPreference.UpToH265 => null,
            CallVideoCodecPreference.H264 => CallVideoCodecKind.H264,
            _ => throw new ArgumentOutOfRangeException(nameof(preference), preference, null),
        };

        IReadOnlyList<CallVideoCodecSupport> eligibleCodecs = supportedCodecs
            .Where(codec => codec.Kind != CallVideoCodecKind.H264 || codec.EncodeHardware != false)
            .ToArray();
        return forcedCodec.HasValue
            ? eligibleCodecs.Where(codec => codec.Kind == forcedCodec.Value).ToArray()
            : eligibleCodecs;
    }

    /// <summary>
    /// The video profile to publish. When the camera can serve the requested profile
    /// this is that profile, marked as sendable. When it cannot — no camera, busy
    /// camera, no format matching the picked quality — we still publish the range we
    /// can DECODE, so the peer with a camera can send us video and we can watch it
    /// while both of us talk. That range is sendable too when this machine can share
    /// its screen: the screen is a video source, and without counting it two machines
    /// with no camera negotiated audio-only and could never show each other a screen.
    /// </summary>
    private static RealtimeVideoProfileCapability BuildVideoProfileCapability(
        CallVideoResolutionOption resolution,
        CallVideoFrameRatePreference frameRatePreference,
        CallVideoCaptureProbeResult captureProbe,
        bool canShareScreen)
        => BuildCaptureProfileOrNull(resolution, frameRatePreference, captureProbe)
           ?? BuildDecodeOnlyProfile(canShareScreen);

    /// No camera to send. Sendable when this machine can share its screen, so a video direction
    /// exists before the share starts; the share announces its own size then (`BuildScreenProfile`).
    /// What it RECEIVES is not limited here — that is its codec lines' `decode` side. This profile
    /// used to be pinned to 1280x720, and through the intersection of the two passports it capped
    /// the peer's screen share at 720p (docs/bugs/screen-share-capped-at-720p.md).
    private static RealtimeVideoProfileCapability BuildDecodeOnlyProfile(bool canShareScreen)
        => new(
            MinWidth: 1,
            MinHeight: 1,
            MinFps: 1,
            PreferredWidth: DefaultPreferredCaptureWidth,
            PreferredHeight: DefaultPreferredCaptureHeight,
            PreferredFps: DefaultTargetFps,
            MaxWidth: DefaultPreferredCaptureWidth,
            MaxHeight: DefaultPreferredCaptureHeight,
            MaxFps: DefaultMaxFps,
            PixelFormats: SupportedCapturePixelFormats,
            CanSendVideo: canShareScreen);

    /// The shared screen in the size picked for it, up to <see cref="ScreenFps"/>.
    internal static RealtimeVideoProfileCapability BuildScreenProfile(CallVideoResolutionOption screen)
        => new(
            MinWidth: (uint)screen.Width,
            MinHeight: (uint)screen.Height,
            MinFps: 1,
            PreferredWidth: (uint)screen.Width,
            PreferredHeight: (uint)screen.Height,
            PreferredFps: ScreenFps,
            MaxWidth: (uint)screen.Width,
            MaxHeight: (uint)screen.Height,
            MaxFps: ScreenFps,
            PixelFormats: SupportedCapturePixelFormats,
            CanSendVideo: true);

    private static RealtimeVideoProfileCapability? BuildCaptureProfileOrNull(
        CallVideoResolutionOption resolution,
        CallVideoFrameRatePreference frameRatePreference,
        CallVideoCaptureProbeResult captureProbe)
    {
        if (captureProbe.Formats.Count == 0)
        {
            return null;
        }

        uint preferredFps = ResolvePreferredFps(frameRatePreference);
        uint minFps = ResolveMinFps(frameRatePreference);
        uint maxFps = ResolveMaxFps(frameRatePreference);
        (uint minWidth, uint preferredWidth, uint maxWidth) = ResolveWidthRange(resolution);
        (uint minHeight, uint preferredHeight, uint maxHeight) = ResolveHeightRange(resolution);
        IReadOnlyList<CallVideoCaptureFormat> candidates = SelectCaptureCandidates(
            captureProbe,
            minWidth,
            minHeight,
            minFps);
        if (candidates.Count == 0)
        {
            return null;
        }


        if (frameRatePreference == CallVideoFrameRatePreference.Auto)
        {
            CallVideoCaptureFormat autoPreferred = SelectAutoPreferredFormat(candidates);
            preferredFps = Math.Min(autoPreferred.Fps, (uint)DefaultTargetFps);
            minFps = preferredFps;
            maxFps = preferredFps;
        }

        IReadOnlyList<string> pixelFormats = SelectPixelFormatsForPublishedProfile(
            captureProbe,
            minWidth,
            minHeight,
            minFps);
        if (pixelFormats.Count == 0)
        {
            return null;
        }

        return new RealtimeVideoProfileCapability(
            minWidth,
            minHeight,
            minFps,
            preferredWidth,
            preferredHeight,
            preferredFps,
            maxWidth,
            maxHeight,
            maxFps,
            pixelFormats,
            CanSendVideo: true);
    }

    private static IReadOnlyList<CallVideoCaptureFormat> SelectCaptureCandidates(
        CallVideoCaptureProbeResult captureProbe,
        uint minWidth,
        uint minHeight,
        uint minFps)
        => captureProbe.Formats
            .Where(format => format.Width >= minWidth &&
                             format.Height >= minHeight &&
                             format.Fps >= minFps)
            .ToArray();

    private static IReadOnlyList<string> SelectPixelFormatsForPublishedProfile(
        CallVideoCaptureProbeResult captureProbe,
        uint minWidth,
        uint minHeight,
        uint minFps)
        => captureProbe.Formats
            .Where(format => format.Width >= minWidth &&
                             format.Height >= minHeight &&
                             format.Fps >= minFps)
            .Select(format => format.PixelFormat)
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .OrderBy(format => format == CapturePixelFormatNv12 ? 0 : 1)
            .ToArray();

    private static CallVideoCaptureFormat SelectAutoPreferredFormat(IReadOnlyList<CallVideoCaptureFormat> formats)
    {
        CallVideoCaptureFormat? bestUnderDefault = formats
            .Where(format => format.Width <= DefaultPreferredCaptureWidth &&
                             format.Height <= DefaultPreferredCaptureHeight &&
                             format.Fps >= DefaultTargetFps)
            .OrderByDescending(format => format.Width * format.Height)
            .ThenBy(format => format.Fps)
            .FirstOrDefault();
        if (bestUnderDefault is not null)
        {
            return bestUnderDefault;
        }

        return formats
            .OrderBy(format => format.Width * format.Height)
            .ThenBy(format => Math.Abs((int)format.Fps - DefaultTargetFps))
            .First();
    }

    // Выбор отправителя — ТОЧНЫЙ размер, а не диапазон. Раньше здесь стоял Auto с диапазоном
    // (1 .. потолок), и именно он позволял захвату молча приехать не в том разрешении.
    private static (uint Min, uint Preferred, uint Max) ResolveWidthRange(CallVideoResolutionOption resolution)
        => Exact((uint)Math.Max(1, resolution.Width));

    private static (uint Min, uint Preferred, uint Max) ResolveHeightRange(CallVideoResolutionOption resolution)
        => Exact((uint)Math.Max(1, resolution.Height));

    private static (uint Min, uint Preferred, uint Max) Exact(uint value)
        => (value, value, value);

    private static uint ResolvePreferredFps(CallVideoFrameRatePreference preference)
        => preference switch
        {
            CallVideoFrameRatePreference.Auto => DefaultTargetFps,
            CallVideoFrameRatePreference.Fps30 => 30,
            CallVideoFrameRatePreference.Fps60 => 60,
            _ => throw new ArgumentOutOfRangeException(nameof(preference), preference, null),
        };

    private static uint ResolveMinFps(CallVideoFrameRatePreference preference)
        => preference == CallVideoFrameRatePreference.Auto ? 1u : ResolvePreferredFps(preference);

    private static uint ResolveMaxFps(CallVideoFrameRatePreference preference)
        => preference == CallVideoFrameRatePreference.Auto ? DefaultMaxFps : ResolvePreferredFps(preference);

    private static string FormatCodecPreference(CallVideoCodecPreference preference)
        => preference switch
        {
            CallVideoCodecPreference.Auto => "auto",
            CallVideoCodecPreference.UpToH265 => "up_to_h265",
            CallVideoCodecPreference.H264 => "h264",
            _ => throw new ArgumentOutOfRangeException(nameof(preference), preference, null),
        };

    private static string FormatQualityPreference(CallVideoResolutionOption resolution)
        => $"{resolution.Height}p";

    private static string FormatFrameRatePreference(CallVideoFrameRatePreference preference)
        => preference switch
        {
            CallVideoFrameRatePreference.Auto => "auto",
            CallVideoFrameRatePreference.Fps30 => "30fps",
            CallVideoFrameRatePreference.Fps60 => "60fps",
            _ => throw new ArgumentOutOfRangeException(nameof(preference), preference, null),
        };

    private static string? NormalizeCodecImplementation(string? implementation)
    {
        if (string.IsNullOrWhiteSpace(implementation))
        {
            return null;
        }

        string trimmed = implementation.Trim();
        if (Encoding.UTF8.GetByteCount(trimmed) <= MaxCodecImplementationUtf8Bytes)
        {
            return trimmed;
        }

        StringBuilder builder = new(trimmed.Length);
        int bytes = 0;
        foreach (Rune rune in trimmed.EnumerateRunes())
        {
            if (bytes + rune.Utf8SequenceLength > MaxCodecImplementationUtf8Bytes)
            {
                break;
            }

            builder.Append(rune);
            bytes += rune.Utf8SequenceLength;
        }

        return builder.Length == 0 ? null : builder.ToString();
    }
}
