using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniDiscoverabilityGetOmniIdDelegate? omniDiscoverabilityGetOmniId;
    private static OmniDiscoverabilityGetModeDelegate? omniDiscoverabilityGetMode;
    private static OmniDiscoverabilitySetModeDelegate? omniDiscoverabilitySetMode;
    private static OmniDiscoverabilityFindByOmniIdDelegate? omniDiscoverabilityFindByOmniId;
    private static OmniDiscoverabilityGetModeDelegate? omniDiscoverabilityGetNameFindable;
    private static OmniDiscoverabilitySetModeDelegate? omniDiscoverabilitySetNameFindable;
    private static OmniDiscoverabilityFindByOmniIdDelegate? omniDiscoverabilitySearchByName;
    private static OmniDiscoverabilityGetModeDelegate? omniRelayDirectoryGetRole;
    private static OmniDiscoverabilitySetModeDelegate? omniRelayDirectorySetRole;
    private static OmniDiscoverabilityGetModeDelegate? omniRelayDirectorySearch;
    private static OmniDiscoverabilityGetModeDelegate? omniPinnedRelayGet;
    private static OmniDiscoverabilitySetModeDelegate? omniPinnedRelaySet;
    private static OmniDiscoverabilityGetModeDelegate? omniPinnedRelayClear;

    private static void LoadDiscoverabilityExports()
    {
        LoadExport(ref omniDiscoverabilityGetOmniId, nameof(omni_discoverability_get_omni_id));
        LoadExport(ref omniDiscoverabilityGetMode, nameof(omni_discoverability_get_mode));
        LoadExport(ref omniDiscoverabilitySetMode, nameof(omni_discoverability_set_mode));
        LoadExport(ref omniDiscoverabilityFindByOmniId, nameof(omni_discoverability_find_by_omni_id));
        LoadExport(ref omniDiscoverabilityGetNameFindable, nameof(omni_discoverability_get_name_findable));
        LoadExport(ref omniDiscoverabilitySetNameFindable, nameof(omni_discoverability_set_name_findable));
        LoadExport(ref omniDiscoverabilitySearchByName, nameof(omni_discoverability_search_by_name));
        LoadExport(ref omniRelayDirectoryGetRole, nameof(omni_relay_directory_get_role));
        LoadExport(ref omniRelayDirectorySetRole, nameof(omni_relay_directory_set_role));
        LoadExport(ref omniRelayDirectorySearch, nameof(omni_relay_directory_search));
        LoadExport(ref omniPinnedRelayGet, nameof(omni_pinned_relay_get));
        LoadExport(ref omniPinnedRelaySet, nameof(omni_pinned_relay_set));
        LoadExport(ref omniPinnedRelayClear, nameof(omni_pinned_relay_clear));
    }

    internal static IntPtr omni_pinned_relay_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniPinnedRelayGet!(handle);
    }

    internal static IntPtr omni_pinned_relay_set(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniPinnedRelaySet!(handle, requestJson);
    }

    internal static IntPtr omni_pinned_relay_clear(IntPtr handle)
    {
        EnsureLoaded();
        return omniPinnedRelayClear!(handle);
    }

    internal static IntPtr omni_relay_directory_get_role(IntPtr handle)
    {
        EnsureLoaded();
        return omniRelayDirectoryGetRole!(handle);
    }

    internal static IntPtr omni_relay_directory_set_role(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniRelayDirectorySetRole!(handle, requestJson);
    }

    internal static IntPtr omni_relay_directory_search(IntPtr handle)
    {
        EnsureLoaded();
        return omniRelayDirectorySearch!(handle);
    }

    internal static IntPtr omni_discoverability_get_name_findable(IntPtr handle)
    {
        EnsureLoaded();
        return omniDiscoverabilityGetNameFindable!(handle);
    }

    internal static IntPtr omni_discoverability_set_name_findable(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniDiscoverabilitySetNameFindable!(handle, requestJson);
    }

    internal static IntPtr omni_discoverability_search_by_name(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniDiscoverabilitySearchByName!(handle, requestJson);
    }

    internal static IntPtr omni_discoverability_find_by_omni_id(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniDiscoverabilityFindByOmniId!(handle, requestJson);
    }

    internal static IntPtr omni_discoverability_get_omni_id(IntPtr handle)
    {
        EnsureLoaded();
        return omniDiscoverabilityGetOmniId!(handle);
    }

    internal static IntPtr omni_discoverability_get_mode(IntPtr handle)
    {
        EnsureLoaded();
        return omniDiscoverabilityGetMode!(handle);
    }

    internal static IntPtr omni_discoverability_set_mode(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniDiscoverabilitySetMode!(handle, requestJson);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDiscoverabilityGetOmniIdDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDiscoverabilityGetModeDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDiscoverabilitySetModeDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDiscoverabilityFindByOmniIdDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);
}
