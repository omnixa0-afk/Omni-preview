// Contacts shared by the account's devices (docs/decisions/multi-device-equal-nodes.md,
// «Синхронизация контактов»). The records and their merge live in storage
// (`storage_contact_sync.rs`); this file moves them between our devices and makes the contacts table
// follow what a merge decided.
//
// Each device hands each of our other devices its records in the order it changed them and waits for
// an acknowledgement before the next batch — whatever the other device missed, for however long, it
// gets on the next live link. Owner decisions (2026-10-09): approval, block and removal carry over to
// every device, removal with the chat; "verified" stays each device's own.

use omni_storage::{ContactSyncRecord, ContactSyncState};

/// Contacts per frame: an entry carries the certificates and prekeys of the contact's devices.
const CONTACT_SYNC_BATCH: u64 = 8;

/// A batch that was not acknowledged within this time goes out again.
const CONTACT_SYNC_RESEND_AFTER_MS: i64 = 30_000;

/// One record on the wire. An approved contact carries its devices' bundles — without the one-time
/// prekey, which the contact handed to us and may have used up — so a device of ours that never met
/// the contact can reach it.
fn contact_sync_entry(storage: &Storage, record: &ContactSyncRecord) -> omni_types::ContactSyncEntryV1 {
    let bundles_json = if record.state == ContactSyncState::Approved {
        storage
            .list_remote_prekey_bundles_for_account(&record.account_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|bundle| bundle.device_certificate_chain.is_some())
            .filter_map(|mut bundle| {
                bundle.one_time_prekey = None;
                serde_json::to_string(&bundle).ok()
            })
            .collect()
    } else {
        Vec::new()
    };
    omni_types::ContactSyncEntryV1 {
        account_id: record.account_id.0.clone(),
        state: record.state.as_str().to_string(),
        state_version: record.state_version,
        state_writer: record.state_writer.clone(),
        petname: record.petname.clone(),
        petname_version: record.petname_version,
        petname_writer: record.petname_writer.clone(),
        display_name: record.display_name.clone(),
        bundles_json,
    }
}

/// Note this device's contact changes and hand each reachable device of ours the next batch it has
/// not acknowledged. Caller holds the session lock.
fn sync_contacts_with_own_devices(storage: &Storage, transport: &TransportManager, now_unix_ms: i64) {
    let (Ok(Some(account)), Ok(Some(device))) = (storage.get_local_account(), storage.get_local_device()) else {
        return;
    };
    if let Err(error) = storage.refresh_contact_sync_records(&device.device_id, now_unix_ms) {
        omni_app_trace_log("own_devices/contacts", &format!("refresh failed: {error}"));
        return;
    }
    let own_account_id = account.descriptor.account_id;
    for (device_id, dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            continue;
        }
        let Ok(mark) = storage.contact_sync_mark(&device_id) else {
            continue;
        };
        let records = match storage.list_contact_sync_records_after(mark.acked_seq, CONTACT_SYNC_BATCH) {
            Ok(records) if !records.is_empty() => records,
            _ => continue,
        };
        let through_seq = records.last().map_or(0, |record| record.seq);
        // The last batch is still on its way and nothing changed since: wait for its acknowledgement.
        // A newer change goes at once, with the unacknowledged ones again (a merge takes a record
        // it already has as nothing).
        if through_seq <= mark.sent_through_seq
            && mark.sent_through_seq > mark.acked_seq
            && now_unix_ms - mark.sent_at_unix_ms < CONTACT_SYNC_RESEND_AFTER_MS
        {
            continue;
        }
        let entries = records.iter().map(|record| contact_sync_entry(storage, record)).collect();
        match send_account_event_to_dialog(
            storage,
            transport,
            &own_account_id,
            &dialog_key,
            omni_types::LogicalEventEnvelopeV1::contact_sync(omni_types::ContactSyncV1 { entries, through_seq }),
            SecurityPayloadKind::ContactSync,
            "contact sync waits",
        ) {
            Ok(()) => {
                let _ = storage.record_contact_sync_sent(&device_id, through_seq, now_unix_ms);
                omni_app_trace_log(
                    "own_devices/contacts",
                    &format!(
                        "device={} sent {} contact(s) through seq {through_seq}",
                        device_id.0,
                        records.len()
                    ),
                );
            }
            Err(error) => omni_app_trace_log(
                "own_devices/contacts",
                &format!("device={} not sent: {error}", device_id.0),
            ),
        }
    }
}

/// Records from another device of ours. Each entry is merged and the contacts table follows in the
/// same transaction; a contact removed here also loses its sessions and runtime state. The batch is
/// acknowledged only when every entry got through storage — otherwise the sender sends it again.
fn handle_incoming_contact_sync(
    storage: &Storage,
    transport: &TransportManager,
    sync: &omni_types::ContactSyncV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        omni_app_trace_log(
            "own_devices/contacts_in",
            &format!("dropped: {} is not one of our devices", from.remote_device_id.0),
        );
        return Ok(());
    }
    let now_unix_ms = current_unix_ms();
    let mut removed = Vec::new();
    for entry in &sync.entries {
        if entry.account_id == from.remote_account_id.0 {
            continue; // our own account is not a contact
        }
        match apply_contact_sync_entry(storage, entry, now_unix_ms) {
            Ok(Some(contact)) => removed.push(contact),
            Ok(None) => {}
            Err(error) => {
                omni_app_trace_log(
                    "own_devices/contacts_in",
                    &format!("account={} not applied, batch left unacknowledged: {error}", entry.account_id),
                );
                return Ok(());
            }
        }
    }
    forget_runtime_state_of_removed_contacts(storage, &removed);
    omni_app_trace_log(
        "own_devices/contacts_in",
        &format!(
            "from={} entries={} removed={} through_seq={}",
            from.remote_device_id.0,
            sync.entries.len(),
            removed.len(),
            sync.through_seq
        ),
    );
    send_account_event_to_dialog(
        storage,
        transport,
        &from.remote_account_id,
        &receive_context.dialog_key,
        omni_types::LogicalEventEnvelopeV1::contact_sync_ack(sync.through_seq),
        SecurityPayloadKind::ContactSyncAck,
        "a contact sync acknowledgement waits",
    )
}

/// A contact a merge removed leaves no runtime state behind (its sessions went with the merge).
fn forget_runtime_state_of_removed_contacts(storage: &Storage, removed: &[ContactSnapshot]) {
    for contact in removed {
        let _ = clear_verification_challenges_for_contact(storage, contact);
        let _ = clear_runtime_chat_session_for_contact(storage, contact);
    }
    if !removed.is_empty() {
        let _ = clear_runtime_contact_ephemera_for_active_profile(storage);
    }
}

fn handle_incoming_contact_sync_ack(
    storage: &Storage,
    ack: &omni_types::ContactSyncAckV1,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        return Ok(());
    }
    storage.record_contact_sync_acked(&from.remote_device_id, ack.through_seq)
}

/// Merge one entry and make the contacts table follow. Returns the contact when this removed it.
fn apply_contact_sync_entry(
    storage: &Storage,
    entry: &omni_types::ContactSyncEntryV1,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    let account_id = AccountId(entry.account_id.clone());
    let incoming = ContactSyncRecord {
        account_id: account_id.clone(),
        state: ContactSyncState::parse(&entry.state)?,
        state_version: entry.state_version,
        state_writer: entry.state_writer.clone(),
        petname: entry.petname.clone(),
        petname_version: entry.petname_version,
        petname_writer: entry.petname_writer.clone(),
        display_name: entry.display_name.clone(),
        seq: 0,
    };
    // The devices first: a contact that appears here is reachable from the moment it does.
    if incoming.state == ContactSyncState::Approved {
        record_synced_contact_bundles(storage, &account_id, &entry.bundles_json, now_unix_ms);
    }
    let removed = storage.with_immediate_transaction("apply_contact_sync_entry", |storage| {
        let Some(merge) = storage.merge_contact_sync_record(&incoming, now_unix_ms)? else {
            return Ok(None);
        };
        let record = storage
            .get_contact_sync_record(&account_id)?
            .ok_or_else(|| OmniError::Storage("merged contact record is missing".to_string()))?;
        let existing = storage.find_contact_by_remote_account_id(&account_id)?;
        let new_contact_input = || AddContactInput {
            petname: record.petname.clone(),
            remote_account_id: Some(account_id.clone()),
            public_display_name: record.display_name.clone().or_else(|| Some(account_id.0.clone())),
            remote_instance_id: None,
            primary_dialog_key: None,
            initial_observation: None,
        };
        match merge.state {
            Some(ContactSyncState::Removed) => {
                if let Some(contact) = existing {
                    let removed = storage.forget_contact(&contact.contact_id, now_unix_ms)?;
                    storage.delete_session_state_for_remote_account(&account_id)?;
                    return Ok(Some(removed));
                }
                return Ok(None);
            }
            Some(ContactSyncState::Approved) => match &existing {
                Some(contact) if contact.relationship_state != ContactRelationshipState::Approved => {
                    storage.approve_contact(&contact.contact_id, now_unix_ms)?;
                }
                Some(_) => {}
                None => {
                    storage.add_contact(&new_contact_input(), now_unix_ms)?;
                }
            },
            Some(ContactSyncState::Blocked) => match &existing {
                Some(contact) if contact.relationship_state != ContactRelationshipState::Declined => {
                    storage.decline_contact(&contact.contact_id, now_unix_ms)?;
                }
                Some(_) => {}
                None => {
                    let contact = storage.add_contact(&new_contact_input(), now_unix_ms)?;
                    storage.decline_contact(&contact.contact_id, now_unix_ms)?;
                }
            },
            None => {}
        }
        if merge.petname.is_some() && record.state != ContactSyncState::Removed {
            if let Some(contact) = storage.find_contact_by_remote_account_id(&account_id)? {
                if contact.petname != record.petname && !record.petname.trim().is_empty() {
                    storage.rename_contact(&contact.contact_id, &record.petname, now_unix_ms)?;
                }
            }
        }
        Ok(None)
    })?;
    Ok(removed)
}

/// The devices of a contact, as another device of ours knows them. Each bundle must vouch for itself
/// (its certificate chain against its keys) and belong to the contact's account; a device we already
/// know is left as we know it — what came from the contact directly is fresher.
fn record_synced_contact_bundles(
    storage: &Storage,
    account_id: &AccountId,
    bundles_json: &[String],
    now_unix_ms: i64,
) {
    for bundle_json in bundles_json {
        let bundle: omni_crypto::PrekeyBundle = match serde_json::from_str(bundle_json) {
            Ok(bundle) => bundle,
            Err(error) => {
                omni_app_trace_log("own_devices/contacts_in", &format!("bundle unreadable: {error}"));
                continue;
            }
        };
        if &bundle.account_id != account_id || bundle.device_certificate_chain.is_none() {
            omni_app_trace_log(
                "own_devices/contacts_in",
                &format!("bundle skipped: not a certified device of {}", account_id.0),
            );
            continue;
        }
        if let Err(error) = omni_crypto::verify_prekey_bundle_device_certificate(&bundle) {
            omni_app_trace_log("own_devices/contacts_in", &format!("bundle refused: {error}"));
            continue;
        }
        if storage
            .get_remote_prekey_bundle(account_id, &bundle.device_id)
            .ok()
            .flatten()
            .is_some()
        {
            continue;
        }
        if let Err(error) = storage.record_remote_prekey_bundle(&bundle, None, None, now_unix_ms) {
            omni_app_trace_log("own_devices/contacts_in", &format!("bundle not stored: {error}"));
        }
    }
}
