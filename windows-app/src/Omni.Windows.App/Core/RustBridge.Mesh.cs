using System;
using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// Managed wrappers for the mesh overlay FFI domain. Each native export returns
// null on success / an error-envelope C string on failure, so we surface
// failures as exceptions (callers treat mesh as best-effort and log).
// See docs/decisions/mesh-transport-integration-seam.md.
internal sealed partial class RustBridge
{
    /// <summary>
    /// Start the mesh overlay for the active profile (idempotent). Call after
    /// sign-in so the node announces + accepts and peers can discover/reach it.
    /// </summary>
    public void StartMesh()
    {
        // Стоимость канала сообщаем ДО старта: контроллер перегрузки выбирается при создании
        // эндпоинта, позже смена уже ничего не изменит. См. LinkCostProbe.
        ReportLinkCost();
        // GetInitializedHandleForRealtimeMedia returns the shared core handle
        // (the name is historical — mesh + realtime media use the same handle).
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_start(coreHandle);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Сообщить ядру, считают ли трафик на текущем канале. Осечка здесь не должна мешать
    /// подняться сети: ядро без ответа возьмёт умолчание (BBR), и это записанное решение.
    /// </summary>
    public void ReportLinkCost()
    {
        try
        {
            string cost = LinkCostProbe.Read();
            IntPtr error = NativeMethods.omni_transport_set_link_cost(cost);
            if (error != IntPtr.Zero)
            {
                AppTrace.Write("net/link-cost", $"rejected: {NativeErrorReader.ReadAndFree(error)}");
                return;
            }

            AppTrace.Write("net/link-cost", cost);
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("net/link-cost", "probe", ex);
        }
    }

    /// <summary>
    /// Применить ручной выбор контроллера перегрузки ("auto"/"bbr"/"cubic"/"newreno").
    /// Действует на соединения, поднятые ПОСЛЕ вызова: quinn не умеет менять контроллер на живом
    /// соединении, поэтому для немедленного эффекта меш надо переустановить.
    /// </summary>
    public void SetCongestionControl(string mode)
    {
        IntPtr error = NativeMethods.omni_transport_set_congestion_control(mode);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Что применится к следующему соединению прямо сейчас — с учётом переменной окружения и
    /// стоимости канала. Нужно, чтобы настройка не врала на «авто».
    /// </summary>
    public string ReadEffectiveCongestionControl()
    {
        IntPtr value = NativeMethods.omni_transport_effective_congestion_control();
        return value == IntPtr.Zero ? "unknown" : NativeErrorReader.ReadAndFree(value);
    }

    /// <summary>
    /// Применить выбранный контроллер к живым линкам, НЕ перезапуская меш. Возвращает текст ядра
    /// (`closed=N` или `error: …`): N — сколько соединений пришлось закрыть, чтобы они не доехали
    /// на прежнем контроллере. Ноль значит «применять было не к чему».
    /// </summary>
    public string ApplyCongestionControlNow()
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr result = NativeMethods.omni_transport_apply_congestion_control_now(coreHandle);
        return result == IntPtr.Zero ? "closed=0" : NativeErrorReader.ReadAndFree(result);
    }

    /// <summary>Stop the mesh overlay (call on sign-out).</summary>
    public void StopMesh()
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_stop(coreHandle);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Dial the active call's peer over the mesh and bind it to the call's
    /// dialog so realtime media routes over the verified mesh Link. May block
    /// up to ~5s waiting for multicast discovery — call off the UI thread.
    /// </summary>
    public void ConnectActiveCallToMesh()
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_connect_active_call(coreHandle);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Generate a copy-pasteable mesh pairing invite (`om3mesh:` token) for the
    /// active profile, so a friend can dial + verify us over the mesh and pair
    /// without any legacy connection. See docs/decisions/pairing-over-mesh-adoption.md.
    /// </summary>
    public string GenerateMeshPairingInvite()
        => CallJson(
            () => NativeMethods.omni_mesh_generate_pairing_invite(handle),
            OmniJsonSerializerContext.Default.String,
            "mesh_generate_pairing_invite");

    /// <summary>
    /// The honest connection-state DTO: this node's connectivity + each contact's link-state (with
    /// an honest failure reason) + the orthogonal <c>Verified</c> flag. The core derives it from
    /// existing signals (Rust = truth); the shell renders it and never derives.
    /// See docs/decisions/omni-network-model.md.
    /// </summary>
    public MeshConnectionStateDto MeshConnectionState()
        => CallJson(
            () => NativeMethods.omni_mesh_connection_state(handle),
            OmniJsonSerializerContext.Default.MeshConnectionStateDto,
            "mesh_connection_state");

    /// <summary>
    /// Import a friend's mesh pairing invite (`om3mesh:` token): registers their
    /// mesh node as a pairing target so the auto-connect worker dials and pairs
    /// over the mesh. The existing contact-approval UI then drives the rest.
    /// </summary>
    public void RegisterMeshPairingInvite(string token)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_register_pairing_invite(coreHandle, token);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Read the active profile's connection-privacy mode (<c>auto</c> | <c>direct-only</c>).
    /// Auto allows third-party serverless rendezvous; Direct-only restricts connectivity to
    /// LAN + router port-mapping + manual exchange. See docs/decisions/connection-privacy-modes.md.
    /// </summary>
    public string GetConnectionPrivacyMode()
        => CallJson(
            () => NativeMethods.omni_mesh_connection_privacy_mode_get(handle),
            OmniJsonSerializerContext.Default.String,
            "mesh_connection_privacy_mode_get");

    /// <summary>
    /// Set the active profile's connection-privacy mode (<c>auto</c> | <c>direct-only</c>).
    /// Persisted; takes effect on the next rendezvous sweep.
    /// </summary>
    public void SetConnectionPrivacyMode(string mode)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_connection_privacy_mode_set(coreHandle, mode);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Read the active profile's carrier policy (<c>tcp</c> | <c>auto</c> |
    /// <c>quic</c>) — the R1a connection-quality ladder, axis A. See
    /// docs/decisions/connection-quality-ladder.md.
    /// </summary>
    public string GetCarrierPolicy()
        => CallJson(
            () => NativeMethods.omni_mesh_carrier_policy_get(handle),
            OmniJsonSerializerContext.Default.String,
            "mesh_carrier_policy_get");

    /// <summary>
    /// Set the active profile's carrier policy (<c>tcp</c> | <c>auto</c> |
    /// <c>quic</c>). Persisted; takes effect on the next mesh start.
    /// </summary>
    public void SetCarrierPolicy(string policy)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_carrier_policy_set(coreHandle, policy);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Get the active profile's fixed direct-TCP front-door port as a decimal string ("0" =
    /// ephemeral / UPnP). When non-zero, the user port-forwards this TCP port on their router so a
    /// peer on a UDP-hostile network (mobile) can reach us relay-less over TCP.
    /// See docs/decisions/tcp-direct-peer-fallback.md.
    /// </summary>
    public string GetDirectTcpPort()
        => CallJson(
            () => NativeMethods.omni_mesh_direct_tcp_port_get(handle),
            OmniJsonSerializerContext.Default.String,
            "mesh_direct_tcp_port_get");

    /// <summary>
    /// Set the active profile's fixed direct-TCP front-door port ("0" / blank clears it back to
    /// ephemeral). Persisted; takes effect on the next mesh start.
    /// </summary>
    public void SetDirectTcpPort(string port)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_direct_tcp_port_set(coreHandle, port);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Export a "код связи" reach token (<c>om3reach:…</c>) — this node's current reachable
    /// endpoints, so a friend can dial us directly with no third-party rendezvous (manual /
    /// Direct-only path). Ephemeral: re-export when the address changes.
    /// See docs/decisions/connection-privacy-modes.md.
    /// </summary>
    public string GenerateReachToken()
        => CallJson(
            () => NativeMethods.omni_mesh_generate_reach_token(handle),
            OmniJsonSerializerContext.Default.String,
            "mesh_generate_reach_token");

    /// <summary>
    /// Import a friend's "код связи" reach token (<c>om3reach:…</c>) and dial them at the carried
    /// endpoints (manual reach to an already-paired contact).
    /// </summary>
    public void DialReachToken(string token)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_dial_reach_token(coreHandle, token);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Kick off an on-demand serverless-rendezvous dial to an approved contact that has no mesh
    /// link yet, so cross-network reach is immediate instead of waiting for the periodic rendezvous
    /// sweep. Idempotent + best-effort: the core no-ops when already linked, the mesh isn't started,
    /// there's no rendezvous pair-secret yet, or the profile is Direct-only, and runs the slow dial
    /// on a background thread, so this returns promptly.
    /// See docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
    /// </summary>
    public void RendezvousReachContact(string accountId)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_mesh_rendezvous_reach_contact(coreHandle, accountId);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// The active profile's network entry-node registry (the seeded base node + user-added relays /
    /// bootstrap) — the list the «Узлы сети / Релеи» screen renders. An empty list is valid (LAN +
    /// invite still connect; no node is privileged). See docs/notes/roadmap-CONTINUE.md.
    /// </summary>
    public IReadOnlyList<NetworkNodeDto> ListNetworkNodes()
        => CallJson(
            () => NativeMethods.omni_network_node_list(handle),
            OmniJsonSerializerContext.Default.ListNetworkNodeDto,
            "network_node_list");

    /// <summary>
    /// Add a relay / bootstrap entry node by <c>ip:port</c> with a user label. Throws on an invalid
    /// endpoint or a duplicate. Returns the new node id (callers usually just re-list).
    /// </summary>
    public long AddNetworkNode(string endpoint, string label)
        => CallJson(
            () => NativeMethods.omni_network_node_add(
                GetInitializedHandleForRealtimeMedia(), endpoint, label),
            OmniJsonSerializerContext.Default.Int64,
            "network_node_add");

    /// <summary>
    /// Import a SIGNED <c>om3relay:</c> relay bundle: verifies the issuer signature, then REPLACES that
    /// issuer's prior relay set with the bundle (a strict-newer rotation guard ignores a replay/older
    /// version). Returns the number of relays imported — <c>0</c> means the bundle wasn't newer than
    /// what's already stored (ignored), not a failure. Throws on a malformed / wrong-prefix / bad-signature
    /// token. R1c — see docs/decisions/r1c-rotatable-white-relay.md.
    /// </summary>
    public long ImportRelayBundle(string token)
        => CallJson(
            () => NativeMethods.omni_network_relay_bundle_import(
                GetInitializedHandleForRealtimeMedia(), token),
            OmniJsonSerializerContext.Default.Int64,
            "network_relay_bundle_import");

    /// <summary>
    /// Remove ANY entry node by id — including the seeded base node (the user may delete it; the empty
    /// list still connects via LAN + invite).
    /// </summary>
    public void RemoveNetworkNode(long id)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_network_node_remove(coreHandle, id);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Flag (or unflag) an entry node as part of MY OWN relay bundle — the set auto-propagated to
    /// approved friends over the per-pair beacon (R1c). A change bumps my bundle version so the
    /// rotation propagates. See docs/decisions/r1c-rotatable-white-relay.md.
    /// </summary>
    public void SetRelayShared(long id, bool shared)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_network_relay_set_shared(coreHandle, id, shared ? 1 : 0);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// The active profile's rendezvous courier providers (seeded defaults + user/invite-added) — the list
    /// the «Провайдеры рандеву» screen renders. These are the public pub/sub brokers the app fans beacons
    /// across to MEET a peer; distinct from the relay list. An empty set falls back to the compiled default.
    /// See docs/decisions/global-identity-discovery.md.
    /// </summary>
    public IReadOnlyList<RendezvousProviderDto> ListRendezvousProviders()
        => CallJson(
            () => NativeMethods.omni_rendezvous_provider_list(handle),
            OmniJsonSerializerContext.Default.ListRendezvousProviderDto,
            "rendezvous_provider_list");

    /// <summary>
    /// Add a rendezvous provider by URL (<c>http(s)://</c> ntfy / <c>ws(s)://</c> Nostr relay). Throws on
    /// an invalid scheme or a duplicate. Applies at once.
    /// </summary>
    public void AddRendezvousProvider(string url)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_rendezvous_provider_add(coreHandle, url);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Remove a courier the user or an invite added (a default one is turned off instead).
    /// </summary>
    public void RemoveRendezvousProvider(long id)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_rendezvous_provider_remove(coreHandle, id);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Set how a courier is used: "publish_read", "read" or "off" (Mainline too — its row). At least one
    /// courier has to stay "publish_read". Applies at once.
    /// </summary>
    /// <summary>Where and how often each publication goes (docs/decisions/public-relay-directory.md §10):
    /// the beacons for contacts, Omni-ID, name, relay directory.</summary>
    public IReadOnlyList<PublishPlanView> GetPublishPlans()
        => CallJson(
            () => NativeMethods.omni_publish_plans_get(handle),
            OmniJsonSerializerContext.Default.IReadOnlyListPublishPlanView,
            "publish_plans_get");

    /// <summary>Set one publication's plan; the core checks it and answers with the plan as stored.</summary>
    public PublishPlanView SetPublishPlan(string publication, PublishPlan plan)
        => CallJson(
            () => NativeMethods.omni_publish_plan_set(
                handle,
                JsonSerializer.Serialize(new SetPublishPlanRequest(publication, plan), OmniJsonSerializerContext.Default.SetPublishPlanRequest)),
            OmniJsonSerializerContext.Default.PublishPlanView,
            "publish_plan_set");

    internal sealed record SetPublishPlanRequest(
        [property: JsonPropertyName("publication")] string Publication,
        [property: JsonPropertyName("plan")] PublishPlan Plan);

    public void SetRendezvousProviderMode(long id, string mode)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_rendezvous_provider_set_mode(coreHandle, id, mode);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Whether this profile runs its DHT node as a reachable in-segment ENTRY node — server_mode + a
    /// fixed port + self-UPnP so others can bootstrap FROM it. Off by default. Needs DHT rendezvous on.
    /// </summary>
    public bool GetDhtEntryNodeEnabled()
        => string.Equals(
            CallJson(
                () => NativeMethods.omni_dht_entry_node_get(handle),
                OmniJsonSerializerContext.Default.String,
                "dht_entry_node_get"),
            "on",
            StringComparison.Ordinal);

    /// <summary>
    /// Opt this profile in/out of the DHT entry-node role. Takes effect on the next per-profile
    /// rendezvous spawn.
    /// </summary>
    public void SetDhtEntryNodeEnabled(bool enabled)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_dht_entry_node_set(coreHandle, enabled ? "on" : "off");
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// The node's public DHT entry endpoint (<c>ip:port</c>) when running as a UPnP-mapped entry node —
    /// the address others bootstrap from. Empty if off / not up yet / no public mapping (CGNAT / no UPnP).
    /// </summary>
    public string GetDhtEntryNodeEndpoint()
        => CallJson(
            () => NativeMethods.omni_dht_entry_node_endpoint_get(handle),
            OmniJsonSerializerContext.Default.String,
            "dht_entry_node_endpoint_get") ?? string.Empty;

    /// <summary>
    /// Live BT-DHT rendezvous status for the settings indicator: whether the helper is up (<c>Running</c>)
    /// and how many Mainline nodes it currently sees (<c>NodeCount</c> — the honest "connected to the
    /// swarm" signal), plus the entry-node public address. A cheap poll (one <c>nodes</c> line over
    /// loopback). Null if the core can't be read yet (pre-sign-in) — callers treat it as "unknown".
    /// </summary>
    public DhtRendezvousStatus? GetDhtRendezvousStatus()
        => CallJson(
            () => NativeMethods.omni_dht_rendezvous_status_get(handle),
            OmniJsonSerializerContext.Default.DhtRendezvousStatus,
            "dht_rendezvous_status_get");

    /// <summary>
    /// Apply the DHT-rendezvous / entry-node toggles immediately by re-creating this profile's rendezvous
    /// runtime (respawns the <c>omni-dht-helper</c> now instead of on the next dial). Spawns a child
    /// process — call OFF the UI thread. Throws on failure (callers treat mesh as best-effort).
    /// </summary>
    public void RespawnRendezvousRuntime()
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_rendezvous_respawn(coreHandle);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Live status of the BT-DHT rendezvous leg (mirrors the Rust <c>DhtRendezvousStatus</c>).
    /// </summary>
    public sealed record DhtRendezvousStatus(
        [property: JsonPropertyName("enabled")] bool Enabled,
        [property: JsonPropertyName("entry_node_enabled")] bool EntryNodeEnabled,
        [property: JsonPropertyName("running")] bool Running,
        [property: JsonPropertyName("node_count")] long NodeCount,
        [property: JsonPropertyName("empty_for_ms")] long EmptyForMs,
        [property: JsonPropertyName("entry_endpoint")] string? EntryEndpoint,
        [property: JsonPropertyName("way_in")] string? WayIn,
        [property: JsonPropertyName("friend_proxy")] bool FriendProxy);

    /// <summary>
    /// This node's local transport (QUIC) <c>ip:port</c>. The PORT is what a peer on the same segment
    /// dials to reach this node's listener; the GUI pairs it with a LAN IP for an entry-node address.
    /// Throws if the mesh isn't started.
    /// </summary>
    public string GetLocalTransportEndpoint()
        => CallJson(
            () => NativeMethods.omni_local_transport_endpoint_get(handle),
            OmniJsonSerializerContext.Default.String,
            "local_transport_endpoint_get");

    /// <summary>
    /// Custom DHT bootstrap nodes (<c>ip:port</c>) — custom DHT entry points that let the DHT work
    /// inside an isolated network segment. Empty = the global Mainline routers.
    /// </summary>
    public IReadOnlyList<string> GetDhtBootstrapNodes()
    {
        string joined = CallJson(
            () => NativeMethods.omni_dht_bootstrap_list_get(handle),
            OmniJsonSerializerContext.Default.String,
            "dht_bootstrap_list_get");
        return string.IsNullOrEmpty(joined)
            ? Array.Empty<string>()
            : joined.Split('\n', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
    }

    public void AddDhtBootstrapNode(string addr)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_dht_bootstrap_add(coreHandle, addr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    public void RemoveDhtBootstrapNode(string addr)
    {
        IntPtr coreHandle = GetInitializedHandleForRealtimeMedia();
        IntPtr error = NativeMethods.omni_dht_bootstrap_remove(coreHandle, addr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
        }
    }

    /// <summary>
    /// Export the configured DHT bootstrap nodes as a shareable <c>om3dhtboot:</c> bundle token — a
    /// copy-pasteable string for out-of-band community distribution (anyone can import it, not only
    /// contacts). Throws if the list is empty.
    /// </summary>
    public string ExportDhtBootstrapBundle()
    {
        return CallJson(
            () => NativeMethods.omni_dht_bootstrap_bundle_export(handle),
            OmniJsonSerializerContext.Default.String,
            "dht_bootstrap_bundle_export") ?? string.Empty;
    }

    /// <summary>
    /// Import a shareable <c>om3dhtboot:</c> bundle token, merging its nodes into the configured list.
    /// Returns the count of NEW nodes added (0 = all already present). Throws on a malformed token.
    /// </summary>
    public long ImportDhtBootstrapBundle(string token)
    {
        return CallJson(
            () => NativeMethods.omni_dht_bootstrap_bundle_import(handle, token),
            OmniJsonSerializerContext.Default.Int64,
            "dht_bootstrap_bundle_import");
    }
}
