use omni_types::{OmniError, OmniResult};
use serde::Serialize;

use crate::{
    AccountCryptoDescriptorV1, AccountPublicDescriptorV1, ContactVerificationPayloadV1,
    DeviceCryptoDescriptorV1, DevicePublicDescriptorV1, ImportableContactPayloadV1,
    MyVerificationPayloadV1, PrekeyBundleV1, ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
    CONTACT_VERIFICATION_PAYLOAD_VERSION_V1, DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1,
    IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1, MY_VERIFICATION_PAYLOAD_VERSION_V1,
    PREKEY_BUNDLE_VERSION_V1, PUBLIC_DESCRIPTOR_VERSION_V1,
};

pub fn validate_account_public_descriptor_v1(
    descriptor: &AccountPublicDescriptorV1,
) -> OmniResult<()> {
    if descriptor.version != PUBLIC_DESCRIPTOR_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported account descriptor version '{}'",
            descriptor.version
        )));
    }
    if descriptor.account_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "account descriptor account_id must not be empty".to_string(),
        ));
    }
    if descriptor.display_name.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "account descriptor display_name must not be empty".to_string(),
        ));
    }

    Ok(())
}

pub fn validate_device_public_descriptor_v1(
    descriptor: &DevicePublicDescriptorV1,
) -> OmniResult<()> {
    if descriptor.version != PUBLIC_DESCRIPTOR_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported device descriptor version '{}'",
            descriptor.version
        )));
    }
    if descriptor.device_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device descriptor device_id must not be empty".to_string(),
        ));
    }
    if descriptor.account_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device descriptor account_id must not be empty".to_string(),
        ));
    }
    if descriptor.device_label.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device descriptor device_label must not be empty".to_string(),
        ));
    }

    Ok(())
}

pub fn validate_contact_verification_payload_v1(
    payload: &ContactVerificationPayloadV1,
) -> OmniResult<()> {
    if payload.version != CONTACT_VERIFICATION_PAYLOAD_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported contact verification payload version '{}'",
            payload.version
        )));
    }
    validate_account_public_descriptor_v1(&payload.account)?;
    validate_devices_for_account(&payload.devices, &payload.account.account_id)?;
    validate_fingerprint_and_safety_number(&payload.fingerprint, &payload.safety_number)?;

    Ok(())
}

pub fn validate_my_verification_payload_v1(payload: &MyVerificationPayloadV1) -> OmniResult<()> {
    if payload.version != MY_VERIFICATION_PAYLOAD_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported my verification payload version '{}'",
            payload.version
        )));
    }
    validate_account_public_descriptor_v1(&payload.account)?;
    validate_devices_for_account(&payload.devices, &payload.account.account_id)?;
    validate_fingerprint_and_safety_number(&payload.fingerprint, &payload.safety_number)?;

    Ok(())
}

pub fn validate_importable_contact_payload_v1(
    payload: &ImportableContactPayloadV1,
) -> OmniResult<()> {
    if payload.version != IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported importable contact payload version '{}'",
            payload.version
        )));
    }
    validate_account_public_descriptor_v1(&payload.account)?;
    validate_devices_for_account(&payload.devices, &payload.account.account_id)?;
    validate_fingerprint_and_safety_number(&payload.fingerprint, &payload.safety_number)?;

    Ok(())
}

pub fn contact_payload_from_my_payload(
    payload: &MyVerificationPayloadV1,
) -> OmniResult<ContactVerificationPayloadV1> {
    validate_my_verification_payload_v1(payload)?;

    Ok(ContactVerificationPayloadV1 {
        version: CONTACT_VERIFICATION_PAYLOAD_VERSION_V1,
        account: payload.account.clone(),
        devices: payload.devices.clone(),
        fingerprint: payload.fingerprint.clone(),
        safety_number: payload.safety_number.clone(),
        exported_at_unix_ms: payload.exported_at_unix_ms,
    })
}

pub fn importable_contact_payload_from_verification(
    payload: &ContactVerificationPayloadV1,
) -> OmniResult<ImportableContactPayloadV1> {
    validate_contact_verification_payload_v1(payload)?;

    Ok(ImportableContactPayloadV1 {
        version: IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1,
        account: payload.account.clone(),
        devices: payload.devices.clone(),
        fingerprint: payload.fingerprint.clone(),
        safety_number: payload.safety_number.clone(),
        exported_at_unix_ms: payload.exported_at_unix_ms,
    })
}

pub fn validate_account_crypto_descriptor_v1(
    descriptor: &AccountCryptoDescriptorV1,
) -> OmniResult<()> {
    if descriptor.version != ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported account crypto descriptor version '{}'",
            descriptor.version
        )));
    }
    if descriptor.account_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "account crypto descriptor account_id must not be empty".to_string(),
        ));
    }
    validate_hex_field(
        "account crypto descriptor identity_public_key",
        &descriptor.identity_public_key,
    )?;
    validate_hex_field(
        "account crypto descriptor identity_signing_public_key",
        &descriptor.identity_signing_public_key,
    )?;
    Ok(())
}

pub fn validate_device_crypto_descriptor_v1(
    descriptor: &DeviceCryptoDescriptorV1,
) -> OmniResult<()> {
    if descriptor.version != DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported device crypto descriptor version '{}'",
            descriptor.version
        )));
    }
    if descriptor.device_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device crypto descriptor device_id must not be empty".to_string(),
        ));
    }
    if descriptor.account_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device crypto descriptor account_id must not be empty".to_string(),
        ));
    }
    if descriptor.device_label.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device crypto descriptor device_label must not be empty".to_string(),
        ));
    }
    validate_hex_field(
        "device crypto descriptor device_public_key",
        &descriptor.device_public_key,
    )?;
    Ok(())
}

pub fn validate_prekey_bundle_v1(bundle: &PrekeyBundleV1) -> OmniResult<()> {
    if bundle.version != PREKEY_BUNDLE_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported prekey bundle version '{}'",
            bundle.version
        )));
    }
    validate_account_crypto_descriptor_v1(&bundle.account)?;
    validate_device_crypto_descriptor_v1(&bundle.device)?;
    if bundle.device.account_id != bundle.account.account_id {
        return Err(OmniError::InvalidInput(
            "device crypto descriptor account_id does not match account crypto descriptor"
                .to_string(),
        ));
    }
    validate_hex_field("signed prekey public_key", &bundle.signed_prekey.public_key)?;
    validate_hex_field(
        "signed prekey signature_r",
        &bundle.signed_prekey.signature_r,
    )?;
    validate_hex_field(
        "signed prekey signature_s",
        &bundle.signed_prekey.signature_s,
    )?;
    if let Some(one_time_prekey) = &bundle.one_time_prekey {
        validate_hex_field("one-time prekey public_key", &one_time_prekey.public_key)?;
    }
    Ok(())
}

fn validate_devices_for_account(
    devices: &[DevicePublicDescriptorV1],
    account_id: &str,
) -> OmniResult<()> {
    if devices.is_empty() {
        return Err(OmniError::InvalidInput(
            "verification payload must include at least one device".to_string(),
        ));
    }

    for device in devices {
        validate_device_public_descriptor_v1(device)?;
        if device.account_id != account_id {
            return Err(OmniError::InvalidInput(
                "device descriptor account_id does not match account descriptor".to_string(),
            ));
        }
    }

    Ok(())
}

fn validate_fingerprint_and_safety_number(
    fingerprint: &str,
    safety_number: &str,
) -> OmniResult<()> {
    if fingerprint.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "verification payload fingerprint must not be empty".to_string(),
        ));
    }
    if safety_number.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "verification payload safety_number must not be empty".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_hex_field(field_name: &str, value: &str) -> OmniResult<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must not be empty"
        )));
    }
    if !trimmed.len().is_multiple_of(2) {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must contain an even-length hex string"
        )));
    }
    if !trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must contain only hex characters"
        )));
    }

    Ok(())
}

pub(crate) fn validate_max_utf8_len(
    field_name: &str,
    value: &str,
    max_bytes: usize,
) -> OmniResult<()> {
    if value.len() > max_bytes {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} exceeds the {max_bytes} byte limit"
        )));
    }

    Ok(())
}

pub(crate) fn validate_hex_decoded_len(
    field_name: &str,
    value: &str,
    max_bytes: usize,
) -> OmniResult<()> {
    validate_hex_field(field_name, value)?;
    let decoded_len = value.trim().len() / 2;
    if decoded_len > max_bytes {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} exceeds the {max_bytes} byte decoded limit"
        )));
    }

    Ok(())
}

pub(crate) fn validate_serialized_json_len<T: Serialize>(
    field_name: &str,
    value: &T,
    max_bytes: usize,
) -> OmniResult<()> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        OmniError::InvalidInput(format!(
            "failed to encode {field_name} for size validation: {error}"
        ))
    })?;
    if encoded.len() > max_bytes {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} exceeds the {max_bytes} byte serialized limit"
        )));
    }

    Ok(())
}

pub(crate) fn validate_non_empty_field(field_name: &str, value: &str) -> OmniResult<()> {
    if value.trim().is_empty() {
        return Err(OmniError::InvalidInput(format!(
            "{} must not be empty",
            field_name
        )));
    }
    Ok(())
}

pub(crate) fn validate_positive_i64(field_name: &str, value: i64) -> OmniResult<()> {
    if value <= 0 {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must be positive"
        )));
    }

    Ok(())
}

pub(crate) fn validate_non_negative_i64(field_name: &str, value: i64) -> OmniResult<()> {
    if value < 0 {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must not be negative"
        )));
    }

    Ok(())
}

pub(crate) fn validate_ascii_hexdigits_only(field_name: &str, value: &str) -> OmniResult<()> {
    if !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must contain only hex characters"
        )));
    }

    Ok(())
}
