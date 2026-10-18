/// This node's `SessionHello`, sent as the first frame on a new mesh link. It carries no
/// listener endpoint any more: the pre-mesh listener is gone, the mesh knows how to reach us.
pub(crate) fn build_local_session_hello_request(
    shared: &Arc<Mutex<SharedState>>,
    local_metadata: &LocalMetadata,
) -> TransportRequestEnvelopeV1 {
    let state = lock_shared_state(shared);
    let local_account_id = state.local_account_id.clone();
    let local_prekey_bundle_json = state.local_prekey_bundle_json.clone();
    drop(state);

    TransportRequestEnvelopeV1::session_hello(
        local_metadata.instance_id.clone(),
        local_metadata.transport_mode,
        local_metadata.device_name.clone(),
        local_account_id,
        local_prekey_bundle_json,
    )
}

pub(crate) fn activate_connection_from_session_hello(
    shared: &Arc<Mutex<SharedState>>,
    peer_id: &str,
    hello: &SessionHelloV1,
    local_instance_id: &str,
    initiated_locally: bool,
) -> OmniResult<ReceivedChatContext> {
    let local_transport_mode = {
        let state = lock_shared_state(shared);
        state.transport_mode
    };
    if hello.transport_mode != local_transport_mode {
        return Err(OmniError::Transport(format!(
            "transport mode mismatch: local={} remote={}",
            local_transport_mode.as_db_value(),
            hello.transport_mode.as_db_value()
        )));
    }

    let remote_prekey_bundle = validate_remote_prekey_bundle(hello.prekey_bundle_json.as_deref())?;
    let remote_account_id = hello
        .account_id
        .clone()
        .or_else(|| {
            remote_prekey_bundle
                .as_ref()
                .map(|bundle| bundle.account.account_id.clone())
        });

    let dialog_key = dialog_key_from_instance_id(&hello.instance_id);
    let connection = ConnectionState {
        peer_id: peer_id.to_string(),
        dialog_key: dialog_key.clone(),
        remote_instance_id: hello.instance_id.clone(),
        _remote_transport_mode: hello.transport_mode,
        remote_device_name: hello.device_name.clone(),
        remote_account_id,
        remote_prekey_bundle: remote_prekey_bundle.clone(),
        initiated_locally,
    };

    {
        let mut state = lock_shared_state(shared);
        state.last_error = None;
        let (should_promote_active, duplicate_observed) = match state.connection.as_ref() {
            None => (true, false),
            Some(active) if duplicate_connection_observed(
                active,
                peer_id,
                hello.instance_id.as_str(),
                initiated_locally,
            ) => {
                (duplicate_connection_prefers_candidate(
                    local_instance_id,
                    hello.instance_id.as_str(),
                    initiated_locally,
                    active.initiated_locally,
                ), true)
            }
            Some(active) if active.remote_instance_id == hello.instance_id => (true, false),
            Some(active) if active.peer_id == peer_id => (true, false),
            Some(_) => (false, false),
        };
        if duplicate_observed {
            state.duplicate_resolved = true;
        }
        if should_promote_active {
            state.verified_connections.retain(|existing_peer_id, existing| {
                existing_peer_id == peer_id || existing.remote_instance_id != hello.instance_id
            });
        }
        state
            .verified_connections
            .insert(peer_id.to_string(), connection.clone());
        if should_promote_active {
            state.connection = Some(connection);
        }
    }

    Ok(ReceivedChatContext {
        peer_id: peer_id.to_string(),
        dialog_key,
        remote_instance_id: hello.instance_id.clone(),
        remote_prekey_bundle,
        initiated_locally,
    })
}

fn duplicate_connection_observed(
    active: &ConnectionState,
    peer_id: &str,
    remote_instance_id: &str,
    initiated_locally: bool,
) -> bool {
    let same_remote = active.peer_id == peer_id || active.remote_instance_id == remote_instance_id;
    same_remote && active.initiated_locally != initiated_locally
}

fn duplicate_connection_prefers_candidate(
    local_instance_id: &str,
    remote_instance_id: &str,
    candidate_initiated_locally: bool,
    active_initiated_locally: bool,
) -> bool {
    if candidate_initiated_locally == active_initiated_locally {
        return false;
    }

    let local_instance_id = local_instance_id.trim();
    let remote_instance_id = remote_instance_id.trim();
    let locally_initiated_connection_wins = local_instance_id <= remote_instance_id;
    candidate_initiated_locally == locally_initiated_connection_wins
}
