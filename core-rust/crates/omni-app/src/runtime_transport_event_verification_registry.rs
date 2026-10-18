// Verification-challenge registry + payload helpers: build/sign the local verification payload
// and manage the pending-challenge registry (register/take/clear/already-pending).
// include!d into runtime_transport_events.rs - same module scope + imports. Split for size.

/// What a contact compares with us: the safety number of our account — of its account id alone,
/// the same on every device of ours (docs/bugs/safety-number-differs-per-own-device.md) — and our
/// devices: this one and every other one of the account that is not revoked.
fn build_local_verification_payload(storage: &Storage) -> OmniResult<MyVerificationPayloadV1> {
    let local_account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::InvalidInput("local account is not available".to_string()))?;
    let local_device = storage
        .get_local_device()?
        .ok_or_else(|| OmniError::InvalidInput("local device is not available".to_string()))?;
    let account_id = local_account.descriptor.account_id.clone();
    let material = omni_identity::verification_material_for_identity_key(Some(&account_id.0));
    let descriptor = |device_id: &DeviceId, device_label: &str| DevicePublicDescriptorV1 {
        version: omni_protocol::PUBLIC_DESCRIPTOR_VERSION_V1,
        device_id: device_id.0.clone(),
        account_id: account_id.0.clone(),
        device_label: device_label.to_string(),
    };
    let mut devices = vec![descriptor(&local_device.device_id, &local_device.device_label)];
    for record in storage.list_account_devices(&account_id).unwrap_or_default() {
        if record.revoked_at_unix_ms.is_none() && record.chain.device.device_id != local_device.device_id {
            devices.push(descriptor(&record.chain.device.device_id, &record.device_label));
        }
    }

    Ok(MyVerificationPayloadV1 {
        version: omni_protocol::MY_VERIFICATION_PAYLOAD_VERSION_V1,
        account: AccountPublicDescriptorV1 {
            version: omni_protocol::PUBLIC_DESCRIPTOR_VERSION_V1,
            account_id: account_id.0.clone(),
            display_name: local_account.descriptor.display_name,
        },
        devices,
        fingerprint: material.fingerprint.ok_or_else(|| {
            OmniError::Internal("verification fingerprint was not derived".to_string())
        })?,
        safety_number: material.safety_number.ok_or_else(|| {
            OmniError::Internal("verification safety number was not derived".to_string())
        })?,
        exported_at_unix_ms: current_unix_ms(),
    })
}

fn verification_challenge_signing_bytes(
    challenge: &omni_types::VerificationChallengeV1,
) -> OmniResult<Vec<u8>> {
    #[derive(Serialize)]
    struct SigningPayload<'a> {
        version: u16,
        challenge_id: &'a str,
        nonce_hex: &'a str,
        issued_at_unix_ms: i64,
        expires_at_unix_ms: i64,
        remote_instance_id: Option<&'a str>,
        remote_account_id: Option<&'a str>,
    }

    serde_json::to_vec(&SigningPayload {
        version: challenge.version,
        challenge_id: &challenge.challenge_id,
        nonce_hex: &challenge.nonce_hex,
        issued_at_unix_ms: challenge.issued_at_unix_ms,
        expires_at_unix_ms: challenge.expires_at_unix_ms,
        remote_instance_id: challenge.remote_instance_id.as_deref(),
        remote_account_id: challenge.remote_account_id.as_deref(),
    })
    .map_err(|error| {
        OmniError::Internal(format!(
            "failed to serialize verification challenge: {error}"
        ))
    })
}

fn verification_registry_for_db(
    _db_path: &str,
) -> &'static Mutex<HashMap<String, VerificationChallengeRegistry>> {
    VERIFICATION_CHALLENGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_pending_verification_challenge(
    db_path: &str,
    challenge_id: String,
    challenge: PendingVerificationChallenge,
) {
    let Ok(mut registry) = lock_verification_registry() else {
        return;
    };
    let entry = registry
        .entry(db_path.to_string())
        .or_insert_with(VerificationChallengeRegistry::default);
    entry.pending_by_id.insert(challenge_id, challenge);
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
pub(crate) fn register_pending_verification_challenge_for_test(
    db_path: &str,
    challenge_id: String,
    dialog_key: String,
    peer_id: String,
    remote_instance_id: Option<String>,
    remote_account_id: Option<String>,
    challenge_bytes: Vec<u8>,
    expires_at_unix_ms: i64,
) {
    register_pending_verification_challenge(
        db_path,
        challenge_id,
        PendingVerificationChallenge {
            dialog_key,
            peer_id,
            remote_instance_id,
            remote_account_id,
            challenge_bytes,
            expires_at_unix_ms,
        },
    );
}

fn take_pending_verification_challenge(
    db_path: &str,
    challenge_id: &str,
) -> Option<PendingVerificationChallenge> {
    let Ok(mut registry) = lock_verification_registry() else {
        return None;
    };
    let removed = registry
        .get_mut(db_path)
        .and_then(|entry| entry.pending_by_id.remove(challenge_id));
    remove_registry_entry_if_empty(&mut registry, db_path);
    removed
}

fn clear_pending_verification_challenge(db_path: &str, challenge_id: &str) {
    let Ok(mut registry) = lock_verification_registry() else {
        return;
    };
    if let Some(entry) = registry.get_mut(db_path) {
        entry.pending_by_id.remove(challenge_id);
    }
    remove_registry_entry_if_empty(&mut registry, db_path);
}

fn clear_pending_verification_challenges_for_session(
    db_path: &str,
    dialog_key: &str,
    peer_id: &str,
) {
    let Ok(mut registry) = lock_verification_registry() else {
        return;
    };
    if let Some(entry) = registry.get_mut(db_path) {
        entry.pending_by_id.retain(|_, challenge| {
            challenge.dialog_key != dialog_key || challenge.peer_id != peer_id
        });
    }
    remove_registry_entry_if_empty(&mut registry, db_path);
}

fn challenge_already_pending(
    db_path: &str,
    dialog_key: &str,
    peer_id: &str,
    now_unix_ms: i64,
) -> bool {
    let Ok(mut registry) = lock_verification_registry() else {
        return false;
    };
    let Some(entry) = registry.get_mut(db_path) else {
        return false;
    };
    entry
        .pending_by_id
        .retain(|_, challenge| challenge.expires_at_unix_ms > now_unix_ms);
    let already_pending = entry.pending_by_id.values().any(|challenge| {
        challenge.dialog_key == dialog_key && challenge.peer_id == peer_id
    });
    remove_registry_entry_if_empty(&mut registry, db_path);
    already_pending
}
