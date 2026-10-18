use serde::{Deserialize, Serialize};

use crate::{OmniError, OmniResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentTransferState {
    /// Ours, its tree still being read (in the background for a big file); not offered yet.
    Preparing,
    Offered,
    Available,
    Requested,
    Downloading,
    WaitingForReconnect,
    Verifying,
    Downloaded,
    Revoked,
    Unavailable,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentMediaProtectionMode {
    /// The only mode: the file is sealed under its own media key, carried inside the 1:1 session.
    /// (A "transport only" mode — plaintext over the link — was never chosen and was removed
    /// 2026-10-12: an offer naming it is refused.)
    E2eeMedia,
}

impl AttachmentMediaProtectionMode {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::E2eeMedia => "chacha20_poly1305_v1",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "e2ee_media" | "chacha20_poly1305_v1" => Ok(Self::E2eeMedia),
            other => Err(OmniError::InvalidInput(format!(
                "unsupported attachment media protection mode '{}'",
                other
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDownloadRequest {
    pub message_id: String,
    pub save_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRecord {
    pub message_id: String,
    pub file_name: String,
    pub file_size_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub content_id_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encryption: Option<String>,
    pub state: AttachmentTransferState,
    pub bytes_transferred: i64,
    #[serde(default)]
    pub local_blob_available: bool,
    #[serde(default)]
    pub local_serving_enabled: bool,
    #[serde(default)]
    pub remote_holder_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_saved_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Only what the receiver checked and accepted (a sender's claim that fails the limits is a
    /// plain file here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<AttachmentPresentationV1>,
    /// The file on this machine, if there is one (the sender's own copy or the downloaded one) —
    /// what the chat draws a photo from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_copy_path: Option<String>,
    /// Holders a download is coming from right now (a group file, several at once —
    /// docs/decisions/attachment-content-tree.md, stage 3); 0 when not counted (a 1:1 download has
    /// its one sender).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub source_count: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// How the chat shows a file (docs/decisions/media-messages.md). Absent = a plain file plate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentPresentationKindV1 {
    /// A picture shown in the bubble: the sender re-encoded it (JPEG, metadata gone, long side
    /// capped) and put a small preview in the offer.
    Photo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentPresentationV1 {
    pub kind: AttachmentPresentationKindV1,
    pub width: u32,
    pub height: u32,
    /// A small JPEG the bubble draws before (or instead of) the full picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_jpeg_b64: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentOfferV1 {
    pub message_id: String,
    pub body: String,
    pub file_name: String,
    pub file_size_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    pub content_id_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encryption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_nonce_seed_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_chunk_size_bytes: Option<u32>,
    pub created_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<AttachmentPresentationV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDownloadRequestV1 {
    pub transfer_id: String,
    pub message_id: String,
    pub resume_offset_bytes: i64,
    pub requested_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferFailedV1 {
    pub message_id: String,
    pub reason_code: String,
    pub reason_message: String,
    pub failed_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDownloadedV1 {
    pub message_id: String,
    pub downloaded_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentAccessRevokedV1 {
    pub message_id: String,
    pub revoked_at_unix_ms: i64,
}
