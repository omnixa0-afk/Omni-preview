using Windows.Media.Capture;
using Windows.Media.Capture.Frames;
using Windows.Media.MediaProperties;

namespace Omni.Windows.App.Features.Calls;

internal sealed record CallVideoCaptureFormat(
    uint Width,
    uint Height,
    uint Fps,
    string PixelFormat,
    string Source);

internal sealed record CallVideoCaptureProbeResult(
    IReadOnlyList<CallVideoCaptureFormat> Formats,
    string Diagnostics)
{
    internal static CallVideoCaptureProbeResult Unavailable(string reason)
        => new([], $"camera_probe=unavailable:{reason}");

    internal string BuildShortDiagnostics()
    {
        if (Formats.Count == 0)
        {
            return Diagnostics;
        }

        string profiles = string.Join(
            ',',
            Formats
                .Select(format => $"{format.Width}x{format.Height}@{format.Fps}/{format.PixelFormat}")
                .Distinct(StringComparer.OrdinalIgnoreCase)
                .Take(8));
        return $"{Diagnostics}; profiles={profiles}";
    }
}

internal static class CallVideoCaptureProbe
{
    internal static async Task<CallVideoCaptureProbeResult> ProbeAsync(CancellationToken token)
    {
        try
        {
            IReadOnlyList<MediaFrameSourceGroup> groups = await MediaFrameSourceGroup.FindAllAsync().AsTask(token);
            if (groups.Count == 0)
            {
                return CallVideoCaptureProbeResult.Unavailable("camera_not_found");
            }

            List<CallVideoCaptureFormat> formats = [];
            List<string> failures = [];
            foreach (MediaFrameSourceGroup group in groups)
            {
                token.ThrowIfCancellationRequested();
                MediaCapture? capture = null;
                try
                {
                    capture = new MediaCapture();
                    MediaCaptureInitializationSettings settings = new()
                    {
                        SourceGroup = group,
                        StreamingCaptureMode = StreamingCaptureMode.Video,
                        SharingMode = MediaCaptureSharingMode.SharedReadOnly,
                        MemoryPreference = MediaCaptureMemoryPreference.Cpu,
                        MediaCategory = MediaCategory.Communications,
                    };
                    await capture.InitializeAsync(settings).AsTask(token);
                    foreach (MediaFrameSource source in capture.FrameSources.Values.Where(IsColorVideoSource))
                    {
                        foreach (MediaFrameFormat format in source.SupportedFormats)
                        {
                            AddFormats(formats, format);
                        }
                    }
                }
                catch (Exception ex) when (ex is not OperationCanceledException)
                {
                    failures.Add($"{group.DisplayName}: {ex.GetType().Name}");
                }
                finally
                {
                    capture?.Dispose();
                }
            }

            return FromObservedFormats(
                formats,
                failures.Count == 0
                    ? $"camera_probe=ok groups={groups.Count}"
                    : $"camera_probe=partial groups={groups.Count} failures={string.Join(',', failures.Take(3))}");
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (Exception ex)
        {
            return CallVideoCaptureProbeResult.Unavailable($"{ex.GetType().Name}:{ex.Message}");
        }
    }

    internal static CallVideoCaptureProbeResult FromObservedFormats(
        IEnumerable<CallVideoCaptureFormat> formats,
        string diagnostics = "camera_probe=test")
    {
        CallVideoCaptureFormat[] normalized = formats
            .Where(format => format.Width > 0 &&
                             format.Height > 0 &&
                             format.Fps > 0 &&
                             !string.IsNullOrWhiteSpace(format.PixelFormat))
            .GroupBy(
                format => (format.Width, format.Height, format.Fps, PixelFormat: NormalizePixelFormat(format.PixelFormat)),
                format => format,
                (key, group) =>
                {
                    CallVideoCaptureFormat first = group.First();
                    return new CallVideoCaptureFormat(
                        key.Width,
                        key.Height,
                        key.Fps,
                        key.PixelFormat,
                        first.Source);
                })
            .OrderBy(format => format.Width * format.Height)
            .ThenBy(format => format.Fps)
            .ThenBy(format => format.PixelFormat, StringComparer.Ordinal)
            .ToArray();

        return normalized.Length == 0
            ? CallVideoCaptureProbeResult.Unavailable(diagnostics)
            : new CallVideoCaptureProbeResult(normalized, diagnostics);
    }

    private static bool IsColorVideoSource(MediaFrameSource source)
        => source.Info.SourceKind == MediaFrameSourceKind.Color &&
           source.Info.MediaStreamType is MediaStreamType.VideoRecord or MediaStreamType.VideoPreview;

    private static void AddFormats(List<CallVideoCaptureFormat> formats, MediaFrameFormat format)
    {
        uint fps = ResolveFps(format);
        if (fps == 0)
        {
            return;
        }

        uint width = format.VideoFormat.Width;
        uint height = format.VideoFormat.Height;
        if (width == 0 || height == 0)
        {
            return;
        }

        string subtype = format.Subtype;
        if (string.Equals(subtype, MediaEncodingSubtypes.Nv12, StringComparison.OrdinalIgnoreCase))
        {
            formats.Add(new CallVideoCaptureFormat(width, height, fps, CallVideoPreferences.CapturePixelFormatNv12, "WinRT/NV12"));
            return;
        }

        if (string.Equals(subtype, MediaEncodingSubtypes.Bgra8, StringComparison.OrdinalIgnoreCase))
        {
            formats.Add(new CallVideoCaptureFormat(width, height, fps, CallVideoPreferences.CapturePixelFormatBgra, "WinRT/BGRA"));
            return;
        }

        // MJPG принимается и отдаётся как NV12: конвертер вставляет сам Media Foundation, когда
        // `CreateFrameReaderAsync` просит NV12 у источника, стоящего в MJPG.
        //
        // Прежде здесь стояло «YUY2 / MJPG не поддерживаются — OpenCV fallback удалён в Wave 1
        // (2026-05-16); камеры, выдающие только эти форматы, отклоняются visibly». Незамеченное
        // следствие оказалось другим и куда шире: камера, отдающая NV12 только в низких режимах, а
        // 720p/1080p — в MJPG (то есть почти любая), молча зажималась своим NV12-максимумом.
        //
        // И это не «картинка чуть хуже»: список отсюда идёт в СОГЛАСОВАНИЕ профиля звонка
        // (`CaptureProfileMeetsNegotiatedSelection` требует формат не меньше согласованного), а
        // согласованный профиль становится потолком `negotiated_width_` / `negotiated_bitrate_bps_`
        // в ядре, под которым ходит лестница ABR. Замер 2026-08-15: камера упиралась в 960x540 на
        // гигабитном LAN. См. docs/decisions/video-quality-ceilings-and-twcc.md.
        if (string.Equals(subtype, MediaEncodingSubtypes.Mjpg, StringComparison.OrdinalIgnoreCase))
        {
            formats.Add(new CallVideoCaptureFormat(width, height, fps, CallVideoPreferences.CapturePixelFormatNv12, "WinRT/MJPG"));
        }
        // YUY2 по-прежнему не берём: сырой формат без распаковки, но вдвое тяжелее NV12 по шине,
        // а разрешения у него те же, что у NV12 — выигрыша нет.
    }

    private static uint ResolveFps(MediaFrameFormat format)
    {
        if (format.FrameRate.Denominator == 0)
        {
            return 0;
        }

        double fps = (double)format.FrameRate.Numerator / format.FrameRate.Denominator;
        return checked((uint)Math.Clamp((int)Math.Round(fps), 1, 240));
    }

    private static string NormalizePixelFormat(string pixelFormat)
        => string.Equals(pixelFormat, CallVideoPreferences.CapturePixelFormatNv12, StringComparison.OrdinalIgnoreCase)
            ? CallVideoPreferences.CapturePixelFormatNv12
            : CallVideoPreferences.CapturePixelFormatBgra;
}
