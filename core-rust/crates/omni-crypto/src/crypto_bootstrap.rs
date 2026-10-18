pub fn bootstrap_initiator_session(
    local_identity_key: &LocalIdentityKeyPair,
    local_device_key: &LocalDeviceKeyPair,
    remote_bundle: &PrekeyBundle,
    now_unix_ms: i64,
) -> OmniResult<(SessionRecord, SessionBootstrapMessage)> {
    verify_prekey_bundle(remote_bundle)?;
    if local_identity_key.account_id != local_device_key.account_id {
        return Err(OmniError::InvalidInput(
            "local identity key and device key belong to different accounts".to_string(),
        ));
    }

    let local_identity_private = unprotect_x25519_secret(&local_identity_key.protected_private_key)?;
    let local_signing_private =
        unprotect_ed25519_signing_key(&local_identity_key.protected_signing_private_key)?;
    let ephemeral_private = generate_x25519_secret();
    let ephemeral_public =
        EphemeralPublicKey(X25519PublicKey::from(&ephemeral_private).to_bytes());

    let shared_secret = derive_x3dh_initiator_secret(
        &local_identity_private,
        &ephemeral_private,
        remote_bundle,
    )?;
    let root_key = derive_root_key(&shared_secret)?;
    let send_chain_key = derive_initial_send_chain(&shared_secret)?;

    let bootstrap = SessionBootstrapMessage {
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        initiator_account_id: local_identity_key.account_id.clone(),
        initiator_device_id: local_device_key.device_id.clone(),
        initiator_identity_key: local_identity_key.public_key,
        initiator_identity_signing_key: local_identity_key.signing_public_key,
        initiator_device_key: local_device_key.public_key,
        initiator_ephemeral_key: ephemeral_public,
        remote_signed_prekey_id: remote_bundle.signed_prekey.prekey_id,
        remote_one_time_prekey_id: remote_bundle.one_time_prekey.as_ref().map(|value| value.prekey_id),
        bootstrap_signature: sign_session_bootstrap(
            &local_signing_private,
            &local_identity_key.account_id,
            &local_device_key.device_id,
            &local_identity_key.public_key,
            &local_identity_key.signing_public_key,
            &local_device_key.public_key,
            &remote_bundle.signed_prekey.public_key,
            remote_bundle.one_time_prekey.as_ref().map(|value| value.prekey_id),
            &ephemeral_public,
            now_unix_ms,
        )?,
        created_at_unix_ms: now_unix_ms,
    };

    Ok((
        SessionRecord {
            session_address: SessionAddress {
                remote_account_id: remote_bundle.account_id.clone(),
                remote_device_id: remote_bundle.device_id.clone(),
            },
            local_account_id: local_identity_key.account_id.clone(),
            local_device_id: local_device_key.device_id.clone(),
            local_identity_key: local_identity_key.public_key,
            local_identity_signing_key: local_identity_key.signing_public_key,
            local_device_key: local_device_key.public_key,
            remote_identity_key: remote_bundle.identity_key,
            remote_identity_signing_key: remote_bundle.identity_signing_key,
            remote_device_key: remote_bundle.device_key,
            protected_session_key: protect_bytes(&root_key)?,
            protected_send_chain_key: Some(protect_bytes(&send_chain_key)?),
            protected_receive_chain_key: None,
            protected_rendezvous_secret: Some(protect_bytes(&derive_rendezvous_pair_secret(
                &shared_secret,
            )?)?),
            protected_skipped_message_keys: None,
            protected_local_ratchet_key: protect_bytes(ephemeral_private.to_bytes().as_ref())?,
            local_ratchet_public_key: ephemeral_public,
            remote_ratchet_public_key: EphemeralPublicKey(remote_bundle.signed_prekey.public_key.0),
            bootstrap_state: SessionBootstrapState::InitiatorBootstrapCreated,
            send_counter: 0,
            receive_counter: 0,
            previous_send_chain_length: 0,
            needs_send_ratchet: false,
            remote_signed_prekey_id: remote_bundle.signed_prekey.prekey_id,
            remote_one_time_prekey_id: remote_bundle.one_time_prekey.as_ref().map(|value| value.prekey_id),
            suite_version: CRYPTO_SUITE_VERSION_CURRENT,
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
            revision: 0,
        },
        bootstrap,
    ))
}

/// The stable per-contact rendezvous secret for an established session (32 bytes), or `None` if the
/// session predates the field (re-bootstrap to populate it). Both peers get the identical value —
/// it seeds the blinded serverless-rendezvous topic + beacon AEAD (omni-rendezvous).
pub fn session_rendezvous_pair_secret(session: &SessionRecord) -> OmniResult<Option<[u8; 32]>> {
    let Some(protected) = session.protected_rendezvous_secret.as_ref() else {
        return Ok(None);
    };
    let bytes = unprotect_bytes(protected)?;
    let secret: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| OmniError::Internal("rendezvous secret is not 32 bytes".to_string()))?;
    Ok(Some(secret))
}

pub fn bootstrap_responder_session(
    local_account_id: &AccountId,
    local_identity_key: &LocalIdentityKeyPair,
    local_device_key: &LocalDeviceKeyPair,
    local_signed_prekey: &SignedPrekey,
    local_one_time_prekey: Option<&OneTimePrekey>,
    bootstrap: &SessionBootstrapMessage,
    now_unix_ms: i64,
) -> OmniResult<SessionRecord> {
    if bootstrap.suite_version != CRYPTO_SUITE_VERSION_CURRENT {
        return Err(OmniError::InvalidInput(format!(
            "unsupported bootstrap suite version '{}'",
            bootstrap.suite_version
        )));
    }
    if local_account_id != &local_identity_key.account_id
        || local_account_id != &local_device_key.account_id
    {
        return Err(OmniError::InvalidInput(
            "local account and local key hierarchy do not match".to_string(),
        ));
    }
    if local_signed_prekey.device_id != local_device_key.device_id {
        return Err(OmniError::InvalidInput(
            "local signed prekey belongs to a different device".to_string(),
        ));
    }
    if bootstrap.remote_signed_prekey_id != local_signed_prekey.prekey_id {
        return Err(OmniError::InvalidInput(
            "bootstrap message references an unexpected signed prekey".to_string(),
        ));
    }
    if let Some(expected_prekey_id) = bootstrap.remote_one_time_prekey_id {
        let Some(local_one_time_prekey) = local_one_time_prekey else {
            return Err(OmniError::InvalidInput(
                "bootstrap message references a missing one-time prekey".to_string(),
            ));
        };
        if local_one_time_prekey.prekey_id != expected_prekey_id {
            return Err(OmniError::InvalidInput(
                "bootstrap message references a different one-time prekey".to_string(),
            ));
        }
    }

    verify_session_bootstrap_signature(bootstrap, local_signed_prekey)?;

    let local_identity_private = unprotect_x25519_secret(&local_identity_key.protected_private_key)?;
    let local_signed_prekey_private =
        unprotect_x25519_secret(&local_signed_prekey.protected_private_key)?;
    let local_one_time_private = local_one_time_prekey
        .map(|value| unprotect_x25519_secret(&value.protected_private_key))
        .transpose()?;

    let shared_secret = derive_x3dh_responder_secret(
        &local_identity_private,
        &local_signed_prekey_private,
        local_one_time_private.as_ref(),
        bootstrap,
    )?;
    let root_key = derive_root_key(&shared_secret)?;
    let receive_chain_key = derive_initial_send_chain(&shared_secret)?;

    Ok(SessionRecord {
        session_address: SessionAddress {
            remote_account_id: bootstrap.initiator_account_id.clone(),
            remote_device_id: bootstrap.initiator_device_id.clone(),
        },
        local_account_id: local_account_id.clone(),
        local_device_id: local_device_key.device_id.clone(),
        local_identity_key: local_identity_key.public_key,
        local_identity_signing_key: local_identity_key.signing_public_key,
        local_device_key: local_device_key.public_key,
        remote_identity_key: bootstrap.initiator_identity_key,
        remote_identity_signing_key: bootstrap.initiator_identity_signing_key,
        remote_device_key: bootstrap.initiator_device_key,
        protected_session_key: protect_bytes(&root_key)?,
        protected_send_chain_key: None,
        protected_receive_chain_key: Some(protect_bytes(&receive_chain_key)?),
        protected_rendezvous_secret: Some(protect_bytes(&derive_rendezvous_pair_secret(
            &shared_secret,
        )?)?),
        protected_skipped_message_keys: None,
        protected_local_ratchet_key: local_signed_prekey.protected_private_key.clone(),
        local_ratchet_public_key: EphemeralPublicKey(local_signed_prekey.public_key.0),
        remote_ratchet_public_key: bootstrap.initiator_ephemeral_key,
        bootstrap_state: SessionBootstrapState::ResponderBootstrapAccepted,
        send_counter: 0,
        receive_counter: 0,
        previous_send_chain_length: 0,
        needs_send_ratchet: true,
        remote_signed_prekey_id: local_signed_prekey.prekey_id,
        remote_one_time_prekey_id: bootstrap.remote_one_time_prekey_id,
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        revision: 0,
    })
}

