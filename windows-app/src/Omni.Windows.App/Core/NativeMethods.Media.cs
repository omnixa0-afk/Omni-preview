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
