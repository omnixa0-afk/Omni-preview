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

// Frame-buffer value types and pixel-format (NV12/BGRA) copy/convert helpers for the call
// video pipeline. Split out of CallVideoService.cs (which exceeded 5k lines) as a partial —
// pure leaf helpers + IDisposable frame wrappers; no behaviour change, same class.
internal sealed partial class CallVideoService
{
    private static Task<CapturedVideoFrame> CopyCapturedFrameAsync(
        VideoMediaFrame source,
        bool includePreview)
    {
        SoftwareBitmap? sourceBitmap = source.SoftwareBitmap;
        if (sourceBitmap is null)
        {
            throw new InvalidOperationException("Camera frame does not expose a software bitmap.");
        }

        if (sourceBitmap.BitmapPixelFormat == BitmapPixelFormat.Nv12)
        {
            return Task.FromResult(CopyNv12Frame(sourceBitmap, includePreview));
        }

        if (sourceBitmap.BitmapPixelFormat == BitmapPixelFormat.Bgra8 &&
            sourceBitmap.BitmapAlphaMode != BitmapAlphaMode.Premultiplied)
        {
            return Task.FromResult(CapturedVideoFrame.FromBgra(CopyBgraFrame(sourceBitmap), includePreview));
        }

        return Task.FromResult(CapturedVideoFrame.FromBgra(CopyBgraFrame(sourceBitmap), includePreview));
    }

    // Screen-share frames arrive as BGRA8 SoftwareBitmaps (Windows.Graphics.Capture). Reuse the same
    // BGRA copy path camera BGRA frames take, so the encode/send pipeline is source-agnostic.
    private static CapturedVideoFrame CopyCapturedFrameFromBitmap(SoftwareBitmap source, bool includePreview)
        => CapturedVideoFrame.FromBgra(CopyBgraFrame(source), includePreview);

    private static CapturedVideoFrame CopyNv12Frame(SoftwareBitmap source, bool includePreview)
    {
        Nv12FrameBuffer nv12 = CopyNv12SoftwareBitmap(source);
        bool ownershipTransferred = false;
        try
        {
            BgraFrame? previewFrame = includePreview
                ? Nv12ToBgra(nv12.Buffer, nv12.Width, nv12.Height)
                : null;
            ownershipTransferred = true;
            return new CapturedVideoFrame(
                nv12.Width,
                nv12.Height,
                nv12.Buffer,
                nv12.Length,
                CallVideoPixelFormat.NV12,
                previewFrame);
        }
        finally
        {
            if (!ownershipTransferred)
            {
                ArrayPool<byte>.Shared.Return(nv12.Buffer);
            }
        }
    }

    private static unsafe Nv12FrameBuffer CopyNv12SoftwareBitmap(SoftwareBitmap source)
    {
        using BitmapBuffer buffer = source.LockBuffer(BitmapBufferAccessMode.Read);
        using IMemoryBufferReference reference = buffer.CreateReference();
        reference.As<IMemoryBufferByteAccess>().GetBuffer(out byte* sourceBytes, out uint sourceCapacity);

        BitmapPlaneDescription yPlane = buffer.GetPlaneDescription(0);
        BitmapPlaneDescription uvPlane = buffer.GetPlaneDescription(1);
        if (TryCopyPlaneDescribedNv12(
            sourceBytes,
            sourceCapacity,
            yPlane,
            uvPlane,
            out Nv12FrameBuffer frame))
        {
            return frame;
        }

        if (TryCopyCompactNv12(
            sourceBytes,
            sourceCapacity,
            yPlane,
            out frame))
        {
            return frame;
        }

        throw new InvalidOperationException(
            $"NV12 camera buffer layout is not readable: y={FormatPlaneForDiagnostics(yPlane)}, uv={FormatPlaneForDiagnostics(uvPlane)}, capacity={sourceCapacity}.");
    }

    private static unsafe bool TryCopyPlaneDescribedNv12(
        byte* sourceBytes,
        uint sourceCapacity,
        BitmapPlaneDescription yPlane,
        BitmapPlaneDescription uvPlane,
        out Nv12FrameBuffer frame)
    {
        frame = default;
        int width = yPlane.Width;
        int height = yPlane.Height;
        if (width <= 0 ||
            height <= 0 ||
            (width & 1) != 0 ||
            (height & 1) != 0 ||
            uvPlane.Width * 2 < width ||
            uvPlane.Height * 2 < height ||
            yPlane.Stride < width ||
            uvPlane.Stride < width ||
            CalculateReadableRows(sourceCapacity, yPlane, width) < height ||
            CalculateReadableRows(sourceCapacity, uvPlane, width) < height / 2)
        {
            return false;
        }

        int encodeLength = checked(width * height * 3 / 2);
        byte[] destination = ArrayPool<byte>.Shared.Rent(encodeLength);
        bool copied = false;
        try
        {
            CopyBitmapPlaneRows(
                sourceBytes,
                sourceCapacity,
                yPlane.StartIndex,
                yPlane.Stride,
                width,
                height,
                destination,
                0,
                width,
                validateSourceCapacity: true);
            CopyBitmapPlaneRows(
                sourceBytes,
                sourceCapacity,
                uvPlane.StartIndex,
                uvPlane.Stride,
                width,
                height / 2,
                destination,
                width * height,
                width,
                validateSourceCapacity: true);
            copied = true;
            frame = new Nv12FrameBuffer(width, height, destination, encodeLength);
            return true;
        }
        finally
        {
            if (!copied)
            {
                ArrayPool<byte>.Shared.Return(destination);
            }
        }
    }

    private static unsafe bool TryCopyCompactNv12(
        byte* sourceBytes,
        uint sourceCapacity,
        BitmapPlaneDescription yPlane,
        out Nv12FrameBuffer frame)
    {
        frame = default;
        if (sourceCapacity == 0 ||
            sourceCapacity > int.MaxValue ||
            yPlane.StartIndex != 0 ||
            yPlane.Stride <= 0 ||
            (yPlane.Stride & 1) != 0)
        {
            return false;
        }

        int width = yPlane.Stride;
        ulong numerator = (ulong)sourceCapacity * 2;
        ulong denominator = (ulong)width * 3;
        if (denominator == 0 ||
            numerator % denominator != 0)
        {
            return false;
        }

        int height = checked((int)(numerator / denominator));
        if (height <= 0 ||
            (height & 1) != 0 ||
            (yPlane.Height > 0 && height > yPlane.Height))
        {
            return false;
        }

        int encodeLength = checked(width * height * 3 / 2);
        if (encodeLength != sourceCapacity)
        {
            return false;
        }

        byte[] destination = ArrayPool<byte>.Shared.Rent(encodeLength);
        Marshal.Copy((IntPtr)sourceBytes, destination, 0, encodeLength);
        frame = new Nv12FrameBuffer(width, height, destination, encodeLength);
        return true;
    }

    private static int CalculateReadableRows(
        uint sourceCapacity,
        BitmapPlaneDescription plane,
        int rowBytes)
    {
        if (plane.StartIndex < 0 ||
            plane.Stride <= 0 ||
            rowBytes <= 0 ||
            rowBytes > plane.Stride ||
            plane.StartIndex + rowBytes > sourceCapacity)
        {
            return 0;
        }

        return checked((int)((sourceCapacity - (uint)plane.StartIndex - (uint)rowBytes) / (uint)plane.Stride) + 1);
    }

    private static string FormatPlaneForDiagnostics(BitmapPlaneDescription plane)
        => $"start={plane.StartIndex}, stride={plane.Stride}, width={plane.Width}, height={plane.Height}";

    private static BgraFrame CopyBgraFrame(SoftwareBitmap source)
    {
        SoftwareBitmap? converted = null;
        SoftwareBitmap bitmap = source;
        if (source.BitmapPixelFormat != BitmapPixelFormat.Bgra8)
        {
            converted = SoftwareBitmap.Convert(source, BitmapPixelFormat.Bgra8, BitmapAlphaMode.Ignore);
            bitmap = converted;
        }

        try
        {
            int width = bitmap.PixelWidth;
            int height = bitmap.PixelHeight;
            int rowBytes = checked(width * 4);
            byte[] rented = ArrayPool<byte>.Shared.Rent(checked(rowBytes * height));
            CopyBgraSoftwareBitmap(bitmap, rented, width, height);
            return new BgraFrame(width, height, rented, rowBytes * height);
        }
        finally
        {
            converted?.Dispose();
        }
    }

    private static unsafe void CopyBgraSoftwareBitmap(
        SoftwareBitmap source,
        byte[] destination,
        int width,
        int height)
    {
        string step = "lock";
        try
        {
            using BitmapBuffer buffer = source.LockBuffer(BitmapBufferAccessMode.Read);
            step = "create_reference";
            using IMemoryBufferReference reference = buffer.CreateReference();
            step = "get_buffer";
            reference.As<IMemoryBufferByteAccess>().GetBuffer(out byte* sourceBytes, out uint sourceCapacity);
            step = "copy_plane";
            BitmapPlaneDescription plane = buffer.GetPlaneDescription(0);
            CopyBitmapPlaneRows(
                sourceBytes,
                sourceCapacity,
                plane.StartIndex,
                plane.Stride,
                checked(width * 4),
                height,
                destination,
                0,
                checked(width * 4),
                validateSourceCapacity: true);
        }
        catch (Exception ex)
        {
            throw new InvalidOperationException($"BGRA bitmap copy failed at {step}: {ex.GetType().Name}: {ex.Message}", ex);
        }
    }

    private static unsafe void CopyBitmapPlaneRows(
        byte* source,
        uint sourceCapacity,
        int sourceStartIndex,
        int sourceStride,
        int rowBytes,
        int rows,
        byte[] destination,
        int destinationOffset,
        int destinationStride,
        bool validateSourceCapacity)
    {
        if (sourceStride <= 0)
        {
            throw new InvalidOperationException("Camera bitmap plane has an invalid stride.");
        }

        for (int row = 0; row < rows; row++)
        {
            int sourceOffset = checked(sourceStartIndex + row * sourceStride);
            int targetOffset = checked(destinationOffset + row * destinationStride);
            if (sourceOffset < 0 ||
                targetOffset < 0 ||
                (validateSourceCapacity && sourceOffset + rowBytes > sourceCapacity) ||
                targetOffset + rowBytes > destination.Length)
            {
                throw new InvalidOperationException(
                    $"Camera bitmap plane is outside the reported memory buffer: row={row}, sourceStart={sourceStartIndex}, sourceStride={sourceStride}, rowBytes={rowBytes}, rows={rows}, sourceOffset={sourceOffset}, sourceCapacity={sourceCapacity}, targetOffset={targetOffset}, destinationLength={destination.Length}.");
            }

            Marshal.Copy((IntPtr)(source + sourceOffset), destination, targetOffset, rowBytes);
        }
    }

    private static BgraFrame Nv12ToBgra(byte[] nv12, int width, int height)
    {
        using CallVideoFrameScaler.BgraBuffer bgra = CallVideoFrameScaler.ConvertNv12ToBgra(
            nv12,
            checked(width * height * 3 / 2),
            width,
            height);
        return new BgraFrame(width, height, bgra.DetachBuffer(), bgra.Length);
    }

    // Self-view path (task #86): the PiP is tiny (~190 px), so converting + blitting the full
    // capture resolution (e.g. 720p) is ~11x wasted work on the UI thread — that was the choppy
    // preview. Downscale the NV12 to the PiP's on-screen size (longest side <= LocalPreviewMax-
    // Dimension, even, aspect-preserving) with libyuv BEFORE the color convert + WriteableBitmap
    // blit. libyuv NV12 scale on a 720p frame is far cheaper than converting+blitting it whole.
    private static BgraFrame Nv12ToPreviewBgra(byte[] nv12, int width, int height)
    {
        int longest = Math.Max(width, height);
        if (longest <= LocalPreviewMaxDimension)
        {
            return Nv12ToBgra(nv12, width, height);
        }

        double scale = LocalPreviewMaxDimension / (double)longest;
        int targetWidth = MakeEvenAtLeastTwo((int)Math.Round(width * scale));
        int targetHeight = MakeEvenAtLeastTwo((int)Math.Round(height * scale));

        using CallVideoFrameScaler.Nv12Buffer scaled = CallVideoFrameScaler.ScaleNv12(
            nv12,
            checked(width * height * 3 / 2),
            width,
            height,
            targetWidth,
            targetHeight);
        using CallVideoFrameScaler.BgraBuffer bgra = CallVideoFrameScaler.ConvertNv12ToBgra(
            scaled.Buffer,
            scaled.Length,
            scaled.Width,
            scaled.Height);
        return new BgraFrame(scaled.Width, scaled.Height, bgra.DetachBuffer(), bgra.Length);
    }

    private static int MakeEvenAtLeastTwo(int value)
    {
        if (value < 2)
        {
            return 2;
        }

        return value % 2 == 0 ? value : value - 1;
    }

    private readonly record struct Nv12FrameBuffer(int Width, int Height, byte[] Buffer, int Length);

    // Приводит кадр к разрешению, которое выбрал ABR, если оно меньше исходного.
    //
    // Нужно ТОЛЬКО пути демонстрации экрана: у камеры решение ABR исполняется перезапуском захвата
    // под ограничением профиля (`MaybeScheduleCaptureReconfigure`), то есть кадры уже
    // приходят нужного размера и здесь `TrySelectScaledSize` вернёт false. У экрана перезапускать
    // нечего, и без этого шага решение ABR о разрешении просто терялось.
    //
    // Пропорции сохраняются (`TrySelectScaledSize` вписывает в рамку, а не растягивает), стороны
    // остаются чётными. При любом сбое возвращается исходный кадр: ухудшить картинку допустимо,
    // уронить демонстрацию — нет.
    private CapturedVideoFrame DownscaleToAdaptiveTarget(CapturedVideoFrame frame)
    {
        if (frame.EncodePixelFormat != CallVideoPixelFormat.Bgra)
        {
            return frame;
        }

        int targetWidth = Volatile.Read(ref targetCaptureWidth);
        int targetHeight = Volatile.Read(ref targetCaptureHeight);
        if (!CallVideoFrameScaler.TrySelectScaledSize(
                frame.Width,
                frame.Height,
                targetWidth,
                targetHeight,
                out int scaledWidth,
                out int scaledHeight))
        {
            return frame;
        }

        try
        {
            using CallVideoFrameScaler.BgraBuffer scaled = CallVideoFrameScaler.ScaleBgra(
                frame.EncodeBuffer,
                frame.EncodeLength,
                frame.Width,
                frame.Height,
                scaledWidth,
                scaledHeight);
            CapturedVideoFrame downscaled = CapturedVideoFrame.FromBgra(
                new BgraFrame(scaledWidth, scaledHeight, scaled.DetachBuffer(), scaled.Length),
                includePreview: false);
            frame.Dispose();
            return downscaled;
        }
        catch (Exception ex)
        {
            RecordError(FormatExceptionForDiagnostics(ex));
            return frame;
        }
    }

    private sealed class CapturedVideoFrame : IDisposable
    {
        private BgraFrame? previewFrame;
        private bool ownsEncodeBuffer = true;
        private bool disposed;

        internal CapturedVideoFrame(
            int width,
            int height,
            byte[] encodeBuffer,
            int encodeLength,
            CallVideoPixelFormat encodePixelFormat,
            BgraFrame? previewFrame)
        {
            Width = width;
            Height = height;
            EncodeBuffer = encodeBuffer;
            EncodeLength = encodeLength;
            EncodePixelFormat = encodePixelFormat;
            this.previewFrame = previewFrame;
        }

        internal int Width { get; }

        internal int Height { get; }

        internal byte[] EncodeBuffer { get; }

        internal int EncodeLength { get; }

        internal CallVideoPixelFormat EncodePixelFormat { get; }

        internal BgraFrame? PreviewFrame => previewFrame;

        internal LocalPreviewFrame? DetachForLocalPreview()
        {
            if (disposed || !ownsEncodeBuffer)
            {
                return null;
            }

            previewFrame = null;
            ownsEncodeBuffer = false;
            return new LocalPreviewFrame(Width, Height, EncodeBuffer, EncodeLength, EncodePixelFormat);
        }

        internal static CapturedVideoFrame FromBgra(BgraFrame frame, bool includePreview)
            => new(
                frame.Width,
                frame.Height,
                frame.Buffer,
                frame.Length,
                CallVideoPixelFormat.Bgra,
                includePreview ? frame : null);

        public void Dispose()
        {
            if (disposed)
            {
                return;
            }

            disposed = true;
            BgraFrame? preview = previewFrame;
            previewFrame = null;
            if (preview is not null &&
                ReferenceEquals(EncodeBuffer, preview.Buffer))
            {
                preview.Dispose();
                return;
            }

            if (ownsEncodeBuffer)
            {
                ArrayPool<byte>.Shared.Return(EncodeBuffer);
            }

            preview?.Dispose();
        }
    }

    private sealed class LocalPreviewFrame : IDisposable
    {
        private byte[]? buffer;

        internal LocalPreviewFrame(
            int width,
            int height,
            byte[] buffer,
            int length,
            CallVideoPixelFormat pixelFormat)
        {
            Width = width;
            Height = height;
            this.buffer = buffer;
            Length = length;
            PixelFormat = pixelFormat;
        }

        private int Width { get; }

        private int Height { get; }

        private int Length { get; }

        private CallVideoPixelFormat PixelFormat { get; }

        internal BgraFrame ToBgraFrame()
        {
            byte[] source = buffer ?? throw new ObjectDisposedException(nameof(LocalPreviewFrame));
            if (PixelFormat == CallVideoPixelFormat.Bgra)
            {
                buffer = null;
                return new BgraFrame(Width, Height, source, Length);
            }

            if (PixelFormat == CallVideoPixelFormat.NV12)
            {
                return Nv12ToPreviewBgra(source, Width, Height);
            }

            throw new InvalidOperationException($"Local preview does not support {PixelFormat} frames.");
        }

        public void Dispose()
        {
            byte[]? owned = buffer;
            buffer = null;
            if (owned is not null)
            {
                ArrayPool<byte>.Shared.Return(owned);
            }
        }
    }

    private sealed class BgraFrame : IDisposable
    {
        private readonly bool returnToPool;

        internal BgraFrame(int width, int height, byte[] buffer, int length, bool returnToPool = true)
        {
            Width = width;
            Height = height;
            Buffer = buffer;
            Length = length;
            this.returnToPool = returnToPool;
        }

        internal int Width { get; }

        internal int Height { get; }

        internal byte[] Buffer { get; }

        internal int Length { get; }

        public void Dispose()
        {
            if (returnToPool)
            {
                ArrayPool<byte>.Shared.Return(Buffer);
            }
        }
    }
}
