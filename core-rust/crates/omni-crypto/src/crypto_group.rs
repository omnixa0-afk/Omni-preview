// Group-call media keys. Group messaging and membership crypto is MLS (`omni-group-mls`,
// docs/decisions/group-crypto-mls.md); what stays here is the call's SFrame-style per-sender media
// keys, derived from a call secret that travels inside an MLS-encrypted call invite.

pub const GROUP_KEY_LEN_BYTES: usize = 32;

/// Random 32-byte key (call secrets, random ids).
pub fn generate_group_key() -> [u8; GROUP_KEY_LEN_BYTES] {
    let mut key = [0u8; GROUP_KEY_LEN_BYTES];
    OsRng.fill_bytes(&mut key);
    key
}

/// Per-sender SFrame-style media key for a group call: HKDF(call_secret,
/// sender identity pub). The forwarder relays ciphertext datagrams without
/// these keys as a mesh node; participants derive every sender's key from the
/// call secret carried in the (MLS-encrypted) group-call invite.
pub fn derive_group_call_media_key(
    call_secret: &[u8],
    sender_pub: &str,
) -> OmniResult<[u8; GROUP_KEY_LEN_BYTES]> {
    let hkdf = Hkdf::<Sha256>::new(Some(b"omni_group_call_media_v1"), call_secret);
    let mut key = [0u8; GROUP_KEY_LEN_BYTES];
    hkdf.expand(sender_pub.as_bytes(), &mut key)
        .map_err(|_| OmniError::Internal("group call media key derivation failed".to_string()))?;
    Ok(key)
}

/// SFrame-style media datagram seal (G6): encrypt an RTP payload under the
/// sender's per-call media key. The output is `[ssrc_tag(4) ‖ seq(8) ‖ AEAD]`.
/// The 12-byte header stays PLAINTEXT so the peer-forwarder can route/select a
/// layer without the key (E2E preserved — it never sees the media). `seq` is the
/// per-key monotone counter; the recipient derives the nonce from it.
pub const GROUP_CALL_MEDIA_HEADER_LEN: usize = 12;

/// Bytes sealing adds to a call-media datagram: the plaintext header, the AEAD tag and — for a group
/// call — the sender's signature. The media engine's packet budget leaves room for it.
pub const CALL_MEDIA_SEAL_OVERHEAD_BYTES: usize = GROUP_CALL_MEDIA_HEADER_LEN + 16 + GROUP_CALL_MEDIA_SIGNATURE_LEN;

/// A group-call media datagram ends with its sender's Ed25519 signature.
pub const GROUP_CALL_MEDIA_SIGNATURE_LEN: usize = 64;

const GROUP_CALL_MEDIA_SIGNING_CONTEXT: &[u8] = b"omni-group-call-media-v1\0";

/// A group-call sender's signing key: the member's own Ed25519 key, the one its MLS leaf is signed
/// with. Every member can derive every other member's media KEY from the call secret, so the key does
/// not tell who sealed a frame; the signature does (security review 2026-11-09, F1).
pub struct GroupCallMediaSigner(Ed25519SigningKey);

impl GroupCallMediaSigner {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(Ed25519SigningKey::from_bytes(seed))
    }
}

impl std::fmt::Debug for GroupCallMediaSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupCallMediaSigner")
    }
}

impl Clone for GroupCallMediaSigner {
    fn clone(&self) -> Self {
        Self(Ed25519SigningKey::from_bytes(&self.0.to_bytes()))
    }
}

/// A group-call member's public key, to check its frames' signatures.
#[derive(Debug, Clone)]
pub struct GroupCallMediaVerifier(Ed25519VerifyingKey);

impl GroupCallMediaVerifier {
    /// From the member's public key (32 bytes); `None` for bytes that are no Ed25519 key.
    pub fn from_public_key(public_key: &[u8]) -> Option<Self> {
        let bytes: [u8; 32] = public_key.try_into().ok()?;
        Ed25519VerifyingKey::from_bytes(&bytes).ok().map(Self)
    }
}

fn group_call_media_signed_bytes(call_id: &str, sealed: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(GROUP_CALL_MEDIA_SIGNING_CONTEXT.len() + call_id.len() + 1 + sealed.len());
    bytes.extend_from_slice(GROUP_CALL_MEDIA_SIGNING_CONTEXT);
    bytes.extend_from_slice(call_id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(sealed);
    bytes
}

/// Sign a sealed group-call datagram as its sender: the signature over the call id and the whole
/// sealed datagram (header and ciphertext) is appended.
pub fn sign_group_call_media(signer: &GroupCallMediaSigner, call_id: &str, mut sealed: Vec<u8>) -> Vec<u8> {
    let signature = signer.0.sign(&group_call_media_signed_bytes(call_id, &sealed));
    sealed.extend_from_slice(&signature.to_bytes());
    sealed
}

/// The sealed datagram of `signed`, when its signature is `verifier`'s over this call; `None`
/// otherwise — a frame another member made under this one's name, or one of another call.
pub fn verified_group_call_media<'a>(verifier: &GroupCallMediaVerifier, call_id: &str, signed: &'a [u8]) -> Option<&'a [u8]> {
    if signed.len() < GROUP_CALL_MEDIA_HEADER_LEN + GROUP_CALL_MEDIA_SIGNATURE_LEN {
        return None;
    }
    let (sealed, signature) = signed.split_at(signed.len() - GROUP_CALL_MEDIA_SIGNATURE_LEN);
    let signature = Ed25519Signature::from_slice(signature).ok()?;
    verifier
        .0
        .verify(&group_call_media_signed_bytes(call_id, sealed), &signature)
        .ok()
        .map(|()| sealed)
}

/// Size of each side's contribution to a 1:1 call's media keys.
pub const DIRECT_CALL_MEDIA_SHARE_BYTES: usize = 32;

/// Media keys of a 1:1 call, one per direction: `(caller → callee, callee → caller)`. Each side
/// contributes 32 random bytes — the caller in its invite, the callee in its accept. Both travel
/// inside the pair's ratchet session, so only the two ends ever know them, and a relay on the path
/// carries ciphertext it can neither read nor alter. A new call is a new pair of shares; the call id
/// separates calls even if a share repeated. Sealed and opened with the same
/// [`seal_group_call_media`] / [`open_group_call_media`] as group media.
pub fn derive_direct_call_media_keys(
    caller_share: &[u8],
    callee_share: &[u8],
    call_id: &str,
) -> OmniResult<([u8; GROUP_KEY_LEN_BYTES], [u8; GROUP_KEY_LEN_BYTES])> {
    if caller_share.len() != DIRECT_CALL_MEDIA_SHARE_BYTES
        || callee_share.len() != DIRECT_CALL_MEDIA_SHARE_BYTES
    {
        return Err(OmniError::InvalidInput(
            "a call media key share must be 32 bytes".to_string(),
        ));
    }
    let mut secret = Vec::with_capacity(2 * DIRECT_CALL_MEDIA_SHARE_BYTES);
    secret.extend_from_slice(caller_share);
    secret.extend_from_slice(callee_share);
    let hkdf = Hkdf::<Sha256>::new(Some(b"omni_direct_call_media_v1"), &secret);
    let mut caller_to_callee = [0u8; GROUP_KEY_LEN_BYTES];
    let mut callee_to_caller = [0u8; GROUP_KEY_LEN_BYTES];
    for (direction, key) in [("caller", &mut caller_to_callee), ("callee", &mut callee_to_caller)] {
        hkdf.expand(format!("{call_id}|{direction}").as_bytes(), key)
            .map_err(|_| OmniError::Internal("call media key derivation failed".to_string()))?;
    }
    Ok((caller_to_callee, callee_to_caller))
}

pub fn seal_group_call_media(
    media_key: &[u8],
    ssrc_tag: u32,
    seq: u64,
    rtp_payload: &[u8],
) -> OmniResult<Vec<u8>> {
    let nonce = derive_group_call_media_nonce(ssrc_tag, seq);
    let mut header = Vec::with_capacity(GROUP_CALL_MEDIA_HEADER_LEN);
    header.extend_from_slice(&ssrc_tag.to_be_bytes());
    header.extend_from_slice(&seq.to_be_bytes());
    let cipher = ChaCha20Poly1305::new_from_slice(media_key)
        .map_err(|error| OmniError::Internal(format!("call media key init failed: {error}")))?;
    let ciphertext = cipher
        .encrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: rtp_payload,
                aad: &header, // binds the routing header to the ciphertext
            },
        )
        .map_err(|_| OmniError::Internal("group call media encryption failed".to_string()))?;
    let mut out = Vec::with_capacity(header.len() + ciphertext.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Read the plaintext routing header of a sealed media datagram without the key
/// — this is exactly what the forwarder uses to route/select (it never decrypts).
pub fn peek_group_call_media_header(data: &[u8]) -> Option<(u32, u64)> {
    if data.len() < GROUP_CALL_MEDIA_HEADER_LEN {
        return None;
    }
    let ssrc = u32::from_be_bytes(data[..4].try_into().ok()?);
    let seq = u64::from_be_bytes(data[4..12].try_into().ok()?);
    Some((ssrc, seq))
}

pub fn open_group_call_media(media_key: &[u8], data: &[u8]) -> OmniResult<Vec<u8>> {
    if data.len() <= GROUP_CALL_MEDIA_HEADER_LEN {
        return Err(OmniError::InvalidInput(
            "group call media datagram too short".to_string(),
        ));
    }
    let (header, ciphertext) = data.split_at(GROUP_CALL_MEDIA_HEADER_LEN);
    let ssrc = u32::from_be_bytes(header[..4].try_into().expect("4 bytes"));
    let seq = u64::from_be_bytes(header[4..12].try_into().expect("8 bytes"));
    let nonce = derive_group_call_media_nonce(ssrc, seq);
    let cipher = ChaCha20Poly1305::new_from_slice(media_key)
        .map_err(|error| OmniError::Internal(format!("call media key init failed: {error}")))?;
    cipher
        .decrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: ciphertext,
                aad: header,
            },
        )
        .map_err(|_| OmniError::InvalidInput("group call media authentication failed".to_string()))
}

fn derive_group_call_media_nonce(ssrc_tag: u32, seq: u64) -> [u8; CHACHA_NONCE_LEN] {
    let mut nonce = [0u8; CHACHA_NONCE_LEN];
    nonce[..4].copy_from_slice(&ssrc_tag.to_be_bytes());
    nonce[4..].copy_from_slice(&seq.to_be_bytes());
    nonce
}
