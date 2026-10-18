fn seal_payload(session: &mut SessionRecord, plaintext: &[u8]) -> OmniResult<EncryptedPayload> {
    ensure_send_chain(session)?;
    let chain_key = session
        .protected_send_chain_key
        .as_ref()
        .ok_or_else(|| OmniError::Internal("send chain key was not initialized".to_string()))?;
    let current_chain_key = unprotect_chain_key(chain_key)?;
    let counter = session.send_counter.saturating_add(1);
    let (message_key, next_chain_key) = derive_chain_step(&current_chain_key)?;
    let nonce = random_bytes(CHACHA_NONCE_LEN)?;
    let aad = build_network_aad(
        session,
        &session.local_ratchet_public_key,
        session.previous_send_chain_length,
        counter,
    );
    let cipher = ChaCha20Poly1305::new_from_slice(&message_key)
        .map_err(|error| OmniError::Internal(format!("chacha20-poly1305 key init failed: {error}")))?;
    let sealed = cipher
        .encrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| OmniError::Internal("network payload encryption failed".to_string()))?;
    let (ciphertext, mac) = split_ciphertext_and_tag(&sealed)?;

    session.protected_send_chain_key = Some(protect_bytes(&next_chain_key)?);
    session.send_counter = counter;
    session.updated_at_unix_ms = session.updated_at_unix_ms.saturating_add(1);

    Ok(EncryptedPayload {
        counter,
        ratchet_public_key_hex: ephemeral_public_key_to_wire(session.local_ratchet_public_key),
        previous_chain_length: session.previous_send_chain_length,
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(ciphertext),
        mac_hex: hex::encode(mac),
    })
}

fn open_payload(
    session: &mut SessionRecord,
    replay_window: &mut ReplayWindowState,
    payload: &EncryptedPayload,
) -> OmniResult<Vec<u8>> {
    let payload_ratchet_public = ephemeral_public_key_from_wire(&payload.ratchet_public_key_hex)?;
    // A frame we already opened is a duplicate, not a broken session: mesh durable delivery
    // re-offers what it holds after every reconnect, by design, and leaves dedup to the receiver.
    // Checked FIRST, by tag alone — a duplicate from an older ratchet chain would otherwise run the
    // receive ratchet against a key that has moved on and fail as "authentication failed", which
    // the app counts toward rebuilding the session
    // (docs/bugs/receipt-after-restart-fails-to-decrypt.md).
    let replay_tag = replay_tag(session, &payload_ratchet_public, payload.counter);
    if replay_window.recent_tags.iter().any(|existing| existing == &replay_tag) {
        return Err(OmniError::InvalidInput(
            "encrypted payload was rejected by replay window".to_string(),
        ));
    }

    // Everything below works on a copy and commits only an authentic frame: a forged or stale
    // frame must neither advance the ratchet nor spend a remembered key.
    let mut working = session.clone();
    let mut skipped = load_skipped_message_keys(&working)?;

    // A frame a later one overtook, or a late frame of the previous chain: its key was remembered
    // when the chain stepped past it. Without it such a frame never opens — the mesh and the
    // couriers do not promise order (docs/bugs/message-stuck-sending-after-restart.md).
    let remembered = skipped.iter().position(|entry| {
        entry.ratchet_public == payload_ratchet_public && entry.counter == payload.counter
    });
    // Set when a frame of a NEW chain is opened past the gap (see below): if it does not authenticate,
    // it is reported as the unknown far chain it looked like — not as a broken session.
    let mut jumped_a_new_chain = false;
    let message_key = if let Some(index) = remembered {
        skipped.remove(index).message_key
    } else {
        if payload_ratchet_public != working.remote_ratchet_public_key {
            // The rest of the current chain may still be on its way: the sender says how long it
            // was. Remember those keys before the chain is replaced.
            let previous_remote = working.remote_ratchet_public_key;
            skip_receive_chain_to(
                &mut working,
                &mut skipped,
                previous_remote,
                payload.previous_chain_length,
            )?;
            apply_receive_ratchet(&mut working, &payload_ratchet_public)?;
        }

        if payload.counter <= working.receive_counter {
            return Err(OmniError::InvalidInput(
                "encrypted payload counter was replayed".to_string(),
            ));
        }
        if payload.counter > working.receive_counter.saturating_add(MAX_RECEIVE_GAP) {
            let new_chain = payload_ratchet_public != session.remote_ratchet_public_key;
            if new_chain && payload.counter <= MAX_NEW_CHAIN_JUMP {
                // The peer's NEW chain, far along: it kept sending while we could not hear it (a
                // relay-only stretch — live 2026-10-15: 1464 frames). Neither side can ratchet on
                // its own — it steps when it hears our new key, we when we open one of its frames —
                // so refusing it locked the session one way for good. Try it: step the chain up to
                // it without keeping the keys passed (those frames are gone), and commit only if it
                // authenticates — a long-dead session's frame does not (the root key moved on).
                fast_forward_receive_chain(&mut working, payload.counter.saturating_sub(1))?;
                jumped_a_new_chain = true;
            } else if new_chain {
                // Far past the window on a chain we do not know is not news about OUR chain: it is
                // what a long-dead session's frames look like when durable delivery offers them
                // again (live: counters ~3000 on every restart, each one asking for a rebuild). Said
                // apart so the app does not rebuild over it.
                return Err(OmniError::InvalidInput(
                    "encrypted payload is from an unknown ratchet chain far past the receive gap".to_string(),
                ));
            } else {
                return Err(OmniError::InvalidInput(
                    "encrypted payload counter exceeded the supported receive gap".to_string(),
                ));
            }
        }
        skip_receive_chain_to(
            &mut working,
            &mut skipped,
            payload_ratchet_public,
            payload.counter.saturating_sub(1),
        )?;
        let chain_key = working
            .protected_receive_chain_key
            .as_ref()
            .ok_or_else(|| OmniError::InvalidInput("receive chain key is not available".to_string()))
            .and_then(unprotect_chain_key)?;
        let (message_key, next_chain_key) = derive_chain_step(&chain_key)?;
        working.protected_receive_chain_key = Some(protect_bytes(&next_chain_key)?);
        working.receive_counter = payload.counter;
        message_key
    };

    let nonce = hex_decode_exact_len("encrypted_payload_nonce", &payload.nonce_hex, CHACHA_NONCE_LEN)?;
    let ciphertext = hex::decode(payload.ciphertext_hex.trim()).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid encrypted payload ciphertext '{}': {error}",
            payload.ciphertext_hex
        ))
    })?;
    let mac = hex_decode_exact_len("encrypted_payload_mac", &payload.mac_hex, AEAD_TAG_LEN)?;
    let aad = build_network_aad(
        &working,
        &payload_ratchet_public,
        payload.previous_chain_length,
        payload.counter,
    );
    let mut combined = ciphertext;
    combined.extend_from_slice(&mac);
    let cipher = ChaCha20Poly1305::new_from_slice(&message_key)
        .map_err(|error| OmniError::Internal(format!("chacha20-poly1305 key init failed: {error}")))?;
    let plaintext = cipher
        .decrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: &combined,
                aad: &aad,
            },
        )
        .map_err(|_| {
            OmniError::InvalidInput(if jumped_a_new_chain {
                "encrypted payload is from an unknown ratchet chain far past the receive gap".to_string()
            } else {
                "encrypted payload authentication failed".to_string()
            })
        })?;

    store_skipped_message_keys(&mut working, &skipped)?;
    working.updated_at_unix_ms = working.updated_at_unix_ms.saturating_add(1);
    *session = working;
    replay_window.highest_counter = replay_window.highest_counter.max(payload.counter);
    let mut tags = VecDeque::from(replay_window.recent_tags.clone());
    tags.push_back(replay_tag);
    while tags.len() > REPLAY_WINDOW_LIMIT {
        let _ = tags.pop_front();
    }
    replay_window.recent_tags = tags.into_iter().collect();
    replay_window.updated_at_unix_ms = replay_window.updated_at_unix_ms.saturating_add(1);

    Ok(plaintext)
}

/// The message key of a frame the receive chain stepped past without it.
struct SkippedMessageKey {
    ratchet_public: EphemeralPublicKey,
    counter: u64,
    message_key: [u8; 32],
}

/// ratchet public key (32) + counter (8, big-endian) + message key (32).
const SKIPPED_MESSAGE_KEY_LEN: usize = 32 + 8 + 32;

fn load_skipped_message_keys(session: &SessionRecord) -> OmniResult<Vec<SkippedMessageKey>> {
    let Some(protected) = session.protected_skipped_message_keys.as_ref() else {
        return Ok(Vec::new());
    };
    let bytes = unprotect_bytes(protected)?;
    if bytes.len() % SKIPPED_MESSAGE_KEY_LEN != 0 {
        return Err(OmniError::Storage(format!(
            "skipped message keys had an unexpected length: {}",
            bytes.len()
        )));
    }
    Ok(bytes
        .as_chunks::<SKIPPED_MESSAGE_KEY_LEN>()
        .0
        .iter()
        .map(|entry| {
            let mut ratchet_public = [0u8; 32];
            ratchet_public.copy_from_slice(&entry[..32]);
            let mut counter = [0u8; 8];
            counter.copy_from_slice(&entry[32..40]);
            let mut message_key = [0u8; 32];
            message_key.copy_from_slice(&entry[40..]);
            SkippedMessageKey {
                ratchet_public: EphemeralPublicKey(ratchet_public),
                counter: u64::from_be_bytes(counter),
                message_key,
            }
        })
        .collect())
}

fn store_skipped_message_keys(
    session: &mut SessionRecord,
    skipped: &[SkippedMessageKey],
) -> OmniResult<()> {
    if skipped.is_empty() {
        session.protected_skipped_message_keys = None;
        return Ok(());
    }
    let mut bytes = Vec::with_capacity(skipped.len() * SKIPPED_MESSAGE_KEY_LEN);
    for entry in skipped {
        bytes.extend_from_slice(&entry.ratchet_public.0);
        bytes.extend_from_slice(&entry.counter.to_be_bytes());
        bytes.extend_from_slice(&entry.message_key);
    }
    session.protected_skipped_message_keys = Some(protect_bytes(&bytes)?);
    Ok(())
}

/// Step the receive chain up to `until` (inclusive), remembering each key it passes. A gap wider
/// than `MAX_RECEIVE_GAP` is not walked: that is work a forger could impose, and such a frame is
/// refused anyway. Past `MAX_SKIPPED_MESSAGE_KEYS` the oldest keys go first.
fn skip_receive_chain_to(
    session: &mut SessionRecord,
    skipped: &mut Vec<SkippedMessageKey>,
    ratchet_public: EphemeralPublicKey,
    until: u64,
) -> OmniResult<()> {
    if until <= session.receive_counter
        || until > session.receive_counter.saturating_add(MAX_RECEIVE_GAP)
    {
        return Ok(());
    }
    let Some(protected) = session.protected_receive_chain_key.as_ref() else {
        return Ok(());
    };
    let mut chain_key = unprotect_chain_key(protected)?;
    while session.receive_counter < until {
        let (message_key, next_chain_key) = derive_chain_step(&chain_key)?;
        session.receive_counter = session.receive_counter.saturating_add(1);
        chain_key = next_chain_key;
        skipped.push(SkippedMessageKey {
            ratchet_public,
            counter: session.receive_counter,
            message_key,
        });
    }
    session.protected_receive_chain_key = Some(protect_bytes(&chain_key)?);
    if skipped.len() > MAX_SKIPPED_MESSAGE_KEYS {
        let excess = skipped.len() - MAX_SKIPPED_MESSAGE_KEYS;
        skipped.drain(..excess);
    }
    Ok(())
}

/// How far into a NEW chain a frame may be and still be tried (a chain step is one HKDF: this bounds
/// what a junk frame costs). A sender ten seconds apart fills it in about two days of silence.
const MAX_NEW_CHAIN_JUMP: u64 = 16_384;

/// Step the receive chain up to `until` without remembering the keys passed — for a new chain far
/// past the gap, whose earlier frames are gone (see `open_payload`).
fn fast_forward_receive_chain(session: &mut SessionRecord, until: u64) -> OmniResult<()> {
    let Some(protected) = session.protected_receive_chain_key.as_ref() else {
        return Ok(());
    };
    let mut chain_key = unprotect_chain_key(protected)?;
    while session.receive_counter < until {
        let (_, next_chain_key) = derive_chain_step(&chain_key)?;
        chain_key = next_chain_key;
        session.receive_counter = session.receive_counter.saturating_add(1);
    }
    session.protected_receive_chain_key = Some(protect_bytes(&chain_key)?);
    Ok(())
}

fn ensure_send_chain(session: &mut SessionRecord) -> OmniResult<()> {
    if !session.needs_send_ratchet && session.protected_send_chain_key.is_some() {
        return Ok(());
    }

    let root_key = unprotect_chain_key(&session.protected_session_key)?;
    let remote_ratchet_public = x25519_from_ephemeral_public(&session.remote_ratchet_public_key);
    let new_local_ratchet = generate_x25519_secret();
    let new_local_ratchet_public =
        EphemeralPublicKey(X25519PublicKey::from(&new_local_ratchet).to_bytes());
    let dh = new_local_ratchet.diffie_hellman(&remote_ratchet_public);
    let (new_root_key, new_send_chain_key) = derive_ratchet_step(&root_key, &dh.to_bytes())?;

    session.previous_send_chain_length = session.send_counter;
    session.send_counter = 0;
    session.needs_send_ratchet = false;
    session.local_ratchet_public_key = new_local_ratchet_public;
    session.protected_local_ratchet_key = protect_bytes(new_local_ratchet.to_bytes().as_ref())?;
    session.protected_session_key = protect_bytes(&new_root_key)?;
    session.protected_send_chain_key = Some(protect_bytes(&new_send_chain_key)?);
    Ok(())
}

fn apply_receive_ratchet(
    session: &mut SessionRecord,
    new_remote_ratchet_public: &EphemeralPublicKey,
) -> OmniResult<()> {
    let root_key = unprotect_chain_key(&session.protected_session_key)?;
    let local_ratchet_private = unprotect_x25519_secret(&session.protected_local_ratchet_key)?;
    let dh = local_ratchet_private.diffie_hellman(&x25519_from_ephemeral_public(new_remote_ratchet_public));
    let (new_root_key, new_receive_chain_key) = derive_ratchet_step(&root_key, &dh.to_bytes())?;

    session.protected_session_key = protect_bytes(&new_root_key)?;
    session.protected_receive_chain_key = Some(protect_bytes(&new_receive_chain_key)?);
    session.remote_ratchet_public_key = *new_remote_ratchet_public;
    session.receive_counter = 0;
    session.needs_send_ratchet = true;
    Ok(())
}

fn derive_ratchet_step(root_key: &[u8; 32], dh_output: &[u8; 32]) -> OmniResult<([u8; 32], [u8; 32])> {
    let mut ikm = Vec::new();
    ikm.extend_from_slice(dh_output);
    let okm = hkdf_expand_multi(Some(root_key), &ikm, b"omni-stage5-ratchet-step-v2", 64)?;
    Ok((
        slice_to_array::<32>(&okm[..32])?,
        slice_to_array::<32>(&okm[32..64])?,
    ))
}

fn derive_chain_step(chain_key: &[u8; 32]) -> OmniResult<([u8; 32], [u8; 32])> {
    let okm = hkdf_expand_multi(
        Some(chain_key),
        b"omni-stage5-chain-step-input-v2",
        b"omni-stage5-chain-step-v2",
        64,
    )?;
    Ok((
        slice_to_array::<32>(&okm[..32])?,
        slice_to_array::<32>(&okm[32..64])?,
    ))
}

fn replay_tag(
    session: &SessionRecord,
    ratchet_public_key: &EphemeralPublicKey,
    counter: u64,
) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(b"omni-stage5-replay-tag-v2");
    input.extend_from_slice(session.session_address.storage_key().as_bytes());
    input.push(0);
    input.extend_from_slice(&ratchet_public_key.0);
    input.extend_from_slice(&counter.to_be_bytes());
    hex::encode(sha256(&input))
}

fn build_network_aad(
    session: &SessionRecord,
    ratchet_public_key: &EphemeralPublicKey,
    previous_chain_length: u64,
    counter: u64,
) -> Vec<u8> {
    let mut parties = [
        (
            session.local_account_id.0.as_str(),
            session.local_device_id.0.as_str(),
        ),
        (
            session.session_address.remote_account_id.0.as_str(),
            session.session_address.remote_device_id.0.as_str(),
        ),
    ];
    parties.sort();

    let mut aad = Vec::new();
    aad.extend_from_slice(b"omni-stage5-network-aad-v2");
    for (account_id, device_id) in parties {
        aad.extend_from_slice(account_id.as_bytes());
        aad.push(0);
        aad.extend_from_slice(device_id.as_bytes());
        aad.push(0);
    }
    aad.extend_from_slice(&ratchet_public_key.0);
    aad.extend_from_slice(&previous_chain_length.to_be_bytes());
    aad.extend_from_slice(&counter.to_be_bytes());
    aad
}

fn fingerprint_input(material: &FingerprintMaterial) -> Vec<u8> {
    let mut accounts = [
        (
            material.local_account_id.0.as_str(),
            &material.local_identity_key.0,
            &material.local_identity_signing_key.0,
        ),
        (
            material.remote_account_id.0.as_str(),
            &material.remote_identity_key.0,
            &material.remote_identity_signing_key.0,
        ),
    ];
    accounts.sort_by(|lhs, rhs| lhs.0.cmp(rhs.0));

    let mut devices = [
        (
            material.local_account_id.0.as_str(),
            material.local_device_id.0.as_str(),
            &material.local_device_key.0,
        ),
        (
            material.remote_account_id.0.as_str(),
            material.remote_device_id.0.as_str(),
            &material.remote_device_key.0,
        ),
    ];
    devices.sort_by(|lhs, rhs| (lhs.0, lhs.1).cmp(&(rhs.0, rhs.1)));

    let mut input = Vec::new();
    input.extend_from_slice(b"omni-stage5-fingerprint-v2");
    for (account_id, agreement_key, signing_key) in accounts {
        input.extend_from_slice(account_id.as_bytes());
        input.push(0);
        input.extend_from_slice(agreement_key);
        input.extend_from_slice(signing_key);
    }
    for (account_id, device_id, device_key) in devices {
        input.extend_from_slice(account_id.as_bytes());
        input.push(0);
        input.extend_from_slice(device_id.as_bytes());
        input.push(0);
        input.extend_from_slice(device_key);
    }
    input
}

