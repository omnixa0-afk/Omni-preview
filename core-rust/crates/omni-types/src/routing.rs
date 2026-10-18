use serde::{Deserialize, Serialize};

use crate::MessageStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueFailureKind {
    NoActiveSession,
    SessionMismatch,
    TransportWriteFailed,
    AckTimeout,
    StorageFailure,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundQueueRecord {
    pub message_id: String,
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub status: MessageStatus,
    pub attempt_count: i64,
    pub next_attempt_unix_ms: Option<i64>,
    pub last_attempt_unix_ms: Option<i64>,
    pub last_failure_kind: Option<QueueFailureKind>,
    pub last_failure_message: Option<String>,
    pub remote_delivered_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundQueueSnapshot {
    pub queued_count: usize,
    pub sending_count: usize,
    pub retrying_count: usize,
    pub sent_waiting_ack_count: usize,
    pub failed_count: usize,
    pub items: Vec<OutboundQueueRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogQueueDiagnostics {
    #[serde(rename = "peer_addr", alias = "peer_key", alias = "dialog_key")]
    pub dialog_key: String,
    pub pending_count: usize,
    pub sent_waiting_ack_count: usize,
    pub retrying_count: usize,
    pub failed_count: usize,
    pub last_failure_kind: Option<QueueFailureKind>,
    pub last_failure_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueDiagnosticsSnapshot {
    pub queue_length: usize,
    pub queued_count: usize,
    pub sending_count: usize,
    pub retrying_count: usize,
    pub sent_waiting_ack_count: usize,
    pub failed_count: usize,
    pub last_delivery_error: Option<String>,
    pub last_failure_kind: Option<QueueFailureKind>,
    pub last_failure_message: Option<String>,
    pub pending_by_dialog: Vec<DialogQueueDiagnostics>,
}
