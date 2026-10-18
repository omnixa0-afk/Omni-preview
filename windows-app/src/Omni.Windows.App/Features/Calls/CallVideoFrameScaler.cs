using System.Buffers;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// Bilinear scaling and color conversion for video frames. As of Wave 3
/// (2026-05-16) all heavy work is delegated to libyuv via
/// <see cref="NativeWebRtcMediaEngine"/> — the managed kernels were
/// removed (~225 MB/s of pointer arithmetic was leaving the C# hot path).
/// This file now only owns size selection and ArrayPool-backed buffer
/// wrappers; the pixel ops are SSE2/AVX2 paths inside libyuv.
/// </summary>
internal static class CallVideoFrameScaler
{
    internal static bool TrySelectScaledSize(
        int sourceWidth,
        int sourceHeight,
        int targetWidth,
        int targetHeight,
        out int scaledWidth,
        out int scaledHeight)
    {
        scaledWidth = 0;
        scaledHeight = 0;
        if (sourceWidth <= 0 ||
            sourceHeight <= 0 ||
            targetWidth <= 0 ||
            targetHeight <= 0)
        {
            return false;
        }

        int boundedTargetWidth = MakeEven(Math.Min(targetWidth, sourceWidth));
        int boundedTargetHeight = MakeEven(Math.Min(targetHeight, sourceHeight));
        if (boundedTargetWidth < 2 ||
            boundedTargetHeight < 2 ||
            (boundedTargetWidth >= sourceWidth && boundedTargetHeight >= sourceHeight))
        {
            return false;
        }

        double sourceAspect = sourceWidth / (double)sourceHeight;
        double targetAspect = boundedTargetWidth / (double)boundedTargetHeight;
        if (Math.Abs(sourceAspect - targetAspect) / sourceAspect <= 0.02)
        {
            scaledWidth = boundedTargetWidth;
            scaledHeight = boundedTargetHeight;
            return true;
        }

        double scale = Math.Min(
            boundedTargetWidth / (double)sourceWidth,
            boundedTargetHeight / (double)sourceHeight);
        if (scale >= 1.0)
        {
            return false;
        }

        scaledWidth = MakeEven((int)Math.Round(sourceWidth * scale));
        scaledHeight = MakeEven((int)Math.Round(sourceHeight * scale));
        return scaledWidth >= 2 && scaledHeight >= 2;
    }

    internal static BgraBuffer ScaleBgra(
        byte[] source,
        int sourceLength,
        int sourceWidth,
        int sourceHeight,
        int targetWidth,
        int targetHeight)
    {
        int requiredSourceLength = checked(sourceWidth * sourceHeight * 4);
        if (sourceLength < requiredSourceLength ||
            source.Length < requiredSourceLength)
        {
            throw new InvalidOperationException("BGRA frame is shorter than expected.");
        }

        int destinationLength = checked(targetWidth * targetHeight * 4);
        byte[] destination = ArrayPool<byte>.Shared.Rent(destinationLength);
        try
        {
            NativeWebRtcMediaEngine.ScaleBgra(
                source.AsSpan(0, requiredSourceLength),
                sourceWidth,
                sourceHeight,
                destination.AsSpan(0, destinationLength),
                targetWidth,
                targetHeight);
        }
        catch
        {
            ArrayPool<byte>.Shared.Return(destination);
            throw;
        }

        return new BgraBuffer(targetWidth, targetHeight, destination, destinationLength);
    }

    internal static BgraBuffer ConvertNv12ToBgra(
        byte[] nv12,
        int nv12Length,
        int width,
        int height)
    {
        int requiredLength = checked(width * height * 3 / 2);
        if (nv12Length < requiredLength ||
            nv12.Length < requiredLength)
        {
            throw new InvalidOperationException("NV12 frame is shorter than expected.");
        }

        int bgraLength = checked(width * height * 4);
        byte[] bgra = ArrayPool<byte>.Shared.Rent(bgraLength);
        try
        {
            NativeWebRtcMediaEngine.Nv12ToBgra(
                nv12.AsSpan(0, requiredLength),
                width,
                height,
                bgra.AsSpan(0, bgraLength));
        }
        catch
        {
            ArrayPool<byte>.Shared.Return(bgra);
            throw;
        }

        return new BgraBuffer(width, height, bgra, bgraLength);
    }

    internal static Nv12Buffer ScaleNv12(
        byte[] nv12,
        int nv12Length,
        int sourceWidth,
        int sourceHeight,
        int targetWidth,
        int targetHeight)
    {
        int requiredLength = checked(sourceWidth * sourceHeight * 3 / 2);
        if (nv12Length < requiredLength ||
            nv12.Length < requiredLength)
        {
            throw new InvalidOperationException("NV12 frame is shorter than expected.");
        }

        if ((targetWidth & 1) != 0 ||
            (targetHeight & 1) != 0)
        {
            throw new InvalidOperationException("NV12 target size must be even.");
        }

        int targetLength = checked(targetWidth * targetHeight * 3 / 2);
        byte[] scaled = ArrayPool<byte>.Shared.Rent(targetLength);
        try
        {
            NativeWebRtcMediaEngine.ScaleNv12(
                nv12.AsSpan(0, requiredLength),
                sourceWidth,
                sourceHeight,
                scaled.AsSpan(0, targetLength),
                targetWidth,
                targetHeight);
        }
        catch
        {
            ArrayPool<byte>.Shared.Return(scaled);
            throw;
        }

        return new Nv12Buffer(targetWidth, targetHeight, scaled, targetLength);
    }

    private static int MakeEven(int value)
        => value % 2 == 0 ? value : value - 1;

    internal sealed class BgraBuffer : IDisposable
    {
        private bool ownsBuffer = true;

        internal BgraBuffer(int width, int height, byte[] buffer, int length)
        {
            Width = width;
            Height = height;
            Buffer = buffer;
            Length = length;
        }

        internal int Width { get; }

        internal int Height { get; }

        internal byte[] Buffer { get; }

        internal int Length { get; }

        internal byte[] DetachBuffer()
        {
            ownsBuffer = false;
            return Buffer;
        }

        public void Dispose()
        {
            if (ownsBuffer)
            {
                ArrayPool<byte>.Shared.Return(Buffer);
            }
        }
    }

    internal sealed class Nv12Buffer : IDisposable
    {
        private bool ownsBuffer = true;

        internal Nv12Buffer(int width, int height, byte[] buffer, int length)
        {
            Width = width;
            Height = height;
            Buffer = buffer;
            Length = length;
        }

        internal int Width { get; }

        internal int Height { get; }

        internal byte[] Buffer { get; }

        internal int Length { get; }

        internal byte[] DetachBuffer()
        {
            ownsBuffer = false;
            return Buffer;
        }

        public void Dispose()
        {
            if (ownsBuffer)
            {
                ArrayPool<byte>.Shared.Return(Buffer);
            }
        }
    }
}
