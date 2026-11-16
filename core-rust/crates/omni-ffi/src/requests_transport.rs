use omni_messaging::DialogDeltaCursor;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DialogKeyRequest {
    pub(crate) dialog_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DialogDeltaRequest {
    pub(crate) dialog_key: String,
    pub(crate) cursor: DialogDeltaCursor,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct QuickAddContactFromDialogRequest {
    pub(crate) dialog_key: String,
    pub(crate) petname: String,
    pub(crate) public_display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AttachDialogToContactRequest {
    pub(crate) contact_id: omni_identity::ContactId,
    pub(crate) dialog_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessageIdRequest {
    pub(crate) message_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessageIdBatchRequest {
    pub(crate) message_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct VideoNoteStageRequest {
    pub(crate) video_b64: String,
    #[serde(default)]
    pub(crate) preview_b64: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct VoiceStageRequest {
    pub(crate) packets_b64: String,
    #[serde(default)]
    pub(crate) waveform_b64: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MediaFileRequest {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) file_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessageDeleteRequest {
    pub(crate) message_id: String,
    #[serde(default)]
    pub(crate) for_everyone: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessagePinRequest {
    pub(crate) message_id: String,
    pub(crate) pinned: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessageActionsRequest {
    pub(crate) dialog_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MessageReactRequest {
    pub(crate) message_id: String,
    #[serde(default)]
    pub(crate) emoji: String,
}
