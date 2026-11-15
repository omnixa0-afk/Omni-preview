using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniMediaReadFileDelegate? omniMediaReadFile;
    private static OmniMediaOpenCopyDelegate? omniMediaOpenCopy;

    private static void LoadMediaFileExports()
    {
        LoadExport(ref omniMediaReadFile, nameof(omni_media_read_file));
        LoadExport(ref omniMediaOpenCopy, nameof(omni_media_open_copy));
        LoadExport(ref omniVoiceStage, nameof(omni_voice_stage));
        LoadExport(ref omniVoiceRead, nameof(omni_voice_read));
        LoadExport(ref omniVideoNoteStage, nameof(omni_video_note_stage));
    }

    private static OmniMediaReadFileDelegate? omniVideoNoteStage;

    internal static IntPtr omni_video_note_stage(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniVideoNoteStage!(handle, requestJson);
    }

    private static OmniMediaReadFileDelegate? omniVoiceStage;
    private static OmniMediaReadFileDelegate? omniVoiceRead;

    internal static IntPtr omni_voice_stage(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniVoiceStage!(handle, requestJson);
    }

    internal static IntPtr omni_voice_read(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniVoiceRead!(handle, requestJson);
    }

    internal static IntPtr omni_media_read_file(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniMediaReadFile!(handle, requestJson);
    }

    internal static IntPtr omni_media_open_copy(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniMediaOpenCopy!(handle, requestJson);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMediaReadFileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMediaOpenCopyDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);
}
