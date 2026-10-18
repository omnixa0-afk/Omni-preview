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

/// A profile frame from another device of ours: take it into our own profile. Returns whether the
/// frame was from one of our devices (then it is not about a contact, whatever it changed).
fn adopt_profile_from_own_device(
    storage: &Storage,
    session: &omni_crypto::SessionRecord,
    display_name: Option<&str>,
    avatar_hash_hex: Option<&str>,
    avatar_mime_type: Option<&str>,
    avatar_bytes: Option<&[u8]>,
    updated_at_unix_ms: i64,
) -> OmniResult<bool> {
    let from = &session.session_address;
    if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        return Ok(false);
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
    Ok(true)
}
