fn resolve_canonical_dialog_key(
    storage: &Storage,
    dialog_key: &str,
) -> OmniResult<String> {
    let dialog_key = normalize_dialog_key(dialog_key);
    let Some(account_id) = omni_types::account_id_from_dialog_key(&dialog_key) else {
        return Ok(dialog_key);
    };
    let Some(contact) =
        storage.find_contact_by_remote_account_id(&omni_identity::AccountId(account_id))?
    else {
        return Ok(dialog_key);
    };
    if contact.remote_instance_id.as_deref().is_none() {
        return Ok(dialog_key);
    }
    if let Some(primary_dialog_key) = contact.primary_dialog_key.as_deref() {
        let primary_dialog_key = normalize_dialog_key(primary_dialog_key);
        if primary_dialog_key != dialog_key {
            return Ok(primary_dialog_key);
        }
    }
    Ok(dialog_key)
}

fn resolve_canonical_dialog_key_for_selector(
    storage: &Storage,
    selector: &str,
) -> OmniResult<String> {
    let dialog_key = normalize_dialog_key(selector);
    resolve_canonical_dialog_key(storage, &dialog_key)
}

fn session_matches_bootstrap_message(
    session: &omni_crypto::SessionRecord,
    message: &SessionBootstrapMessageV1,
) -> bool {
    session.session_address.remote_account_id.0 == message.initiator_account_id
        && session.session_address.remote_device_id.0 == message.initiator_device_id
        && identity_public_key_to_wire(session.remote_identity_key)
            == message.initiator_identity_public_key
        && identity_signing_public_key_to_wire(session.remote_identity_signing_key)
            == message.initiator_identity_signing_public_key
        && device_public_key_to_wire(session.remote_device_key) == message.initiator_device_public_key
        && ephemeral_public_key_to_wire(session.remote_ratchet_public_key)
            == message.initiator_ephemeral_public_key
        && session.remote_signed_prekey_id == message.remote_signed_prekey_id
        && session.remote_one_time_prekey_id == message.remote_one_time_prekey_id
}

fn existing_session_should_absorb_incoming_bootstrap(
    local_account_id: &AccountId,
    local_device_id: &DeviceId,
    existing_session: &omni_crypto::SessionRecord,
    message: &SessionBootstrapMessageV1,
    has_approved_contact: bool,
    // Newest bootstrap `created_at_unix_ms` already seen from this peer (peer's clock), if any.
    last_seen_bootstrap_created_at: Option<i64>,
) -> bool {
    if existing_session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        && session_matches_bootstrap_message(existing_session, message)
    {
        return true;
    }

    // An ESTABLISHED session must never be torn down by a bootstrap that PREDATES it.
    //
    // In a mutual pairing collision both peers bootstrap each other at once. The tie-break below
    // settles that correctly while both are still `InitiatorBootstrapCreated`: the loser becomes a
    // responder and acks, the winner establishes off that ack. But the loser's own bootstrap — and
    // every durable-backstop re-delivery of it — then arrives at the winner AFTER it is already
    // Established. It matches neither the identity check above (the winner's session is an initiator
    // one, built from the peer's BUNDLE, so its `remote_ratchet_public_key` is not the bootstrap's
    // ephemeral) nor the collision clause (state is no longer `InitiatorBootstrapCreated`), so it
    // fell through to the approved-contact clause and rebuilt a responder session over the working
    // one. The ratchets desynced and messages stopped arriving — silently, and permanently, because
    // the backstop keeps re-delivering the same bootstrap.
    //
    // A peer that genuinely re-bootstraps (reinstall) sends one that is NEWER than any we have seen
    // from it, so that path keeps working. Both timestamps compared here come from the PEER's clock,
    // which is what makes this immune to clock skew between the machines: an earlier version of this
    // guard compared the peer's timestamp against our own session's and was defeated by a 4.3 s skew
    // on a real two-machine run (same-host passed 5/5 because both nodes shared one clock).
    // See docs/bugs/mutual-pairing-wedges-session.md.
    if existing_session.bootstrap_state == omni_crypto::SessionBootstrapState::Established {
        if let Some(seen_at) = last_seen_bootstrap_created_at {
            if message.created_at_unix_ms <= seen_at {
                return true;
            }
        }
    }

    if existing_session.bootstrap_state
        == omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated
    {
        return local_endpoint_wins_session_collision(
            local_account_id,
            local_device_id,
            message.initiator_account_id.as_str(),
            message.initiator_device_id.as_str(),
        );
    }

    !has_approved_contact
        && existing_session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        && local_endpoint_wins_session_collision(
            local_account_id,
            local_device_id,
            message.initiator_account_id.as_str(),
            message.initiator_device_id.as_str(),
        )
}

fn local_endpoint_wins_session_collision(
    local_account_id: &AccountId,
    local_device_id: &DeviceId,
    remote_account_id: &str,
    remote_device_id: &str,
) -> bool {
    (local_account_id.0.as_str(), local_device_id.0.as_str())
        < (remote_account_id, remote_device_id)
}

/// Whether `selector` names the active connection: its dialog key, or the peer it is with.
fn selector_matches_snapshot(selector: &str, snapshot: &TransportSnapshot) -> bool {
    let Some(active_session_peer_addr) = snapshot.peer_id.as_deref() else {
        return false;
    };
    snapshot.dialog_key.as_deref() == Some(normalize_dialog_key(selector).as_str())
        || selector.trim() == active_session_peer_addr
}

fn next_message_id() -> String {
    format!(
        "msg-{}-{}-{}",
        std::process::id(),
        current_unix_ms(),
        MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

const DEFAULT_DEVICE_LABEL: &str = "Omni Device";

fn choose_local_device_name(
    platform_override: Option<&str>,
    computer_name: Option<&str>,
    host_name: Option<&str>,
) -> String {
    normalize_device_label(platform_override)
        .or_else(|| normalize_device_label(computer_name))
        .or_else(|| normalize_device_label(host_name))
        .unwrap_or_else(|| DEFAULT_DEVICE_LABEL.to_string())
}

fn normalize_device_label(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .map(ToString::to_string)
}

fn local_device_name() -> String {
    let platform_override = std::env::var("OMNI_LOCAL_DEVICE_LABEL").ok();
    let computer_name = std::env::var("COMPUTERNAME").ok();
    let host_name = std::env::var("HOSTNAME").ok();

    choose_local_device_name(
        platform_override.as_deref(),
        computer_name.as_deref(),
        host_name.as_deref(),
    )
}

fn observation_from_contact_payload(
    payload: &ImportableContactPayloadV1,
) -> ContactObservationSnapshot {
    ContactObservationSnapshot {
        identity_key: Some(payload.account.account_id.clone()),
        observed_display_name: Some(payload.account.display_name.clone()),
        observed_at_unix_ms: payload.exported_at_unix_ms,
    }
}

fn observation_from_verification_payload(
    payload: &ContactVerificationPayloadV1,
) -> ContactObservationSnapshot {
    ContactObservationSnapshot {
        identity_key: Some(payload.account.account_id.clone()),
        observed_display_name: Some(payload.account.display_name.clone()),
        observed_at_unix_ms: payload.exported_at_unix_ms,
    }
}

/// The wire bundle as the core uses it, with its device certificate checked against its keys —
/// every received bundle passes here before it is stored.
fn prekey_bundle_from_protocol(bundle: &PrekeyBundleV1) -> OmniResult<omni_crypto::PrekeyBundle> {
    let device_certificate_chain = bundle
        .device_certificate_chain_json
        .as_deref()
        .map(serde_json::from_str::<omni_crypto::DeviceCertificateChainV1>)
        .transpose()
        .map_err(|error| {
            OmniError::InvalidInput(format!("device certificate chain is malformed: {error}"))
        })?;
    let domain = omni_crypto::PrekeyBundle {
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        account_id: omni_identity::AccountId(bundle.account.account_id.clone()),
        device_id: omni_identity::DeviceId(bundle.device.device_id.clone()),
        identity_key: identity_public_key_from_wire(&bundle.account.identity_public_key)?,
        identity_signing_key: identity_signing_public_key_from_wire(
            &bundle.account.identity_signing_public_key,
        )?,
        device_key: device_public_key_from_wire(&bundle.device.device_public_key)?,
        signed_prekey: omni_crypto::SignedPrekeyDescriptor {
            prekey_id: bundle.signed_prekey.prekey_id,
            public_key: prekey_public_key_from_wire(&bundle.signed_prekey.public_key)?,
            signature: signature_from_wire(
                &bundle.signed_prekey.signature_r,
                &bundle.signed_prekey.signature_s,
            )?,
            expires_at_unix_ms: bundle.signed_prekey.expires_at_unix_ms,
        },
        one_time_prekey: bundle
            .one_time_prekey
            .as_ref()
            .map(|value| {
                Ok(omni_crypto::OneTimePrekeyDescriptor {
                    prekey_id: value.prekey_id,
                    public_key: prekey_public_key_from_wire(&value.public_key)?,
                })
            })
            .transpose()?,
        generated_at_unix_ms: bundle.generated_at_unix_ms,
        device_certificate_chain,
    };
    omni_crypto::verify_prekey_bundle_device_certificate(&domain)?;
    Ok(domain)
}

/// Whether the bundle's certificate chain is valid and vouches for exactly its account, device and
/// keys — without the signed prekey. A context built from a bare session bootstrap (one that came
/// without a hello) carries the initiator's keys and chain but only a placeholder prekey, so the
/// full [`prekey_bundle_from_protocol`] cannot judge it.
fn bundle_certificate_vouches_for_its_keys(bundle: &PrekeyBundleV1) -> OmniResult<()> {
    let chain: omni_crypto::DeviceCertificateChainV1 = serde_json::from_str(
        bundle.device_certificate_chain_json.as_deref().ok_or_else(|| {
            OmniError::InvalidInput("the bundle carries no device certificate".to_string())
        })?,
    )
    .map_err(|error| OmniError::InvalidInput(format!("device certificate chain is malformed: {error}")))?;
    omni_crypto::verify_device_certificate_chain(&chain)?;
    let device = &chain.device;
    if device.account_id.0 != bundle.account.account_id
        || device.device_id.0 != bundle.device.device_id
        || device.identity_public_key != identity_public_key_from_wire(&bundle.account.identity_public_key)?
        || device.identity_signing_public_key
            != identity_signing_public_key_from_wire(&bundle.account.identity_signing_public_key)?
        || device.device_public_key != device_public_key_from_wire(&bundle.device.device_public_key)?
    {
        return Err(OmniError::InvalidInput(
            "the device certificate vouches for other keys than the bundle carries".to_string(),
        ));
    }
    Ok(())
}

fn session_bootstrap_from_protocol(
    message: &SessionBootstrapMessageV1,
) -> OmniResult<omni_crypto::SessionBootstrapMessage> {
    Ok(omni_crypto::SessionBootstrapMessage {
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        initiator_account_id: omni_identity::AccountId(message.initiator_account_id.clone()),
        initiator_device_id: omni_identity::DeviceId(message.initiator_device_id.clone()),
        initiator_identity_key: identity_public_key_from_wire(
            &message.initiator_identity_public_key,
        )?,
        initiator_identity_signing_key: identity_signing_public_key_from_wire(
            &message.initiator_identity_signing_public_key,
        )?,
        initiator_device_key: device_public_key_from_wire(
            &message.initiator_device_public_key,
        )?,
        initiator_ephemeral_key: ephemeral_public_key_from_wire(
            &message.initiator_ephemeral_public_key,
        )?,
        remote_signed_prekey_id: message.remote_signed_prekey_id,
        remote_one_time_prekey_id: message.remote_one_time_prekey_id,
        bootstrap_signature: signature_from_wire(
            &message.bootstrap_signature_r,
            &message.bootstrap_signature_s,
        )?,
        created_at_unix_ms: message.created_at_unix_ms,
    })
}
