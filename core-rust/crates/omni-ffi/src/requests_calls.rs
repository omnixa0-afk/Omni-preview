use omni_types::{CallDeclineReason, RealtimeMediaCapabilitiesV1};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CallIdRequest {
    pub(crate) call_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DeclineCallRequest {
    pub(crate) call_id: String,
    pub(crate) reason: CallDeclineReason,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetLocalMicMutedRequest {
    pub(crate) call_id: String,
    pub(crate) is_muted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PublishRealtimeMediaCapabilitiesRequest {
    pub(crate) capabilities: RealtimeMediaCapabilitiesV1,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RealtimeMediaDialogRequest {
    pub(crate) dialog_key: String,
}


#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetCallScreenControlRequest {
    pub(crate) call_id: String,
    pub(crate) granted: bool,
    /// Present: not a grant change but a sharer-side state report (locked / owner's hand / ready).
    #[serde(default)]
    pub(crate) sharer_state: Option<omni_types::ScreenControlSharerState>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SetGroupCallScreenControlRequest {
    pub(crate) group_id: String,
    #[serde(default)]
    pub(crate) everyone: bool,
    #[serde(default)]
    pub(crate) members: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ScreenControlSendRequest {
    pub(crate) dialog_key: String,
    #[serde(default)]
    pub(crate) sharer: Option<String>,
    #[serde(default)]
    pub(crate) pointer: Option<[u16; 2]>,
    #[serde(default)]
    pub(crate) events: Vec<omni_protocol::ScreenInputEvent>,
}
