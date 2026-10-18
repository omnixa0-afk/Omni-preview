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

// Capture-format selection and matching: picks the best MediaFrameFormat for the negotiated
// resolution/codec/pixel-format and decides whether a candidate format satisfies the active
// selection / explicit profile. Split out of CallVideoService.cs as a partial; no behaviour change.
internal sealed partial class CallVideoService
{
    private IReadOnlyList<MediaFrameFormat> SelectPreferredVideoFormats(MediaFrameSource source)
    {
        IReadOnlyList<MediaFrameFormat> rawFormats = source.SupportedFormats
            .Where(IsRawVideoFormat)
            .ToArray();
        IEnumerable<MediaFrameFormat> candidates = rawFormats.Count > 0
            ? rawFormats
            : source.SupportedFormats;
        CaptureProfileCap profileCap = CurrentCaptureProfileCap();
        if (profileCap.IsActive)
        {
            // ПОЛИТИКА (2026-08-15): в пределах потолка побеждает БОЛЬШЕЕ разрешение, а тип формата
            // решает лишь при равном размере (сырой дешевле — не надо распаковывать JPEG).
            // Раньше тип шёл ПЕРВЫМ ключом, и сырой 960x540 обходил MJPG 1280x720 — то есть даже
            // после добавления MJPG в кандидаты выбор бы не изменился.
            MediaFrameFormat[] cappedFormats = candidates
                .Where(format => FormatMatchesSelectedPixelFormat(format.Subtype))
                .Where(format => CaptureProfileMeetsActiveCap(format, profileCap))
                .OrderBy(format => AspectBucket(format, profileCap.Aspect))
                .ThenByDescending(format => format.VideoFormat.Width * format.VideoFormat.Height)
                .ThenByDescending(format => GetCaptureSubtypePriority(format.Subtype))
                .ThenBy(format => AspectDistance(format, profileCap.Aspect))
                .ThenBy(format => Math.Abs(GetFrameRate(format) - targetFps))
                .ToArray();
            if (cappedFormats.Length > 0)
            {
                return cappedFormats;
            }

            throw new InvalidOperationException(
                $"capture_unavailable: selected camera source does not expose fallback <= {profileCap.MaxWidth}x{profileCap.MaxHeight}@{targetFps} {selectedCapturePixelFormat} profile.");
        }

        if (RequiresExplicitCaptureProfile())
        {
            candidates = candidates.Where(FormatMeetsExplicitCaptureProfile).ToArray();
            if (!candidates.Any())
            {
                throw new InvalidOperationException(
                    $"capture_unavailable: selected camera source does not expose the requested {targetCaptureWidth}x{targetCaptureHeight}@{targetFps} profile.");
            }
        }

        // Здесь отбор идёт по «не меньше согласованного», поэтому берём САМЫЙ БЛИЗКИЙ сверху —
        // порядок по размеру остаётся возрастающим. Тип формата решает при равном размере: сырой
        // предпочтительнее MJPG, потому что не требует распаковки.
        MediaFrameFormat[] negotiatedFormats = candidates
            .Where(FormatMeetsNegotiatedSelection)
            .OrderBy(format => format.VideoFormat.Width * format.VideoFormat.Height)
            .ThenByDescending(format => GetCaptureSubtypePriority(format.Subtype))
            .ThenBy(format => Math.Abs(GetFrameRate(format) - targetFps))
            .ToArray();
        if (negotiatedFormats.Length > 0)
        {
            return negotiatedFormats;
        }

        throw new InvalidOperationException(
            $"capture_unavailable: selected camera source does not expose negotiated {targetCaptureWidth}x{targetCaptureHeight}@{targetFps} {selectedCapturePixelFormat} profile.");

    }

    private bool CurrentFormatMatchesActiveSelection(MediaFrameFormat format, out string failure)
    {
        if (!FormatMatchesSelectedPixelFormat(format.Subtype))
        {
            failure = $"expected {selectedCapturePixelFormat}; got {format.Subtype}.";
            return false;
        }

        return CaptureProfileMeetsActiveSelection(
            checked((int)format.VideoFormat.Width),
            checked((int)format.VideoFormat.Height),
            GetFrameRate(format),
            out failure);
    }

    private static string FormatMediaFrameFormat(MediaFrameFormat format)
        => $"{format.VideoFormat.Width}x{format.VideoFormat.Height}@{GetFrameRate(format):0.#} {format.Subtype}";

    private CaptureProfileCap CurrentCaptureProfileCap()
    {
        int maxWidth = Volatile.Read(ref captureProfileMaxWidth);
        int maxHeight = Volatile.Read(ref captureProfileMaxHeight);
        return maxWidth > 0 && maxHeight > 0
            ? new CaptureProfileCap(maxWidth, maxHeight, maxWidth / (double)maxHeight)
            : default;
    }

    private bool CaptureProfileMeetsActiveCap(MediaFrameFormat format, CaptureProfileCap cap)
        => format.VideoFormat.Width <= cap.MaxWidth &&
           format.VideoFormat.Height <= cap.MaxHeight &&
           GetFrameRate(format) + 0.5 >= targetFps;

    private static int AspectBucket(MediaFrameFormat format, double targetAspect)
        => AspectDistance(format, targetAspect) <= 0.08 ? 0 : 1;

    private static double AspectDistance(MediaFrameFormat format, double targetAspect)
        => format.VideoFormat.Height == 0
            ? double.MaxValue
            : Math.Abs(format.VideoFormat.Width / (double)format.VideoFormat.Height - targetAspect);

    private bool FormatMeetsExplicitCaptureProfile(MediaFrameFormat format)
        => CaptureProfileMeetsExplicitPreferences(
            checked((int)format.VideoFormat.Width),
            checked((int)format.VideoFormat.Height),
            GetFrameRate(format),
            out _);

    private bool FormatMeetsNegotiatedSelection(MediaFrameFormat format)
        => FormatMatchesSelectedPixelFormat(format.Subtype) &&
           CaptureProfileMeetsNegotiatedSelection(
               checked((int)format.VideoFormat.Width),
               checked((int)format.VideoFormat.Height),
               GetFrameRate(format),
               out _);

    private bool CaptureProfileMeetsActiveSelection(
        int width,
        int height,
        double fps,
        out string failure)
    {
        CaptureProfileCap cap = CurrentCaptureProfileCap();
        if (!cap.IsActive)
        {
            return CaptureProfileMeetsNegotiatedSelection(width, height, fps, out failure);
        }

        failure = string.Empty;
        if (width > cap.MaxWidth || height > cap.MaxHeight)
        {
            failure = $"does not satisfy fallback cap <= {cap.MaxWidth}x{cap.MaxHeight}; got {width}x{height}.";
            return false;
        }

        if (fps + 0.5 < targetFps)
        {
            failure = $"does not satisfy target frame rate {targetFps} fps; got {fps:0.#} fps.";
            return false;
        }

        return true;
    }

    /// Годится ли формат источника под выбранный пиксельный формат.
    ///
    /// MJPG годится под NV12: читатель кадров просит у источника NV12, и Media Foundation сам
    /// вставляет распаковщик. Для всего, что ниже по потоку, кадр приходит уже как NV12 — поэтому
    /// проба и помечает такие режимы как NV12.
    private bool FormatMatchesSelectedPixelFormat(string subtype)
    {
        if (string.Equals(selectedCapturePixelFormat, CallVideoPreferences.CapturePixelFormatNv12, StringComparison.Ordinal))
        {
            return string.Equals(subtype, MediaEncodingSubtypes.Nv12, StringComparison.OrdinalIgnoreCase) ||
                   string.Equals(subtype, MediaEncodingSubtypes.Mjpg, StringComparison.OrdinalIgnoreCase);
        }

        return string.Equals(subtype, MediaEncodingSubtypes.Bgra8, StringComparison.OrdinalIgnoreCase);
    }

    private bool CaptureProfileMeetsNegotiatedSelection(
        int width,
        int height,
        double fps,
        out string failure)
    {
        failure = string.Empty;
        if (width < targetCaptureWidth || height < targetCaptureHeight)
        {
            failure = $"does not satisfy negotiated resolution {targetCaptureWidth}x{targetCaptureHeight}; got {width}x{height}.";
            return false;
        }

        if (fps + 0.5 < targetFps)
        {
            failure = $"does not satisfy negotiated frame rate {targetFps} fps; got {fps:0.#} fps.";
            return false;
        }

        return true;
    }

    private bool CaptureProfileMeetsExplicitPreferences(
        int width,
        int height,
        double fps,
        out string failure)
    {
        failure = string.Empty;
        bool requireFrameRate;
        lock (lifecycleSync)
        {
            requireFrameRate = frameRatePreference != CallVideoFrameRatePreference.Auto;
        }

        // Разрешение спрашивается ВСЕГДА: «Auto» больше нет, отправитель всегда выбрал конкретный
        // размер (по умолчанию 720p или максимум источника, если он меньше).
        if (width < targetCaptureWidth || height < targetCaptureHeight)
        {
            failure = $"does not satisfy requested resolution {targetCaptureWidth}x{targetCaptureHeight}; got {width}x{height}.";
            return false;
        }

        if (requireFrameRate && fps + 0.5 < targetFps)
        {
            failure = $"does not satisfy requested frame rate {targetFps} fps; got {fps:0.#} fps.";
            return false;
        }

        return true;
    }

    // Профиль захвата теперь ВСЕГДА явный: разрешение выбирает отправитель, и подобрать «что-то
    // похожее» вместо выбранного — значит молча отдать не то, что человек попросил.
    private bool RequiresExplicitCaptureProfile() => true;

    private string ResolveSelectedReaderSubtype(MediaFrameSource source)
    {
        if (string.Equals(selectedCapturePixelFormat, CallVideoPreferences.CapturePixelFormatNv12, StringComparison.Ordinal))
        {
            // NV12 просим и у MJPG-источника: распаковщик вставляет сам Media Foundation. Прежняя
            // проверка требовала, чтобы камера отдавала NV12 СОБСТВЕННОРУЧНО, и камера с одним лишь
            // MJPG отвергалась целиком — хотя читать её мы умеем.
            if (HasFormat(source, MediaEncodingSubtypes.Nv12) ||
                HasFormat(source, MediaEncodingSubtypes.Mjpg))
            {
                return MediaEncodingSubtypes.Nv12;
            }

            throw new InvalidOperationException("capture_unavailable: negotiated NV12 capture, but the selected camera source exposes neither NV12 nor MJPG.");
        }

        if (HasFormat(source, MediaEncodingSubtypes.Bgra8))
        {
            return MediaEncodingSubtypes.Bgra8;
        }

        throw new InvalidOperationException("capture_unavailable: negotiated BGRA capture, but the selected camera source does not expose BGRA.");
    }

    /// Форматы, из которых вообще выбираем. MJPG здесь наравне с сырыми: без него камера,
    /// отдающая высокие режимы только в MJPG, зажимала бы весь звонок своим NV12-максимумом
    /// (см. `CallVideoCaptureProbe.AddFormats`). «Сырой» остаётся предпочтительным, но это решает
    /// СОРТИРОВКА при равном разрешении, а не отбор кандидатов.
    private static bool IsRawVideoFormat(MediaFrameFormat format)
        => string.Equals(format.Subtype, MediaEncodingSubtypes.Bgra8, StringComparison.OrdinalIgnoreCase) ||
           string.Equals(format.Subtype, MediaEncodingSubtypes.Nv12, StringComparison.OrdinalIgnoreCase) ||
           string.Equals(format.Subtype, MediaEncodingSubtypes.Mjpg, StringComparison.OrdinalIgnoreCase) ||
           string.Equals(format.Subtype, MediaEncodingSubtypes.Yuy2, StringComparison.OrdinalIgnoreCase);

    private static bool HasFormat(MediaFrameSource source, string subtype)
        => source.SupportedFormats.Any(format => string.Equals(format.Subtype, subtype, StringComparison.OrdinalIgnoreCase));

    private int GetCaptureSubtypePriority(string subtype)
    {
        if (string.Equals(selectedCapturePixelFormat, CallVideoPreferences.CapturePixelFormatNv12, StringComparison.Ordinal) &&
            string.Equals(subtype, MediaEncodingSubtypes.Nv12, StringComparison.OrdinalIgnoreCase))
        {
            return 4;
        }

        if (string.Equals(selectedCapturePixelFormat, CallVideoPreferences.CapturePixelFormatBgra, StringComparison.Ordinal) &&
            string.Equals(subtype, MediaEncodingSubtypes.Bgra8, StringComparison.OrdinalIgnoreCase))
        {
            return 4;
        }

        if (string.Equals(subtype, MediaEncodingSubtypes.Nv12, StringComparison.OrdinalIgnoreCase))
        {
            return 3;
        }

        if (string.Equals(subtype, MediaEncodingSubtypes.Bgra8, StringComparison.OrdinalIgnoreCase))
        {
            return 2;
        }

        return string.Equals(subtype, MediaEncodingSubtypes.Yuy2, StringComparison.OrdinalIgnoreCase) ? 1 : 0;
    }

    private static double GetFrameRate(MediaFrameFormat format)
        => format.FrameRate.Denominator == 0
            ? 0
            : (double)format.FrameRate.Numerator / format.FrameRate.Denominator;

    private static string BuildCaptureDiagnostics(
        MediaFrameFormat format,
        string readerSubtype,
        MediaStreamType streamType,
        MediaCaptureSharingMode sharingMode)
        => $"{format.VideoFormat.Width}x{format.VideoFormat.Height}@{GetFrameRate(format):0.#} {format.Subtype}/{readerSubtype} source={streamType} sharing={sharingMode}";
}
