using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

// Remote screen control (docs/decisions/remote-screen-control.md): grants, the viewer's input out,
// the sharer's verified input in through an injector callback.
internal static partial class NativeMethods
{
    private static OmniScreenControlSetSinkDelegate? omniScreenControlSetSink;
    private static OmniScreenControlJsonDelegate? omniScreenControlSend;
    private static OmniScreenControlJsonDelegate? omniCallsSetScreenControl;
    private static OmniScreenControlHandleOnlyDelegate? omniCallsGetScreenControlState;
    private static OmniScreenControlJsonDelegate? omniGroupsSetCallScreenControl;

    private static void LoadScreenControlExports()
    {
        LoadExport(ref omniScreenControlSetSink, nameof(omni_screen_control_set_sink));
        LoadExport(ref omniScreenControlSend, nameof(omni_screen_control_send));
        LoadExport(ref omniCallsSetScreenControl, nameof(omni_calls_set_screen_control));
        LoadExport(ref omniCallsGetScreenControlState, nameof(omni_calls_get_screen_control_state));
        LoadExport(ref omniGroupsSetCallScreenControl, nameof(omni_groups_set_call_screen_control));
    }

    /// `sink(context, json, len)` gets every input batch the core verified; zero removes it.
    /// Returns zero or an error envelope.
    internal static IntPtr omni_screen_control_set_sink(IntPtr handle, IntPtr sink, IntPtr context)
    {
        EnsureLoaded();
        return omniScreenControlSetSink!(handle, sink, context);
    }

    internal static IntPtr omni_screen_control_send(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniScreenControlSend!(handle, requestJson);
    }

    internal static IntPtr omni_calls_set_screen_control(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsSetScreenControl!(handle, requestJson);
    }

    internal static IntPtr omni_calls_get_screen_control_state(IntPtr handle)
    {
        EnsureLoaded();
        return omniCallsGetScreenControlState!(handle);
    }

    internal static IntPtr omni_groups_set_call_screen_control(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSetCallScreenControl!(handle, requestJson);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniScreenControlSetSinkDelegate(IntPtr handle, IntPtr sink, IntPtr context);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniScreenControlJsonDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniScreenControlHandleOnlyDelegate(IntPtr handle);
}
