// The courier sets the devices we talk to use (docs/decisions/courier-metadata-and-quota.md, step 6).
//
// Each device tells the devices it talks to — every device of every approved contact, every other
// device of ours — where it posts its beacons and where it looks for others'. With that, a pair's
// beacon goes where the other device looks, and we look where it posts: a contact who set up its own
// courier on a VPS is met there, and one who turned a courier off is not posted to through it.
//
// Told once per live link (the caller keeps what it told, per dialog, and forgets a dialog whose link
// dropped) and again whenever our set changes.

/// Bumped whenever a profile's courier set changes (any profile: a spare pass elsewhere is harmless).
static COURIER_SET_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn note_courier_set_changed() {
    COURIER_SET_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// What the courier pass depends on besides storage: which links are live, and the set generation.
fn courier_pass_key(transport: &TransportManager) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut peers = transport.live_peer_ids();
    peers.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    peers.hash(&mut hasher);
    COURIER_SET_GENERATION.load(std::sync::atomic::Ordering::Relaxed).hash(&mut hasher);
    hasher.finish()
}

/// Our courier set as we tell it: where we post, and every courier we look at.
fn our_courier_set(storage: &Storage) -> omni_types::CourierSetV1 {
    let mut publish = Vec::new();
    let mut read = Vec::new();
    for entry in courier_entries(storage) {
        match omni_rendezvous::parse_courier_entry(&entry) {
            // Another device's courier is not ours to tell about.
            (_, omni_rendezvous::CourierUse::PairOnly) => {}
            (url, omni_rendezvous::CourierUse::ReadOnly) => read.push(url.to_string()),
            (url, omni_rendezvous::CourierUse::PublishRead) => {
                publish.push(url.to_string());
                read.push(url.to_string());
            }
        }
    }
    omni_types::CourierSetV1 { publish, read }
}

fn courier_set_fingerprint(set: &omni_types::CourierSetV1) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    set.publish.hash(&mut hasher);
    set.read.hash(&mut hasher);
    hasher.finish()
}

/// Tell every device we have a live link with our courier set, unless it already heard this one on
/// this link. `told`: dialog → fingerprint of the set told there. `true` when a device on a live link
/// is still owed it (its session was not ready yet) — the caller tries again soon.
fn tell_devices_our_couriers(
    storage: &Storage,
    transport: &TransportManager,
    told: &mut std::collections::HashMap<String, u64>,
) -> bool {
    let mut owed = false;
    let set = our_courier_set(storage);
    let fingerprint = courier_set_fingerprint(&set);
    let mut dialogs: Vec<(AccountId, String)> = Vec::new();
    if let Ok(Some(account)) = storage.get_local_account() {
        for (_, dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
            dialogs.push((account.descriptor.account_id.clone(), dialog_key));
        }
    }
    for contact in storage.list_contacts().unwrap_or_default() {
        if contact.relationship_state != ContactRelationshipState::Approved {
            continue;
        }
        let Some(account_id) = contact.remote_account_id.clone() else {
            continue;
        };
        for device_dialog in storage.list_contact_device_dialogs(&contact.contact_id).unwrap_or_default() {
            dialogs.push((account_id.clone(), device_dialog.dialog_key));
        }
    }
    for (account_id, dialog_key) in dialogs {
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            told.remove(&dialog_key);
            continue;
        }
        if told.get(&dialog_key) == Some(&fingerprint) {
            continue;
        }
        match send_account_event_to_dialog(
            storage,
            transport,
            &account_id,
            &dialog_key,
            omni_types::LogicalEventEnvelopeV1::courier_set(set.clone()),
            SecurityPayloadKind::CourierSet,
            "our courier set waits",
        ) {
            Ok(()) => {
                told.insert(dialog_key, fingerprint);
            }
            Err(error) => {
                owed = true;
                omni_app_trace_log("couriers/tell", &format!("dialog={dialog_key} not told: {error}"));
            }
        }
    }
    owed
}

/// A device we talk to told us where it posts and looks. Taken only from a device of ours or of an
/// approved contact, and only couriers we could use at all.
fn handle_incoming_courier_set(
    storage: &Storage,
    set: &omni_types::CourierSetV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    let from_own_device = storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)?;
    let from_approved_contact = storage
        .resolve_contact_for_dialog(&receive_context.dialog_key)?
        .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved);
    if !from_own_device && !from_approved_contact {
        omni_app_trace_log(
            "couriers/heard",
            &format!("dropped: {} is neither ours nor an approved contact", from.remote_device_id.0),
        );
        return Ok(());
    }
    let usable = |urls: &[String]| -> Vec<String> {
        urls.iter()
            .map(|url| url.trim().to_string())
            .filter(|url| is_valid_provider_url(url))
            .collect()
    };
    let peer = omni_storage::PeerCourierSet { publish: usable(&set.publish), read: usable(&set.read) };
    let urls_before = storage.list_peer_courier_urls().unwrap_or_default();
    if storage.record_peer_courier_set(&from.remote_account_id, &from.remote_device_id, &peer, current_unix_ms())? {
        omni_app_trace_log(
            "couriers/heard",
            &format!(
                "device={} publish={} read={}",
                from.remote_device_id.0,
                peer.publish.len(),
                peer.read.len()
            ),
        );
        on_peer_courier_urls_changed(storage, &urls_before);
    }
    Ok(())
}

/// A device we talk to uses a courier none did before (or none does any more): the profile's
/// rendezvous runtime restarts, so the helper — which knows its couriers from the start — has it.
fn on_peer_courier_urls_changed(storage: &Storage, before: &[String]) {
    if storage.list_peer_courier_urls().unwrap_or_default() != before {
        clear_rendezvous_runtime_for_profile(&storage.db_path().to_string_lossy());
    }
}
