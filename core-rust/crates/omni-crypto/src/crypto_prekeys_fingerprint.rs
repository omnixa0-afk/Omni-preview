pub fn build_prekey_bundle(
    identity_key: &LocalIdentityKeyPair,
    device_key: &LocalDeviceKeyPair,
    signed_prekey: &SignedPrekey,
    one_time_prekey: Option<&OneTimePrekey>,
    generated_at_unix_ms: i64,
) -> OmniResult<PrekeyBundle> {
    if identity_key.account_id != device_key.account_id {
        return Err(OmniError::InvalidInput(
            "identity key and device key belong to different accounts".to_string(),
        ));
    }
    if signed_prekey.device_id != device_key.device_id {
        return Err(OmniError::InvalidInput(
            "signed prekey belongs to a different device".to_string(),
        ));
    }
    if let Some(one_time_prekey) = one_time_prekey {
        if one_time_prekey.device_id != device_key.device_id {
            return Err(OmniError::InvalidInput(
                "one-time prekey belongs to a different device".to_string(),
            ));
        }
    }

    Ok(PrekeyBundle {
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        account_id: identity_key.account_id.clone(),
        device_id: device_key.device_id.clone(),
        identity_key: identity_key.public_key,
        identity_signing_key: identity_key.signing_public_key,
        device_key: device_key.public_key,
        signed_prekey: SignedPrekeyDescriptor {
            prekey_id: signed_prekey.prekey_id,
            public_key: signed_prekey.public_key,
            signature: signed_prekey.signature.clone(),
            expires_at_unix_ms: signed_prekey.expires_at_unix_ms,
        },
        one_time_prekey: one_time_prekey.map(|prekey| OneTimePrekeyDescriptor {
            prekey_id: prekey.prekey_id,
            public_key: prekey.public_key,
        }),
        generated_at_unix_ms,
        device_certificate_chain: None,
    })
}

pub fn verify_prekey_bundle(bundle: &PrekeyBundle) -> OmniResult<()> {
    if bundle.suite_version != CRYPTO_SUITE_VERSION_CURRENT {
        return Err(OmniError::InvalidInput(format!(
            "unsupported crypto suite version '{}'",
            bundle.suite_version
        )));
    }

    let message = signed_prekey_signature_message(
        &bundle.account_id,
        &bundle.device_id,
        bundle.identity_key,
        bundle.identity_signing_key,
        bundle.signed_prekey.public_key,
    );
    verify_signature(&bundle.identity_signing_key, &message, &bundle.signed_prekey.signature)
}

#[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
pub fn derive_fingerprint_material(
    local_account_id: &AccountId,
    local_device_id: &DeviceId,
    local_identity_key: IdentityPublicKey,
    local_identity_signing_key: IdentitySigningPublicKey,
    local_device_key: DevicePublicKey,
    remote_account_id: &AccountId,
    remote_device_id: &DeviceId,
    remote_identity_key: IdentityPublicKey,
    remote_identity_signing_key: IdentitySigningPublicKey,
    remote_device_key: DevicePublicKey,
) -> FingerprintMaterial {
    FingerprintMaterial {
        local_account_id: local_account_id.clone(),
        local_device_id: local_device_id.clone(),
        local_identity_key,
        local_identity_signing_key,
        local_device_key,
        remote_account_id: remote_account_id.clone(),
        remote_device_id: remote_device_id.clone(),
        remote_identity_key,
        remote_identity_signing_key,
        remote_device_key,
    }
}

pub fn fingerprint_hex(material: &FingerprintMaterial) -> String {
    hex::encode(sha256(&fingerprint_input(material)))
}

pub fn safety_number(material: &FingerprintMaterial) -> String {
    let digest = sha256(&fingerprint_input(material));
    let digits = digest
        .iter()
        .take(15)
        .map(|value| format!("{:02}", value % 100))
        .collect::<Vec<_>>()
        .join("");

    digits
        .chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

