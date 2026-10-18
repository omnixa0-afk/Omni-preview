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
pub(crate) struct MediaFileRequest {
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) file_name: String,
}
