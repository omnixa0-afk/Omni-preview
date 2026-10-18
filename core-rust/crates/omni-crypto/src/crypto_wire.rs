pub fn identity_public_key_to_wire(key: IdentityPublicKey) -> String {
    hex::encode(key.0)
}

pub fn identity_signing_public_key_to_wire(key: IdentitySigningPublicKey) -> String {
    hex::encode(key.0)
}

pub fn device_public_key_to_wire(key: DevicePublicKey) -> String {
    hex::encode(key.0)
}

pub fn prekey_public_key_to_wire(key: PrekeyPublicKey) -> String {
    hex::encode(key.0)
}

pub fn ephemeral_public_key_to_wire(key: EphemeralPublicKey) -> String {
    hex::encode(key.0)
}

pub fn signature_to_wire(signature: Signature) -> (String, String) {
    (
        hex::encode(&signature.bytes[..32]),
        hex::encode(&signature.bytes[32..]),
    )
}

pub fn identity_public_key_from_wire(value: &str) -> OmniResult<IdentityPublicKey> {
    Ok(IdentityPublicKey(hex_decode_array("identity_public_key", value)?))
}

pub fn identity_signing_public_key_from_wire(
    value: &str,
) -> OmniResult<IdentitySigningPublicKey> {
    Ok(IdentitySigningPublicKey(hex_decode_array(
        "identity_signing_public_key",
        value,
    )?))
}

pub fn device_public_key_from_wire(value: &str) -> OmniResult<DevicePublicKey> {
    Ok(DevicePublicKey(hex_decode_array("device_public_key", value)?))
}

pub fn prekey_public_key_from_wire(value: &str) -> OmniResult<PrekeyPublicKey> {
    Ok(PrekeyPublicKey(hex_decode_array("prekey_public_key", value)?))
}

pub fn ephemeral_public_key_from_wire(value: &str) -> OmniResult<EphemeralPublicKey> {
    Ok(EphemeralPublicKey(hex_decode_array(
        "ephemeral_public_key",
        value,
    )?))
}

pub fn signature_from_wire(left: &str, right: &str) -> OmniResult<Signature> {
    let left = hex::decode(left.trim()).map_err(|error| {
        OmniError::InvalidInput(format!("invalid signature_r hex value '{left}': {error}"))
    })?;
    let right = hex::decode(right.trim()).map_err(|error| {
        OmniError::InvalidInput(format!("invalid signature_s hex value '{right}': {error}"))
    })?;
    if left.len() != 32 || right.len() != 32 {
        return Err(OmniError::InvalidInput(
            "signature wire halves must each be 32 bytes".to_string(),
        ));
    }
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(&left);
    bytes.extend_from_slice(&right);
    Ok(Signature { bytes })
}

