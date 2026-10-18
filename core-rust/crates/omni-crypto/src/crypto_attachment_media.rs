pub fn seal_attachment_media_chunk(
    media_key: &[u8],
    nonce_seed: &[u8],
    transfer_id: &str,
    chunk_counter: u64,
    aad: &[u8],
    plaintext: &[u8],
) -> OmniResult<Vec<u8>> {
    let nonce = derive_attachment_media_nonce(nonce_seed, transfer_id, chunk_counter)?;
    let cipher = ChaCha20Poly1305::new_from_slice(media_key)
        .map_err(|error| OmniError::Internal(format!("attachment media key init failed: {error}")))?;
    cipher
        .encrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| OmniError::Internal("attachment media encryption failed".to_string()))
}

pub fn open_attachment_media_chunk(
    media_key: &[u8],
    nonce_seed: &[u8],
    transfer_id: &str,
    chunk_counter: u64,
    aad: &[u8],
    ciphertext: &[u8],
) -> OmniResult<Vec<u8>> {
    let nonce = derive_attachment_media_nonce(nonce_seed, transfer_id, chunk_counter)?;
    let cipher = ChaCha20Poly1305::new_from_slice(media_key)
        .map_err(|error| OmniError::Internal(format!("attachment media key init failed: {error}")))?;
    cipher
        .decrypt(
            ChaChaNonce::from_slice(&nonce),
            AeadPayload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| OmniError::InvalidInput("attachment media authentication failed".to_string()))
}

fn derive_attachment_media_nonce(
    nonce_seed: &[u8],
    transfer_id: &str,
    chunk_counter: u64,
) -> OmniResult<[u8; CHACHA_NONCE_LEN]> {
    if nonce_seed.len() != ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES {
        return Err(OmniError::InvalidInput(format!(
            "attachment media nonce seed must be {} bytes",
            ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES
        )));
    }

    let mut digest = Sha256::new();
    digest.update(b"omni_attachment_media_chunk_v1");
    digest.update(nonce_seed);
    digest.update(transfer_id.as_bytes());
    digest.update(chunk_counter.to_le_bytes());
    let digest = digest.finalize();
    let mut nonce = [0_u8; CHACHA_NONCE_LEN];
    nonce.copy_from_slice(&digest[..CHACHA_NONCE_LEN]);
    Ok(nonce)
}
