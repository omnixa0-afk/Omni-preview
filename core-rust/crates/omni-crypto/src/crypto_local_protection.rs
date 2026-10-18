fn secret_protection_scheme_for_target(target_os: &str) -> SecretProtectionScheme {
    match target_os {
        "windows" => SecretProtectionScheme::DpapiV1,
        "android" => SecretProtectionScheme::PlatformWrappedV1,
        _ => SecretProtectionScheme::PlaintextFallbackV1,
    }
}

fn current_secret_protection_scheme() -> SecretProtectionScheme {
    secret_protection_scheme_for_target(std::env::consts::OS)
}

pub fn protect_bytes(bytes: &[u8]) -> OmniResult<ProtectedSecret> {
    match current_secret_protection_scheme() {
        SecretProtectionScheme::DpapiV1 => {
            #[cfg(windows)]
            {
                let protected = windows_protect(bytes)?;
                Ok(ProtectedSecret {
                    scheme: SecretProtectionScheme::DpapiV1,
                    blob_hex: hex::encode(protected),
                })
            }
            #[cfg(not(windows))]
            {
                Err(OmniError::Internal(
                    "dpapi secret protection is not available on this target".to_string(),
                ))
            }
        }
        SecretProtectionScheme::PlatformWrappedV1 => {
            let protected = platform_wrap(bytes)?;
            Ok(ProtectedSecret {
                scheme: SecretProtectionScheme::PlatformWrappedV1,
                blob_hex: hex::encode(protected),
            })
        }
        SecretProtectionScheme::PlaintextFallbackV1 => Ok(ProtectedSecret {
            scheme: SecretProtectionScheme::PlaintextFallbackV1,
            blob_hex: hex::encode(bytes),
        }),
    }
}

pub fn unprotect_bytes(secret: &ProtectedSecret) -> OmniResult<Vec<u8>> {
    let blob = hex::decode(secret.blob_hex.trim()).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid protected secret blob hex '{}': {error}",
            secret.blob_hex
        ))
    })?;

    match secret.scheme {
        #[cfg(windows)]
        SecretProtectionScheme::DpapiV1 => windows_unprotect(&blob),
        #[cfg(not(windows))]
        SecretProtectionScheme::DpapiV1 => Err(OmniError::Internal(
            "dpapi secret protection is not available on this target".to_string(),
        )),
        SecretProtectionScheme::PlatformWrappedV1 => platform_unwrap(&blob),
        SecretProtectionScheme::PlaintextFallbackV1 => {
            #[cfg(target_os = "android")]
            {
                Err(OmniError::Internal(
                    "plaintext secret protection is not allowed on android targets".to_string(),
                ))
            }
            #[cfg(not(target_os = "android"))]
            {
                Ok(blob)
            }
        }
    }
}

pub fn encrypt_at_rest_bytes(
    master_key: &[u8; 32],
    aad: &[u8],
    plaintext: &[u8],
) -> OmniResult<AtRestEncryptedBlob> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|error| OmniError::Internal(format!("aes-gcm key init failed: {error}")))?;
    let nonce = random_bytes(AES_GCM_NONCE_LEN)?;
    let sealed = cipher
        .encrypt(
            AesNonce::from_slice(&nonce),
            AeadPayload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| OmniError::Internal("aes-gcm encryption failed".to_string()))?;
    let (ciphertext, mac) = split_ciphertext_and_tag(&sealed)?;

    Ok(AtRestEncryptedBlob {
        nonce_hex: hex::encode(nonce),
        ciphertext_hex: hex::encode(ciphertext),
        mac_hex: hex::encode(mac),
    })
}

pub fn decrypt_at_rest_bytes(
    master_key: &[u8; 32],
    aad: &[u8],
    blob: &AtRestEncryptedBlob,
) -> OmniResult<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(master_key)
        .map_err(|error| OmniError::Internal(format!("aes-gcm key init failed: {error}")))?;
    let nonce = hex_decode_exact_len("at_rest_nonce_hex", &blob.nonce_hex, AES_GCM_NONCE_LEN)?;
    let ciphertext = hex::decode(blob.ciphertext_hex.trim()).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid at-rest ciphertext hex '{}': {error}",
            blob.ciphertext_hex
        ))
    })?;
    let mac = hex_decode_exact_len("at_rest_mac_hex", &blob.mac_hex, AEAD_TAG_LEN)?;
    let mut combined = ciphertext;
    combined.extend_from_slice(&mac);

    cipher
        .decrypt(
            AesNonce::from_slice(&nonce),
            AeadPayload {
                msg: &combined,
                aad,
            },
        )
        .map_err(|_| OmniError::InvalidInput("aes-gcm payload authentication failed".to_string()))
}

