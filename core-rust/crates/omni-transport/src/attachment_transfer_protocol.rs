#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferStartV2 {
    pub transfer_id: String,
    pub payload: Vec<u8>,
    pub started_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferChunkV2 {
    pub transfer_id: String,
    pub chunk_index: u64,
    pub payload: Vec<u8>,
    pub sent_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferCompleteV2 {
    pub transfer_id: String,
    pub payload: Vec<u8>,
    pub completed_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame_type", rename_all = "snake_case")]
pub enum AttachmentTransferFrameV2 {
    Start(AttachmentTransferStartV2),
    Chunk(AttachmentTransferChunkV2),
    Complete(AttachmentTransferCompleteV2),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferRequestEnvelopeV2 {
    pub protocol_version: u16,
    pub frame: AttachmentTransferFrameV2,
}

impl AttachmentTransferRequestEnvelopeV2 {
    pub fn new(frame: AttachmentTransferFrameV2) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            frame,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferAckV2 {
    pub received_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentTransferResponseEnvelopeV2 {
    pub protocol_version: u16,
    pub ack: AttachmentTransferAckV2,
}

impl AttachmentTransferResponseEnvelopeV2 {
    pub fn ack(received_at_unix_ms: i64) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            ack: AttachmentTransferAckV2 {
                received_at_unix_ms,
            },
        }
    }
}

/// The largest stream frame: a Start carrying the block layer of a 2 TB file (4 MiB, see
/// `ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES`) with room for its framing.
pub(crate) const ATTACHMENT_TRANSFER_REQUEST_SIZE_MAXIMUM_BYTES: u64 = 4 * 1024 * 1024 + 64 * 1024;
/// A sealed Start: the file's whole block layer (32 bytes per 16 MiB block — 4 MiB for the 2 TB a
/// file may be), its few fixed fields and the AEAD tag (docs/decisions/attachment-content-tree.md).
pub(crate) const ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES: usize = 4 * 1024 * 1024 + 4096;
/// A sealed chunk: 1 MiB of data, its framing and AEAD tag, and — on the first chunk of a block —
/// the block proof (16 leaf hashes + a path of at most 64; docs/decisions/attachment-content-tree.md).
pub(crate) const ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES: usize = 1024 * 1024 + 4096;

pub(crate) fn validate_attachment_transfer_frame_v2(
    frame: &AttachmentTransferFrameV2,
) -> OmniResult<()> {
    match frame {
        AttachmentTransferFrameV2::Start(start) => {
            validate_attachment_transfer_id("start transfer_id", &start.transfer_id)?;
            if start.payload.is_empty() {
                return Err(OmniError::InvalidInput(
                    "attachment transfer start payload must not be empty".to_string(),
                ));
            }
            if start.payload.len() > ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES {
                return Err(OmniError::InvalidInput(format!(
                    "attachment transfer start payload exceeds maximum size of {} bytes",
                    ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES
                )));
            }
            if start.started_at_unix_ms <= 0 {
                return Err(OmniError::InvalidInput(
                    "attachment transfer start started_at_unix_ms must be positive".to_string(),
                ));
            }
        }
        AttachmentTransferFrameV2::Chunk(chunk) => {
            validate_attachment_transfer_id("chunk transfer_id", &chunk.transfer_id)?;
            if chunk.payload.is_empty() {
                return Err(OmniError::InvalidInput(
                    "attachment transfer chunk payload must not be empty".to_string(),
                ));
            }
            if chunk.payload.len() > ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES {
                return Err(OmniError::InvalidInput(format!(
                    "attachment transfer chunk payload exceeds maximum size of {} bytes",
                    ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES
                )));
            }
            if chunk.sent_at_unix_ms <= 0 {
                return Err(OmniError::InvalidInput(
                    "attachment transfer chunk sent_at_unix_ms must be positive".to_string(),
                ));
            }
        }
        AttachmentTransferFrameV2::Complete(complete) => {
            validate_attachment_transfer_id("complete transfer_id", &complete.transfer_id)?;
            if complete.payload.is_empty() {
                return Err(OmniError::InvalidInput(
                    "attachment transfer complete payload must not be empty".to_string(),
                ));
            }
            if complete.payload.len() > ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES {
                return Err(OmniError::InvalidInput(format!(
                    "attachment transfer complete payload exceeds maximum size of {} bytes",
                    ATTACHMENT_TRANSFER_CHUNK_SIZE_MAXIMUM_BYTES
                )));
            }
            if complete.completed_at_unix_ms <= 0 {
                return Err(OmniError::InvalidInput(
                    "attachment transfer complete completed_at_unix_ms must be positive"
                        .to_string(),
                ));
            }
        }
    }

    Ok(())
}

fn validate_attachment_transfer_id(field_name: &str, value: &str) -> OmniResult<()> {
    if value.trim().is_empty() {
        return Err(OmniError::InvalidInput(format!(
            "{field_name} must not be empty"
        )));
    }
    Ok(())
}
