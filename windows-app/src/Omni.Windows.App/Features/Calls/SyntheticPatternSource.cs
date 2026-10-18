using System.Runtime.InteropServices.WindowsRuntime;
using Windows.Graphics.Imaging;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// A synthetic video source for UNATTENDED measurements: 1280x720 BGRA at 30 fps, deterministic
/// (fixed seed), handed to the screen-share frame callback so everything from the bitmap on —
/// downscale, adaptation, encoder, pacing, transport — is the product path unchanged.
///
/// Why it exists. Screen capture only delivers a frame when the picture changes, and a camera in a
/// dark room delivers one or two a second: on 2026-09-25 four WAN runs out of six carried no video at
/// all (laptop display off, `loop=0fps`), and the camera series that replaced them was so starved
/// (`presentGap` ~700 ms) that the rate controller sat at the application limit and the A/B compared
/// nothing. A measurement of how fast the rate controller climbs needs a source that always wants
/// more than the network gives — here part of every frame is fresh noise, which no encoder can
/// compress, so the only limiter left is the path.
///
/// Reached only through the UI test channel (`pattern.on`); not a user-facing source.
/// </summary>
internal sealed class SyntheticPatternSource : IDisposable
{
    internal const int Width = 1280;
    internal const int Height = 720;
    private const int FramesPerSecond = 30;
    // Rows of fresh noise per frame: 1/8 of the picture changes every frame, which alone asks for far
    // more than 4 Mbit/s at 720p — the encoder is never what holds the rate back.
    private const int FreshRowsPerFrame = Height / 8;

    private readonly byte[] pixels = new byte[Width * Height * 4];
    private readonly Random random = new(20260925);
    private CancellationTokenSource? cancellation;
    private Thread? thread;

    internal void Start(Action<SoftwareBitmap> frameCallback)
    {
        random.NextBytes(pixels);
        cancellation = new CancellationTokenSource();
        CancellationToken token = cancellation.Token;
        thread = new Thread(() => Run(frameCallback, token))
        {
            IsBackground = true,
            Name = "omni-synthetic-pattern",
        };
        thread.Start();
    }

    private void Run(Action<SoftwareBitmap> frameCallback, CancellationToken token)
    {
        long frameIndex = 0;
        long startedAt = Environment.TickCount64;
        while (!token.IsCancellationRequested)
        {
            // Fresh noise in a band that walks down the frame, plus a bright bar across it so a
            // human looking at the far end can see motion and freezes.
            int band = (int)(frameIndex % (Height / FreshRowsPerFrame)) * FreshRowsPerFrame;
            random.NextBytes(pixels.AsSpan(band * Width * 4, FreshRowsPerFrame * Width * 4));
            int bar = (int)(frameIndex * 8 % Width);
            for (int y = 0; y < Height; y++)
            {
                pixels.AsSpan((y * Width + bar) * 4, Math.Min(16, Width - bar) * 4).Fill(0xFF);
            }

            SoftwareBitmap bitmap = SoftwareBitmap.CreateCopyFromBuffer(
                pixels.AsBuffer(),
                BitmapPixelFormat.Bgra8,
                Width,
                Height,
                BitmapAlphaMode.Ignore);
            frameCallback(bitmap);

            frameIndex++;
            long due = startedAt + frameIndex * 1000 / FramesPerSecond;
            int wait = (int)(due - Environment.TickCount64);
            if (wait > 0 && token.WaitHandle.WaitOne(wait))
            {
                break;
            }
        }
    }

    public void Dispose()
    {
        cancellation?.Cancel();
        thread?.Join(TimeSpan.FromSeconds(2));
        cancellation?.Dispose();
        cancellation = null;
        thread = null;
    }
}
