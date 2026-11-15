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

/// A file name as someone else wrote it, made safe to show and to write to disk (V-18): no control
/// characters, no bidirectional overrides or isolates and no zero-width characters — `invoice<U+202E>fdp.exe`
/// would show as `invoiceexe.pdf` — no characters Windows forbids in a name, no leading or trailing
/// dots or spaces, and no name of a Windows device (`CON`, `NUL`, `COM1`…) whatever its extension.
/// Empty when nothing is left.
pub fn clean_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|ch| !is_hidden_or_reordering(*ch))
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect();
    let cleaned = cleaned.trim_matches(|ch: char| ch == '.' || ch.is_whitespace()).to_string();
    let stem = cleaned.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if device {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Characters a name must not carry: controls, and those that change how the rest is shown
/// (bidirectional marks, embeddings, overrides, isolates; zero-width joiners and spaces; BOM).
fn is_hidden_or_reordering(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
        )
}

/// Extensions Windows runs as a program or a script (or that hand one over: a shortcut, a disk image,
/// an installer, a help file with scripts) rather than open as a document.
const PROGRAM_EXTENSIONS: &[&str] = &[
    "exe", "com", "scr", "pif", "cpl", "msi", "msp", "msix", "msixbundle", "appx", "appxbundle", "appref-ms",
    "application", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "wsc", "hta", "jar",
    "lnk", "url", "scf", "reg", "inf", "msc", "chm", "iso", "img", "vhd", "vhdx", "library-ms",
    "settingcontent-ms", "search-ms", "gadget", "xll", "dll",
];

/// Whether opening a file of this name starts a program or a script: the shell asks first (V-18).
pub fn file_name_opens_as_program(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| PROGRAM_EXTENSIONS.contains(&extension.trim().to_ascii_lowercase().as_str()))
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
    /// Opening it starts a program or a script (by its name): the shell asks first (V-18).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub opens_as_program: bool,
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
    /// A voice message: an Ogg/Opus file played in the bubble.
    Voice,
    /// A round video message: a short MP4 played in the bubble.
    VideoNote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentPresentationV1 {
    pub kind: AttachmentPresentationKindV1,
    /// A photo's size.
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// A small JPEG the bubble draws before (or instead of) the full picture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_jpeg_b64: Option<String>,
    /// A voice or video message's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// A voice message's outline: up to 64 bars, 0..255.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waveform_b64: Option<String>,
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
    /// The message this one answers, in the same conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
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
