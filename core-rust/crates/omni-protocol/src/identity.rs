use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPublicDescriptorV1 {
    pub version: u16,
    pub account_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePublicDescriptorV1 {
    pub version: u16,
    pub device_id: String,
    pub account_id: String,
    pub device_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationPayloadV1 {
    pub version: u16,
    pub account: AccountPublicDescriptorV1,
    pub devices: Vec<DevicePublicDescriptorV1>,
    pub fingerprint: String,
    pub safety_number: String,
    pub exported_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MyVerificationPayloadV1 {
    pub version: u16,
    pub account: AccountPublicDescriptorV1,
    pub devices: Vec<DevicePublicDescriptorV1>,
    pub fingerprint: String,
    pub safety_number: String,
    pub exported_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportableContactPayloadV1 {
    pub version: u16,
    pub account: AccountPublicDescriptorV1,
    pub devices: Vec<DevicePublicDescriptorV1>,
    pub fingerprint: String,
    pub safety_number: String,
    pub exported_at_unix_ms: i64,
}
