use serde::{Deserialize, Serialize};

use crate::{OmniError, OmniResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    Incoming,
    Outgoing,
}

impl CallDirection {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Incoming => "incoming",
            Self::Outgoing => "outgoing",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "incoming" => Ok(Self::Incoming),
            "outgoing" => Ok(Self::Outgoing),
            _ => Err(OmniError::Storage(format!(
                "unsupported call direction '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Calling,
    Ringing,
    Connecting,
    Active,
    Reconnecting,
    Ended,
}

impl CallState {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Calling => "calling",
            Self::Ringing => "ringing",
            Self::Connecting => "connecting",
            Self::Active => "active",
            Self::Reconnecting => "reconnecting",
            Self::Ended => "ended",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "calling" => Ok(Self::Calling),
            "ringing" => Ok(Self::Ringing),
            "connecting" => Ok(Self::Connecting),
            "active" => Ok(Self::Active),
            "reconnecting" => Ok(Self::Reconnecting),
            "ended" => Ok(Self::Ended),
            _ => Err(OmniError::Storage(format!(
                "unsupported call state '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    Completed,
    Declined,
    Busy,
    Missed,
    Cancelled,
    ConnectionLost,
    Failed,
}

impl CallOutcome {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Declined => "declined",
            Self::Busy => "busy",
            Self::Missed => "missed",
            Self::Cancelled => "cancelled",
            Self::ConnectionLost => "connection_lost",
            Self::Failed => "failed",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "completed" => Ok(Self::Completed),
            "declined" => Ok(Self::Declined),
            "busy" => Ok(Self::Busy),
            "missed" => Ok(Self::Missed),
            "cancelled" => Ok(Self::Cancelled),
            "connection_lost" => Ok(Self::ConnectionLost),
            "failed" => Ok(Self::Failed),
            _ => Err(OmniError::Storage(format!(
                "unsupported call outcome '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDeclineReason {
    Declined,
    Busy,
    NotAvailable,
    SecurityBlocked,
    AlreadyInCall,
}

impl CallDeclineReason {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Declined => "declined",
            Self::Busy => "busy",
            Self::NotAvailable => "not_available",
            Self::SecurityBlocked => "security_blocked",
            Self::AlreadyInCall => "already_in_call",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "declined" => Ok(Self::Declined),
            "busy" => Ok(Self::Busy),
            "not_available" => Ok(Self::NotAvailable),
            "security_blocked" => Ok(Self::SecurityBlocked),
            "already_in_call" => Ok(Self::AlreadyInCall),
            _ => Err(OmniError::Storage(format!(
                "unsupported call decline reason '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallNetworkQuality {
    Good,
    Fair,
    Poor,
    Recovering,
}

impl CallNetworkQuality {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Fair => "fair",
            Self::Poor => "poor",
            Self::Recovering => "recovering",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "good" => Ok(Self::Good),
            "fair" => Ok(Self::Fair),
            "poor" => Ok(Self::Poor),
            "recovering" => Ok(Self::Recovering),
            _ => Err(OmniError::Storage(format!(
                "unsupported call network quality '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallParticipantSummary {
    pub participant_id: String,
    pub account_id: String,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub is_muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHistoryEvent {
    pub call_id: String,
    pub direction: CallDirection,
    pub outcome: CallOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decline_reason: Option<CallDeclineReason>,
    pub participants: Vec<CallParticipantSummary>,
    pub started_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u32>,
    /// A remote-control session rather than a conversation (shown as «Сеанс управления»).
    #[serde(default, skip_serializing_if = "is_false")]
    pub remote_control: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Which side of a remote-control session we are (docs/decisions/remote-access-for-contacts.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteControlRole {
    /// We asked to control the peer's computer.
    Controller,
    /// The peer controls ours.
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteControlPhase {
    /// Waiting for the owner to answer (owner: we must answer).
    AwaitingOwner,
    /// The owner agreed (or needs no question): the owner shows its screen and grants input.
    Approved,
    /// The owner declined, did not answer in time, or no longer grants access.
    Refused,
}

/// The remote-control side of a call. A remote-control session IS a call — no microphone, no
/// camera — so the one-call-per-profile invariant covers it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteControlSession {
    pub role: RemoteControlRole,
    pub phase: RemoteControlPhase,
    /// Owner side, `AwaitingOwner`: when an unanswered request counts as declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_deadline_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSummary {
    pub call_id: String,
    pub dialog_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<String>,
    pub direction: CallDirection,
    pub state: CallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CallOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decline_reason: Option<CallDeclineReason>,
    pub participants: Vec<CallParticipantSummary>,
    #[serde(default)]
    pub local_mic_muted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_quality: Option<CallNetworkQuality>,
    pub started_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ringing_started_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_unix_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_deadline_unix_ms: Option<i64>,
    pub updated_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_control: Option<RemoteControlSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallInboxSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_call: Option<CallSummary>,
    #[serde(default)]
    pub incoming_calls: Vec<CallSummary>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallParticipantDescriptorV1 {
    pub participant_id: String,
    pub account_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallInviteV1 {
    pub call_id: String,
    pub dialog_key: String,
    pub participants: Vec<CallParticipantDescriptorV1>,
    pub created_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    /// The caller asks to control the callee's computer (standing remote access). For a call that
    /// is already running, an invite with ITS call id asks to turn that call into a session.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remote_control: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRingingV1 {
    pub call_id: String,
    pub ringing_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallAcceptV1 {
    pub call_id: String,
    pub accepted_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDeclineV1 {
    pub call_id: String,
    pub reason: CallDeclineReason,
    pub declined_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallBusyV1 {
    pub call_id: String,
    pub busy_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallCancelV1 {
    pub call_id: String,
    pub cancelled_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHangupV1 {
    pub call_id: String,
    pub ended_at_unix_ms: i64,
}

/// How a contact with standing remote access gets in (docs/decisions/remote-access-for-contacts.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAccessMode {
    /// The owner confirms every session.
    Ask,
    /// Sessions start without a question (unattended access).
    Unattended,
}

impl RemoteAccessMode {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Unattended => "unattended",
        }
    }

    pub fn from_db_value(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "unattended" => Some(Self::Unattended),
            _ => None,
        }
    }
}

/// The sender lets (`mode` = Some) or no longer lets (`None`) the receiver control the sender's
/// computer. Informational for the receiver — it shows the "Управлять" button; every session is still
/// decided by the owner's core. Sent on change and again whenever a session with the contact comes up,
/// so a revocation made while the contact was offline still reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAccessGrantV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<RemoteAccessMode>,
    pub sent_at_unix_ms: i64,
}

/// What the sharer's side can honestly tell the controller about why input may not land
/// (docs/decisions/remote-access-for-contacts.md, «честные состояния»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenControlSharerState {
    /// Input lands.
    Ready,
    /// The computer is locked: Windows gives the lock screen to nobody's injected input.
    Locked,
    /// The person at the computer is using the mouse/keyboard: their hand wins, remote input waits.
    OwnerActive,
    /// The screen gives capture no frames (the monitor is off or asleep): nothing to show.
    NoPicture,
}

/// The sharer allows (`granted`) or stops allowing the peer to control the screen they share.
/// Meaningful only while the sender shows a whole monitor; the peer's input then arrives as signed
/// screen-control datagrams. See docs/decisions/remote-screen-control.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallScreenControlV1 {
    pub call_id: String,
    pub granted: bool,
    pub sent_at_unix_ms: i64,
    /// Sharer-side condition, reported while granted; absent = nothing to report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharer_state: Option<ScreenControlSharerState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTimeoutV1 {
    pub call_id: String,
    pub timed_out_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallReconnectV1 {
    pub call_id: String,
    pub reconnecting_at_unix_ms: i64,
}

/// Video codecs a call can carry. Which one goes S → R is chosen per pair and per direction
/// (`video_codec_choice`, docs/decisions/per-pair-video-codec-selection.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeVideoCodec {
    H264,
    H265,
    Av1,
}

/// One half of what a machine does with a codec — encode or decode — as PROBED on it (the codec
/// really opened and ran frames), not as a driver claims. Zero limits mean "no known limit".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeVideoCodecSideV1 {
    pub hardware: bool,
    #[serde(default)]
    pub max_width: u32,
    #[serde(default)]
    pub max_height: u32,
    #[serde(default)]
    pub max_fps: u32,
}

/// A codec line of the capability passport: whether this machine encodes it, decodes it, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeVideoCodecCapabilityV1 {
    pub codec: RealtimeVideoCodec,
    pub payload_type: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encode: Option<RealtimeVideoCodecSideV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<RealtimeVideoCodecSideV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
}

/// From the mains or from a battery: software decoding on a battery is the expensive kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimePowerSource {
    #[default]
    Mains,
    Battery,
}

/// Settings → Calls → «Битрейт»: a multiplier on the bitrate CEILING of a direction, not a rate —
/// the congestion controller still sets the rate below it, and an easy picture takes less.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeVideoBitrateLevel {
    /// Half the ceiling: a metered or mobile link.
    Economy,
    #[default]
    Auto,
    /// One and a half times the ceiling: a fast link and a picture that wants more.
    Max,
}

impl RealtimeVideoBitrateLevel {
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }

    /// The percent of the ceiling one direction gets from both ends' levels. Economy on either end
    /// wins — the receiver pays for the bytes as much as the sender; otherwise Max on either end
    /// asks for more, which only the asking side has to set.
    pub fn percent_for_pair(sender: Self, receiver: Self) -> u32 {
        match (sender, receiver) {
            (Self::Economy, _) | (_, Self::Economy) => 50,
            (Self::Max, _) | (_, Self::Max) => 150,
            _ => 100,
        }
    }
}

/// Coarse processor load: decides whether software encoding is allowed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeCpuLoad {
    Low,
    #[default]
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeVideoPixelFormat {
    Nv12,
    I420,
    Bgra,
}

/// What a peer SENDS in a call: the format of its camera or of its shared screen, whichever it
/// shows now (`RealtimeMediaCapabilitiesV1::content`). What it can RECEIVE is not here but in its
/// codec lines (`decode` side limits) — docs/decisions/per-direction-video-format.md.
/// `can_send_video = false` means this peer has nothing to send (no camera, no share): it only
/// receives, and the session is still perfectly usable — one way video plus two-way audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeVideoProfileCapabilityV1 {
    pub min_width: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub preferred_width: u32,
    pub preferred_height: u32,
    pub preferred_fps: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub pixel_formats: Vec<RealtimeVideoPixelFormat>,
    #[serde(default = "default_true")]
    pub can_send_video: bool,
}

fn default_true() -> bool {
    true
}

/// What a peer brings to a call.
///
/// Audio is unconditional and needs no negotiation (Opus, fixed payload type), so
/// EVERY field here is about video and every one of them may say "none". A peer
/// with no camera, no hardware H.264, or a capture stack that failed to probe
/// still publishes this — with `codecs` empty and/or `video: None` — because
/// publishing nothing at all is what used to take the call's AUDIO down with the
/// video. See docs/decisions/call-audio-without-video.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMediaCapabilitiesV1 {
    pub dialog_key: String,
    /// Names this announcement; the selection labels each direction by the sender's id, so both
    /// sides read "what I send" and "what I receive" from the same selection whoever computed it.
    /// The core fills it on publish when the shell leaves it empty.
    #[serde(default)]
    pub capabilities_id: String,
    pub codecs: Vec<RealtimeVideoCodecCapabilityV1>,
    #[serde(default)]
    pub power: RealtimePowerSource,
    #[serde(default)]
    pub cpu_load: RealtimeCpuLoad,
    /// The user's ceiling from Settings → Calls ("Авто" = none): codecs above it are not used for
    /// what this machine sends or receives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_ceiling: Option<RealtimeVideoCodec>,
    /// The user's bitrate level from Settings → Calls; it scales the bitrate ceiling of both
    /// directions of this machine's calls (`RealtimeVideoBitrateLevel::percent_for_pair`).
    #[serde(default, skip_serializing_if = "RealtimeVideoBitrateLevel::is_auto")]
    pub bitrate_level: RealtimeVideoBitrateLevel,
    /// What this machine sends right now: its camera or its screen. The codec of its direction is
    /// chosen for that picture, and a share that starts or stops is announced again (ADR §3).
    #[serde(default)]
    pub content: crate::VideoContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<RealtimeVideoProfileCapabilityV1>,
    pub max_datagram_bytes: u32,
    #[serde(default)]
    pub supports_rtcp_feedback: bool,
    pub sent_at_unix_ms: i64,
}

/// The video half of a negotiated session. Absent from the selection entirely
/// when the two peers have no video in common — see `RealtimeMediaSelectionV1`.
/// No common size: each direction has its own (docs/decisions/per-direction-video-format.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeVideoSelectionV1 {
    /// One per direction that carries video, labelled by its sender's `capabilities_id`.
    pub streams: Vec<RealtimeVideoStreamV1>,
    pub capture_pixel_format: RealtimeVideoPixelFormat,
    #[serde(default)]
    pub rtcp_feedback_enabled: bool,
}

/// One direction of a call: who sends, in what codec and format, at what target, and why that
/// codec. The format is the sender's, fitted to what the receiver decodes in that codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeVideoStreamV1 {
    pub sender_capabilities_id: String,
    pub codec: RealtimeVideoCodec,
    pub payload_type: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub target_bitrate_bps: u32,
    #[serde(default)]
    pub reason: String,
}

/// The agreed media session. `video: None` is an ordinary, fully working call —
/// audio both ways, no video — not a failure. The selection existing at all is
/// what tells both shells to bring their audio pipelines up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMediaSelectionV1 {
    pub dialog_key: String,
    pub session_id: String,
    pub max_datagram_bytes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<RealtimeVideoSelectionV1>,
    pub selected_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeMediaControlType {
    Pli,
    Fir,
    Nack,
    ReceiverReport,
    SenderReport,
    BitrateHint,
    KeyframeRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMediaControlV1 {
    pub dialog_key: String,
    pub session_id: String,
    pub control_type: RealtimeMediaControlType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_ssrc: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packet_ids: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fir_sequence_number: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction_lost: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cumulative_lost: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highest_sequence: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtp_timestamp: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub octet_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bitrate_bps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub sent_at_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealtimeMediaNegotiationStatus {
    Idle,
    WaitingForRemoteCapabilities,
    Negotiated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealtimeMediaNegotiationSnapshot {
    pub dialog_key: String,
    pub status: RealtimeMediaNegotiationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_capabilities: Option<RealtimeMediaCapabilitiesV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_capabilities: Option<RealtimeMediaCapabilitiesV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<RealtimeMediaSelectionV1>,
    /// This side's two directions out of `selected` (by `capabilities_id`), so the shell does not
    /// have to know how directions are labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_video: Option<RealtimeVideoStreamV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_video: Option<RealtimeVideoStreamV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    pub updated_at_unix_ms: i64,
}
