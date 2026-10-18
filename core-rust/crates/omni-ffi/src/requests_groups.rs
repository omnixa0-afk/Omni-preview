use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CreateGroupRequest {
    pub(crate) name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GroupIdRequest {
    pub(crate) group_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SendGroupTextRequest {
    pub(crate) group_id: String,
    pub(crate) body: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SendGroupAttachmentRequest {
    pub(crate) group_id: String,
    #[serde(default)]
    pub(crate) body: String,
    pub(crate) source_file_path: String,
    #[serde(default)]
    pub(crate) display_file_name: Option<String>,
    #[serde(default)]
    pub(crate) as_photo: bool,
}

/// Invite contacts by their 1:1 dialog: `dialog_keys`, or the single `dialog_key`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct InviteToGroupRequest {
    pub(crate) group_id: String,
    #[serde(default)]
    pub(crate) dialog_key: String,
    #[serde(default)]
    pub(crate) dialog_keys: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct GroupMemberRequest {
    pub(crate) group_id: String,
    pub(crate) member_pub: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RenameGroupRequest {
    pub(crate) group_id: String,
    pub(crate) name: String,
}

/// `source_path`: an image file to normalize and set; `null` clears the picture.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetGroupAvatarRequest {
    pub(crate) group_id: String,
    pub(crate) source_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetGroupNewcomerHistoryRequest {
    pub(crate) group_id: String,
    pub(crate) on: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetGroupAdminRequest {
    pub(crate) group_id: String,
    pub(crate) member_pub: String,
    pub(crate) admin: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetGroupCallMediaStateRequest {
    pub(crate) group_id: String,
    pub(crate) muted: bool,
    pub(crate) camera_off: bool,
    #[serde(default)]
    pub(crate) screen_sharing: bool,
}
