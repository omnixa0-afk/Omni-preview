fn active_profile_context(
    storage: &Storage,
) -> OmniResult<(
    LocalProfileSummary,
    LocalAccountSnapshot,
    LocalDeviceSnapshot,
)> {
    let profile_id = storage
        .get_active_profile_id()?
        .ok_or_else(|| OmniError::InvalidInput("no active profile is selected".to_string()))?;
    let profile = storage
        .get_local_profile_summary(&profile_id)?
        .ok_or_else(|| OmniError::InvalidInput("active profile is not available".to_string()))?;
    let account = storage
        .get_local_account_by_profile_id(&profile_id)?
        .ok_or_else(|| {
            OmniError::InvalidInput("active profile account is not available".to_string())
        })?;
    let device = storage
        .get_local_device_by_profile_id(&profile_id)?
        .ok_or_else(|| {
            OmniError::InvalidInput("active profile device is not available".to_string())
        })?;
    Ok((profile, account, device))
}

fn received_message_encryption_state(received_encrypted: bool) -> MessageEncryptionState {
    if received_encrypted {
        MessageEncryptionState::EncryptedReceived
    } else {
        MessageEncryptionState::LegacyPlaintext
    }
}

fn ensure_send_ready_session_for_remote_account(
    storage: &Storage,
    transport: &TransportManager,
    remote_account_id: &AccountId,
    target_dialog_key: Option<&str>,
) -> OmniResult<(omni_crypto::SessionRecord, Option<i64>)> {
    let existing_session = match target_dialog_key {
        Some(dialog_key) => storage.get_session_for_dialog(remote_account_id, dialog_key)?,
        None => storage.get_latest_session_for_remote_account(remote_account_id)?,
    };
    if let Some(existing_session) = existing_session {
        if existing_session.bootstrap_state == omni_crypto::SessionBootstrapState::Established {
            return Ok((existing_session, None));
        }
    }

    let remote_bundle = if let Some(bundle) = transport.active_remote_prekey_bundle() {
        if bundle.account.account_id != remote_account_id.0 {
            return Err(OmniError::Transport(
                "active transport bundle does not match the requested remote account".to_string(),
            ));
        }
        let domain_bundle = prekey_bundle_from_protocol(&bundle)?;
        storage.record_remote_prekey_bundle(
            &domain_bundle,
            None,
            Some(&bundle.device.device_label),
            bundle.generated_at_unix_ms,
        )?;
        domain_bundle
    } else if let Some(bundle) =
        storage.get_latest_remote_prekey_bundle_for_account(remote_account_id)?
    {
        bundle
    } else {
        return Err(OmniError::Transport(
            "no remote prekey bundle is available for the selected contact".to_string(),
        ));
    };

    let (session, bootstrap, observed_at) =
        bootstrap_session_for_remote_bundle(storage, &remote_bundle, current_unix_ms())?;
    let envelope = omni_messaging::wrap_session_bootstrap(bootstrap)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &envelope)?;
    } else {
        transport.send_wire_envelope(&envelope)?;
    }
    Ok((session, Some(observed_at)))
}

fn prepare_encrypted_payload_for_remote_account(
    storage: &Storage,
    transport: &TransportManager,
    remote_account_id: &AccountId,
    target_dialog_key: Option<&str>,
    payload: &omni_types::WirePayloadV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<(omni_messaging::OutgoingWireEnvelope, Option<i64>)> {
    let (session, bootstrap_sent_at) = ensure_send_ready_session_for_remote_account(
        storage,
        transport,
        remote_account_id,
        target_dialog_key,
    )?;
    let outgoing = omni_messaging::build_encrypted_wire_envelope(
        &session,
        payload,
        payload_kind,
        current_unix_ms(),
    )?;
    Ok((outgoing, bootstrap_sent_at))
}

/// Encrypt for the device behind `dialog_key` — the envelope is sent on that dialog, so it must be
/// that device's session, not merely the account's latest one.
fn prepare_encrypted_payload_for_established_session(
    storage: &Storage,
    remote_account_id: &AccountId,
    dialog_key: &str,
    payload: &omni_types::WirePayloadV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<omni_messaging::OutgoingWireEnvelope> {
    let session = storage
        .get_session_for_dialog(remote_account_id, dialog_key)?
        .ok_or_else(|| {
            OmniError::Transport(
                "no established session is available for the selected contact".to_string(),
            )
        })?;
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Err(OmniError::Transport(
            "session is not fully established yet".to_string(),
        ));
    }
    let outgoing = omni_messaging::build_encrypted_wire_envelope(
        &session,
        payload,
        payload_kind,
        current_unix_ms(),
    )?;
    Ok(outgoing)
}

fn prepare_encrypted_payload_for_session_address(
    storage: &Storage,
    session_address: &omni_crypto::SessionAddress,
    payload: &omni_types::WirePayloadV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<omni_messaging::OutgoingWireEnvelope> {
    let session = storage
        .get_session_record(session_address)?
        .ok_or_else(|| {
            OmniError::Transport(
                "no established session is available for the selected dialog".to_string(),
            )
        })?;
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Err(OmniError::Transport(
            "session is not fully established yet".to_string(),
        ));
    }
    let outgoing = omni_messaging::build_encrypted_wire_envelope(
        &session,
        payload,
        payload_kind,
        current_unix_ms(),
    )?;
    Ok(outgoing)
}

#[derive(Debug, Clone)]
struct AttachmentContentCryptoMaterial {
    media_key: Option<Vec<u8>>,
    nonce_seed: Option<Vec<u8>>,
    chunk_size_bytes: u32,
}

/// The head of a stream: what file it is, and the file's whole block layer (n × 32 bytes, 4 MiB
/// for 2 TB), so the receiver can prove any block it holds to the next who asks before its copy is
/// whole (docs/decisions/attachment-content-tree.md, stage 3). The same for every stream of a
/// transfer, from any source: the Start of each is sealed under one nonce, so nothing that differs
/// between streams — where it starts, which blocks it carries — may ride in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentTransferStartPayloadV2 {
    total_size_bytes: i64,
    content_id_hex: String,
    chunk_size_bytes: u32,
    block_layer: Vec<omni_crypto::ContentHash>,
}

/// Blocks of the largest file the protocol allows: the bound of a layer read from the wire.
const ATTACHMENT_BLOCK_LAYER_MAX: usize =
    (omni_protocol::ATTACHMENT_FILE_SIZE_MAX_BYTES as u64).div_ceil(omni_crypto::CONTENT_TREE_BLOCK_BYTES) as usize;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentTransferChunkPayloadV2 {
    offset_bytes: i64,
    data: Vec<u8>,
    /// On the first chunk a source sends of a block: what proves the block against the file's
    /// identity (docs/decisions/attachment-content-tree.md).
    block_proof: Option<AttachmentBlockProofV2>,
}

/// A block's leaf hashes (one per 1 MiB chunk) and its path to the tree root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AttachmentBlockProofV2 {
    leaves: Vec<omni_crypto::ContentHash>,
    path: Vec<omni_crypto::ContentHash>,
}

/// A path longer than this cannot belong to a file the protocol allows (2 TB is 2^21 leaves).
const ATTACHMENT_BLOCK_PROOF_PATH_MAX: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentTransferCompletePayloadV2 {
    total_size_bytes: i64,
    content_id_hex: String,
}

const ATTACHMENT_TRANSFER_COUNTER_START: u64 = 0;
const ATTACHMENT_TRANSFER_COUNTER_COMPLETE: u64 = u64::MAX - 2;

fn attachment_content_crypto_material(
    runtime: &AttachmentRuntimeInfo,
) -> OmniResult<AttachmentContentCryptoMaterial> {
    let content_encryption = runtime.content_encryption.as_deref().ok_or_else(|| {
        OmniError::Storage("attachment content encryption mode is missing".to_string())
    })?;
    let mode = omni_types::AttachmentMediaProtectionMode::from_db_value(content_encryption)?;
    let chunk_size_bytes = runtime.content_chunk_size_bytes.ok_or_else(|| {
        OmniError::Storage("attachment content chunk size is missing".to_string())
    })?;
    if chunk_size_bytes == 0 {
        return Err(OmniError::Storage(
            "attachment media chunk size must be positive".to_string(),
        ));
    }

    match mode {
        omni_types::AttachmentMediaProtectionMode::E2eeMedia => {
            let key_hex = runtime
                .content_key_hex
                .as_deref()
                .ok_or_else(|| OmniError::Storage("attachment media key is missing".to_string()))?;
            let nonce_seed_hex = runtime.content_nonce_seed_hex.as_deref().ok_or_else(|| {
                OmniError::Storage("attachment media nonce seed is missing".to_string())
            })?;
            let media_key = hex::decode(key_hex).map_err(|error| {
                OmniError::Storage(format!(
                    "attachment media key could not be decoded from hex: {error}"
                ))
            })?;
            let nonce_seed = hex::decode(nonce_seed_hex).map_err(|error| {
                OmniError::Storage(format!(
                    "attachment media nonce seed could not be decoded from hex: {error}"
                ))
            })?;
            if media_key.len() != omni_crypto::ATTACHMENT_MEDIA_KEY_LEN_BYTES {
                return Err(OmniError::Storage(format!(
                    "attachment media key must be {} bytes",
                    omni_crypto::ATTACHMENT_MEDIA_KEY_LEN_BYTES
                )));
            }
            if nonce_seed.len() != omni_crypto::ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES {
                return Err(OmniError::Storage(format!(
                    "attachment media nonce seed must be {} bytes",
                    omni_crypto::ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES
                )));
            }
            Ok(AttachmentContentCryptoMaterial {
                media_key: Some(media_key),
                nonce_seed: Some(nonce_seed),
                chunk_size_bytes,
            })
        }
    }
}

fn attachment_chunk_counter(offset_bytes: i64, chunk_size_bytes: u32) -> OmniResult<u64> {
    if offset_bytes < 0 {
        return Err(OmniError::Storage(
            "attachment chunk offset must not be negative".to_string(),
        ));
    }
    let chunk_size_bytes = i64::from(chunk_size_bytes);
    if chunk_size_bytes <= 0 {
        return Err(OmniError::Storage(
            "attachment chunk size must be positive".to_string(),
        ));
    }
    if offset_bytes % chunk_size_bytes != 0 {
        return Err(OmniError::Storage(
            "attachment resume offset is not aligned to the encrypted chunk size".to_string(),
        ));
    }
    Ok((offset_bytes / chunk_size_bytes) as u64)
}

fn attachment_chunk_aad(
    transfer_id: &str,
    frame_kind: &str,
    counter: u64,
    sent_at_unix_ms: i64,
) -> Vec<u8> {
    format!("omni_attachment_frame_v2::{transfer_id}::{frame_kind}::{counter}::{sent_at_unix_ms}")
        .into_bytes()
}

fn next_attachment_transfer_id() -> OmniResult<String> {
    Ok(hex::encode(omni_crypto::random_bytes(16)?))
}

fn seal_attachment_transfer_payload(
    material: &AttachmentContentCryptoMaterial,
    transfer_id: &str,
    frame_kind: &str,
    counter: u64,
    sent_at_unix_ms: i64,
    plaintext: &[u8],
) -> OmniResult<Vec<u8>> {
    let aad = attachment_chunk_aad(transfer_id, frame_kind, counter, sent_at_unix_ms);
    omni_crypto::seal_attachment_media_chunk(
        material
            .media_key
            .as_ref()
            .ok_or_else(|| OmniError::Storage("attachment media key is missing".to_string()))?,
        material.nonce_seed.as_ref().ok_or_else(|| {
            OmniError::Storage("attachment media nonce seed is missing".to_string())
        })?,
        transfer_id,
        counter,
        &aad,
        plaintext,
    )
}

fn seal_attachment_transfer_payload_owned(
    material: &AttachmentContentCryptoMaterial,
    transfer_id: &str,
    frame_kind: &str,
    counter: u64,
    sent_at_unix_ms: i64,
    plaintext: Vec<u8>,
) -> OmniResult<Vec<u8>> {
    seal_attachment_transfer_payload(
        material,
        transfer_id,
        frame_kind,
        counter,
        sent_at_unix_ms,
        &plaintext,
    )
}

fn open_attachment_transfer_payload(
    material: &AttachmentContentCryptoMaterial,
    transfer_id: &str,
    frame_kind: &str,
    counter: u64,
    sent_at_unix_ms: i64,
    ciphertext: &[u8],
) -> OmniResult<Vec<u8>> {
    let aad = attachment_chunk_aad(transfer_id, frame_kind, counter, sent_at_unix_ms);
    omni_crypto::open_attachment_media_chunk(
        material
            .media_key
            .as_ref()
            .ok_or_else(|| OmniError::Storage("attachment media key is missing".to_string()))?,
        material.nonce_seed.as_ref().ok_or_else(|| {
            OmniError::Storage("attachment media nonce seed is missing".to_string())
        })?,
        transfer_id,
        counter,
        &aad,
        ciphertext,
    )
}

fn open_attachment_transfer_payload_owned(
    material: &AttachmentContentCryptoMaterial,
    transfer_id: &str,
    frame_kind: &str,
    counter: u64,
    sent_at_unix_ms: i64,
    ciphertext: Vec<u8>,
) -> OmniResult<Vec<u8>> {
    open_attachment_transfer_payload(
        material,
        transfer_id,
        frame_kind,
        counter,
        sent_at_unix_ms,
        &ciphertext,
    )
}

trait AttachmentTransferPayloadCodec: Sized {
    fn encode_payload(&self) -> OmniResult<Vec<u8>>;
    fn decode_payload(bytes: &[u8]) -> OmniResult<Self>;
}

fn serialize_attachment_transfer_payload<T: AttachmentTransferPayloadCodec>(
    value: &T,
) -> OmniResult<Vec<u8>> {
    value.encode_payload()
}

fn serialize_attachment_transfer_chunk_payload(
    offset_bytes: i64,
    data: &[u8],
    block_proof: Option<&AttachmentBlockProofV2>,
) -> OmniResult<Vec<u8>> {
    let mut buffer = Vec::with_capacity(16 + data.len());
    attachment_payload_write_i64(&mut buffer, offset_bytes);
    attachment_payload_write_bytes(&mut buffer, data)?;
    attachment_payload_write_block_proof(&mut buffer, block_proof)?;
    Ok(buffer)
}

fn attachment_payload_write_hashes(buffer: &mut Vec<u8>, hashes: &[omni_crypto::ContentHash]) -> OmniResult<()> {
    let count = u32::try_from(hashes.len())
        .map_err(|_| OmniError::InvalidInput("attachment hash list is too long".to_string()))?;
    attachment_payload_write_u32(buffer, count);
    for hash in hashes {
        buffer.extend_from_slice(hash);
    }
    Ok(())
}

fn attachment_payload_read_hashes(bytes: &[u8], cursor: &mut usize, max: usize) -> OmniResult<Vec<omni_crypto::ContentHash>> {
    let count = attachment_payload_read_u32(bytes, cursor)? as usize;
    if count > max {
        return Err(OmniError::InvalidInput(format!(
            "attachment hash list holds {count} hashes, at most {max} fit"
        )));
    }
    (0..count)
        .map(|_| {
            let raw = attachment_payload_read_exact(bytes, cursor, 32)?;
            Ok(<omni_crypto::ContentHash>::try_from(raw).expect("read 32 bytes"))
        })
        .collect()
}

fn attachment_payload_write_block_proof(buffer: &mut Vec<u8>, proof: Option<&AttachmentBlockProofV2>) -> OmniResult<()> {
    match proof {
        None => buffer.push(0),
        Some(proof) => {
            buffer.push(1);
            attachment_payload_write_hashes(buffer, &proof.leaves)?;
            attachment_payload_write_hashes(buffer, &proof.path)?;
        }
    }
    Ok(())
}

fn attachment_payload_read_block_proof(bytes: &[u8], cursor: &mut usize) -> OmniResult<Option<AttachmentBlockProofV2>> {
    match attachment_payload_read_exact(bytes, cursor, 1)?[0] {
        0 => Ok(None),
        1 => Ok(Some(AttachmentBlockProofV2 {
            leaves: attachment_payload_read_hashes(bytes, cursor, omni_crypto::CONTENT_TREE_BLOCK_LEAVES as usize)?,
            path: attachment_payload_read_hashes(bytes, cursor, ATTACHMENT_BLOCK_PROOF_PATH_MAX)?,
        })),
        other => Err(OmniError::InvalidInput(format!("attachment block proof flag {other} is unknown"))),
    }
}

fn deserialize_attachment_transfer_payload<T: AttachmentTransferPayloadCodec>(
    bytes: &[u8],
) -> OmniResult<T> {
    T::decode_payload(bytes)
}

fn attachment_payload_write_i64(buffer: &mut Vec<u8>, value: i64) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn attachment_payload_write_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

fn attachment_payload_write_bytes(buffer: &mut Vec<u8>, value: &[u8]) -> OmniResult<()> {
    let length = u32::try_from(value.len()).map_err(|_| {
        OmniError::InvalidInput(
            "attachment payload field exceeds maximum encodable length".to_string(),
        )
    })?;
    attachment_payload_write_u32(buffer, length);
    buffer.extend_from_slice(value);
    Ok(())
}

fn attachment_payload_write_string(buffer: &mut Vec<u8>, value: &str) -> OmniResult<()> {
    attachment_payload_write_bytes(buffer, value.as_bytes())
}

fn attachment_payload_read_exact<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> OmniResult<&'a [u8]> {
    let end = cursor.checked_add(length).ok_or_else(|| {
        OmniError::InvalidInput("attachment payload length overflowed while decoding".to_string())
    })?;
    let slice = bytes.get(*cursor..end).ok_or_else(|| {
        OmniError::InvalidInput(format!(
            "attachment transfer payload is truncated: expected {} more bytes",
            length
        ))
    })?;
    *cursor = end;
    Ok(slice)
}

fn attachment_payload_read_i64(bytes: &[u8], cursor: &mut usize) -> OmniResult<i64> {
    let raw = attachment_payload_read_exact(bytes, cursor, std::mem::size_of::<i64>())?;
    let mut encoded = [0_u8; std::mem::size_of::<i64>()];
    encoded.copy_from_slice(raw);
    Ok(i64::from_le_bytes(encoded))
}

fn attachment_payload_read_u32(bytes: &[u8], cursor: &mut usize) -> OmniResult<u32> {
    let raw = attachment_payload_read_exact(bytes, cursor, std::mem::size_of::<u32>())?;
    let mut encoded = [0_u8; std::mem::size_of::<u32>()];
    encoded.copy_from_slice(raw);
    Ok(u32::from_le_bytes(encoded))
}

fn attachment_payload_read_bytes(bytes: &[u8], cursor: &mut usize) -> OmniResult<Vec<u8>> {
    let length = attachment_payload_read_u32(bytes, cursor)? as usize;
    Ok(attachment_payload_read_exact(bytes, cursor, length)?.to_vec())
}

fn attachment_payload_read_string(bytes: &[u8], cursor: &mut usize) -> OmniResult<String> {
    let raw = attachment_payload_read_bytes(bytes, cursor)?;
    String::from_utf8(raw).map_err(|error| {
        OmniError::InvalidInput(format!(
            "attachment transfer payload contains invalid UTF-8: {error}"
        ))
    })
}

fn ensure_attachment_payload_consumed(bytes: &[u8], cursor: usize) -> OmniResult<()> {
    if cursor == bytes.len() {
        return Ok(());
    }
    Err(OmniError::InvalidInput(format!(
        "attachment transfer payload has {} unexpected trailing bytes",
        bytes.len().saturating_sub(cursor)
    )))
}

impl AttachmentTransferPayloadCodec for AttachmentTransferStartPayloadV2 {
    fn encode_payload(&self) -> OmniResult<Vec<u8>> {
        let mut buffer = Vec::with_capacity(64 + self.content_id_hex.len() + 32 * self.block_layer.len());
        attachment_payload_write_i64(&mut buffer, self.total_size_bytes);
        attachment_payload_write_string(&mut buffer, &self.content_id_hex)?;
        attachment_payload_write_u32(&mut buffer, self.chunk_size_bytes);
        attachment_payload_write_hashes(&mut buffer, &self.block_layer)?;
        Ok(buffer)
    }

    fn decode_payload(bytes: &[u8]) -> OmniResult<Self> {
        let mut cursor = 0;
        let total_size_bytes = attachment_payload_read_i64(bytes, &mut cursor)?;
        let content_id_hex = attachment_payload_read_string(bytes, &mut cursor)?;
        let chunk_size_bytes = attachment_payload_read_u32(bytes, &mut cursor)?;
        let block_layer = attachment_payload_read_hashes(bytes, &mut cursor, ATTACHMENT_BLOCK_LAYER_MAX)?;
        ensure_attachment_payload_consumed(bytes, cursor)?;
        Ok(Self {
            total_size_bytes,
            content_id_hex,
            chunk_size_bytes,
            block_layer,
        })
    }
}

impl AttachmentTransferPayloadCodec for AttachmentTransferChunkPayloadV2 {
    fn encode_payload(&self) -> OmniResult<Vec<u8>> {
        serialize_attachment_transfer_chunk_payload(self.offset_bytes, &self.data, self.block_proof.as_ref())
    }

    fn decode_payload(bytes: &[u8]) -> OmniResult<Self> {
        let mut cursor = 0;
        let offset_bytes = attachment_payload_read_i64(bytes, &mut cursor)?;
        let data = attachment_payload_read_bytes(bytes, &mut cursor)?;
        let block_proof = attachment_payload_read_block_proof(bytes, &mut cursor)?;
        ensure_attachment_payload_consumed(bytes, cursor)?;
        Ok(Self { offset_bytes, data, block_proof })
    }
}

impl AttachmentTransferPayloadCodec for AttachmentTransferCompletePayloadV2 {
    fn encode_payload(&self) -> OmniResult<Vec<u8>> {
        let mut buffer = Vec::with_capacity(32 + self.content_id_hex.len());
        attachment_payload_write_i64(&mut buffer, self.total_size_bytes);
        attachment_payload_write_string(&mut buffer, &self.content_id_hex)?;
        Ok(buffer)
    }

    fn decode_payload(bytes: &[u8]) -> OmniResult<Self> {
        let mut cursor = 0;
        let total_size_bytes = attachment_payload_read_i64(bytes, &mut cursor)?;
        let content_id_hex = attachment_payload_read_string(bytes, &mut cursor)?;
        ensure_attachment_payload_consumed(bytes, cursor)?;
        Ok(Self {
            total_size_bytes,
            content_id_hex,
        })
    }
}

