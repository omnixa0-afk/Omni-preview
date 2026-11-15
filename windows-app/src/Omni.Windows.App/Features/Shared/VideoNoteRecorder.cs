using Microsoft.UI.Xaml.Controls;
using Windows.Graphics.Imaging;
using Windows.Media.Capture;
using Windows.Media.Capture.Frames;
using Windows.Media.Core;
using Windows.Media.Editing;
using Windows.Media.MediaProperties;
using Windows.Media.Playback;
using Windows.Storage;
using Windows.Storage.Streams;

namespace Omni.Windows.App.Features.Shared;

/// What a finished video message hands the host: the MP4 and its first frame's small JPEG.
public sealed record VideoNoteRecording(byte[] Video, byte[] Preview);

/// A round video message being recorded: camera and microphone into an MP4 in memory, the camera shown
/// live in the host's element. Nothing reaches the disk before the person sends. The file keeps the
/// camera's shape; the circle is cut when it is shown.
internal sealed class VideoNoteRecorder : IDisposable
{
    /// As in the core (`VIDEO_NOTE_MAX_MS`): at the limit the recording goes as it is.
    internal const int MaxMs = 60 * 1000;

    /// Fewer than this is a click, not a message.
    private const int MinMs = 1000;

    /// Sharp in a circle of 220 on a 200% screen, about 7 MB a minute.
    private const uint BestHeight = 480;
    private const uint VideoBitrateBps = 900_000;
    private const uint LowestFrameRate = 24;

    /// The largest the core takes in an offer, for a circle of 360 not to look blurred before it plays.
    private const int PreviewSide = 320;
    private const int PreviewMaxBytes = 16 * 1024;
    private static readonly double[] PreviewQualities = [0.6, 0.4, 0.25];

    /// Past the first dark frames a camera gives while it adjusts.
    private const int PreviewAfterMs = 500;

    /// The camera chosen in the settings.
    internal static Func<string?>? CameraId { get; set; }

    private readonly MediaCapture capture;
    private readonly MediaPlayerElement surface;
    private readonly MediaPlayer live;
    private readonly InMemoryRandomAccessStream memory = new();
    private readonly long startedAtMs = Environment.TickCount64;
    private MediaFrameReader? reader;
    private byte[] preview = [];
    private int previewTaken;
    private bool recording;

    private VideoNoteRecorder(MediaCapture capture, MediaPlayerElement surface, MediaPlayer live)
    {
        this.capture = capture;
        this.surface = surface;
        this.live = live;
    }

    internal int ElapsedMs => (int)Math.Min(Environment.TickCount64 - startedAtMs, int.MaxValue);

    internal long BytesSoFar => (long)memory.Size;

    internal bool HasPreview => preview.Length > 0;

    /// `null` with the reason in words when the camera or microphone cannot be had.
    internal static async Task<(VideoNoteRecorder? Recorder, string? Why)> TryStartAsync(MediaPlayerElement surface)
    {
        MediaCapture capture = new();
        MediaPlayer? live = null;
        try
        {
            MediaFrameSourceGroup? group = await FindCameraAsync();
            if (group is null)
            {
                capture.Dispose();
                return (null, "Камера не найдена.");
            }

            await capture.InitializeAsync(new MediaCaptureInitializationSettings
            {
                SourceGroup = group,
                StreamingCaptureMode = StreamingCaptureMode.AudioAndVideo,
                SharingMode = MediaCaptureSharingMode.ExclusiveControl,
                MemoryPreference = MediaCaptureMemoryPreference.Cpu,
                MediaCategory = MediaCategory.Communications,
            });

            VideoEncodingProperties size = await ChooseSizeAsync(capture);
            MediaFrameSource source = capture.FrameSources.Values
                .Where(s => s.Info.SourceKind == MediaFrameSourceKind.Color)
                .OrderByDescending(s => s.Info.MediaStreamType == MediaStreamType.VideoRecord)
                .First();
            live = new MediaPlayer { RealTimePlayback = true, IsMuted = true };
            live.Source = MediaSource.CreateFromMediaFrameSource(source);
            surface.SetMediaPlayer(live);
            live.Play();

            VideoNoteRecorder recorder = new(capture, surface, live);
            await recorder.StartPreviewFrameAsync(source);
            MediaEncodingProfile profile = MediaEncodingProfile.CreateMp4(VideoEncodingQuality.Vga);
            (profile.Video.Width, profile.Video.Height) = FitWithinBest(size.Width, size.Height);
            profile.Video.Bitrate = VideoBitrateBps;
            await capture.StartRecordToStreamAsync(profile, recorder.memory);
            recorder.recording = true;
            return (recorder, null);
        }
        catch (Exception ex) when (ex is UnauthorizedAccessException or System.Runtime.InteropServices.COMException or InvalidOperationException)
        {
            Core.AppTrace.Write("video-note", $"capture failed: {ex.GetType().Name} {ex.Message}");
            surface.SetMediaPlayer(null);
            live?.Dispose();
            capture.Dispose();
            return (null, "Камера или микрофон недоступны: проверьте, что они подключены, разрешены в Windows и не заняты.");
        }
    }

    private static async Task<MediaFrameSourceGroup?> FindCameraAsync()
    {
        IReadOnlyList<MediaFrameSourceGroup> groups = await MediaFrameSourceGroup.FindAllAsync();
        string? chosen = CameraId?.Invoke();
        return groups
            .Where(g => g.SourceInfos.Any(info => info.SourceKind == MediaFrameSourceKind.Color))
            .OrderByDescending(g => g.Id == chosen)
            .FirstOrDefault();
    }

    /// The smallest the camera gives at or above the height we want, at a smooth rate.
    private static async Task<VideoEncodingProperties> ChooseSizeAsync(MediaCapture capture)
    {
        List<VideoEncodingProperties> all = capture.VideoDeviceController
            .GetAvailableMediaStreamProperties(MediaStreamType.VideoRecord)
            .OfType<VideoEncodingProperties>()
            .ToList();
        VideoEncodingProperties chosen = all
            .OrderBy(p => p.FrameRate.Denominator == 0 || p.FrameRate.Numerator / p.FrameRate.Denominator < LowestFrameRate)
            .ThenBy(p => p.Height < BestHeight)
            .ThenBy(p => p.Height >= BestHeight ? p.Height : -p.Height)
            .First();
        await capture.VideoDeviceController.SetMediaStreamPropertiesAsync(MediaStreamType.VideoRecord, chosen);
        return chosen;
    }

    private static (uint Width, uint Height) FitWithinBest(uint width, uint height)
    {
        if (height <= BestHeight)
        {
            return (width, height);
        }

        // An encoder takes even sides only.
        uint scaled = (uint)((ulong)width * BestHeight / height) & ~1u;
        return (scaled, BestHeight);
    }

    private async Task StartPreviewFrameAsync(MediaFrameSource source)
    {
        reader = await capture.CreateFrameReaderAsync(source, MediaEncodingSubtypes.Bgra8);
        reader.AcquisitionMode = MediaFrameReaderAcquisitionMode.Realtime;
        reader.FrameArrived += OnFrameArrived;
        await reader.StartAsync();
    }

    private async void OnFrameArrived(MediaFrameReader sender, MediaFrameArrivedEventArgs args)
    {
        if (ElapsedMs < PreviewAfterMs || Interlocked.Exchange(ref previewTaken, 1) != 0)
        {
            return;
        }

        try
        {
            using MediaFrameReference? frame = sender.TryAcquireLatestFrame();
            if (frame?.VideoMediaFrame?.SoftwareBitmap is { } bitmap)
            {
                preview = await EncodePreviewAsync(bitmap);
            }
            else
            {
                previewTaken = 0;
            }
        }
        catch (Exception ex) when (ex is System.Runtime.InteropServices.COMException or ObjectDisposedException)
        {
            // No first frame: the message draws an empty circle until it plays.
            Core.AppTrace.Write("video-note", $"no first frame: {ex.Message}");
        }
    }

    /// A centred square of the frame, small: the circle a message shows before it is played.
    private static async Task<byte[]> EncodePreviewAsync(SoftwareBitmap frame)
    {
        using SoftwareBitmap bitmap = SoftwareBitmap.Convert(frame, BitmapPixelFormat.Bgra8, BitmapAlphaMode.Ignore);
        // A busy picture takes a rougher preview: over the core's limit there would be none at all.
        byte[] jpeg = [];
        foreach (double quality in PreviewQualities)
        {
            jpeg = await EncodePreviewAsync(bitmap, quality);
            if (jpeg.Length <= PreviewMaxBytes)
            {
                break;
            }
        }

        return jpeg;
    }

    private static async Task<byte[]> EncodePreviewAsync(SoftwareBitmap bitmap, double quality)
    {
        using InMemoryRandomAccessStream stream = new();
        BitmapPropertySet options = new() { ["ImageQuality"] = new BitmapTypedValue(quality, global::Windows.Foundation.PropertyType.Single) };
        BitmapEncoder encoder = await BitmapEncoder.CreateAsync(BitmapEncoder.JpegEncoderId, stream, options);
        encoder.SetSoftwareBitmap(bitmap);
        double scale = (double)PreviewSide / Math.Min(bitmap.PixelWidth, bitmap.PixelHeight);
        uint scaledWidth = (uint)Math.Round(bitmap.PixelWidth * scale);
        uint scaledHeight = (uint)Math.Round(bitmap.PixelHeight * scale);
        encoder.BitmapTransform.ScaledWidth = scaledWidth;
        encoder.BitmapTransform.ScaledHeight = scaledHeight;
        encoder.BitmapTransform.Bounds = new BitmapBounds
        {
            X = (scaledWidth - PreviewSide) / 2,
            Y = (scaledHeight - PreviewSide) / 2,
            Width = PreviewSide,
            Height = PreviewSide,
        };
        await encoder.FlushAsync();
        return await ReadAllAsync(stream);
    }

    private static async Task<byte[]> ReadAllAsync(IRandomAccessStream stream)
    {
        byte[] bytes = new byte[stream.Size];
        using DataReader reader = new(stream.GetInputStreamAt(0));
        await reader.LoadAsync((uint)bytes.Length);
        reader.ReadBytes(bytes);
        return bytes;
    }

    /// The recording, the camera closed. `null`: too short to be a message, or it failed.
    internal async Task<VideoNoteRecording?> FinishAsync()
    {
        int elapsed = ElapsedMs;
        try
        {
            if (recording)
            {
                recording = false;
                await capture.StopRecordAsync();
            }

            return elapsed < MinMs || memory.Size == 0 ? null : new VideoNoteRecording(await ReadAllAsync(memory), preview);
        }
        catch (Exception ex) when (ex is System.Runtime.InteropServices.COMException or InvalidOperationException)
        {
            Core.AppTrace.Write("video-note", $"finish failed: {ex.Message}");
            return null;
        }
        finally
        {
            Dispose();
        }
    }

    public void Dispose()
    {
        if (reader is { } running)
        {
            reader = null;
            running.FrameArrived -= OnFrameArrived;
            running.Dispose();
        }

        surface.SetMediaPlayer(null);
        live.Dispose();
        capture.Dispose();
        memory.Dispose();
    }

    /// Test channel: what a camera would have given, for machines with none or when nothing of a room should
    /// leave: coloured seconds with no sound.
    internal static async Task<VideoNoteRecording> ToneAsync(double seconds)
    {
        MediaComposition composition = new();
        TimeSpan part = TimeSpan.FromSeconds(seconds / 2);
        composition.Clips.Add(MediaClip.CreateFromColor(global::Windows.UI.Color.FromArgb(255, 200, 80, 40), part));
        composition.Clips.Add(MediaClip.CreateFromColor(global::Windows.UI.Color.FromArgb(255, 40, 120, 200), part));
        StorageFolder folder = await StorageFolder.GetFolderFromPathAsync(Path.GetTempPath());
        StorageFile file = await folder.CreateFileAsync($"omni-note-{Guid.NewGuid():N}.mp4", CreationCollisionOption.ReplaceExisting);
        try
        {
            MediaEncodingProfile profile = MediaEncodingProfile.CreateMp4(VideoEncodingQuality.Vga);
            profile.Video.Width = 640;
            profile.Video.Height = 360;
            await composition.RenderToFileAsync(file, MediaTrimmingPreference.Fast, profile);
            using global::Windows.Graphics.Imaging.ImageStream thumbnail =
                await composition.GetThumbnailAsync(TimeSpan.Zero, PreviewSide, PreviewSide, VideoFramePrecision.NearestFrame);
            BitmapDecoder decoder = await BitmapDecoder.CreateAsync(thumbnail);
            using SoftwareBitmap bitmap = await decoder.GetSoftwareBitmapAsync();
            return new VideoNoteRecording(await File.ReadAllBytesAsync(file.Path), await EncodePreviewAsync(bitmap));
        }
        finally
        {
            await file.DeleteAsync();
        }
    }
}
