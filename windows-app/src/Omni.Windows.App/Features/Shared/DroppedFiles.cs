using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Windows.ApplicationModel.DataTransfer;
using Windows.Graphics.Imaging;
using Windows.Storage;
using Windows.Storage.Streams;

namespace Omni.Windows.App.Features.Shared;

/// What a drag onto a conversation brings to attach: files from disk, or a picture dragged out of a
/// browser or another app (no file of its own — saved here as a PNG). One reader for the 1:1 chat, the
/// group and the chat list, so they accept the same things.
internal static class DroppedFiles
{
    /// Where a dragged picture is saved; the shell points it at its own temp folder, which it cleans.
    internal static string TempDirectory { get; set; } = Path.Combine(Path.GetTempPath(), "omni-dropped");

    internal static bool CanAttach(DataPackageView view) =>
        view.Contains(StandardDataFormats.StorageItems) || view.Contains(StandardDataFormats.Bitmap);

    /// The files on the drag, folders left out; a picture without a file becomes one PNG.
    internal static async Task<IReadOnlyList<StorageFile>> ReadAsync(DataPackageView view)
    {
        if (view.Contains(StandardDataFormats.StorageItems))
        {
            IReadOnlyList<IStorageItem> items = await view.GetStorageItemsAsync();
            List<StorageFile> files = items.OfType<StorageFile>().ToList();
            if (files.Count > 0)
            {
                return files;
            }
        }

        if (view.Contains(StandardDataFormats.Bitmap))
        {
            RandomAccessStreamReference reference = await view.GetBitmapAsync();
            using IRandomAccessStreamWithContentType source = await reference.OpenReadAsync();
            BitmapDecoder decoder = await BitmapDecoder.CreateAsync(source);
            SoftwareBitmap pixels = await decoder.GetSoftwareBitmapAsync(BitmapPixelFormat.Bgra8, BitmapAlphaMode.Premultiplied);
            Directory.CreateDirectory(TempDirectory);
            string path = Path.Combine(TempDirectory, $"picture-{DateTime.Now:yyyyMMdd-HHmmss}.png");
            using (FileStream file = new(path, FileMode.Create, FileAccess.ReadWrite))
            {
                BitmapEncoder encoder = await BitmapEncoder.CreateAsync(BitmapEncoder.PngEncoderId, file.AsRandomAccessStream());
                encoder.SetSoftwareBitmap(pixels);
                await encoder.FlushAsync();
            }
            return new[] { await StorageFile.GetFileFromPathAsync(path) };
        }

        return Array.Empty<StorageFile>();
    }

    /// What to tell the person when more than one file came: a message carries one file.
    internal static string? MoreThanOneNote(IReadOnlyList<StorageFile> files) =>
        files.Count > 1
            ? $"Прикреплён {files[0].Name}. В сообщении один файл — остальные {files.Count - 1} перетащите после отправки."
            : null;
}
