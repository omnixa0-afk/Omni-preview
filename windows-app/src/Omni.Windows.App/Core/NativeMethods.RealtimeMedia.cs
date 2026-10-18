using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniRealtimeMediaPacketTargetCreateDelegate? omniRealtimeMediaPacketTargetCreate;
    private static OmniRealtimeMediaPacketTargetDestroyDelegate? omniRealtimeMediaPacketTargetDestroy;
    private static OmniRealtimeMediaPacketTargetCountersDelegate? omniRealtimeMediaPacketTargetCounters;
    private static IntPtr omniRealtimeMediaPacketSink;
    private static OmniRealtimeMediaSetInboundSinkDelegate? omniRealtimeMediaSetInboundSink;

    private static void LoadRealtimeMediaExports()
    {
        LoadExport(ref omniRealtimeMediaPacketTargetCreate, nameof(omni_realtime_media_packet_target_create));
        LoadExport(ref omniRealtimeMediaPacketTargetDestroy, nameof(omni_realtime_media_packet_target_destroy));
        LoadExport(ref omniRealtimeMediaPacketTargetCounters, nameof(omni_realtime_media_packet_target_counters));
        omniRealtimeMediaPacketSink = NativeLibrary.GetExport(libraryHandle, "omni_realtime_media_packet_sink");
        LoadExport(ref omniRealtimeMediaSetInboundSink, nameof(omni_realtime_media_set_inbound_sink));
    }

    // Outgoing media (2026-09-26): the media engine hands every packet straight to the core through
    // omni_realtime_media_packet_sink with a per-call target ID as its context. The shell only
    // creates the target and wires the two together — no packet passes through it. See
    // docs/bugs/media-rode-the-shells-timer.md.
    internal static IntPtr omni_realtime_media_packet_target_create(
        IntPtr handle,
        IntPtr dialogKey,
        UIntPtr dialogKeyLen,
        out ulong targetId)
    {
        EnsureLoaded();
        return omniRealtimeMediaPacketTargetCreate!(handle, dialogKey, dialogKeyLen, out targetId);
    }

    internal static void omni_realtime_media_packet_target_destroy(ulong targetId)
    {
        EnsureLoaded();
        omniRealtimeMediaPacketTargetDestroy!(targetId);
    }

    internal static void omni_realtime_media_packet_target_counters(
        ulong targetId,
        out ulong accepted,
        out ulong refused,
        out ulong testDropped)
    {
        EnsureLoaded();
        omniRealtimeMediaPacketTargetCounters!(targetId, out accepted, out refused, out testDropped);
    }

    /// Address of the core's packet sink — handed to the media engine next to a target ID.
    internal static IntPtr RealtimeMediaPacketSinkFunction
    {
        get
        {
            EnsureLoaded();
            return omniRealtimeMediaPacketSink;
        }
    }

    // Inbound media (2026-09-27): the core hands every datagram of the dialog to `sink(context, …)`
    // at once — the native media router's deliver entry with the router as context. Zero sink
    // unwires, and returns only when no delivery is in flight. Replaces the queue the shell drained
    // on a timer.
    internal static IntPtr omni_realtime_media_set_inbound_sink(
        IntPtr handle,
        IntPtr dialogKey,
        UIntPtr dialogKeyLen,
        IntPtr sink,
        IntPtr context)
    {
        EnsureLoaded();
        return omniRealtimeMediaSetInboundSink!(handle, dialogKey, dialogKeyLen, sink, context);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRealtimeMediaPacketTargetCreateDelegate(
        IntPtr handle,
        IntPtr dialogKey,
        UIntPtr dialogKeyLen,
        out ulong targetId);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OmniRealtimeMediaPacketTargetDestroyDelegate(ulong targetId);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OmniRealtimeMediaPacketTargetCountersDelegate(
        ulong targetId,
        out ulong accepted,
        out ulong refused,
        out ulong testDropped);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRealtimeMediaSetInboundSinkDelegate(
        IntPtr handle,
        IntPtr dialogKey,
        UIntPtr dialogKeyLen,
        IntPtr sink,
        IntPtr context);

}
