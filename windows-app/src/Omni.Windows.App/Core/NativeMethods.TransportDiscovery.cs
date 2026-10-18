using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniTransportSetLinkCostDelegate? omniTransportSetLinkCost;
    private static OmniTransportSetCongestionControlDelegate? omniTransportSetCongestionControl;
    private static OmniTransportEffectiveCongestionControlDelegate? omniTransportEffectiveCongestionControl;
    private static OmniTransportApplyCongestionControlNowDelegate? omniTransportApplyCongestionControlNow;
    private static OmniTransportSnapshotDelegate? omniTransportSnapshot;
    private static OmniTransportDiscoveryRefreshDelegate? omniTransportDiscoveryRefresh;
    private static OmniQueueSnapshotDelegate? omniQueueSnapshot;
    private static OmniQueueDiagnosticsDelegate? omniQueueDiagnostics;

    private static void LoadTransportDiscoveryExports()
    {
        LoadExport(ref omniTransportSetLinkCost, nameof(omni_transport_set_link_cost));
        LoadExport(ref omniTransportSetCongestionControl, nameof(omni_transport_set_congestion_control));
        LoadExport(
            ref omniTransportEffectiveCongestionControl,
            nameof(omni_transport_effective_congestion_control));
        LoadExport(
            ref omniTransportApplyCongestionControlNow,
            nameof(omni_transport_apply_congestion_control_now));
        LoadExport(ref omniTransportSnapshot, nameof(omni_transport_snapshot));
        LoadExport(ref omniTransportDiscoveryRefresh, nameof(omni_transport_discovery_refresh));
        LoadExport(ref omniQueueSnapshot, nameof(omni_queue_snapshot));
        LoadExport(ref omniQueueDiagnostics, nameof(omni_queue_diagnostics));
    }

    // Стоимость канала для пользователя: "metered" / "unmetered" / "unknown". Ядро само этого
    // знать не может — на Windows ответ даёт WinRT. Решает выбор контроллера перегрузки и
    // применяется при СОЗДАНИИ эндпоинта, то есть на уже поднятом соединении не отразится.
    internal static IntPtr omni_transport_set_link_cost(string cost)
    {
        EnsureLoaded();
        return omniTransportSetLinkCost!(cost);
    }

    // Ручной выбор контроллера перегрузки: "auto" / "bbr" / "cubic" / "newreno". Как и стоимость
    // канала, читается при СОЗДАНИИ эндпоинта — quinn не умеет менять контроллер на живом
    // соединении, поэтому уже поднятые линки доживают на прежнем.
    internal static IntPtr omni_transport_set_congestion_control(string mode)
    {
        EnsureLoaded();
        return omniTransportSetCongestionControl!(mode);
    }

    // Что применится к следующему соединению прямо сейчас, с учётом OMNI_QUIC_CC и стоимости
    // канала. Возвращает строку всегда (не ошибку) — её надо освободить omni_string_free.
    internal static IntPtr omni_transport_effective_congestion_control()
    {
        EnsureLoaded();
        return omniTransportEffectiveCongestionControl!();
    }

    // Применить выбранный контроллер к ЖИВОМУ мешу: пересобрать конфиги интерфейсов и закрыть
    // соединения, которые едут на прежнем. Дешёвая замена перезапуску меша — тот сносит дискавери,
    // анонсы и проброс порта и стоит больше минуты без связи.
    internal static IntPtr omni_transport_apply_congestion_control_now(IntPtr handle)
    {
        EnsureLoaded();
        return omniTransportApplyCongestionControlNow!(handle);
    }

    internal static IntPtr omni_transport_snapshot(IntPtr handle)
    {
        EnsureLoaded();
        return omniTransportSnapshot!(handle);
    }

    internal static IntPtr omni_transport_discovery_refresh(IntPtr handle)
    {
        EnsureLoaded();
        return omniTransportDiscoveryRefresh!(handle);
    }

    internal static IntPtr omni_queue_snapshot(IntPtr handle)
    {
        EnsureLoaded();
        return omniQueueSnapshot!(handle);
    }

    internal static IntPtr omni_queue_diagnostics(IntPtr handle)
    {
        EnsureLoaded();
        return omniQueueDiagnostics!(handle);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportSetLinkCostDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string cost);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportSetCongestionControlDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string mode);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportEffectiveCongestionControlDelegate();

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportApplyCongestionControlNowDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportSnapshotDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniTransportDiscoveryRefreshDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniQueueSnapshotDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniQueueDiagnosticsDelegate(IntPtr handle);

}
