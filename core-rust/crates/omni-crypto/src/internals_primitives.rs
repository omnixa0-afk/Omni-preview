fn generate_x25519_secret() -> StaticSecret {
    StaticSecret::random_from_rng(OsRng)
}

pub fn random_bytes(length: usize) -> OmniResult<Vec<u8>> {
    let mut bytes = vec![0u8; length];
    OsRng.fill_bytes(&mut bytes);
    Ok(bytes)
}

fn hkdf_expand(
    salt: Option<&[u8; 32]>,
    input_key_material: &[u8],
    info: &[u8],
) -> OmniResult<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::new(salt.map(|value| value.as_slice()), input_key_material);
    let mut output = [0u8; 32];
    hkdf.expand(info, &mut output)
        .map_err(|_| OmniError::Internal("hkdf expand failed".to_string()))?;
    Ok(output)
}

fn hkdf_expand_multi(
    salt: Option<&[u8; 32]>,
    input_key_material: &[u8],
    info: &[u8],
    len: usize,
) -> OmniResult<Vec<u8>> {
    let hkdf = Hkdf::<Sha256>::new(salt.map(|value| value.as_slice()), input_key_material);
    let mut output = vec![0u8; len];
    hkdf.expand(info, &mut output)
        .map_err(|_| OmniError::Internal("hkdf expand failed".to_string()))?;
    Ok(output)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut result = [0u8; 32];
    result.copy_from_slice(&digest);
    result
}

fn x25519_from_identity_public(public: &IdentityPublicKey) -> X25519PublicKey {
    X25519PublicKey::from(public.0)
}

fn x25519_from_prekey_public(public: &PrekeyPublicKey) -> X25519PublicKey {
    X25519PublicKey::from(public.0)
}

fn x25519_from_ephemeral_public(public: &EphemeralPublicKey) -> X25519PublicKey {
    X25519PublicKey::from(public.0)
}

fn ed25519_verifying_key_from_public(
    public: &IdentitySigningPublicKey,
) -> OmniResult<Ed25519VerifyingKey> {
    Ed25519VerifyingKey::from_bytes(&public.0).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid ed25519 signing public key bytes: {error}"
        ))
    })
}

fn unprotect_x25519_secret(secret: &ProtectedSecret) -> OmniResult<StaticSecret> {
    let bytes = unprotect_bytes(secret)?;
    let raw = slice_to_array::<32>(&bytes)?;
    Ok(StaticSecret::from(raw))
}

fn unprotect_ed25519_signing_key(secret: &ProtectedSecret) -> OmniResult<Ed25519SigningKey> {
    let bytes = unprotect_bytes(secret)?;
    let raw = slice_to_array::<32>(&bytes)?;
    Ok(Ed25519SigningKey::from_bytes(&raw))
}

fn unprotect_chain_key(secret: &ProtectedSecret) -> OmniResult<[u8; 32]> {
    let bytes = unprotect_bytes(secret)?;
    slice_to_array::<32>(&bytes)
}

fn slice_to_array<const N: usize>(bytes: &[u8]) -> OmniResult<[u8; N]> {
    if bytes.len() != N {
        return Err(OmniError::Storage(format!(
            "protected secret had an unexpected length: expected {N}, got {}",
            bytes.len()
        )));
    }
    let mut result = [0u8; N];
    result.copy_from_slice(bytes);
    Ok(result)
}

fn hex_decode_array<const N: usize>(field_name: &str, value: &str) -> OmniResult<[u8; N]> {
    let bytes = hex::decode(value.trim()).map_err(|error| {
        OmniError::InvalidInput(format!("invalid {field_name} hex value '{value}': {error}"))
    })?;
    slice_to_array::<N>(&bytes)
}

fn hex_decode_exact_len(field_name: &str, value: &str, expected_len: usize) -> OmniResult<Vec<u8>> {
    let bytes = hex::decode(value.trim()).map_err(|error| {
        OmniError::InvalidInput(format!("invalid {field_name} hex value '{value}': {error}"))
    })?;
    if bytes.len() != expected_len {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must be {expected_len} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn split_ciphertext_and_tag(sealed: &[u8]) -> OmniResult<(&[u8], &[u8])> {
    if sealed.len() < AEAD_TAG_LEN {
        return Err(OmniError::Internal(
            "encrypted payload did not include an authentication tag".to_string(),
        ));
    }
    Ok(sealed.split_at(sealed.len() - AEAD_TAG_LEN))
}

fn map_serde_to_internal(error: serde_json::Error) -> OmniError {
    OmniError::Internal(format!("failed to serialize encrypted payload: {error}"))
}

fn map_serde_to_invalid(error: serde_json::Error) -> OmniError {
    OmniError::InvalidInput(format!(
        "failed to decode decrypted payload as structured JSON: {error}"
    ))
}

