// C ABI exports for the mesh overlay domain. Each returns a `*mut c_char`:
// null on success, an error-envelope C string (freed via `omni_string_free`)
// on failure — the same convention as every other omni-ffi export. The impl
// bodies live in exports_impl_mesh.rs.
//
// See docs/decisions/mesh-transport-integration-seam.md.

/// Start the mesh overlay for the active profile (idempotent). Call after
/// sign-in so the node announces + accepts and can be discovered/reached.
#[no_mangle]
pub extern "C" fn omni_mesh_start(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_start_impl(handle))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Stop the mesh overlay (call on sign-out).
#[no_mangle]
pub extern "C" fn omni_mesh_stop(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_stop_impl(handle))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Dial the active call's peer over the mesh and bind it to that dialog, so
/// realtime media routes over the verified mesh Link. Both peers call this on
/// call-connect. Requires the mesh to be started and the peer discovered.
#[no_mangle]
pub extern "C" fn omni_mesh_connect_active_call(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_connect_active_call_impl(handle))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Generate a copy-pasteable mesh pairing invite (`om3mesh:` token) for the
/// active profile. Returns a JSON payload — the token string on success, or an
/// error envelope on failure (parsed + freed by the C# bridge) — the same
/// JSON-payload convention the contact/identity exports use.
#[no_mangle]
pub extern "C" fn omni_mesh_generate_pairing_invite(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_generate_pairing_invite_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The honest connection-state DTO (this node's connectivity + per-contact link-state + failure
/// reasons + the orthogonal `verified` flag) as a JSON payload, parsed + freed by the C# bridge.
/// The structured replacement for the old prose reachability summary (Rust = truth, C# = render).
/// See docs/decisions/omni-network-model.md (honest connection-state surface).
#[no_mangle]
pub extern "C" fn omni_mesh_connection_state(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_connection_state_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Read the active profile's connection-privacy mode. Returns a JSON payload (the db-value
/// string `auto` | `direct-only`, or an error envelope), parsed + freed by the C# bridge —
/// matching the `omni_mesh_generate_pairing_invite` convention. Auto = full ladder incl. serverless
/// rendezvous; Direct-only = LAN + router port-mapping + manual exchange only (zero third parties).
/// See docs/decisions/connection-privacy-modes.md.
#[no_mangle]
pub extern "C" fn omni_mesh_connection_privacy_mode_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        mesh_connection_privacy_mode_get_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Set the active profile's connection-privacy mode (`auto` | `direct-only`). Returns null on
/// success, an error-envelope C string on failure. Takes effect on the next rendezvous sweep.
#[no_mangle]
pub extern "C" fn omni_mesh_connection_privacy_mode_set(
    handle: *mut OmniAppHandle,
    mode: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        mesh_connection_privacy_mode_set_impl(handle, mode)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Get the active profile's carrier policy (R1a connection-quality ladder, axis A) — the
/// payload is `tcp` | `auto` | `quic`. Returns an OK-envelope C string.
#[no_mangle]
pub extern "C" fn omni_mesh_carrier_policy_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_carrier_policy_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Set the active profile's carrier policy (`tcp` | `auto` | `quic`). Returns
/// null on success, an error-envelope C string on failure. Takes effect on the next mesh start.
#[no_mangle]
pub extern "C" fn omni_mesh_carrier_policy_set(
    handle: *mut OmniAppHandle,
    policy: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        mesh_carrier_policy_set_impl(handle, policy)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Get the active profile's fixed direct-TCP front-door port as a decimal string (`"0"` = ephemeral /
/// UPnP). When non-zero, the user is expected to port-forward this TCP port to their machine for a
/// reliable relay-less inbound path (a mobile peer dials OUT to it, surviving UDP drops).
#[no_mangle]
pub extern "C" fn omni_mesh_direct_tcp_port_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_direct_tcp_port_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Set the active profile's fixed direct-TCP front-door port (`"0"`/blank clears it back to
/// ephemeral). Returns null on success, an error-envelope C string on failure. Takes effect on the
/// next mesh start.
#[no_mangle]
pub extern "C" fn omni_mesh_direct_tcp_port_set(
    handle: *mut OmniAppHandle,
    port: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_direct_tcp_port_set_impl(handle, port))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Export a "код связи" reach token (`om3reach:…`) for the active node — its current reachable
/// endpoints, so a friend can dial us directly without any third-party rendezvous (the manual /
/// Direct-only path). Returns a JSON payload (the token or an error envelope), parsed + freed by the
/// C# bridge. See docs/decisions/connection-privacy-modes.md.
#[no_mangle]
pub extern "C" fn omni_mesh_generate_reach_token(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_generate_reach_token_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Import a friend's "код связи" reach token (`om3reach:…`) and dial them at the carried endpoints
/// (manual reach to an already-paired contact). Returns null on success, an error-envelope C string
/// on failure.
#[no_mangle]
pub extern "C" fn omni_mesh_dial_reach_token(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| mesh_dial_reach_token_impl(handle, token))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Import a friend's mesh pairing invite (`om3mesh:` token): registers their
/// mesh node as a pairing target so the auto-connect worker dials and pairs over
/// the mesh (no legacy connection). Returns null on success, an error-envelope
/// C string on failure.
#[no_mangle]
pub extern "C" fn omni_mesh_register_pairing_invite(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        mesh_register_pairing_invite_impl(handle, token)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Kick off an on-demand serverless-rendezvous dial to an approved contact when the mesh has
/// NO link to it yet. The shell fires this on user intent to reach a contact (open a chat /
/// start a call) so cross-network reach is immediate instead of waiting for the periodic
/// rendezvous sweep. Idempotent + best-effort: a no-op when already linked, the mesh isn't
/// started, there's no rendezvous pair-secret yet, or the profile is Direct-only; the slow
/// dial runs on a background thread in the core, so this returns promptly. Returns null on
/// success, an error-envelope C string on failure.
/// See docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
#[no_mangle]
pub extern "C" fn omni_mesh_rendezvous_reach_contact(
    handle: *mut OmniAppHandle,
    account_id: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        mesh_rendezvous_reach_contact_impl(handle, account_id)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// The active profile's network entry-node registry (the seeded base node + user-added relays /
/// bootstrap) as a JSON payload — an array of `{id, endpoint, node_address_hex, label, source,
/// removable}`, parsed + freed by the C# bridge. The list the «Узлы сети / Релеи» screen renders; an
/// empty list is valid (LAN + invite still connect). See docs/notes/roadmap-CONTINUE.md.
#[no_mangle]
pub extern "C" fn omni_network_node_list(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| network_node_list_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Add a relay / bootstrap entry node by `ip:port` with a user label. Returns a JSON payload (the new
/// node id on success, or an error envelope — invalid endpoint / duplicate), parsed + freed by the C#
/// bridge. The node is a HINT — the ed25519 challenge on every dial stays the sole authority.
#[no_mangle]
pub extern "C" fn omni_network_node_add(
    handle: *mut OmniAppHandle,
    endpoint: *const c_char,
    label: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        network_node_add_impl(handle, endpoint, label)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Remove ANY entry node by id — including the seeded base node (the user may delete it; the empty list
/// still connects via LAN + invite). Returns null on success, an error-envelope C string on failure.
#[no_mangle]
pub extern "C" fn omni_network_node_remove(handle: *mut OmniAppHandle, id: i64) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| network_node_remove_impl(handle, id))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// The active profile's rendezvous courier providers (seeded defaults + user/invite-added) as a JSON
/// payload — an array of `{id, url, source, removable}`, parsed + freed by the C# bridge. The list the
/// «Провайдеры рандеву» screen renders; these are the pub/sub brokers used to MEET a peer (distinct from
/// the relay list). See docs/decisions/global-identity-discovery.md.
#[no_mangle]
pub extern "C" fn omni_rendezvous_provider_list(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| rendezvous_provider_list_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Add a rendezvous provider by URL (`http(s)://` ntfy / `ws(s)://` Nostr relay / `dht://`). Returns null
/// on success, an error-envelope C string on failure (invalid scheme / duplicate). Applies at once (the
/// rendezvous runtime restarts with the new set). The courier card's add field calls this.
#[no_mangle]
pub extern "C" fn omni_rendezvous_provider_add(
    handle: *mut OmniAppHandle,
    url: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| rendezvous_provider_add_impl(handle, url))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Remove a courier the user or an invite added (a default one is turned off instead, see
/// `omni_rendezvous_provider_set_mode`). Returns null on success, an error-envelope C string on failure.
#[no_mangle]
pub extern "C" fn omni_rendezvous_provider_remove(
    handle: *mut OmniAppHandle,
    id: i64,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| rendezvous_provider_remove_impl(handle, id))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Set how a courier is used: `mode` = `"publish_read"` (post our beacons and look for contacts),
/// `"read"` (only look), `"off"`. Mainline is turned on and off this way too (its row,
/// `dht://mainline`). At least one courier has to stay `publish_read`. Applies at once. Returns null on
/// success, an error-envelope C string on failure. See docs/decisions/courier-metadata-and-quota.md.
#[no_mangle]
pub extern "C" fn omni_rendezvous_provider_set_mode(
    handle: *mut OmniAppHandle,
    id: i64,
    mode: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| rendezvous_provider_set_mode_impl(handle, id, mode))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Every publication's plan (where and how often it goes) as a JSON array of `PublishPlanViewV1`:
/// the beacons for contacts, Omni-ID, name, relay directory (docs/decisions/public-relay-directory.md §10).
#[no_mangle]
pub extern "C" fn omni_publish_plans_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| publish_plans_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Set one publication's plan: `{"publication": "contacts", "plan": {"auto": false, "steps": [...]}}`
/// (`auto: true` — back to its preset). Applies at once; returns the plan as stored (`PublishPlanViewV1`).
#[no_mangle]
pub extern "C" fn omni_publish_plan_set(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| publish_plan_set_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The active profile's DHT entry-node opt-in as a JSON string payload (`"on"` | `"off"`). When on, the
/// node runs its DHT engine as a reachable in-segment bootstrap for others (server_mode + fixed port +
/// self-UPnP). See docs/decisions/dht-bootstrap-mesh-propagation.md.
#[no_mangle]
pub extern "C" fn omni_dht_entry_node_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_entry_node_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Opt this profile in/out of the DHT entry-node role. `mode` = `"on"` | `"off"`. Takes effect on the
/// next per-profile rendezvous spawn. Returns null on success, an error-envelope C string on failure.
#[no_mangle]
pub extern "C" fn omni_dht_entry_node_set(
    handle: *mut OmniAppHandle,
    mode: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_entry_node_set_impl(handle, mode))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// The node's public DHT entry endpoint (`ip:port`) when running as a UPnP-mapped entry node, as a JSON
/// string payload; empty string if off / not up / no public mapping (CGNAT / no IGD gateway). The shell
/// shows it so the user can share their entry-point address.
#[no_mangle]
pub extern "C" fn omni_dht_entry_node_endpoint_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_entry_node_endpoint_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Live BT-DHT rendezvous status as a JSON payload (`{enabled, entry_node_enabled, running, node_count,
/// entry_endpoint}`) — the settings UI polls this for its live swarm-connection indicator.
#[no_mangle]
pub extern "C" fn omni_dht_rendezvous_status_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_rendezvous_status_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Re-create this profile's rendezvous runtime so a DHT-rendezvous / entry-node toggle takes effect
/// immediately (respawn the `omni-dht-helper` now). Returns null on success, an error-envelope C string
/// on failure. Spawns a child process — call it off the UI thread.
#[no_mangle]
pub extern "C" fn omni_rendezvous_respawn(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| rendezvous_respawn_impl(handle))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// This node's local transport (QUIC) `ip:port` as a JSON string payload — the GUI pairs the port with
/// LAN IPs for a local-segment entry-node address. Error payload if the mesh isn't started.
#[no_mangle]
pub extern "C" fn omni_local_transport_endpoint_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| local_transport_endpoint_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The profile's custom DHT bootstrap nodes as a newline-joined JSON string (custom DHT entry
/// points for an isolated network segment; empty = the global Mainline routers).
#[no_mangle]
pub extern "C" fn omni_dht_bootstrap_list_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_bootstrap_list_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Add a custom DHT bootstrap node (`ip:port`). Null on success, error-envelope C string on failure.
#[no_mangle]
pub extern "C" fn omni_dht_bootstrap_add(
    handle: *mut OmniAppHandle,
    addr: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_bootstrap_add_impl(handle, addr))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Remove a custom DHT bootstrap node by `ip:port`. Null on success, error-envelope C string on failure.
#[no_mangle]
pub extern "C" fn omni_dht_bootstrap_remove(
    handle: *mut OmniAppHandle,
    addr: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_bootstrap_remove_impl(handle, addr))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Export the configured DHT bootstrap nodes as a shareable `om3dhtboot:` bundle (step A) — a
/// copy-pasteable token for out-of-band community distribution. Result-payload C string (the token on
/// success, or an error envelope, e.g. when the list is empty).
#[no_mangle]
pub extern "C" fn omni_dht_bootstrap_bundle_export(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_bootstrap_bundle_export_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Import a shareable `om3dhtboot:` bundle (step A): merge its `ip:port` nodes into the configured list.
/// Result-payload C string carrying the count of NEW nodes added (or an error envelope on a malformed
/// token). Contact-independent — the out-of-band seed channel for an isolated segment.
#[no_mangle]
pub extern "C" fn omni_dht_bootstrap_bundle_import(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| dht_bootstrap_bundle_import_impl(handle, token))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// R1c inc.3b: flag (`shared` != 0) or unflag entry node `id` as part of MY OWN relay bundle — the set
/// auto-propagated to approved friends over the per-pair beacon. A real change bumps my bundle version
/// so the rotation propagates. Returns null on success, an error-envelope C string on failure. The relay
/// screen's "поделиться с друзьями" checkbox calls this. See docs/decisions/r1c-rotatable-white-relay.md.
#[no_mangle]
pub extern "C" fn omni_network_relay_set_shared(
    handle: *mut OmniAppHandle,
    id: i64,
    shared: i32,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        network_relay_set_shared_impl(handle, id, shared != 0)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Import a SIGNED `om3relay:` relay-bundle token (R1c): verify the issuer signature, then REPLACE that
/// issuer's prior relay set with the bundle (a strict-newer rotation guard ignores a replay/older
/// version). Returns a JSON payload — the count of relays imported on success (0 = the bundle was not
/// newer, ignored), or an error envelope (not a relay bundle / bad signature / malformed) — parsed +
/// freed by the C# bridge. The «add relay» screen calls this when the pasted text is an `om3relay:`
/// bundle (vs a plain `ip:port` → `omni_network_node_add`). See
/// docs/decisions/r1c-rotatable-white-relay.md.
#[no_mangle]
pub extern "C" fn omni_network_relay_bundle_import(
    handle: *mut OmniAppHandle,
    token: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        network_node_relay_bundle_import_impl(handle, token)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
