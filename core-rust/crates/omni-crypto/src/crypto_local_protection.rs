/// Development only, and only where the platform has no key store wired in yet (not Windows, not
/// Android): `1` keeps secrets as plain hex, as every such platform did before 2026-10-20.
const INSECURE_PLAINTEXT_SECRETS_ENV: &str = "OMNI_INSECURE_PLAINTEXT_SECRETS";

/// Windows seals with DPAPI. Everywhere else secrets are wrapped with a key the host supplies
/// (`configure_platform_secret_wrap_key_hex`, or `OMNI_PLATFORM_SECRET_WRAP_KEY_HEX` for a headless
/// node): the Android Keystore, later the macOS Keychain or libsecret. Without that key nothing is
/// written or read — the profile does not start, rather than leave its keys in the clear (security
/// review 2026-10-18, V-16).
fn secret_protection_scheme_for_target(target_os: &str, plaintext_allowed: bool) -> SecretProtectionScheme {
    match target_os {
        "windows" => SecretProtectionScheme::DpapiV1,
        "android" => SecretProtectionScheme::PlatformWrappedV1,
        _ if plaintext_allowed => SecretProtectionScheme::PlaintextFallbackV1,
        _ => SecretProtectionScheme::PlatformWrappedV1,
    }
}

fn plaintext_secrets_allowed() -> bool {
    std::env::var(INSECURE_PLAINTEXT_SECRETS_ENV).is_ok_and(|value| value.trim() == "1")
}

fn current_secret_protection_scheme() -> SecretProtectionScheme {
    secret_protection_scheme_for_target(std::env::consts::OS, plaintext_secrets_allowed())
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

/// The secret in the clear; wiped from memory when the caller drops it.
pub fn unprotect_bytes(secret: &ProtectedSecret) -> OmniResult<Zeroizing<Vec<u8>>> {
    let blob = hex::decode(secret.blob_hex.trim()).map_err(|error| {
        OmniError::InvalidInput(format!(
            "invalid protected secret blob hex '{}': {error}",
            secret.blob_hex
        ))
    })?;

    match secret.scheme {
        #[cfg(windows)]
        SecretProtectionScheme::DpapiV1 => windows_unprotect(&blob).map(Zeroizing::new),
        #[cfg(not(windows))]
        SecretProtectionScheme::DpapiV1 => Err(OmniError::Internal(
            "dpapi secret protection is not available on this target".to_string(),
        )),
        SecretProtectionScheme::PlatformWrappedV1 => platform_unwrap(&blob).map(Zeroizing::new),
        // Read only where it may be written: a row someone put into the database as
        // `plaintext_fallback_v1` would otherwise stand in for our own key (V-16/V-23).
        SecretProtectionScheme::PlaintextFallbackV1 => {
            if secret_protection_scheme_for_target(std::env::consts::OS, plaintext_secrets_allowed())
                == SecretProtectionScheme::PlaintextFallbackV1
            {
                Ok(Zeroizing::new(blob))
            } else {
                Err(OmniError::InvalidInput(format!(
                    "an unprotected secret is not accepted here (development only: {INSECURE_PLAINTEXT_SECRETS_ENV}=1 \
                     where the platform has no key store)"
                )))
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

