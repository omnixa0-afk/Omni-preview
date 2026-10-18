use serde::{Deserialize, Serialize};

use crate::{AttachmentMediaProtectionMode, AttachmentOfferV1, AttachmentRecord, CallHistoryEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Sending,
    Sent,
    Delivered,
    Retrying,
    Failed,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub body: String,
    pub status: MessageStatus,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendTextRequest {
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendAttachmentRequest {
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub body: String,
    pub source_file_path: String,
    pub display_file_name: Option<String>,
    pub media_protection_mode: AttachmentMediaProtectionMode,
    /// «Фото» rather than «Файл»: re-encode and show it in the chat.
    #[serde(default)]
    pub as_photo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessageRecord {
    pub id: String,
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub direction: ChatDirection,
    pub body: String,
    pub status: MessageStatus,
    pub remote_delivered_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_history_event: Option<CallHistoryEvent>,
    /// The line came back from the contact's copy of the chat after a restore, not from its author's
    /// own send («со слов собеседника», account-recovery-from-friends.md Б2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub told_by_contact: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMessagesRequest {
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMessagesResponse {
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub messages: Vec<ChatMessageRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMessagesPageRequest {
    pub dialog_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_created_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_message_id: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMessagesPageResponse {
    pub dialog_key: String,
    pub messages: Vec<ChatMessageRecord>,
    pub has_older: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_loaded_created_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_loaded_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatThreadListRefreshSummary {
    pub thread_count: usize,
    pub last_contact_updated_at_unix_ms: Option<i64>,
    pub last_dialog_link_updated_at_unix_ms: Option<i64>,
    pub last_public_profile_updated_at_unix_ms: Option<i64>,
    pub last_message_updated_at_unix_ms: Option<i64>,
    pub last_attachment_updated_at_unix_ms: Option<i64>,
    /// When a chat was last read here: reading moves the list's unread counters, nothing else does.
    #[serde(default)]
    pub last_read_mark_updated_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactsRefreshSummary {
    pub contact_count: usize,
    pub last_contact_updated_at_unix_ms: Option<i64>,
    pub last_public_profile_updated_at_unix_ms: Option<i64>,
    pub last_presence_updated_at_unix_ms: Option<i64>,
    pub last_verification_updated_at_unix_ms: Option<i64>,
    pub last_identity_observation_updated_at_unix_ms: Option<i64>,
    /// When the devices we know of any contact last changed (a bundle came or went, a device was revoked).
    pub last_contact_devices_updated_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedDevicesRefreshSummary {
    pub device_count: usize,
    pub last_device_updated_at_unix_ms: Option<i64>,
    pub last_role_updated_at_unix_ms: Option<i64>,
    pub last_revocation_updated_at_unix_ms: Option<i64>,
    pub last_sync_diagnostics_updated_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRevisionSnapshot {
    pub entity_id: String,
    pub revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceiptV1 {
    pub message_id: String,
    pub delivered_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatTextV1 {
    pub message_id: String,
    pub body: String,
    pub created_at_unix_ms: i64,
}

/// What one of our own devices sent to a contact, so our other devices show it in that chat too
/// (docs/decisions/multi-device-equal-nodes.md). Travels only between devices of one account;
/// `contact_account_id` names the chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentTranscriptV1 {
    pub contact_account_id: String,
    pub message_id: String,
    pub body: String,
    pub created_at_unix_ms: i64,
    /// The file the message carries. The receiving device fetches it from the device that sent this
    /// copy, never from the contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentOfferV1>,
}

/// The account took one of its devices back (docs/decisions/multi-device-equal-nodes.md). The
/// statement is signed by the account's working key and checked by whoever applies it, so it
/// travels as the serialized `omni_crypto::DeviceRevocationV1` (this crate does not know the key
/// types) and is valid whoever hands it over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevocationSyncV1 {
    pub revocation_json: String,
}

/// The account moved to a new working key (docs/decisions/multi-device-equal-nodes.md, «Переезд при
/// краже рабочего ключа»). `move_json` is the serialized `omni_crypto::AccountKeyMoveV1` — the master
/// vouches for the new key, so it is valid whoever hands it over. `working_seed_hex` is the new
/// working key itself and goes ONLY to the owner's own kept devices, never to a contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountKeyMoveSyncV1 {
    pub move_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_seed_hex: Option<String>,
}

/// Our contacts as one of our devices knows them, for our other devices
/// (docs/decisions/multi-device-equal-nodes.md, «Синхронизация контактов»). Travels only between
/// devices of one account. Each entry is the whole state of one contact — state, not an event — so a
/// device that was away for any time catches up from whichever of our devices it meets.
/// `through_seq` is the sender's position in its own list of changes; the receiver acknowledges it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSyncV1 {
    pub entries: Vec<ContactSyncEntryV1>,
    pub through_seq: u64,
}

/// One contact. The relationship and the name are versioned apart: a rename never brings a removed
/// contact back. Versions are counters, not clocks; a tie goes to the writer device id, and for the
/// relationship a removal beats a block beats an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSyncEntryV1 {
    pub account_id: String,
    /// `approved`, `blocked` or `removed`.
    pub state: String,
    pub state_version: u64,
    pub state_writer: String,
    pub petname: String,
    pub petname_version: u64,
    pub petname_writer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The contact's devices (serialized `omni_crypto::PrekeyBundle`, certificate chain included,
    /// one-time prekey left out): what a device that never met the contact needs to reach it. Each is
    /// checked against its own certificate by whoever stores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bundles_json: Vec<String>,
}

/// The account passport between its owner and a holder (docs/decisions/account-recovery-from-friends.md,
/// Б1). The passport itself is sealed under the account's secret: the holder keeps it by `locator_hex`
/// and never reads it. Only a device of the owner's account is answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AccountPassportMessageV1 {
    /// Owner → holder: keep this (the newest version replaces the one held). `mark_secret_hex` seals
    /// the holder's mark at the couriers, by which a restored device finds it.
    Store {
        locator_hex: String,
        version: i64,
        sealed_b64: String,
        mark_secret_hex: String,
    },
    /// Holder → owner: kept `version`.
    Stored { locator_hex: String, version: i64 },
    /// A device of the owner → holder: what do you keep under `locator_hex`?
    Fetch { locator_hex: String },
    /// Holder → that device: what it keeps (`version` 0 and empty `sealed_b64`: nothing).
    Blob {
        locator_hex: String,
        version: i64,
        sealed_b64: String,
    },
}

/// History sync of a 1:1 chat between two devices that both hold it — two devices of ours, or (for a
/// device restored from the phrase) ours and the contact's (docs/decisions/group-history-sync.md,
/// account-recovery-from-friends.md Б2). Lines are compared by fingerprint, first per chat, then per
/// bucket (the first byte of a line id's hash); only the lines of buckets that differ travel.
/// `chat_account_id` names the chat by its other party as the sender sees it: a contact's account
/// between our devices, the receiver's own account from a contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HistorySyncMessageV1 {
    /// The chats the sender holds, each with its line count and fingerprint.
    Summary { chats: Vec<HistoryChatSummaryV1> },
    /// One chat's bucket fingerprints. The receiver sends the lines of the buckets that differ; with
    /// `answer` it also sends its own buckets, so the lines go both ways.
    Buckets {
        chat_account_id: String,
        buckets_hex: Vec<String>,
        answer: bool,
    },
    /// Lines of one chat (newest first).
    Lines {
        chat_account_id: String,
        lines: Vec<HistoryLineV1>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryChatSummaryV1 {
    pub chat_account_id: String,
    pub count: u64,
    pub root_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryLineV1 {
    pub message_id: String,
    /// The account that wrote it: ours or the chat's other party.
    pub author_account_id: String,
    pub body: String,
    pub created_at_unix_ms: i64,
    /// A file or photo line: its offer, so the taker can fetch the file from the device that sent
    /// the line (which serves its copy to the chat's accounts only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<AttachmentOfferV1>,
}

/// Where a device posts its beacons and where it looks for others' — the couriers it uses, by URL
/// (docs/decisions/courier-metadata-and-quota.md, step 6). A contact's device posts a pair's beacon
/// where we look and looks where we post. `read` holds every courier the device looks at — the ones
/// it posts to too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CourierSetV1 {
    pub publish: Vec<String>,
    pub read: Vec<String>,
}

/// The receiving device has applied everything up to `through_seq` of the sender's changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSyncAckV1 {
    pub through_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatSessionState {
    Connected,
    Reconnecting,
    Paused,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatSessionInteractionKind {
    Keepalive,
    Message,
    Receipt,
    Session,
    Approval,
    Verification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSessionSummary {
    pub is_open_lease: bool,
    pub auto_reconnect_paused: bool,
    pub session_state: ChatSessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interaction_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_interaction_kind: Option<ChatSessionInteractionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_confirmed_keepalive_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_round_trip_ms: Option<i64>,
    /// Which of the contact's devices the live link goes to (its device id) — a contact with a laptop
    /// and a PC is one chat, and «4 мс» means little until it says which of them answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_device_id: Option<String>,
    /// How many devices the contact has (not revoked) that we know of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_device_count: Option<u32>,
}
