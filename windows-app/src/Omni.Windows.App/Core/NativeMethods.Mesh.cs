using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

// P/Invoke for the mesh overlay domain (omni_mesh_*). Mirrors the per-domain
// delegate + LoadExport pattern of NativeMethods.RealtimeMedia.cs. Each export
// returns IntPtr: null on success, an error-envelope C string on failure
// (read + freed via NativeErrorReader) — except omni_mesh_generate_pairing_invite,
// which returns a JSON payload (the token string or an error envelope). See
// docs/decisions/mesh-transport-integration-seam.md.
internal static partial class NativeMethods
{
    private static OmniMeshStartDelegate? omniMeshStart;
    private static OmniMeshStopDelegate? omniMeshStop;
    private static OmniMeshConnectActiveCallDelegate? omniMeshConnectActiveCall;
    private static OmniMeshGeneratePairingInviteDelegate? omniMeshGeneratePairingInvite;
    private static OmniMeshRegisterPairingInviteDelegate? omniMeshRegisterPairingInvite;
    private static OmniMeshConnectionStateDelegate? omniMeshConnectionState;
    private static OmniMeshConnectionPrivacyModeGetDelegate? omniMeshConnectionPrivacyModeGet;
    private static OmniMeshConnectionPrivacyModeSetDelegate? omniMeshConnectionPrivacyModeSet;
    private static OmniMeshCarrierPolicyGetDelegate? omniMeshCarrierPolicyGet;
    private static OmniMeshCarrierPolicySetDelegate? omniMeshCarrierPolicySet;
    private static OmniMeshDirectTcpPortGetDelegate? omniMeshDirectTcpPortGet;
    private static OmniMeshDirectTcpPortSetDelegate? omniMeshDirectTcpPortSet;
    private static OmniMeshGenerateReachTokenDelegate? omniMeshGenerateReachToken;
    private static OmniMeshDialReachTokenDelegate? omniMeshDialReachToken;
    private static OmniMeshRendezvousReachContactDelegate? omniMeshRendezvousReachContact;
    private static OmniNetworkNodeListDelegate? omniNetworkNodeList;
    private static OmniNetworkNodeAddDelegate? omniNetworkNodeAdd;
    private static OmniNetworkNodeRemoveDelegate? omniNetworkNodeRemove;
    private static OmniNetworkRelayBundleImportDelegate? omniNetworkRelayBundleImport;
    private static OmniNetworkRelaySetSharedDelegate? omniNetworkRelaySetShared;
    private static OmniRendezvousProviderListDelegate? omniRendezvousProviderList;
    private static OmniRendezvousProviderAddDelegate? omniRendezvousProviderAdd;
    private static OmniRendezvousProviderRemoveDelegate? omniRendezvousProviderRemove;
    private static OmniRendezvousProviderSetModeDelegate? omniRendezvousProviderSetMode;
    private static OmniMeshCarrierPolicyGetDelegate? omniPublishPlansGet;
    private static OmniGroupsJsonDelegate? omniPublishPlanSet;
    private static OmniDhtEntryNodeGetDelegate? omniDhtEntryNodeGet;
    private static OmniDhtEntryNodeSetDelegate? omniDhtEntryNodeSet;
    private static OmniDhtEntryNodeEndpointGetDelegate? omniDhtEntryNodeEndpointGet;
    private static OmniDhtRendezvousStatusGetDelegate? omniDhtRendezvousStatusGet;
    private static OmniRendezvousRespawnDelegate? omniRendezvousRespawn;
    private static OmniLocalTransportEndpointGetDelegate? omniLocalTransportEndpointGet;
    private static OmniDhtBootstrapListGetDelegate? omniDhtBootstrapListGet;
    private static OmniDhtBootstrapAddDelegate? omniDhtBootstrapAdd;
    private static OmniDhtBootstrapRemoveDelegate? omniDhtBootstrapRemove;
    private static OmniDhtBootstrapBundleExportDelegate? omniDhtBootstrapBundleExport;
    private static OmniDhtBootstrapBundleImportDelegate? omniDhtBootstrapBundleImport;

    private static void LoadMeshExports()
    {
        LoadExport(ref omniMeshStart, nameof(omni_mesh_start));
        LoadExport(ref omniMeshStop, nameof(omni_mesh_stop));
        LoadExport(ref omniMeshConnectActiveCall, nameof(omni_mesh_connect_active_call));
        LoadExport(ref omniMeshGeneratePairingInvite, nameof(omni_mesh_generate_pairing_invite));
        LoadExport(ref omniMeshRegisterPairingInvite, nameof(omni_mesh_register_pairing_invite));
        LoadExport(ref omniMeshConnectionState, nameof(omni_mesh_connection_state));
        LoadExport(ref omniMeshConnectionPrivacyModeGet, nameof(omni_mesh_connection_privacy_mode_get));
        LoadExport(ref omniMeshConnectionPrivacyModeSet, nameof(omni_mesh_connection_privacy_mode_set));
        LoadExport(ref omniMeshCarrierPolicyGet, nameof(omni_mesh_carrier_policy_get));
        LoadExport(ref omniMeshCarrierPolicySet, nameof(omni_mesh_carrier_policy_set));
        LoadExport(ref omniMeshDirectTcpPortGet, nameof(omni_mesh_direct_tcp_port_get));
        LoadExport(ref omniMeshDirectTcpPortSet, nameof(omni_mesh_direct_tcp_port_set));
        LoadExport(ref omniMeshGenerateReachToken, nameof(omni_mesh_generate_reach_token));
        LoadExport(ref omniMeshDialReachToken, nameof(omni_mesh_dial_reach_token));
        LoadExport(ref omniMeshRendezvousReachContact, nameof(omni_mesh_rendezvous_reach_contact));
        LoadExport(ref omniNetworkNodeList, nameof(omni_network_node_list));
        LoadExport(ref omniNetworkNodeAdd, nameof(omni_network_node_add));
        LoadExport(ref omniNetworkNodeRemove, nameof(omni_network_node_remove));
        LoadExport(ref omniNetworkRelayBundleImport, nameof(omni_network_relay_bundle_import));
        LoadExport(ref omniNetworkRelaySetShared, nameof(omni_network_relay_set_shared));
        LoadExport(ref omniRendezvousProviderList, nameof(omni_rendezvous_provider_list));
        LoadExport(ref omniRendezvousProviderAdd, nameof(omni_rendezvous_provider_add));
        LoadExport(ref omniRendezvousProviderRemove, nameof(omni_rendezvous_provider_remove));
        LoadExport(ref omniRendezvousProviderSetMode, nameof(omni_rendezvous_provider_set_mode));
        LoadExport(ref omniPublishPlansGet, nameof(omni_publish_plans_get));
        LoadExport(ref omniPublishPlanSet, nameof(omni_publish_plan_set));
        LoadExport(ref omniDhtEntryNodeGet, nameof(omni_dht_entry_node_get));
        LoadExport(ref omniDhtEntryNodeSet, nameof(omni_dht_entry_node_set));
        LoadExport(ref omniDhtEntryNodeEndpointGet, nameof(omni_dht_entry_node_endpoint_get));
        LoadExport(ref omniDhtRendezvousStatusGet, nameof(omni_dht_rendezvous_status_get));
        LoadExport(ref omniRendezvousRespawn, nameof(omni_rendezvous_respawn));
        LoadExport(ref omniLocalTransportEndpointGet, nameof(omni_local_transport_endpoint_get));
        LoadExport(ref omniDhtBootstrapListGet, nameof(omni_dht_bootstrap_list_get));
        LoadExport(ref omniDhtBootstrapAdd, nameof(omni_dht_bootstrap_add));
        LoadExport(ref omniDhtBootstrapRemove, nameof(omni_dht_bootstrap_remove));
        LoadExport(ref omniDhtBootstrapBundleExport, nameof(omni_dht_bootstrap_bundle_export));
        LoadExport(ref omniDhtBootstrapBundleImport, nameof(omni_dht_bootstrap_bundle_import));
    }

    internal static IntPtr omni_mesh_start(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshStart!(handle);
    }

    internal static IntPtr omni_mesh_stop(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshStop!(handle);
    }

    internal static IntPtr omni_mesh_connect_active_call(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshConnectActiveCall!(handle);
    }

    internal static IntPtr omni_mesh_generate_pairing_invite(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshGeneratePairingInvite!(handle);
    }

    internal static IntPtr omni_mesh_register_pairing_invite(IntPtr handle, string token)
    {
        EnsureLoaded();
        return omniMeshRegisterPairingInvite!(handle, token);
    }

    internal static IntPtr omni_mesh_connection_state(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshConnectionState!(handle);
    }

    internal static IntPtr omni_mesh_connection_privacy_mode_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshConnectionPrivacyModeGet!(handle);
    }

    internal static IntPtr omni_mesh_connection_privacy_mode_set(IntPtr handle, string mode)
    {
        EnsureLoaded();
        return omniMeshConnectionPrivacyModeSet!(handle, mode);
    }

    internal static IntPtr omni_mesh_carrier_policy_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshCarrierPolicyGet!(handle);
    }

    internal static IntPtr omni_mesh_carrier_policy_set(IntPtr handle, string policy)
    {
        EnsureLoaded();
        return omniMeshCarrierPolicySet!(handle, policy);
    }

    internal static IntPtr omni_mesh_direct_tcp_port_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshDirectTcpPortGet!(handle);
    }

    internal static IntPtr omni_mesh_direct_tcp_port_set(IntPtr handle, string port)
    {
        EnsureLoaded();
        return omniMeshDirectTcpPortSet!(handle, port);
    }

    internal static IntPtr omni_mesh_generate_reach_token(IntPtr handle)
    {
        EnsureLoaded();
        return omniMeshGenerateReachToken!(handle);
    }

    internal static IntPtr omni_mesh_dial_reach_token(IntPtr handle, string token)
    {
        EnsureLoaded();
        return omniMeshDialReachToken!(handle, token);
    }

    internal static IntPtr omni_mesh_rendezvous_reach_contact(IntPtr handle, string accountId)
    {
        EnsureLoaded();
        return omniMeshRendezvousReachContact!(handle, accountId);
    }

    internal static IntPtr omni_network_node_list(IntPtr handle)
    {
        EnsureLoaded();
        return omniNetworkNodeList!(handle);
    }

    internal static IntPtr omni_network_node_add(IntPtr handle, string endpoint, string label)
    {
        EnsureLoaded();
        return omniNetworkNodeAdd!(handle, endpoint, label);
    }

    internal static IntPtr omni_network_relay_bundle_import(IntPtr handle, string token)
    {
        EnsureLoaded();
        return omniNetworkRelayBundleImport!(handle, token);
    }

    internal static IntPtr omni_network_node_remove(IntPtr handle, long id)
    {
        EnsureLoaded();
        return omniNetworkNodeRemove!(handle, id);
    }

    internal static IntPtr omni_network_relay_set_shared(IntPtr handle, long id, int shared)
    {
        EnsureLoaded();
        return omniNetworkRelaySetShared!(handle, id, shared);
    }

    internal static IntPtr omni_rendezvous_provider_list(IntPtr handle)
    {
        EnsureLoaded();
        return omniRendezvousProviderList!(handle);
    }

    internal static IntPtr omni_rendezvous_provider_add(IntPtr handle, string url)
    {
        EnsureLoaded();
        return omniRendezvousProviderAdd!(handle, url);
    }

    internal static IntPtr omni_rendezvous_provider_remove(IntPtr handle, long id)
    {
        EnsureLoaded();
        return omniRendezvousProviderRemove!(handle, id);
    }

    internal static IntPtr omni_publish_plans_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniPublishPlansGet!(handle);
    }

    internal static IntPtr omni_publish_plan_set(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniPublishPlanSet!(handle, requestJson);
    }

    internal static IntPtr omni_rendezvous_provider_set_mode(IntPtr handle, long id, string mode)
    {
        EnsureLoaded();
        return omniRendezvousProviderSetMode!(handle, id, mode);
    }

    internal static IntPtr omni_dht_entry_node_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniDhtEntryNodeGet!(handle);
    }

    internal static IntPtr omni_dht_entry_node_set(IntPtr handle, string mode)
    {
        EnsureLoaded();
        return omniDhtEntryNodeSet!(handle, mode);
    }

    internal static IntPtr omni_dht_entry_node_endpoint_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniDhtEntryNodeEndpointGet!(handle);
    }

    internal static IntPtr omni_dht_rendezvous_status_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniDhtRendezvousStatusGet!(handle);
    }

    internal static IntPtr omni_rendezvous_respawn(IntPtr handle)
    {
        EnsureLoaded();
        return omniRendezvousRespawn!(handle);
    }

    internal static IntPtr omni_local_transport_endpoint_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniLocalTransportEndpointGet!(handle);
    }

    internal static IntPtr omni_dht_bootstrap_list_get(IntPtr handle)
    {
        EnsureLoaded();
        return omniDhtBootstrapListGet!(handle);
    }

    internal static IntPtr omni_dht_bootstrap_add(IntPtr handle, string addr)
    {
        EnsureLoaded();
        return omniDhtBootstrapAdd!(handle, addr);
    }

    internal static IntPtr omni_dht_bootstrap_remove(IntPtr handle, string addr)
    {
        EnsureLoaded();
        return omniDhtBootstrapRemove!(handle, addr);
    }

    internal static IntPtr omni_dht_bootstrap_bundle_export(IntPtr handle)
    {
        EnsureLoaded();
        return omniDhtBootstrapBundleExport!(handle);
    }

    internal static IntPtr omni_dht_bootstrap_bundle_import(IntPtr handle, string token)
    {
        EnsureLoaded();
        return omniDhtBootstrapBundleImport!(handle, token);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshStartDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshStopDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshConnectActiveCallDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshGeneratePairingInviteDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshRegisterPairingInviteDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string token);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshConnectionStateDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshConnectionPrivacyModeGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshConnectionPrivacyModeSetDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string mode);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshCarrierPolicyGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshCarrierPolicySetDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string policy);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshDirectTcpPortGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshDirectTcpPortSetDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string port);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshGenerateReachTokenDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshDialReachTokenDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string token);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniMeshRendezvousReachContactDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string accountId);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniNetworkNodeListDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniNetworkNodeAddDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string endpoint,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string label);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniNetworkNodeRemoveDelegate(IntPtr handle, long id);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniNetworkRelayBundleImportDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string token);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniNetworkRelaySetSharedDelegate(IntPtr handle, long id, int shared);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRendezvousProviderListDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRendezvousProviderAddDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string url);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRendezvousProviderRemoveDelegate(IntPtr handle, long id);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRendezvousProviderSetModeDelegate(
        IntPtr handle,
        long id,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string mode);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtEntryNodeGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtEntryNodeSetDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string mode);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtEntryNodeEndpointGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtRendezvousStatusGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniRendezvousRespawnDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniLocalTransportEndpointGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtBootstrapListGetDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtBootstrapAddDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string addr);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtBootstrapRemoveDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string addr);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtBootstrapBundleExportDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDhtBootstrapBundleImportDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string token);
}
