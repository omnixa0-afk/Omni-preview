// The profile (name, avatar) shared by the account's devices (docs/decisions/multi-device-equal-nodes.md,
// «Синхронизация профиля»). Our devices get the same two frames our contacts do — the descriptor and
// the avatar — and the per-session sync registry sends each change once per session. A device of ours
// takes a change that is newer than its own (`Storage::adopt_public_profile_from_own_device`); the
// change keeps the moment it was made on the device that made it.

/// Hand each reachable device of ours the profile, when it has not had this version over this
/// session yet. Caller holds the session lock.
fn sync_profile_with_own_devices(storage: &Storage, transport: &TransportManager) {
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    for (device_id, dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            continue;
        }
        // No session yet (two devices linked that never talked): this starts one, the profile goes
        // on a later pass.
        let Ok(Some(session)) = established_session_with_own_device(
            storage,
            transport,
            &own_account_id,
            &device_id,
            &dialog_key,
            "profile sync waits",
        ) else {
            continue;
        };
        if let Err(error) = send_current_public_profile_sync_for_session(storage, transport, &session, Some(&dialog_key)) {
            omni_app_trace_log("own_devices/profile", &format!("device={} not sent: {error}", device_id.0));
        }
    }
}

/// A profile frame from another device of ours: take it into our own profile. `None` — not from a
/// device of ours (then it is about a contact); `Some(changed)` — ours, and whether it changed our
/// profile.
fn adopt_profile_from_own_device(
    storage: &Storage,
    session: &omni_crypto::SessionRecord,
    display_name: Option<&str>,
    avatar_hash_hex: Option<&str>,
    avatar_mime_type: Option<&str>,
    avatar_bytes: Option<&[u8]>,
    updated_at_unix_ms: i64,
) -> OmniResult<Option<bool>> {
    let from = &session.session_address;
    if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        return Ok(None);
    }
    let changed = storage.adopt_public_profile_from_own_device(
        display_name,
        avatar_hash_hex,
        avatar_mime_type,
        avatar_bytes,
        updated_at_unix_ms,
    )?;
    omni_app_trace_log(
        "own_devices/profile_in",
        &format!(
            "from={} {} changed={changed} at={updated_at_unix_ms}",
            from.remote_device_id.0,
            if avatar_bytes.is_some() { "avatar" } else { "descriptor" },
        ),
    );
    Ok(Some(changed))
}

/// A change taken from another device of ours goes on to our contacts and our groups, as a change
/// made here does (`update_local_public_profile`). Nothing passed it on before: it reached a contact
/// only if the queue tick's "active" dialog happened to be that contact's — a pre-mesh connection
/// the mesh never fills, so in the app a name changed on one device never reached contacts the other
/// device talks to (the test `a_profile_change_from_our_other_device_reaches_our_contacts` passed
/// alone and failed in the full run, by which dialog was active). Caller holds the session lock.
fn pass_on_profile_from_own_device(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
) {
    if let Ok(Some(transport)) = clone_active_transport(transport_handle) {
        if let Err(error) = send_current_public_profile_sync_to_approved_contacts(storage, &transport) {
            omni_app_trace_log("own_devices/profile_on", &format!("contacts not reached: {error}"));
        }
    }
    crate::app_group_member_profiles::broadcast_own_profile(storage);
    omni_app_trace_log("own_devices/profile_on", "passed on to contacts and groups");
}
