using Windows.Storage.Streams;

namespace Omni.Windows.App.Core;

/// <summary>
/// How every place that shows a picture gets its bytes. Files in Omni's own storage are encrypted at
/// rest (docs/decisions/db-at-rest-encryption.md, «Вложения»): a photo is decoded from the plaintext
/// the core hands over, never from its path, and an outside program gets a plain copy.
/// </summary>
internal static class MediaFiles
{
    private static RustBridge? bridge;

    internal static void Attach(RustBridge rustBridge) => bridge = rustBridge;

    /// <summary>The file's plaintext, or null when it cannot be read (gone, broken, too big).</summary>
    internal static byte[]? ReadAllBytes(string path)
    {
        if (bridge is { IsInitialized: true } core)
        {
            try
            {
                return core.ReadMediaFile(path);
            }
            catch (InvalidOperationException)
            {
                // The core said why (the bridge wrote the diagnostic): the picture draws as empty.
                return null;
            }
        }

        // Before the core is up only plain files exist to show (a picked picture, a draft).
        return File.Exists(path) ? File.ReadAllBytes(path) : null;
    }

    /// <summary>A path an outside program can open the file at.</summary>
    internal static string OpenablePath(string path, string fileName)
        => bridge is { IsInitialized: true } core ? core.OpenMediaFileCopy(path, fileName) : path;

    /// <summary>The bytes as a stream a <c>BitmapImage</c> or the clipboard takes.</summary>
    internal static async Task<InMemoryRandomAccessStream> ToStreamAsync(byte[] bytes)
    {
        InMemoryRandomAccessStream stream = new();
        using (DataWriter writer = new(stream))
        {
            writer.WriteBytes(bytes);
            await writer.StoreAsync();
            writer.DetachStream();
        }

        stream.Seek(0);
        return stream;
    }
}
