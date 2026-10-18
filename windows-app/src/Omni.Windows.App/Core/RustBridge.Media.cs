using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    /// <summary>
    /// The plaintext of a media file the app shows. Omni's own storage is encrypted at rest
    /// (docs/decisions/db-at-rest-encryption.md, «Вложения»), so the core reads it; a file the user
    /// saved comes back as it is.
    /// </summary>
    public byte[] ReadMediaFile(string path)
    {
        MediaFileBytes bytes = CallJson(
            () => NativeMethods.omni_media_read_file(
                handle,
                JsonSerializer.Serialize(new MediaFileRequest(path, string.Empty), OmniJsonSerializerContext.Default.MediaFileRequest)),
            OmniJsonSerializerContext.Default.MediaFileBytes,
            "media_read_file");
        return Convert.FromBase64String(bytes.BytesB64);
    }

    /// <summary>
    /// A path an outside program can open the file at: its own path for a file the user saved, a plain
    /// copy (cleared at the next start) for one in Omni's storage.
    /// </summary>
    public string OpenMediaFileCopy(string path, string fileName)
        => CallJson(
            () => NativeMethods.omni_media_open_copy(
                handle,
                JsonSerializer.Serialize(new MediaFileRequest(path, fileName), OmniJsonSerializerContext.Default.MediaFileRequest)),
            OmniJsonSerializerContext.Default.MediaFileOpenCopy,
            "media_open_copy").Path;

    internal sealed record MediaFileRequest(
        [property: JsonPropertyName("path")] string Path,
        [property: JsonPropertyName("file_name")] string FileName);

    internal sealed record MediaFileBytes([property: JsonPropertyName("bytes_b64")] string BytesB64);

    internal sealed record MediaFileOpenCopy([property: JsonPropertyName("path")] string Path);
}
