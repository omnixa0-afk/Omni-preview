// Taking a device back (docs/decisions/multi-device-equal-nodes.md, «Устройства», stage 3).
//
// A revocation is signed by the account's working key and checked by whoever applies it
// (`Storage::apply_device_revocation`), so it is valid whoever hands it over. This file moves it:
// to our other devices — the revoked one included, so it can learn — and to our contacts, so they
// stop sending to the device and stop listening to it. What the revoked device does when it learns
// is the next step.

/// One revocation, to one dialog, over the established session with the device behind it.
fn send_device_revocation_to_dialog(
    storage: &Storage,
    transport: &TransportManager,
    remote_account_id: &AccountId,
    dialog_key: &str,
    revocation_json: &str,
) -> OmniResult<()> {
    send_account_event_to_dialog(
        storage,
        transport,
        remote_account_id,
        dialog_key,
        omni_types::LogicalEventEnvelopeV1::device_revocation(revocation_json.to_string()),
        SecurityPayloadKind::DeviceRevocation,
        "a device revocation waits",
    )
}

/// Hand `revocation` to every device that has to act on it: all our other devices, the revoked one
/// too, and, when it is our own account's, every approved contact's devices. `except_dialog` is the
/// dialog it came from. Best effort per recipient: one that is away hears it from the reconcile
/// pass (our devices) or from the next link (contacts).
fn broadcast_device_revocation(
    storage: &Storage,
    transport: &TransportManager,
    revocation: &omni_crypto::DeviceRevocationV1,
    except_dialog: Option<&str>,
) {
    let Ok(revocation_json) = serde_json::to_string(revocation) else {
        return;
    };
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    let mut dialogs: Vec<(AccountId, String)> = Vec::new();
    let own_dialogs = storage
        .list_own_device_dialogs()
        .unwrap_or_default()
        .into_iter()
        .chain(storage.list_revoked_own_device_dialogs().unwrap_or_default());
    for (_device_id, dialog_key) in own_dialogs {
        dialogs.push((own_account_id.clone(), dialog_key));
    }
    if revocation.account_id == own_account_id {
        for contact in storage.list_contacts().unwrap_or_default() {
            if contact.relationship_state != ContactRelationshipState::Approved {
                continue;
            }
            let Some(contact_account_id) = contact.remote_account_id.clone() else {
                continue;
            };
            for device_dialog in storage
                .list_contact_device_dialogs(&contact.contact_id)
                .unwrap_or_default()
            {
                dialogs.push((contact_account_id.clone(), device_dialog.dialog_key));
            }
        }
    }
    for (remote_account_id, dialog_key) in dialogs {
        if except_dialog == Some(dialog_key.as_str()) {
            continue;
        }
        if let Err(error) = send_device_revocation_to_dialog(
            storage,
            transport,
            &remote_account_id,
            &dialog_key,
            &revocation_json,
        ) {
            omni_app_trace_log(
                "own_devices/revocation",
                &format!(
                    "device={} to {dialog_key} not sent: {error}",
                    revocation.device_id.0
                ),
            );
        }
    }
}

/// A revocation arrived over an established session. Only the account's own devices and its
/// approved contacts are listened to, and only about the account they speak for: a contact
/// cannot revoke anything of anybody else's.
fn handle_incoming_device_revocation(
    storage: &Storage,
    transport: &TransportManager,
    sync: &omni_types::DeviceRevocationSyncV1,
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
            "own_devices/revocation_in",
            &format!("dropped: {} is neither ours nor an approved contact", from.remote_device_id.0),
        );
        return Ok(());
    }
    let revocation: omni_crypto::DeviceRevocationV1 = match serde_json::from_str(&sync.revocation_json) {
        Ok(revocation) => revocation,
        Err(error) => {
            omni_app_trace_log("own_devices/revocation_in", &format!("dropped: unreadable: {error}"));
            return Ok(());
        }
    };
    if revocation.account_id != from.remote_account_id {
        omni_app_trace_log(
            "own_devices/revocation_in",
            "dropped: it speaks for another account than the one that sent it",
        );
        return Ok(());
    }
    apply_received_device_revocation(
        storage,
        transport,
        &revocation,
        Some(&receive_context.dialog_key),
        "own_devices/revocation_in",
    );
    Ok(())
}

/// A revocation that reached us — over a session, or from a pair's notice at the couriers — for an
/// account it may speak for (the caller checked that). It proves itself in storage; the first time
/// it is passed on, and when it takes THIS device back, the profile goes. `from_dialog` is where it
/// came from, not to be told again.
fn apply_received_device_revocation(
    storage: &Storage,
    transport: &TransportManager,
    revocation: &omni_crypto::DeviceRevocationV1,
    from_dialog: Option<&str>,
    trace: &str,
) {
    let outcome = match storage.apply_device_revocation(revocation, current_unix_ms()) {
        Ok(outcome) => outcome,
        Err(error) => {
            omni_app_trace_log(trace, &format!("refused: {error}"));
            return;
        }
    };
    omni_app_trace_log(
        trace,
        &format!(
            "device={} newly_applied={} targets_this_device={}",
            revocation.device_id.0, outcome.newly_applied, outcome.targets_local_device
        ),
    );
    if outcome.newly_applied {
        // Pass it on: our other devices and our contacts may not have heard.
        broadcast_device_revocation(storage, transport, revocation, from_dialog);
    }
    if outcome.targets_local_device {
        // The account took THIS device back: it erases the profile it holds, at once
        // (docs/decisions/multi-device-equal-nodes.md, «Устройства»). The storage call re-checks
        // that a verified revocation of this very device is on record.
        match storage.wipe_profile_if_taken_back() {
            Ok(erased) => omni_app_trace_log(
                trace,
                &format!("this device was taken back by its account; profile erased={erased}"),
            ),
            Err(error) => omni_app_trace_log(
                trace,
                &format!("this device was taken back by its account; erasing failed: {error}"),
            ),
        }
    }
}

/// A frame from a device the account took back is not listened to. The one thing it may still send
/// is a revocation, which proves itself.
fn sender_is_revoked_device(
    storage: &Storage,
    session: &omni_crypto::SessionRecord,
    payload: &omni_types::WirePayloadV1,
) -> OmniResult<bool> {
    if matches!(
        payload,
        omni_types::WirePayloadV1::LogicalEvent(event)
            if matches!(event.payload, omni_types::LogicalEventPayloadV1::DeviceRevocation(_))
    ) {
        return Ok(false);
    }
    storage.is_device_revoked(
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    )
}

/// Contacts that were away when a device of ours was revoked hear it when their link is next live:
/// every revocation of our own account goes to every device of every approved contact once per link
/// (`told` remembers what was sent; a dialog without a live link forgets it, so a new link tells
/// again). Hearing it twice is harmless — the contact applies it once and does not pass it on.
fn tell_contacts_about_revocations(
    storage: &Storage,
    transport: &TransportManager,
    told: &mut std::collections::HashSet<String>,
) -> usize {
    let Ok(Some(account)) = storage.get_local_account() else {
        return 0;
    };
    let revocations = storage
        .list_device_revocations_under_current_key(&account.descriptor.account_id)
        .unwrap_or_default();
    if revocations.is_empty() {
        told.clear();
        return 0;
    }
    let mut sent = 0;
    for contact in storage.list_contacts().unwrap_or_default() {
        if contact.relationship_state != ContactRelationshipState::Approved {
            continue;
        }
        let Some(contact_account_id) = contact.remote_account_id.clone() else {
            continue;
        };
        for device_dialog in storage
            .list_contact_device_dialogs(&contact.contact_id)
            .unwrap_or_default()
        {
            let prefix = format!("{}|", device_dialog.dialog_key);
            if transport
                .connection_snapshot_for_dialog(&device_dialog.dialog_key)
                .is_none()
            {
                told.retain(|key| !key.starts_with(&prefix));
                continue;
            }
            for revocation in &revocations {
                let key = format!("{prefix}{}", revocation.device_id.0);
                if told.contains(&key) {
                    continue;
                }
                let Ok(revocation_json) = serde_json::to_string(revocation) else {
                    continue;
                };
                // No session with that device yet is not an error worth a trace: the next pass retries.
                if send_device_revocation_to_dialog(
                    storage,
                    transport,
                    &contact_account_id,
                    &device_dialog.dialog_key,
                    &revocation_json,
                )
                .is_ok()
                {
                    told.insert(key);
                    sent += 1;
                }
            }
        }
    }
    sent
}

/// The reconcile pass for devices we have revoked: they are not copied to any more, but one that
/// is on a live link and has not been told yet must be.
fn tell_revoked_own_devices(
    storage: &Storage,
    transport: &TransportManager,
    backoff: &mut std::collections::HashMap<String, OwnDeviceBackoff>,
    now_unix_ms: i64,
) {
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let account_id = account.descriptor.account_id;
    let revocations = storage.list_device_revocations(&account_id).unwrap_or_default();
    for (device_id, dialog_key) in storage.list_revoked_own_device_dialogs().unwrap_or_default() {
        let key = format!("revoked:{}", device_id.0);
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            backoff.remove(&key);
            continue;
        }
        let state = backoff.get(&key).copied().unwrap_or_default();
        if now_unix_ms < state.next_pass_at_unix_ms {
            continue;
        }
        let Some(revocation) = revocations.iter().find(|revocation| revocation.device_id == device_id) else {
            continue;
        };
        let Ok(revocation_json) = serde_json::to_string(revocation) else {
            continue;
        };
        let sent = send_device_revocation_to_dialog(storage, transport, &account_id, &dialog_key, &revocation_json);
        omni_app_trace_log(
            "own_devices/revocation",
            &format!(
                "told revoked device={} itself: {}",
                device_id.0,
                sent.as_ref().map_or_else(|error| format!("not yet: {error}"), |()| "sent".to_string())
            ),
        );
        // Until it has acted on it the device stays reachable; ask again at growing intervals.
        backoff.insert(key, state.after_pass(usize::from(sent.is_ok()), now_unix_ms));
    }
}

/// One device of our own account, for the Settings list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnDeviceView {
    pub device_id: String,
    pub device_label: String,
    pub is_this_device: bool,
    pub revoked: bool,
    pub first_seen_at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at_unix_ms: Option<i64>,
    /// There is a live link to it right now (never true for this device itself).
    pub online: bool,
    /// Another device that appeared and that the owner has not yet been told about («Добавлено
    /// устройство X»): shown until acknowledged.
    pub is_new: bool,
}

impl OmniApp {
    /// Every device of our account that has a certificate here, this one first, then by when it was
    /// first seen. Revoked devices stay in the list, marked — until the owner takes them out of it
    /// (`hide_revoked_own_devices`).
    pub fn list_own_devices(&self) -> OmniResult<Vec<OwnDeviceView>> {
        let _ = self.ensure_runtime_profile_active("list_own_devices")?;
        let (Some(account), Some(local_device)) =
            (self.storage.get_local_account()?, self.storage.get_local_device()?)
        else {
            return Ok(Vec::new());
        };
        let dialogs: std::collections::HashMap<String, String> = self
            .storage
            .list_own_device_dialogs()?
            .into_iter()
            .map(|(device_id, dialog_key)| (device_id.0, dialog_key))
            .collect();
        let mut devices: Vec<OwnDeviceView> = self
            .storage
            .list_account_devices(&account.descriptor.account_id)?
            .into_iter()
            .filter(|record| record.hidden_at_unix_ms.is_none())
            .map(|record| {
                let device_id = record.chain.device.device_id.0.clone();
                let is_this_device = device_id == local_device.device_id.0;
                let online = dialogs.get(&device_id).is_some_and(|dialog_key| {
                    self.transport.connection_snapshot_for_dialog(dialog_key).is_some()
                });
                OwnDeviceView {
                    is_this_device,
                    is_new: !is_this_device
                        && record.revoked_at_unix_ms.is_none()
                        && record.acknowledged_at_unix_ms.is_none(),
                    revoked: record.revoked_at_unix_ms.is_some(),
                    revoked_at_unix_ms: record.revoked_at_unix_ms,
                    first_seen_at_unix_ms: record.first_seen_at_unix_ms,
                    device_label: record.device_label,
                    device_id,
                    online,
                }
            })
            .collect();
        devices.sort_by_key(|device| (!device.is_this_device, device.first_seen_at_unix_ms));
        Ok(devices)
    }

    /// Take a revoked device (`Some`) or all of them (`None`) out of the Settings list. The
    /// revocation itself stays: the device is still out of the account and is still told so.
    pub fn hide_revoked_own_devices(&self, device_id: Option<&str>) -> OmniResult<usize> {
        let _ = self.ensure_runtime_profile_active("hide_revoked_own_devices")?;
        let device_id = device_id.map(|device_id| DeviceId(device_id.trim().to_string()));
        self.storage.hide_revoked_account_devices(device_id.as_ref(), current_unix_ms())
    }

    /// The owner has seen the list of devices: nothing in it is "new" any more.
    pub fn acknowledge_own_devices(&self) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("acknowledge_own_devices")?;
        self.storage.acknowledge_account_devices(current_unix_ms())
    }

    /// Take another of our devices back: it is dropped from every route, our other devices and our
    /// contacts are told, and the revoked device learns it when it next comes online.
    ///
    /// `password` is the profile password when the working key is kept under one (the folder is
    /// protected): without it such a profile cannot sign a revocation.
    pub fn revoke_own_device(&self, device_id: &str, password: Option<&str>) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("revoke_own_device")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        let now_unix_ms = current_unix_ms();
        let device_id = DeviceId(device_id.trim().to_string());
        let revocation = match password {
            Some(password) => self
                .storage
                .issue_local_device_revocation_with_password(&device_id, password, now_unix_ms)?,
            None => self.storage.issue_local_device_revocation(&device_id, None, now_unix_ms)?,
        };
        self.storage.apply_device_revocation(&revocation, now_unix_ms)?;
        broadcast_device_revocation(&self.storage, &self.transport, &revocation, None);
        // The device itself is told by the reconcile pass: run it now.
        crate::request_reconcile_pass();
        Ok(())
    }
}
