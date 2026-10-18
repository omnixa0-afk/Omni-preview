use omni_identity::{ContactId, VerificationMethod};
use omni_protocol::{ContactVerificationPayloadV1, ImportableContactPayloadV1};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AddContactByIdentifierRequest {
    pub(crate) account_id: String,
    pub(crate) petname: String,
    pub(crate) public_display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RequestContactByIdentifierRequest {
    pub(crate) account_id: String,
    pub(crate) petname: String,
    pub(crate) public_display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AddContactFromPayloadRequest {
    pub(crate) payload: ImportableContactPayloadV1,
    pub(crate) petname: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ContactIdRequest {
    pub(crate) contact_id: ContactId,
}

/// Friend-proxy (Axis 2): per-contact "carry this friend's traffic through me" consent toggle.
/// See docs/decisions/friend-proxy-by-consent.md (Phase C).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetContactFriendRelayConsentRequest {
    pub(crate) contact_id: ContactId,
    pub(crate) consent: bool,
}

/// Remote access (remote-access-for-contacts.md): let this contact control our computer, or stop.
/// `mode` absent / null = stop (revoke).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetContactRemoteAccessGrantedRequest {
    pub(crate) contact_id: ContactId,
    #[serde(default)]
    pub(crate) mode: Option<omni_types::RemoteAccessMode>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RenameContactRequest {
    pub(crate) contact_id: ContactId,
    pub(crate) petname: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ConfirmContactVerificationRequest {
    pub(crate) contact_id: ContactId,
    pub(crate) payload: ContactVerificationPayloadV1,
    pub(crate) method: VerificationMethod,
}

/// Take revoked devices out of the Settings list (the revocation stays).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HideRevokedOwnDevicesRequest {
    /// One revoked device; absent = all of them.
    #[serde(default)]
    pub(crate) device_id: Option<String>,
}

/// Take one of our own devices back (multi-device-equal-nodes.md, «Устройства»).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RevokeOwnDeviceRequest {
    pub(crate) device_id: String,
    /// The profile password, when the folder is protected by one.
    #[serde(default)]
    pub(crate) password: Option<String>,
}

/// "The account key was stolen": move the account to a new working key keeping these devices
/// (multi-device-equal-nodes.md, «Переезд при краже рабочего ключа»).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MoveAccountKeyRequest {
    pub(crate) keep_device_ids: Vec<String>,
    /// The recovery phrase, when this computer cannot open the stored root (a copied folder).
    #[serde(default)]
    pub(crate) phrase: Option<String>,
    /// The profile password, when the folder is protected by one.
    #[serde(default)]
    pub(crate) password: Option<String>,
}
