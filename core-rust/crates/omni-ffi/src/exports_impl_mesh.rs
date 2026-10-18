// FFI implementation for the mesh overlay domain. Thin wrappers over OmniApp's
// mesh orchestration (start / stop / connect-active-call). The C ABI shells
// live in ffi_exports_mesh.rs.
//
// See docs/decisions/mesh-transport-integration-seam.md.

fn mesh_start_impl(handle: *mut OmniAppHandle) -> OmniResult<()> {
    // The shell is the real product: bind the LAN address so it can mesh with another
    // machine (start_mesh_networked auto-detects, falls back to same-host if isolated).
    // See docs/decisions/mesh-network-binding.md.
    require_handle(handle)?.inner.start_mesh_networked()
}

fn mesh_stop_impl(handle: *mut OmniAppHandle) -> OmniResult<()> {
    require_handle(handle)?.inner.stop_mesh();
    Ok(())
}

fn mesh_connect_active_call_impl(handle: *mut OmniAppHandle) -> OmniResult<()> {
    require_handle(handle)?.inner.mesh_connect_active_call()
}

fn mesh_generate_pairing_invite_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result =
        (|| -> OmniResult<String> { require_handle(handle)?.inner.generate_mesh_pairing_invite() })();
    to_result_payload(result)
}

fn mesh_connection_state_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result = (|| -> OmniResult<omni_app::MeshConnectionState> {
        require_handle(handle)?.inner.mesh_connection_state()
    })();
    to_result_payload(result)
}

fn mesh_register_pairing_invite_impl(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> OmniResult<()> {
    let token = parse_utf8_arg(token, "mesh pairing invite token")?;
    require_handle(handle)?
        .inner
        .register_mesh_pairing_invite(&token)
        .map(|_node| ())
}

fn mesh_connection_privacy_mode_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result = (|| -> OmniResult<String> {
        Ok(require_handle(handle)?
            .inner
            .connection_privacy_mode()
            .as_db_value()
            .to_string())
    })();
    to_result_payload(result)
}

fn mesh_connection_privacy_mode_set_impl(
    handle: *mut OmniAppHandle,
    mode: *const c_char,
) -> OmniResult<()> {
    let mode = parse_utf8_arg(mode, "connection privacy mode")?;
    let mode = omni_app::ConnectionPrivacyMode::from_db_value(&mode)?;
    require_handle(handle)?.inner.set_connection_privacy_mode(mode)
}

fn mesh_carrier_policy_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    // R1a connection-quality ladder (axis A): tcp / auto / quic.
    let result = (|| -> OmniResult<String> {
        Ok(require_handle(handle)?
            .inner
            .carrier_policy()
            .as_db_value()
            .to_string())
    })();
    to_result_payload(result)
}

fn mesh_carrier_policy_set_impl(
    handle: *mut OmniAppHandle,
    policy: *const c_char,
) -> OmniResult<()> {
    let policy = parse_utf8_arg(policy, "carrier policy")?;
    let policy = omni_types::CarrierPolicy::from_db_value(&policy)?;
    require_handle(handle)?.inner.set_carrier_policy(policy)
}

fn mesh_direct_tcp_port_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    // Fixed direct-TCP front-door port for a reliable relay-less inbound path. "0" = ephemeral/UPnP.
    let result = (|| -> OmniResult<String> {
        Ok(require_handle(handle)?
            .inner
            .direct_tcp_bind_port()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "0".to_string()))
    })();
    to_result_payload(result)
}

fn mesh_direct_tcp_port_set_impl(
    handle: *mut OmniAppHandle,
    port: *const c_char,
) -> OmniResult<()> {
    // "0"/blank clears back to ephemeral; else a valid TCP port the user has port-forwarded.
    let port = parse_utf8_arg(port, "direct-tcp port")?;
    let port_val: u16 = port.trim().parse().map_err(|_| {
        omni_types::OmniError::InvalidInput(format!("'{port}' is not a valid TCP port (0-65535)"))
    })?;
    require_handle(handle)?
        .inner
        .set_direct_tcp_bind_port(Some(port_val))
}

fn mesh_generate_reach_token_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result =
        (|| -> OmniResult<String> { require_handle(handle)?.inner.generate_reach_token() })();
    to_result_payload(result)
}

fn mesh_dial_reach_token_impl(handle: *mut OmniAppHandle, token: *const c_char) -> OmniResult<()> {
    let token = parse_utf8_arg(token, "reach token")?;
    require_handle(handle)?
        .inner
        .dial_reach_token(&token)
        .map(|_node| ())
}

fn mesh_rendezvous_reach_contact_impl(
    handle: *mut OmniAppHandle,
    account_id: *const c_char,
) -> OmniResult<()> {
    let account_id = parse_utf8_arg(account_id, "rendezvous account id")?;
    require_handle(handle)?
        .inner
        .rendezvous_reach_contact_if_no_link(&omni_identity::AccountId(account_id))
        // The bool (dial started vs nothing-to-do) is informational only; the shell fires
        // this fire-and-forget, so collapse it to the null/error-envelope success shape.
        .map(|_started| ())
}

fn network_node_list_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result = (|| -> OmniResult<Vec<omni_app::NetworkNodeView>> {
        require_handle(handle)?.inner.list_network_nodes()
    })();
    to_result_payload(result)
}

fn network_node_add_impl(
    handle: *mut OmniAppHandle,
    endpoint: *const c_char,
    label: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<i64> {
        let endpoint = parse_utf8_arg(endpoint, "network node endpoint")?;
        let label = parse_utf8_arg(label, "network node label")?;
        require_handle(handle)?.inner.add_network_node(&endpoint, &label)
    })();
    to_result_payload(result)
}

fn network_node_remove_impl(handle: *mut OmniAppHandle, id: i64) -> OmniResult<()> {
    require_handle(handle)?.inner.remove_network_node(id)
}

fn rendezvous_provider_list_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    let result = (|| -> OmniResult<Vec<omni_app::RendezvousProviderView>> {
        require_handle(handle)?.inner.list_rendezvous_providers()
    })();
    to_result_payload(result)
}

fn rendezvous_provider_add_impl(handle: *mut OmniAppHandle, url: *const c_char) -> OmniResult<()> {
    let url = parse_utf8_arg(url, "rendezvous provider url")?;
    require_handle(handle)?.inner.add_rendezvous_provider(&url)
}

fn rendezvous_provider_remove_impl(handle: *mut OmniAppHandle, id: i64) -> OmniResult<()> {
    require_handle(handle)?.inner.remove_rendezvous_provider(id)
}

fn rendezvous_provider_set_mode_impl(handle: *mut OmniAppHandle, id: i64, mode: *const c_char) -> OmniResult<()> {
    let mode = parse_utf8_arg(mode, "courier mode")?;
    require_handle(handle)?.inner.set_rendezvous_provider_mode(id, mode.trim())
}

fn publish_plans_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.publish_plans()))
}

#[derive(serde::Deserialize)]
struct SetPublishPlanRequest {
    publication: omni_app::Publication,
    plan: omni_app::PublishPlanV1,
}

fn publish_plan_set_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetPublishPlanRequest = parse_json_request(request_json, "omni_publish_plan_set request")?;
        require_handle(handle)?.inner.set_publish_plan(request.publication, request.plan)
    })();
    to_result_payload(result)
}

// DHT entry-node (server) role: run this node as a reachable in-segment bootstrap for others.
fn dht_entry_node_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| {
        if handle.inner.dht_entry_node_enabled() {
            "on"
        } else {
            "off"
        }
    }))
}

fn dht_entry_node_set_impl(handle: *mut OmniAppHandle, mode: *const c_char) -> OmniResult<()> {
    let mode = parse_utf8_arg(mode, "dht entry-node mode")?;
    let enabled = matches!(mode.trim(), "on" | "true");
    require_handle(handle)?.inner.set_dht_entry_node_enabled(enabled)
}

/// The node's public DHT entry endpoint (`ip:port`) if running as a UPnP-mapped entry node; empty
/// string if off / not up / no public mapping.
fn dht_entry_node_endpoint_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle)
            .map(|handle| handle.inner.dht_entry_node_endpoint().unwrap_or_default()),
    )
}

/// Live BT-DHT rendezvous status (enabled / running / node_count / entry_endpoint) as a JSON payload —
/// the settings UI polls this for its live "connected to the swarm — N nodes" indicator.
fn dht_rendezvous_status_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.dht_rendezvous_status()))
}

/// Re-create this profile's rendezvous runtime so a DHT-rendezvous / entry-node toggle applies NOW
/// (respawn the helper) instead of on the next dial / profile restart.
fn rendezvous_respawn_impl(handle: *mut OmniAppHandle) -> OmniResult<()> {
    require_handle(handle)?.inner.respawn_rendezvous_runtime()
}

// This node's local transport (QUIC) `ip:port` — the GUI takes the PORT and pairs it with LAN IPs
// for a local-segment "add me as an entry node" address. Errors (as a payload) if the mesh is down.
fn local_transport_endpoint_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.local_transport_endpoint()),
    )
}

// Custom DHT bootstrap nodes (isolated-segment support). Get = newline-joined addresses (reuses the
// String serializer — no new list type); add/remove take a raw `ip:port`.
fn dht_bootstrap_list_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).map(|handle| handle.inner.dht_bootstrap_nodes().join("\n")),
    )
}

fn dht_bootstrap_add_impl(handle: *mut OmniAppHandle, addr: *const c_char) -> OmniResult<()> {
    let addr = parse_utf8_arg(addr, "dht bootstrap node")?;
    require_handle(handle)?.inner.add_dht_bootstrap_node(&addr)
}

fn dht_bootstrap_remove_impl(handle: *mut OmniAppHandle, addr: *const c_char) -> OmniResult<()> {
    let addr = parse_utf8_arg(addr, "dht bootstrap node")?;
    require_handle(handle)?.inner.remove_dht_bootstrap_node(&addr)
}

// Shareable `om3dhtboot:` bundle (step A): export the configured list as a copy-pasteable token for
// out-of-band community distribution; import merges a pasted token (returns the count of new nodes).
fn dht_bootstrap_bundle_export_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.export_dht_bootstrap_bundle()),
    )
}

fn dht_bootstrap_bundle_import_impl(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<i64> {
        let token = parse_utf8_arg(token, "dht bootstrap bundle token")?;
        require_handle(handle)?
            .inner
            .import_dht_bootstrap_bundle(&token)
            .map(|count| count as i64)
    })();
    to_result_payload(result)
}

fn network_relay_set_shared_impl(
    handle: *mut OmniAppHandle,
    id: i64,
    shared: bool,
) -> OmniResult<()> {
    require_handle(handle)?.inner.set_relay_node_shared(id, shared)
}

fn network_node_relay_bundle_import_impl(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<i64> {
        let token = parse_utf8_arg(token, "relay bundle token")?;
        // The count of relays imported (0 = the bundle was older/equal and ignored — not an error,
        // so the shell can distinguish "applied N relays" from "already up to date").
        require_handle(handle)?
            .inner
            .import_relay_bundle(&token)
            .map(|count| count as i64)
    })();
    to_result_payload(result)
}
