pub fn sign_message(
    protected_signing_private_key: &ProtectedSecret,
    message: &[u8],
) -> OmniResult<Signature> {
    let signing_key = unprotect_ed25519_signing_key(protected_signing_private_key)?;
    Ok(Signature {
        bytes: signing_key.sign(message).to_bytes().to_vec(),
    })
}

pub fn verify_signature(
    signing_public_key: &IdentitySigningPublicKey,
    message: &[u8],
    signature: &Signature,
) -> OmniResult<()> {
    let verifying_key = ed25519_verifying_key_from_public(signing_public_key)?;
    let signature_bytes = slice_to_array::<64>(&signature.bytes)?;
    let signature = Ed25519Signature::from_bytes(&signature_bytes);
    verifying_key
        .verify(message, &signature)
        .map_err(|error| OmniError::InvalidInput(format!("signature verification failed: {error}")))
}

#[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
fn sign_session_bootstrap(
    signing_key: &Ed25519SigningKey,
    initiator_account_id: &AccountId,
    initiator_device_id: &DeviceId,
    initiator_identity_key: &IdentityPublicKey,
    initiator_identity_signing_key: &IdentitySigningPublicKey,
    initiator_device_key: &DevicePublicKey,
    remote_signed_prekey_public: &PrekeyPublicKey,
    remote_one_time_prekey_id: Option<u32>,
    ephemeral_public: &EphemeralPublicKey,
    created_at_unix_ms: i64,
) -> OmniResult<Signature> {
    let message = session_bootstrap_signature_message(
        initiator_account_id,
        initiator_device_id,
        remote_signed_prekey_public,
        remote_one_time_prekey_id,
        initiator_identity_key,
        initiator_identity_signing_key,
        initiator_device_key,
        ephemeral_public,
        created_at_unix_ms,
    );
    Ok(Signature {
        bytes: signing_key.sign(&message).to_bytes().to_vec(),
    })
}

fn verify_session_bootstrap_signature(
    bootstrap: &SessionBootstrapMessage,
    local_signed_prekey: &SignedPrekey,
) -> OmniResult<()> {
    let message = session_bootstrap_signature_message(
        &bootstrap.initiator_account_id,
        &bootstrap.initiator_device_id,
        &PrekeyPublicKey(local_signed_prekey.public_key.0),
        bootstrap.remote_one_time_prekey_id,
        &bootstrap.initiator_identity_key,
        &bootstrap.initiator_identity_signing_key,
        &bootstrap.initiator_device_key,
        &bootstrap.initiator_ephemeral_key,
        bootstrap.created_at_unix_ms,
    );
    verify_signature(
        &bootstrap.initiator_identity_signing_key,
        &message,
        &bootstrap.bootstrap_signature,
    )
}

fn signed_prekey_signature_message(
    account_id: &AccountId,
    device_id: &DeviceId,
    identity_public_key: IdentityPublicKey,
    identity_signing_public_key: IdentitySigningPublicKey,
    signed_prekey_public: PrekeyPublicKey,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"omni-stage5-signed-prekey-v2");
    bytes.extend_from_slice(account_id.0.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(device_id.0.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&identity_public_key.0);
    bytes.extend_from_slice(&identity_signing_public_key.0);
    bytes.extend_from_slice(&signed_prekey_public.0);
    bytes
}

#[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
fn session_bootstrap_signature_message(
    account_id: &AccountId,
    device_id: &DeviceId,
    remote_signed_prekey_public: &PrekeyPublicKey,
    remote_one_time_prekey_id: Option<u32>,
    initiator_identity_key: &IdentityPublicKey,
    initiator_identity_signing_key: &IdentitySigningPublicKey,
    initiator_device_key: &DevicePublicKey,
    initiator_ephemeral_key: &EphemeralPublicKey,
    created_at_unix_ms: i64,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"omni-stage5-bootstrap-v2");
    bytes.extend_from_slice(account_id.0.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(device_id.0.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&remote_signed_prekey_public.0);
    bytes.extend_from_slice(&remote_one_time_prekey_id.unwrap_or_default().to_be_bytes());
    bytes.extend_from_slice(&initiator_identity_key.0);
    bytes.extend_from_slice(&initiator_identity_signing_key.0);
    bytes.extend_from_slice(&initiator_device_key.0);
    bytes.extend_from_slice(&initiator_ephemeral_key.0);
    bytes.extend_from_slice(&created_at_unix_ms.to_be_bytes());
    bytes
}

fn derive_x3dh_initiator_secret(
    local_identity_private: &StaticSecret,
    ephemeral_private: &StaticSecret,
    remote_bundle: &PrekeyBundle,
) -> OmniResult<[u8; 32]> {
    let dh1 =
        local_identity_private.diffie_hellman(&x25519_from_prekey_public(&remote_bundle.signed_prekey.public_key));
    let dh2 = ephemeral_private.diffie_hellman(&x25519_from_identity_public(&remote_bundle.identity_key));
    let dh3 =
        ephemeral_private.diffie_hellman(&x25519_from_prekey_public(&remote_bundle.signed_prekey.public_key));
    let dh4 = remote_bundle
        .one_time_prekey
        .as_ref()
        .map(|value| {
            ephemeral_private
                .diffie_hellman(&x25519_from_prekey_public(&value.public_key))
                .to_bytes()
        })
        .unwrap_or([0u8; 32]);

    derive_x3dh_secret(&[dh1.to_bytes(), dh2.to_bytes(), dh3.to_bytes(), dh4])
}

fn derive_x3dh_responder_secret(
    local_identity_private: &StaticSecret,
    local_signed_prekey_private: &StaticSecret,
    local_one_time_private: Option<&StaticSecret>,
    bootstrap: &SessionBootstrapMessage,
) -> OmniResult<[u8; 32]> {
    let dh1 =
        local_signed_prekey_private.diffie_hellman(&x25519_from_identity_public(&bootstrap.initiator_identity_key));
    let dh2 =
        local_identity_private.diffie_hellman(&x25519_from_ephemeral_public(&bootstrap.initiator_ephemeral_key));
    let dh3 =
        local_signed_prekey_private.diffie_hellman(&x25519_from_ephemeral_public(&bootstrap.initiator_ephemeral_key));
    let dh4 = local_one_time_private
        .map(|value| {
            value
                .diffie_hellman(&x25519_from_ephemeral_public(&bootstrap.initiator_ephemeral_key))
                .to_bytes()
        })
        .unwrap_or([0u8; 32]);

    derive_x3dh_secret(&[dh1.to_bytes(), dh2.to_bytes(), dh3.to_bytes(), dh4])
}

fn derive_x3dh_secret(parts: &[[u8; 32]; 4]) -> OmniResult<[u8; 32]> {
    let mut input = Vec::new();
    input.extend_from_slice(b"omni-stage5-x3dh-v2");
    for part in parts {
        input.extend_from_slice(part);
    }
    hkdf_expand(None, &input, b"omni-stage5-x3dh-seed-v2")
}

fn derive_root_key(shared_secret: &[u8; 32]) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, shared_secret, b"omni-stage5-root-v2")
}

fn derive_initial_send_chain(shared_secret: &[u8; 32]) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, shared_secret, b"omni-stage5-initial-send-chain-v2")
}

/// A STABLE per-contact rendezvous secret from the SYMMETRIC X3DH shared secret. Both peers compute
/// the identical `shared_secret`, so both derive the identical rendezvous secret — and it does NOT
/// ratchet (unlike root/chain keys), so it's stable for the life of the pairing. Seeds the blinded
/// serverless-rendezvous topic + beacon AEAD (omni-rendezvous). See serverless-bootstrap-rendezvous.md.
fn derive_rendezvous_pair_secret(shared_secret: &[u8; 32]) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, shared_secret, b"omni-rendezvous-pair-secret-v1")
}

/// The rendezvous secret for the FIRST-contact bootstrap, derived from the random nonce carried in a
/// mesh pairing invite (not from an X3DH session — there is none yet). Both the inviter (who minted the
/// nonce) and the importer (who reads it from the invite) derive the identical secret, so they meet on
/// the same blinded bootstrap topic to exchange current endpoints and connect across networks BEFORE
/// the durable [`derive_rendezvous_pair_secret`] exists. Domain-separated by its own HKDF info so a
/// bootstrap topic can never alias a durable one even if a nonce coincided with a pair secret. The
/// nonce is shared out-of-band in the invite, so this secret is only as private as the invite itself —
/// it bootstraps reachability, not message confidentiality (the X3DH session does the latter).
/// See docs/decisions/invite-bootstrap-rendezvous.md.
pub fn derive_rendezvous_bootstrap_secret(invite_nonce: &[u8; 32]) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, invite_nonce, b"omni-rendezvous-bootstrap-secret-v1")
}

/// The inviter's X25519 key for answers to one invite: importers seal their beacons on the invite's
/// topic to its public half (carried in the invite), so one importer cannot read another's addresses
/// the way it reads the topic. Derived from the inviter's identity signing seed and the invite's
/// nonce — re-derivable from the persisted nonce, never stored, a different key per invite, and not
/// the identity key itself (that one only signs). Returns `(secret, public)`.
/// See docs/decisions/invite-bootstrap-rendezvous.md.
pub fn derive_invite_reply_key(
    identity_signing_seed: &[u8; 32],
    invite_nonce: &[u8; 32],
) -> OmniResult<([u8; 32], [u8; 32])> {
    let secret = hkdf_expand(Some(invite_nonce), identity_signing_seed, b"omni-invite-reply-key-v1")?;
    let public = X25519PublicKey::from(&StaticSecret::from(secret)).to_bytes();
    Ok((secret, public))
}

/// A findable device's X25519 key for knocks: someone who found it by Omni-ID and wants a TCP hole
/// punch seals its own punch address to the public half (carried in the signed identity beacon), so
/// only this device reads where the finder is. Derived from the device's identity signing seed —
/// stable across restarts, never stored, not the identity key itself. Returns `(secret, public)`.
/// See docs/decisions/tcp-hole-punch.md.
pub fn derive_knock_key(identity_signing_seed: &[u8; 32]) -> OmniResult<([u8; 32], [u8; 32])> {
    let secret = hkdf_expand(None, identity_signing_seed, b"omni-tcp-punch-knock-key-v1")?;
    let public = X25519PublicKey::from(&StaticSecret::from(secret)).to_bytes();
    Ok((secret, public))
}

/// The importer's proof, carried in its contact request, that it holds the invite with this nonce:
/// a PRF of the nonce over the request's own identity signing key and ephemeral key (HKDF-Expand is
/// HMAC underneath). Bound to those keys, so whoever sees one request (a durable holder, a relay)
/// cannot move the claim into a request of their own — only someone holding the nonce can make it.
/// The inviter accepts the first valid claim as the invite's answer (docs/decisions/pairing-over-mesh-adoption.md).
pub fn invite_claim(
    invite_nonce: &[u8; 32],
    initiator_identity_signing_public_key: &str,
    initiator_ephemeral_public_key: &str,
) -> OmniResult<[u8; 32]> {
    let key = hkdf_expand(None, invite_nonce, b"omni-invite-claim-key-v1")?;
    let mut info = b"omni-invite-claim-v1\0".to_vec();
    info.extend_from_slice(initiator_identity_signing_public_key.as_bytes());
    info.push(0);
    info.extend_from_slice(initiator_ephemeral_public_key.as_bytes());
    hkdf_expand(None, &key, &info)
}

/// Whether `claim_hex` is the [`invite_claim`] of this nonce for these keys. Compared in constant
/// time: the claim comes from the network, and an early exit would tell a forger how many leading
/// bytes it got right.
pub fn invite_claim_matches(
    invite_nonce: &[u8; 32],
    initiator_identity_signing_public_key: &str,
    initiator_ephemeral_public_key: &str,
    claim_hex: &str,
) -> bool {
    let Ok(expected) = invite_claim(invite_nonce, initiator_identity_signing_public_key, initiator_ephemeral_public_key)
    else {
        return false;
    };
    let Ok(claim) = hex::decode(claim_hex.trim()) else {
        return false;
    };
    claim.len() == expected.len() && claim.iter().zip(expected.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}
