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
