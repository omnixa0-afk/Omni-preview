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

    /// <summary>A recording into a file `SendAttachment`/`SendGroupAttachment` take as a voice message.</summary>
    public string StageVoiceRecording(byte[] packets, byte[] waveform)
        => CallJson(
            () => NativeMethods.omni_voice_stage(
                handle,
                JsonSerializer.Serialize(
                    new VoiceStageRequest(Convert.ToBase64String(packets), Convert.ToBase64String(waveform)),
                    OmniJsonSerializerContext.Default.VoiceStageRequest)),
            OmniJsonSerializerContext.Default.MediaFileOpenCopy,
            "voice_stage").Path;

    /// <summary>A recording into a file `SendAttachment`/`SendGroupAttachment` take as a video message; the core checks it.</summary>
    public string StageVideoNote(byte[] video, byte[] preview)
        => CallJson(
            () => NativeMethods.omni_video_note_stage(
                handle,
                JsonSerializer.Serialize(
                    new VideoNoteStageRequest(Convert.ToBase64String(video), Convert.ToBase64String(preview)),
                    OmniJsonSerializerContext.Default.VideoNoteStageRequest)),
            OmniJsonSerializerContext.Default.MediaFileOpenCopy,
            "video_note_stage").Path;

    /// <summary>A voice message's Opus packets, each with its length ahead; the core has checked the file.</summary>
    public (byte[] Packets, int DurationMs) ReadVoiceFile(string path)
    {
        VoicePackets voice = CallJson(
            () => NativeMethods.omni_voice_read(
                handle,
                JsonSerializer.Serialize(new MediaFileRequest(path, string.Empty), OmniJsonSerializerContext.Default.MediaFileRequest)),
            OmniJsonSerializerContext.Default.VoicePackets,
            "voice_read");
        return (Convert.FromBase64String(voice.PacketsB64), voice.DurationMs);
    }

    internal sealed record VideoNoteStageRequest(
        [property: JsonPropertyName("video_b64")] string VideoB64,
        [property: JsonPropertyName("preview_b64")] string PreviewB64);

    internal sealed record VoiceStageRequest(
        [property: JsonPropertyName("packets_b64")] string PacketsB64,
        [property: JsonPropertyName("waveform_b64")] string WaveformB64);

    internal sealed record VoicePackets(
        [property: JsonPropertyName("packets_b64")] string PacketsB64,
        [property: JsonPropertyName("duration_ms")] int DurationMs);

    internal sealed record MediaFileRequest(
        [property: JsonPropertyName("path")] string Path,
        [property: JsonPropertyName("file_name")] string FileName);

    internal sealed record MediaFileBytes([property: JsonPropertyName("bytes_b64")] string BytesB64);

    internal sealed record MediaFileOpenCopy([property: JsonPropertyName("path")] string Path);
}
