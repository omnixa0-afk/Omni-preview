using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniCallsDeclineCallDelegate? omniCallsDeclineCall;
    private static OmniCallsHangUpCallDelegate? omniCallsHangUpCall;
    private static OmniCallsPlaceCallDelegate? omniCallsPlaceCall;
    private static OmniCallsPlaceCallDelegate? omniCallsRequestRemoteControl;
    private static OmniCallsAcceptCallDelegate? omniCallsAcceptCall;
    private static OmniCallsSetLocalMicMutedDelegate? omniCallsSetLocalMicMuted;
    private static OmniCallsGetInboxSnapshotDelegate? omniCallsGetInboxSnapshot;
    private static OmniCallsPublishRealtimeMediaCapabilitiesDelegate? omniCallsPublishRealtimeMediaCapabilities;
    private static OmniCallsGetRealtimeMediaNegotiationDelegate? omniCallsGetRealtimeMediaNegotiation;
    private static OmniCallsClearRealtimeMediaNegotiationDelegate? omniCallsClearRealtimeMediaNegotiation;
    private static OmniCallsWaitChangeDelegate? omniCallsWaitChange;

    private static void LoadCallExports()
    {
        LoadExport(ref omniCallsDeclineCall, nameof(omni_calls_decline_call));
        LoadExport(ref omniCallsHangUpCall, nameof(omni_calls_hang_up_call));
        LoadExport(ref omniCallsPlaceCall, nameof(omni_calls_place_call));
        LoadExport(ref omniCallsRequestRemoteControl, nameof(omni_calls_request_remote_control));
        LoadExport(ref omniCallsAcceptCall, nameof(omni_calls_accept_call));
        LoadExport(ref omniCallsSetLocalMicMuted, nameof(omni_calls_set_local_mic_muted));
        LoadExport(ref omniCallsGetInboxSnapshot, nameof(omni_calls_get_inbox_snapshot));
        LoadExport(ref omniCallsPublishRealtimeMediaCapabilities, nameof(omni_calls_publish_realtime_media_capabilities));
        LoadExport(ref omniCallsGetRealtimeMediaNegotiation, nameof(omni_calls_get_realtime_media_negotiation));
        LoadExport(ref omniCallsClearRealtimeMediaNegotiation, nameof(omni_calls_clear_realtime_media_negotiation));
        LoadExport(ref omniCallsWaitChange, nameof(omni_calls_wait_change));
    }

    /// Blocks up to <paramref name="timeoutMs"/> until a call changed after <paramref name="since"/>;
    /// returns the current revision. Call from a dedicated thread, never the UI thread.
    internal static ulong omni_calls_wait_change(ulong since, uint timeoutMs)
    {
        EnsureLoaded();
        return omniCallsWaitChange!(since, timeoutMs);
    }

    internal static IntPtr omni_calls_decline_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsDeclineCall!(handle, requestJson);
    }

    internal static IntPtr omni_calls_hang_up_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsHangUpCall!(handle, requestJson);
    }

    internal static IntPtr omni_calls_place_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsPlaceCall!(handle, requestJson);
    }

    internal static IntPtr omni_calls_request_remote_control(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsRequestRemoteControl!(handle, requestJson);
    }

    internal static IntPtr omni_calls_accept_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsAcceptCall!(handle, requestJson);
    }

    internal static IntPtr omni_calls_set_local_mic_muted(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsSetLocalMicMuted!(handle, requestJson);
    }

    internal static IntPtr omni_calls_get_inbox_snapshot(IntPtr handle)
    {
        EnsureLoaded();
        return omniCallsGetInboxSnapshot!(handle);
    }

    internal static IntPtr omni_calls_publish_realtime_media_capabilities(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsPublishRealtimeMediaCapabilities!(handle, requestJson);
    }

    internal static IntPtr omni_calls_get_realtime_media_negotiation(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsGetRealtimeMediaNegotiation!(handle, requestJson);
    }

    internal static IntPtr omni_calls_clear_realtime_media_negotiation(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniCallsClearRealtimeMediaNegotiation!(handle, requestJson);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsDeclineCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsHangUpCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsPlaceCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsAcceptCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsSetLocalMicMutedDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsGetInboxSnapshotDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate ulong OmniCallsWaitChangeDelegate(ulong since, uint timeoutMs);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsPublishRealtimeMediaCapabilitiesDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsGetRealtimeMediaNegotiationDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniCallsClearRealtimeMediaNegotiationDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

}
