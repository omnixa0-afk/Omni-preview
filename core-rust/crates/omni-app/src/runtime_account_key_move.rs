// Moving the account to a new working key (docs/decisions/multi-device-equal-nodes.md, «Переезд при
// краже рабочего ключа»).
//
// The master key vouches for the new working key, so a move proves itself and is valid whoever hands
// it over (`Storage::apply_account_key_move`). This file moves it: to the owner's kept devices — with
// the new working key itself, which they need to sign anything from now on — and to every approved
// contact, WITHOUT the key. Devices the move did not keep are told nothing: they are the ones the
// owner moved away from.

/// One account-level event, to one dialog, over the established session with the device behind it.
/// Two devices of one account may be linked without ever having talked (nothing to copy yet), so
/// there may be no session to carry it: for our own devices that starts one and the event goes on a
/// later pass.
fn send_account_event_to_dialog(
    storage: &Storage,
    transport: &TransportManager,
    remote_account_id: &AccountId,
    dialog_key: &str,
    event: omni_types::LogicalEventEnvelopeV1,
    kind: SecurityPayloadKind,
    waits: &str,
) -> OmniResult<()> {
    if let (Some(account), Some(device_id)) = (
        storage.get_local_account()?,
        storage.remote_device_for_dialog(dialog_key)?,
    ) {
        let own_account_id = account.descriptor.account_id;
        if remote_account_id == &own_account_id
            && established_session_with_own_device(
                storage,
                transport,
                &own_account_id,
                &device_id,
                dialog_key,
                waits,
            )?
            .is_none()
        {
            return Err(OmniError::Transport(
                "no established session with that device yet: its bootstrap is on the way".to_string(),
            ));
        }
    }
    let payload = omni_types::WirePayloadV1::LogicalEvent(event);
    let mut outgoing = prepare_encrypted_payload_for_established_session(
        storage,
        remote_account_id,
        dialog_key,
        &payload,
        kind,
    )?;
    // The ratchet moves before the frame leaves.
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)
}

fn send_account_key_move_to_dialog(
    storage: &Storage,
    transport: &TransportManager,
    remote_account_id: &AccountId,
    dialog_key: &str,
    move_json: &str,
    working_seed: Option<&[u8; 32]>,
) -> OmniResult<()> {
    send_account_event_to_dialog(
        storage,
        transport,
        remote_account_id,
        dialog_key,
        omni_types::LogicalEventEnvelopeV1::account_key_move(
            move_json.to_string(),
            working_seed.map(hex::encode),
        ),
        SecurityPayloadKind::AccountKeyMove,
        "an account key move waits",
    )
}

/// Hand the move (without the key) to every device of every approved contact, once per live link
/// (`told` remembers what went out; a dialog without a live link forgets it, so a new link tells
/// again). Hearing it twice is harmless: a contact applies a version once and does not pass it on.
/// Returns how many sends went out. `except_dialog` is the dialog the move came from.
fn tell_contacts_about_key_move(
    storage: &Storage,
    transport: &TransportManager,
    told: &mut std::collections::HashSet<String>,
    except_dialog: Option<&str>,
) -> usize {
    let Ok(Some(key_move)) = storage.latest_own_account_move() else {
        return 0;
    };
    let Ok(move_json) = serde_json::to_string(&key_move) else {
        return 0;
    };
    let tag = format!("move-{}", key_move.working.version);
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
            if except_dialog == Some(device_dialog.dialog_key.as_str()) {
                continue;
            }
            let prefix = format!("{}|", device_dialog.dialog_key);
            if transport
                .connection_snapshot_for_dialog(&device_dialog.dialog_key)
                .is_none()
            {
                told.retain(|key| !key.starts_with(&prefix));
                continue;
            }
            let key = format!("{prefix}{tag}");
            if told.contains(&key) {
                continue;
            }
            if send_account_key_move_to_dialog(
                storage,
                transport,
                &contact_account_id,
                &device_dialog.dialog_key,
                &move_json,
                None,
            )
            .is_ok()
            {
                told.insert(key);
                sent += 1;
            }
        }
    }
    sent
}

/// The kept devices of ours that have not yet shown the new certificate get the move and the new
/// working key, at growing intervals until they do (`Storage::mark_key_move_delivered`).
fn tell_kept_own_devices_of_key_move(
    storage: &Storage,
    transport: &TransportManager,
    backoff: &mut std::collections::HashMap<String, OwnDeviceBackoff>,
    now_unix_ms: i64,
) {
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let account_id = account.descriptor.account_id;
    let Ok(Some((key_move, seed, devices))) = storage.pending_key_move_delivery() else {
        return;
    };
    let Ok(move_json) = serde_json::to_string(&key_move) else {
        return;
    };
    for (device_id, dialog_key) in devices {
        let key = format!("move:{}", device_id.0);
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            backoff.remove(&key);
            continue;
        }
        let state = backoff.get(&key).copied().unwrap_or_default();
        if now_unix_ms < state.next_pass_at_unix_ms {
            continue;
        }
        let sent = send_account_key_move_to_dialog(
            storage,
            transport,
            &account_id,
            &dialog_key,
            &move_json,
            Some(&seed),
        );
        backoff.insert(key, state.after_pass(usize::from(sent.is_ok()), now_unix_ms));
    }
}

/// A move arrived over an established session. Only the account's own devices and its approved
/// contacts are listened to, and only about the account they speak for; the new working key is taken
/// only from one of our own devices and only for our own account.
fn handle_incoming_account_key_move(
    storage: &Storage,
    transport: &TransportManager,
    sync: &omni_types::AccountKeyMoveSyncV1,
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
            "own_devices/key_move_in",
            &format!("dropped: {} is neither ours nor an approved contact", from.remote_device_id.0),
        );
        return Ok(());
    }
    let key_move: omni_crypto::AccountKeyMoveV1 = match serde_json::from_str(&sync.move_json) {
        Ok(key_move) => key_move,
        Err(error) => {
            omni_app_trace_log("own_devices/key_move_in", &format!("dropped: unreadable: {error}"));
            return Ok(());
        }
    };
    if key_move.account_id() != from.remote_account_id {
        omni_app_trace_log(
            "own_devices/key_move_in",
            "dropped: it speaks for another account than the one that sent it",
        );
        return Ok(());
    }
    // The new working key is taken from our own devices only.
    let seed: Option<[u8; 32]> = if from_own_device {
        sync.working_seed_hex
            .as_deref()
            .and_then(|hex_seed| hex::decode(hex_seed).ok())
            .and_then(|bytes| bytes.try_into().ok())
    } else {
        None
    };
    let outcome = match storage.apply_account_key_move(&key_move, seed, None, current_unix_ms()) {
        Ok(outcome) => outcome,
        Err(error) => {
            omni_app_trace_log("own_devices/key_move_in", &format!("refused: {error}"));
            return Ok(());
        }
    };
    omni_app_trace_log(
        "own_devices/key_move_in",
        &format!(
            "version={} newly_applied={} own_account={} this_device_kept={} devices_superseded={} key_taken={}",
            key_move.working.version,
            outcome.newly_applied,
            outcome.own_account,
            outcome.this_device_kept,
            outcome.devices_superseded,
            seed.is_some(),
        ),
    );
    if outcome.newly_applied && outcome.own_account {
        // Our contacts may not have heard either; the key itself goes to nobody but our devices.
        let mut told = std::collections::HashSet::new();
        let _ = tell_contacts_about_key_move(
            storage,
            transport,
            &mut told,
            Some(&receive_context.dialog_key),
        );
    }
    Ok(())
}

/// What a move did, for the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountKeyMoveSummary {
    pub new_working_key_version: u64,
    pub kept_devices: usize,
}

impl OmniApp {
    /// "The account key was stolen": move the account to a new working key. The root comes from this
    /// computer's sealed secrets, or from `phrase` when they cannot be opened here (a copied folder);
    /// a profile with a password needs it too. `keep` are the other devices that stay — this one
    /// always does; every other device of the account is out and is told nothing.
    pub fn move_account_to_new_working_key(
        &self,
        keep: &[String],
        phrase: Option<&str>,
        password: Option<&str>,
    ) -> OmniResult<AccountKeyMoveSummary> {
        let _ = self.ensure_runtime_profile_active("move_account_to_new_working_key")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        let keep: Vec<DeviceId> = keep.iter().map(|id| DeviceId(id.trim().to_string())).collect();
        let (key_move, _seed) = self.storage.move_local_account_to_new_working_key(
            &keep,
            phrase,
            password,
            current_unix_ms(),
        )?;
        // Kept devices of ours: the move and the new key now, the rest on the reconcile pass.
        let mut backoff = std::collections::HashMap::new();
        tell_kept_own_devices_of_key_move(&self.storage, &self.transport, &mut backoff, current_unix_ms());
        let mut told = std::collections::HashSet::new();
        let _ = tell_contacts_about_key_move(&self.storage, &self.transport, &mut told, None);
        Ok(AccountKeyMoveSummary {
            new_working_key_version: key_move.working.version,
            kept_devices: key_move.devices.len(),
        })
    }
}
