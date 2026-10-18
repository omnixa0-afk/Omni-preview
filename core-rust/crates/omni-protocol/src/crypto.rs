use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCryptoDescriptorV1 {
    pub version: u16,
    pub account_id: String,
    pub identity_public_key: String,
    pub identity_signing_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCryptoDescriptorV1 {
    pub version: u16,
    pub device_id: String,
    pub account_id: String,
    pub device_label: String,
    pub device_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPrekeyDescriptorV1 {
    pub prekey_id: u32,
    pub public_key: String,
    pub signature_r: String,
    pub signature_s: String,
    pub expires_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneTimePrekeyDescriptorV1 {
    pub prekey_id: u32,
    pub public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrekeyBundleV1 {
    pub version: u16,
    pub account: AccountCryptoDescriptorV1,
    pub device: DeviceCryptoDescriptorV1,
    pub signed_prekey: SignedPrekeyDescriptorV1,
    pub one_time_prekey: Option<OneTimePrekeyDescriptorV1>,
    pub generated_at_unix_ms: i64,
    /// The device's certificate chain (omni-crypto `DeviceCertificateChainV1`, JSON) — see
    /// docs/decisions/multi-device-equal-nodes.md. Absent from senders that carry none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_certificate_chain_json: Option<String>,
}
