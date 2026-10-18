use std::io::Read;

use omni_types::AttachmentMediaProtectionMode;

#[cfg(not(test))]
const MANAGED_ATTACHMENT_COPY_MAX_BYTES: i64 = 512 * 1024 * 1024;
#[cfg(test)]
const MANAGED_ATTACHMENT_COPY_MAX_BYTES: i64 = 1024;

/// A download's map of blocks (see [`Storage::attachment_received_blocks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedBlocks {
    pub size: u64,
    pub have: Vec<bool>,
    pub hashes: Vec<omni_crypto::ContentHash>,
}

impl ReceivedBlocks {
    /// Bytes of the blocks here.
    pub fn received_bytes(&self) -> i64 {
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES;
        self.have
            .iter()
            .enumerate()
            .filter(|(_, have)| **have)
            .map(|(index, _)| ((index as u64 + 1) * block).min(self.size) - index as u64 * block)
            .sum::<u64>() as i64
    }

    /// Where the first block not here starts; the file's end when every block is here.
    pub fn first_missing_offset(&self) -> i64 {
        self.have
            .iter()
            .position(|have| !*have)
            .map(|block| block as u64 * omni_crypto::CONTENT_TREE_BLOCK_BYTES)
            .unwrap_or(self.size) as i64
    }
}

/// Why a preparation pass ended without a tree: it was asked to stop, or its row is no longer
/// `Preparing` (cancelled, the chat deleted).
pub const ATTACHMENT_PREPARATION_STOPPED: &str = "attachment preparation stopped";

/// How often a preparation keeps the blocks it has read (`preparation_checkpoint`): a restart
/// loses at most this much reading. By time, not by bytes: the layer written grows with the file
/// (4 MiB for 2 TB), and every few blocks it would be written thousands of times.
#[cfg(not(test))]
const PREPARATION_CHECKPOINT_EVERY_MS: u64 = 15_000;
#[cfg(test)]
const PREPARATION_CHECKPOINT_EVERY_MS: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachmentSourceMetadata {
    size_bytes: i64,
    modified_unix_ms: i64,
}

impl Storage {
    fn build_attachment_content_crypto_defaults(
        &self,
        media_protection_mode: AttachmentMediaProtectionMode,
    ) -> (Option<String>, Option<String>) {
        match media_protection_mode {
            AttachmentMediaProtectionMode::E2eeMedia => {
                let mut media_key = [0_u8; omni_crypto::ATTACHMENT_MEDIA_KEY_LEN_BYTES];
                let mut nonce_seed = [0_u8; omni_crypto::ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES];
                OsRng.fill_bytes(&mut media_key);
                OsRng.fill_bytes(&mut nonce_seed);
                (Some(hex::encode(media_key)), Some(hex::encode(nonce_seed)))
            }
        }
    }

    fn attachments_root_for_db_path(path: &Path) -> OmniResult<PathBuf> {
        let parent = path.parent().ok_or_else(|| {
            OmniError::Storage("database path does not have a parent directory".to_string())
        })?;
        Ok(parent.join("attachments"))
    }

    fn attachments_root_dir(&self) -> OmniResult<PathBuf> {
        let root = Self::attachments_root_for_db_path(&self.db_path)?;
        fs::create_dir_all(&root).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create attachments root '{}': {error}",
                root.display()
            ))
        })?;
        Ok(root)
    }

    fn attachments_blobs_dir(&self) -> OmniResult<PathBuf> {
        let path = self.attachments_root_dir()?.join("blobs");
        fs::create_dir_all(&path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create attachments blob directory '{}': {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    fn attachments_temp_dir(&self) -> OmniResult<PathBuf> {
        let path = self.attachments_root_dir()?.join("temp");
        fs::create_dir_all(&path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create attachments temp directory '{}': {error}",
                path.display()
            ))
        })?;
        Ok(path)
    }

    /// Where a picture sent as a photo is re-encoded before it becomes the attachment.
    pub fn stage_outgoing_photo(
        &self,
        message_id: &str,
        source_path: &Path,
    ) -> OmniResult<(PathBuf, omni_types::AttachmentPresentationV1)> {
        let prepared = crate::photo::prepare_photo(source_path)?;
        let staged = self
            .attachments_temp_dir()?
            .join(format!("{}.photo.jpg", sanitize_attachment_file_name(message_id)));
        self.write_media_file(&staged, &prepared.jpeg)?;
        Ok((staged, prepared.presentation))
    }

    /// Where a received photo downloads by itself (the chat shows it; «Сохранить как…» copies it).
    pub fn photo_download_path(&self, message_id: &str) -> OmniResult<PathBuf> {
        let dir = self.attachments_root_dir()?.join("photos");
        fs::create_dir_all(&dir).map_err(|error| {
            OmniError::Storage(format!("failed to create the photo directory '{}': {error}", dir.display()))
        })?;
        Ok(dir.join(format!("{}.jpg", sanitize_attachment_file_name(message_id))))
    }

    fn managed_blob_path_for_attachment(
        &self,
        content_id_hex: &str,
        file_name: &str,
    ) -> OmniResult<PathBuf> {
        let extension = Path::new(file_name)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| format!(".{}", sanitize_attachment_file_name(value)))
            .unwrap_or_default();
        Ok(self
            .attachments_blobs_dir()?
            .join(format!("{content_id_hex}{extension}")))
    }

    fn temp_download_path_for_message(&self, message_id: &str) -> OmniResult<PathBuf> {
        Ok(self.attachments_temp_dir()?.join(format!(
            "{}.part",
            sanitize_attachment_file_name(message_id)
        )))
    }

    fn temp_download_path_for_save_path(
        &self,
        save_path: &Path,
        message_id: &str,
    ) -> OmniResult<PathBuf> {
        let Some(parent) = save_path.parent() else {
            return self.temp_download_path_for_message(message_id);
        };
        let file_name = save_path
            .file_name()
            .and_then(|value| value.to_str())
            .map(sanitize_attachment_file_name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| sanitize_attachment_file_name(message_id));
        let message_part = sanitize_attachment_file_name(message_id);
        Ok(parent.join(format!("{file_name}.{message_part}.part")))
    }

    fn attachment_source_metadata(path: &Path) -> OmniResult<AttachmentSourceMetadata> {
        let metadata = fs::metadata(path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to inspect attachment source '{}': {error}",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(OmniError::InvalidInput(
                "attachment source path must point to a regular file".to_string(),
            ));
        }
        let modified = metadata.modified().map_err(|error| {
            OmniError::Storage(format!(
                "failed to read attachment source last-modified timestamp '{}': {error}",
                path.display()
            ))
        })?;
        let modified_unix_ms = modified
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                OmniError::Storage(format!(
                    "attachment source last-modified timestamp '{}' predates unix epoch: {error}",
                    path.display()
                ))
            })?
            .as_millis() as i64;
        Ok(AttachmentSourceMetadata {
            // The plaintext's size: a file in Omni's storage is encrypted, and its disk size is not it.
            size_bytes: omni_crypto::file_at_rest_len(path)? as i64,
            modified_unix_ms,
        })
    }

    fn validate_attachment_source_metadata_unchanged(
        path: &Path,
        expected: AttachmentSourceMetadata,
    ) -> OmniResult<AttachmentSourceMetadata> {
        let actual = Self::attachment_source_metadata(path)?;
        if actual != expected {
            return Err(OmniError::Storage(format!(
                "attachment source '{}' changed while it was being prepared",
                path.display()
            )));
        }
        Ok(actual)
    }

    fn ensure_attachment_download_space_available(
        &self,
        temp_download_path: &Path,
        file_size_bytes: i64,
        resume_offset_bytes: i64,
    ) -> OmniResult<()> {
        let required_bytes =
            attachment_download_required_space_bytes(file_size_bytes, resume_offset_bytes);
        if required_bytes == 0 {
            return Ok(());
        }
        let probe_path = temp_download_path.parent().unwrap_or(temp_download_path);
        let Some(available_bytes) = attachment_available_space_bytes(probe_path)? else {
            return Ok(());
        };
        if available_bytes < required_bytes {
            return Err(OmniError::Storage(format!(
                "not enough free disk space for attachment download '{}': required {} bytes, available {} bytes",
                temp_download_path.display(),
                required_bytes,
                available_bytes
            )));
        }
        Ok(())
    }

    fn reserve_attachment_download_storage(
        &self,
        temp_download_path: &Path,
        file_size_bytes: i64,
    ) -> OmniResult<()> {
        if file_size_bytes <= 0 {
            return Ok(());
        }
        reserve_attachment_file_allocation(temp_download_path, file_size_bytes as u64)
    }

    fn validate_attachment_serving_source(
        &self,
        runtime: &AttachmentRuntimeInfo,
    ) -> OmniResult<PathBuf> {
        if !runtime.attachment.local_serving_enabled {
            return Err(OmniError::Storage(
                "attachment is not available for download".to_string(),
            ));
        }
        let path = runtime.local_blob_path.clone().ok_or_else(|| {
            OmniError::Storage("attachment source path is missing".to_string())
        })?;
        let actual = Self::attachment_source_metadata(&path)?;
        if actual.size_bytes != runtime.attachment.file_size_bytes {
            return Err(OmniError::Storage(format!(
                "attachment source '{}' changed after it was offered",
                path.display()
            )));
        }
        if let Some(expected_size_bytes) = runtime.local_source_size_bytes {
            if actual.size_bytes != expected_size_bytes {
                return Err(OmniError::Storage(format!(
                    "attachment source '{}' changed after it was offered",
                    path.display()
                )));
            }
        }
        if !runtime.local_blob_managed {
            if let Some(expected_modified_unix_ms) = runtime.local_source_modified_unix_ms {
                if actual.modified_unix_ms != expected_modified_unix_ms {
                    return Err(OmniError::Storage(format!(
                        "attachment source '{}' changed after it was offered",
                        path.display()
                    )));
                }
            }
        }
        Ok(path)
    }

    /// The hash tree of a file (docs/decisions/attachment-content-tree.md) — one pass, the block
    /// layer kept.
    /// The content tree of a file. `progress` hears the bytes hashed so far (about every leaf) and
    /// stops the pass by answering `false`.
    fn hash_file_content_tree(
        &self,
        path: &Path,
        progress: &mut dyn FnMut(u64) -> bool,
    ) -> OmniResult<omni_crypto::ContentTreeLayer> {
        self.hash_file_content_tree_from(path, Vec::new(), progress, &mut |_| {})
    }

    /// The content tree of a file whose first `read_before` blocks were read already (a reading cut
    /// short): the reading picks up after them. `checkpoint` hears the blocks read whole so far,
    /// every `PREPARATION_CHECKPOINT_EVERY_MS`.
    fn hash_file_content_tree_from(
        &self,
        path: &Path,
        read_before: Vec<omni_crypto::ContentHash>,
        progress: &mut dyn FnMut(u64) -> bool,
        checkpoint: &mut dyn FnMut(&[omni_crypto::ContentHash]),
    ) -> OmniResult<omni_crypto::ContentTreeLayer> {
        use std::io::{Seek, SeekFrom};
        // Hashed as plaintext: the content id is the file's, the same on every device whatever its
        // storage (docs/decisions/db-at-rest-encryption.md, «Вложения»).
        let mut file = self.open_media_file(path)?;
        let mut tree = omni_crypto::ContentTreeBuilder::resume(read_before);
        let mut hashed: u64 = tree.size();
        file.seek(SeekFrom::Start(hashed)).map_err(|error| {
            OmniError::Storage(format!("failed to read attachment file '{}': {error}", path.display()))
        })?;
        let mut buffer = vec![0_u8; omni_crypto::CONTENT_TREE_LEAF_BYTES as usize];
        let mut checkpointed = (std::time::Instant::now(), tree.blocks_done().len());

        loop {
            let read = file.read(&mut buffer).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to read attachment file '{}': {error}",
                    path.display()
                ))
            })?;
            if read == 0 {
                break;
            }
            tree.update(&buffer[..read]);
            hashed += read as u64;
            if tree.blocks_done().len() > checkpointed.1
                && checkpointed.0.elapsed() >= std::time::Duration::from_millis(PREPARATION_CHECKPOINT_EVERY_MS)
            {
                checkpoint(tree.blocks_done());
                checkpointed = (std::time::Instant::now(), tree.blocks_done().len());
            }
            if !progress(hashed) {
                return Err(OmniError::Storage(ATTACHMENT_PREPARATION_STOPPED.to_string()));
            }
        }

        Ok(tree.finish())
    }

    /// The block layer of the attachment's tree, when this device has it (it sent the file, or its
    /// copy is complete) and it still proves the offered identity.
    pub fn attachment_content_tree(&self, message_id: &str) -> OmniResult<Option<omni_crypto::ContentTreeLayer>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let row: Option<(i64, String, Option<Vec<u8>>)> = self
            .connection
            .query_row(
                "SELECT file_size_bytes, content_id_hex, content_tree_blocks FROM message_attachments
                 WHERE message_id = ?1 AND profile_id = ?2",
                params![message_id, profile_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((size, id_hex, Some(blob))) = row else {
            return Ok(None);
        };
        if blob.len() % 32 != 0 {
            return Ok(None);
        }
        let blocks = blob
            .chunks_exact(32)
            .map(|hash| <[u8; 32]>::try_from(hash).expect("chunks_exact(32)"))
            .collect();
        Ok(omni_crypto::ContentTreeLayer::from_blocks(size.max(0) as u64, blocks)
            .filter(|layer| hex::encode(layer.id) == id_hex))
    }

    /// The blocks of a download checked against the tree and on disk (stage 3,
    /// attachment-content-tree.md): one bit per 16 MiB block in `content_blocks_have`, its hash at
    /// its place in `content_tree_blocks` (n × 32 bytes, zeros where missing). Blocks land in any
    /// order — from several sources at once — so a download is a map, not a prefix.
    pub fn attachment_received_blocks(&self, message_id: &str) -> OmniResult<ReceivedBlocks> {
        let profile_id = self.resolve_transition_profile_id()?;
        let row: Option<(i64, Option<Vec<u8>>, Option<Vec<u8>>)> = self
            .connection
            .query_row(
                "SELECT file_size_bytes, content_blocks_have, content_tree_blocks FROM message_attachments
                 WHERE message_id = ?1 AND profile_id = ?2",
                params![message_id, profile_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((size, have_bits, hashes_blob)) = row else {
            return Err(OmniError::Storage(format!("attachment '{message_id}' was not found")));
        };
        let count = omni_crypto::content_block_count(size.max(0) as u64) as usize;
        let hashes_blob = hashes_blob.filter(|blob| blob.len() == count * 32).unwrap_or_default();
        let have_bits = have_bits.unwrap_or_default();
        let mut blocks = ReceivedBlocks { size: size.max(0) as u64, have: vec![false; count], hashes: vec![[0_u8; 32]; count] };
        if hashes_blob.is_empty() {
            return Ok(blocks);
        }
        for block in 0..count {
            blocks.hashes[block] = <[u8; 32]>::try_from(&hashes_blob[block * 32..block * 32 + 32]).expect("32 bytes");
            blocks.have[block] = have_bits.get(block / 8).is_some_and(|byte| byte & (1 << (block % 8)) != 0);
        }
        Ok(blocks)
    }

    fn put_attachment_received_blocks(&self, message_id: &str, blocks: &ReceivedBlocks) -> OmniResult<()> {
        let mut have_bits = vec![0_u8; blocks.have.len().div_ceil(8)];
        for (block, have) in blocks.have.iter().enumerate() {
            if *have {
                have_bits[block / 8] |= 1 << (block % 8);
            }
        }
        let hashes: Vec<u8> = blocks.hashes.iter().flatten().copied().collect();
        self.connection
            .execute(
                "UPDATE message_attachments SET content_blocks_have = ?2, content_tree_blocks = ?3
                 WHERE message_id = ?1 AND profile_id = ?4",
                params![message_id, have_bits, hashes, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The block layer a source sent at the start of a stream (stage 3, attachment-content-tree.md):
    /// kept whole once it proves the offered identity, so this download can prove any block it
    /// holds to a member who asks before it has the whole file. The map is left as it is.
    pub fn adopt_received_tree_layer(&self, message_id: &str, blocks: &[omni_crypto::ContentHash]) -> OmniResult<()> {
        let runtime = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?;
        let layer = omni_crypto::ContentTreeLayer::from_blocks(runtime.attachment.file_size_bytes.max(0) as u64, blocks.to_vec())
            .filter(|layer| hex::encode(layer.id) == runtime.attachment.content_id_hex)
            .ok_or_else(|| OmniError::Storage("the tree a source sent does not prove the offered file".to_string()))?;
        if self.attachment_content_tree(message_id)?.is_some() {
            return Ok(()); // the same layer: the id proves it
        }
        self.set_attachment_content_tree(message_id, &layer)
    }

    /// What a download in progress can hand on before it is whole (stage 3): its temp file and the
    /// blocks in its map — checked and synced, never written again. `None` when it is not being
    /// downloaded, has no block yet, or never got the file's layer to prove its blocks with.
    pub fn attachment_partial_source(&self, message_id: &str) -> OmniResult<Option<(PathBuf, Vec<bool>)>> {
        let Some(runtime) = self.get_attachment_runtime_info(message_id)? else {
            return Ok(None);
        };
        if !matches!(
            runtime.attachment.state,
            AttachmentTransferState::Requested
                | AttachmentTransferState::Downloading
                | AttachmentTransferState::WaitingForReconnect
        ) {
            return Ok(None);
        }
        let Some(temp_path) = runtime.temp_download_path.clone() else {
            return Ok(None);
        };
        if self.attachment_content_tree(message_id)?.is_none() {
            return Ok(None);
        }
        let blocks = self.checked_received_blocks(&runtime)?;
        if !blocks.have.iter().any(|have| *have) {
            return Ok(None);
        }
        Ok(Some((temp_path, blocks.have)))
    }

    /// Blocks checked and synced to disk by the receive path: into the map, with their hashes.
    /// The caller has made the data durable first — a bit set here is trusted after a crash.
    pub fn mark_attachment_blocks_received(
        &self,
        message_id: &str,
        received: &[(u64, omni_crypto::ContentHash)],
    ) -> OmniResult<ReceivedBlocks> {
        let mut blocks = self.attachment_received_blocks(message_id)?;
        for (block, hash) in received {
            if let Some(slot) = blocks.have.get_mut(*block as usize) {
                *slot = true;
                blocks.hashes[*block as usize] = *hash;
            }
        }
        self.put_attachment_received_blocks(message_id, &blocks)?;
        Ok(blocks)
    }

    /// The map of a download as its temp file allows: no file — nothing is here; a file shorter
    /// than a block's end — that block is not here either (it was cut, or never written whole).
    fn checked_received_blocks(&self, runtime: &AttachmentRuntimeInfo) -> OmniResult<ReceivedBlocks> {
        let mut blocks = self.attachment_received_blocks(&runtime.attachment.message_id)?;
        if !blocks.have.iter().any(|have| *have) {
            return Ok(blocks);
        }
        let len = match runtime.temp_download_path.as_ref().map(fs::metadata) {
            Some(Ok(metadata)) => metadata.len(),
            Some(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(OmniError::Storage(format!("failed to inspect temporary download file: {error}")));
            }
            _ => 0,
        };
        let mut changed = false;
        for (block, have) in blocks.have.iter_mut().enumerate() {
            let end = ((block as u64 + 1) * omni_crypto::CONTENT_TREE_BLOCK_BYTES).min(blocks.size);
            if *have && len < end {
                *have = false;
                changed = true;
            }
        }
        if changed {
            self.put_attachment_received_blocks(&runtime.attachment.message_id, &blocks)?;
        }
        Ok(blocks)
    }

    /// The tree of a download whose every block is here — the file is complete and checked.
    pub fn received_content_tree(&self, message_id: &str) -> OmniResult<Option<omni_crypto::ContentTreeLayer>> {
        let runtime = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?;
        let blocks = self.checked_received_blocks(&runtime)?;
        if blocks.have.iter().any(|have| !*have) {
            return Ok(None);
        }
        Ok(omni_crypto::ContentTreeLayer::from_blocks(blocks.size, blocks.hashes)
            .filter(|layer| hex::encode(layer.id) == runtime.attachment.content_id_hex))
    }

    fn set_attachment_content_tree(&self, message_id: &str, layer: &omni_crypto::ContentTreeLayer) -> OmniResult<()> {
        let blob: Vec<u8> = layer.blocks.iter().flatten().copied().collect();
        self.connection
            .execute(
                "UPDATE message_attachments SET content_tree_blocks = ?2 WHERE message_id = ?1 AND profile_id = ?3",
                params![message_id, blob, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    fn copy_into_managed_blob_if_needed(
        &self,
        source_path: &Path,
        managed_blob_path: &Path,
    ) -> OmniResult<bool> {
        if managed_blob_path.exists() {
            return Ok(false);
        }
        if let Some(parent) = managed_blob_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to create attachment storage directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }
        self.copy_media_file(source_path, managed_blob_path)?;
        Ok(true)
    }

    fn existing_local_attachment_copy_path(
        local_blob_path: Option<&Path>,
        local_saved_path: Option<&Path>,
    ) -> Option<PathBuf> {
        [local_blob_path, local_saved_path]
            .into_iter()
            .flatten()
            .find(|path| path.is_file())
            .map(Path::to_path_buf)
    }

    pub fn get_attachment_runtime_info(
        &self,
        message_id: &str,
    ) -> OmniResult<Option<AttachmentRuntimeInfo>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT
                    message_id,
                    dialog_key,
                    file_name,
                    file_size_bytes,
                    mime_type,
                    content_id_hex,
                    state,
                    bytes_transferred,
                    local_blob_path,
                    local_serving_enabled,
                    remote_holder_available,
                    local_saved_path,
                    temp_download_path,
                    active_transfer_id,
                    last_error,
                    content_encryption,
                    content_key_hex,
                    content_nonce_seed_hex,
                    content_chunk_size_bytes,
                    local_blob_managed,
                    local_source_size_bytes,
                    local_source_modified_unix_ms,
                    presentation_json
                FROM message_attachments
                WHERE message_id = ?1
                  AND profile_id = ?2
                ",
                params![message_id, profile_id.0],
                |row| {
                    let local_blob_path = row.get::<_, Option<String>>(8)?;
                    let temp_download_path = row.get::<_, Option<String>>(12)?;
                    let local_saved_path = row.get::<_, Option<String>>(11)?;
                    let local_blob_path = local_blob_path.map(PathBuf::from);
                    let local_saved_path = local_saved_path.map(PathBuf::from);
                    let local_copy_path = Self::existing_local_attachment_copy_path(
                        local_blob_path.as_deref(),
                        local_saved_path.as_deref(),
                    );
                    let state = parse_attachment_state(&row.get::<_, String>(6)?)
                        .map_err(into_sqlite_error)?;
                    // A file still being prepared is on this disk, but not yet one to serve: the
                    // bubble shows its preparation, not «here».
                    let local_blob_available =
                        local_copy_path.is_some() && state != AttachmentTransferState::Preparing;

                    Ok(AttachmentRuntimeInfo {
                        attachment: AttachmentRecord {
                            message_id: row.get(0)?,
                            file_name: row.get(2)?,
                            file_size_bytes: row.get(3)?,
                            mime_type: row.get(4)?,
                            content_id_hex: row.get(5)?,
                            content_encryption: row.get(15)?,
                            state,
                            bytes_transferred: row.get(7)?,
                            local_blob_available,
                            local_serving_enabled: row.get::<_, i64>(9)? != 0,
                            remote_holder_available: row.get::<_, i64>(10)? != 0,
                            local_saved_path: local_saved_path
                                .as_ref()
                                .map(|path| path.to_string_lossy().into_owned()),
                            last_error: row.get(14)?,
                            presentation: row
                                .get::<_, Option<String>>(22)?
                                .and_then(|json| serde_json::from_str(&json).ok()),
                            local_copy_path: local_copy_path
                                .map(|path| path.to_string_lossy().into_owned()),
                            source_count: 0,
                        },
                        dialog_key: row.get(1)?,
                        local_blob_path,
                        local_blob_managed: row.get::<_, i64>(19)? != 0,
                        temp_download_path: temp_download_path.map(PathBuf::from),
                        requested_save_path: local_saved_path,
                        active_transfer_id: row.get(13)?,
                        content_encryption: row.get(15)?,
                        content_key_hex: row.get(16)?,
                        content_nonce_seed_hex: row.get(17)?,
                        content_chunk_size_bytes: row.get(18)?,
                        local_source_size_bytes: row.get(20)?,
                        local_source_modified_unix_ms: row.get(21)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_attachment_runtime_info_by_active_transfer_id(
        &self,
        transfer_id: &str,
    ) -> OmniResult<Option<AttachmentRuntimeInfo>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let message_id = self
            .connection
            .query_row(
                "
                SELECT message_id
                FROM message_attachments
                WHERE active_transfer_id = ?1
                  AND profile_id = ?2
                LIMIT 1
                ",
                params![transfer_id, profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        match message_id {
            Some(message_id) => self.get_attachment_runtime_info(&message_id),
            None => Ok(None),
        }
    }

    pub fn attach_runtime_info_to_message(
        &self,
        mut message: ChatMessageRecord,
    ) -> OmniResult<ChatMessageRecord> {
        message.attachment = self
            .get_attachment_runtime_info(&message.id)?
            .map(|runtime| runtime.attachment);
        message.told_by_contact = self.history_import_source(&message.id)?.is_some();
        Ok(message)
    }

    pub fn attachment_serving_blob_path(&self, message_id: &str) -> OmniResult<Option<PathBuf>> {
        let Some(runtime) = self.get_attachment_runtime_info(message_id)? else {
            return Ok(None);
        };
        if !runtime.attachment.local_serving_enabled {
            return Ok(None);
        }

        Ok(Some(self.validate_attachment_serving_source(&runtime)?))
    }

    /// A group member who downloaded a file hands it on too (docs/decisions/media-messages.md §3):
    /// its saved copy is served from now on, while it stays the same file (size and time are taken
    /// now; a changed copy stops being served, and an asker would refuse it by its hash anyway).
    /// `false` — nothing to serve (not downloaded, or the saved copy is gone or not the file).
    pub fn serve_downloaded_attachment(&self, message_id: &str) -> OmniResult<bool> {
        let Some(runtime) = self.get_attachment_runtime_info(message_id)? else {
            return Ok(false);
        };
        if runtime.attachment.state != AttachmentTransferState::Downloaded {
            return Ok(false);
        }
        let Some(saved) = runtime.attachment.local_saved_path.as_deref().map(PathBuf::from) else {
            return Ok(false);
        };
        let Ok(metadata) = Self::attachment_source_metadata(&saved) else {
            return Ok(false);
        };
        if metadata.size_bytes != runtime.attachment.file_size_bytes {
            return Ok(false);
        }
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET local_blob_path = ?2,
                    local_blob_managed = 0,
                    local_source_size_bytes = ?3,
                    local_source_modified_unix_ms = ?4,
                    local_serving_enabled = 1
                WHERE message_id = ?1
                  AND profile_id = ?5
                ",
                params![
                    message_id,
                    saved.to_string_lossy(),
                    metadata.size_bytes,
                    metadata.modified_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(true)
    }

    pub fn attachment_local_blob_path(&self, message_id: &str) -> OmniResult<Option<PathBuf>> {
        let Some(runtime) = self.get_attachment_runtime_info(message_id)? else {
            return Ok(None);
        };

        Ok(runtime.local_blob_path.filter(|path| path.exists()))
    }

    pub fn attachment_download_temp_path(&self, message_id: &str) -> OmniResult<Option<PathBuf>> {
        Ok(self
            .get_attachment_runtime_info(message_id)?
            .and_then(|runtime| runtime.temp_download_path))
    }

    pub fn attachment_requested_save_path(&self, message_id: &str) -> OmniResult<Option<PathBuf>> {
        Ok(self
            .get_attachment_runtime_info(message_id)?
            .and_then(|runtime| runtime.requested_save_path))
    }

    /// Where a download asks to be resumed from: its first block not here (the file's end when
    /// none is missing).
    pub fn attachment_download_resume_offset(&self, message_id: &str) -> OmniResult<i64> {
        let Some(runtime) = self.get_attachment_runtime_info(message_id)? else {
            return Ok(0);
        };
        Ok(self.checked_received_blocks(&runtime)?.first_missing_offset())
    }

    /// Both steps of [`Self::begin_outgoing_attachment`] and
    /// [`Self::finish_outgoing_attachment_preparation`] at once — a small file, ready on return.
    pub fn prepare_outgoing_attachment(
        &self,
        message_id: &str,
        dialog_key: &str,
        source_file_path: &str,
        display_file_name: Option<&str>,
        media_protection_mode: AttachmentMediaProtectionMode,
        created_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        self.begin_outgoing_attachment(
            message_id,
            dialog_key,
            source_file_path,
            display_file_name,
            media_protection_mode,
            created_at_unix_ms,
        )?;
        self.finish_outgoing_attachment_preparation(message_id, &mut |_| true, created_at_unix_ms)?;
        self.get_attachment_runtime_info(message_id)?
            .filter(|runtime| runtime.attachment.state != AttachmentTransferState::Preparing)
            .ok_or_else(|| OmniError::Storage(ATTACHMENT_PREPARATION_STOPPED.to_string()))
    }

    /// The file of an outgoing message, written down as `Preparing`: named, sized, keyed — its tree
    /// not yet read. The queue leaves it be until [`Self::finish_outgoing_attachment_preparation`]
    /// makes it `Available` (docs/decisions/attachment-content-tree.md, «Подготовка у отправителя»):
    /// a terabyte takes hours to read, and the message is in the chat from the first second.
    pub fn begin_outgoing_attachment(
        &self,
        message_id: &str,
        dialog_key: &str,
        source_file_path: &str,
        display_file_name: Option<&str>,
        media_protection_mode: AttachmentMediaProtectionMode,
        created_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let profile_id = self.resolve_transition_profile_id()?;
        let source_path = PathBuf::from(source_file_path);
        let file_name = display_file_name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                source_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
            .ok_or_else(|| {
                OmniError::InvalidInput(
                    "attachment source path must point to a file with a name".to_string(),
                )
            })?;
        let file_name = sanitize_attachment_file_name(&file_name);
        if file_name.is_empty() {
            return Err(OmniError::InvalidInput(
                "attachment file name must not be empty after normalization".to_string(),
            ));
        }
        // What the file was when it was picked: the tree read later must be of this very file.
        let source_metadata = Self::attachment_source_metadata(&source_path)?;
        let (content_key_hex, content_nonce_seed_hex) =
            self.build_attachment_content_crypto_defaults(media_protection_mode);

        self.connection
            .execute(
                "
                INSERT INTO message_attachments (
                    message_id,
                    dialog_key,
                    file_name,
                    file_size_bytes,
                    mime_type,
                    content_id_hex,
                    state,
                    bytes_transferred,
                    local_blob_path,
                    local_serving_enabled,
                    remote_holder_available,
                    local_saved_path,
                    temp_download_path,
                    active_transfer_id,
                    last_error,
                    content_encryption,
                    content_key_hex,
                    content_nonce_seed_hex,
                    content_chunk_size_bytes,
                    local_blob_managed,
                    local_source_size_bytes,
                    local_source_modified_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (
                    ?1, ?2, ?3, ?4, NULL, '', ?5, 0, NULL, 1, 0, ?6, NULL, NULL, NULL, ?7, ?8, ?9, NULL, 0, ?10, ?11, ?12, ?12, ?13
                )
                ON CONFLICT(message_id) DO UPDATE SET
                    dialog_key = excluded.dialog_key,
                    file_name = excluded.file_name,
                    file_size_bytes = excluded.file_size_bytes,
                    mime_type = excluded.mime_type,
                    content_id_hex = excluded.content_id_hex,
                    content_tree_blocks = NULL,
                    state = excluded.state,
                    bytes_transferred = excluded.bytes_transferred,
                    local_blob_path = excluded.local_blob_path,
                    local_serving_enabled = excluded.local_serving_enabled,
                    remote_holder_available = excluded.remote_holder_available,
                    local_saved_path = excluded.local_saved_path,
                    temp_download_path = excluded.temp_download_path,
                    active_transfer_id = excluded.active_transfer_id,
                    last_error = excluded.last_error,
                    content_encryption = excluded.content_encryption,
                    content_key_hex = excluded.content_key_hex,
                    content_nonce_seed_hex = excluded.content_nonce_seed_hex,
                    content_chunk_size_bytes = excluded.content_chunk_size_bytes,
                    local_blob_managed = excluded.local_blob_managed,
                    local_source_size_bytes = excluded.local_source_size_bytes,
                    local_source_modified_unix_ms = excluded.local_source_modified_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    message_id,
                    normalize_dialog_key(dialog_key),
                    file_name,
                    source_metadata.size_bytes,
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                    source_path.to_string_lossy(),
                    media_protection_mode.as_db_value(),
                    content_key_hex,
                    content_nonce_seed_hex,
                    source_metadata.size_bytes,
                    source_metadata.modified_unix_ms,
                    created_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not persisted", message_id))
            })
    }

    /// Reads the tree of a `Preparing` file and makes it `Available`: the content id, the block
    /// layer, the managed copy of a small file. **One reading serves every message of the same
    /// file** (C3, attachment-content-tree.md): each row still `Preparing` with the same source —
    /// path, size and time of change as picked — is completed by it; a file whose tree was read
    /// before (sent earlier, unchanged since) is not read again at all. `progress` hears the bytes
    /// read and stops the pass by answering `false`. A file that changed since it was picked, or
    /// no row left `Preparing` (cancelled, the chat deleted), ends it with an error and leaves no
    /// copy behind. Returns the messages it completed.
    pub fn finish_outgoing_attachment_preparation(
        &self,
        message_id: &str,
        progress: &mut dyn FnMut(u64) -> bool,
        now_unix_ms: i64,
    ) -> OmniResult<Vec<String>> {
        let runtime = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?;
        if runtime.attachment.state != AttachmentTransferState::Preparing {
            return Err(OmniError::Storage(ATTACHMENT_PREPARATION_STOPPED.to_string()));
        }
        let source_path = runtime.requested_save_path.clone().ok_or_else(|| {
            OmniError::Storage(format!("attachment '{message_id}' has no source file"))
        })?;
        let picked = AttachmentSourceMetadata {
            size_bytes: runtime.local_source_size_bytes.unwrap_or(-1),
            modified_unix_ms: runtime.local_source_modified_unix_ms.unwrap_or(-1),
        };
        Self::validate_attachment_source_metadata_unchanged(&source_path, picked)?;
        let tree = match self.prepared_tree_of_source(&source_path, picked)? {
            Some(tree) => tree,
            None => {
                // A reading cut short (a restart, the profile stopped) picks up at the blocks it
                // had read: the file is the one picked (checked above), so they still hold.
                let read_before = self.preparation_checkpoint(&source_path, picked)?;
                self.hash_file_content_tree_from(&source_path, read_before, progress, &mut |blocks| {
                    let _ = self.set_preparation_checkpoint(&source_path, picked, blocks);
                })?
            }
        };
        let source_metadata = Self::validate_attachment_source_metadata_unchanged(&source_path, picked)?;
        if source_metadata.size_bytes != tree.size as i64 {
            return Err(OmniError::Storage(format!(
                "attachment source '{}' changed while it was being prepared",
                source_path.display()
            )));
        }
        let mut completed = Vec::new();
        for (sibling, file_name) in self.preparing_rows_of_source(&source_path, picked)? {
            if self.complete_prepared_attachment(&sibling, &file_name, &source_path, source_metadata, &tree, now_unix_ms)? {
                completed.push(sibling);
            }
        }
        if completed.is_empty() {
            return Err(OmniError::Storage(ATTACHMENT_PREPARATION_STOPPED.to_string()));
        }
        Ok(completed)
    }

    /// The blocks a reading of this very file (path, size, time of change) had read whole when it
    /// was cut short — kept in `content_tree_blocks` of its `Preparing` rows, which hold no tree
    /// yet. None, or not of this file's shape: from the start.
    fn preparation_checkpoint(
        &self,
        source_path: &Path,
        picked: AttachmentSourceMetadata,
    ) -> OmniResult<Vec<omni_crypto::ContentHash>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let blob: Option<Vec<u8>> = self
            .connection
            .query_row(
                "
                SELECT content_tree_blocks FROM message_attachments
                WHERE profile_id = ?1 AND state = ?2 AND local_saved_path = ?3
                  AND local_source_size_bytes = ?4 AND local_source_modified_unix_ms = ?5
                  AND content_tree_blocks IS NOT NULL
                ORDER BY length(content_tree_blocks) DESC
                LIMIT 1
                ",
                params![
                    profile_id.0,
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                    source_path.to_string_lossy(),
                    picked.size_bytes,
                    picked.modified_unix_ms,
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some(blob) = blob else {
            return Ok(Vec::new());
        };
        let size = picked.size_bytes.max(0) as u64;
        let blocks = blob.len() as u64 / 32;
        if blob.len() % 32 != 0 || blocks * omni_crypto::CONTENT_TREE_BLOCK_BYTES > size {
            return Ok(Vec::new());
        }
        Ok(blob
            .chunks_exact(32)
            .map(|hash| <[u8; 32]>::try_from(hash).expect("chunks_exact(32)"))
            .collect())
    }

    /// Keeps the blocks read whole so far on every row still `Preparing` from this very file.
    fn set_preparation_checkpoint(
        &self,
        source_path: &Path,
        picked: AttachmentSourceMetadata,
        blocks: &[omni_crypto::ContentHash],
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                UPDATE message_attachments SET content_tree_blocks = ?1
                WHERE profile_id = ?2 AND state = ?3 AND local_saved_path = ?4
                  AND local_source_size_bytes = ?5 AND local_source_modified_unix_ms = ?6
                ",
                params![
                    blocks.concat(),
                    profile_id.0,
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                    source_path.to_string_lossy(),
                    picked.size_bytes,
                    picked.modified_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The rows still `Preparing` from this very file (path, size, time of change), with the name
    /// each goes under.
    fn preparing_rows_of_source(
        &self,
        source_path: &Path,
        picked: AttachmentSourceMetadata,
    ) -> OmniResult<Vec<(String, String)>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id, file_name FROM message_attachments
                WHERE profile_id = ?1 AND state = ?2 AND local_saved_path = ?3
                  AND local_source_size_bytes = ?4 AND local_source_modified_unix_ms = ?5
                ORDER BY created_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    profile_id.0,
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                    source_path.to_string_lossy(),
                    picked.size_bytes,
                    picked.modified_unix_ms,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// The tree read earlier from this very file, unchanged since (same size and time of change):
    /// a file sent to a second chat is not read again. The layer is checked against its id.
    fn prepared_tree_of_source(
        &self,
        source_path: &Path,
        picked: AttachmentSourceMetadata,
    ) -> OmniResult<Option<omni_crypto::ContentTreeLayer>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let done: Option<String> = self
            .connection
            .query_row(
                "
                SELECT message_id FROM message_attachments
                WHERE profile_id = ?1 AND local_saved_path = ?2 AND state != ?3
                  AND local_source_size_bytes = ?4 AND local_source_modified_unix_ms = ?5
                  AND content_tree_blocks IS NOT NULL AND content_id_hex != ''
                ORDER BY updated_at_unix_ms DESC
                LIMIT 1
                ",
                params![
                    profile_id.0,
                    source_path.to_string_lossy(),
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                    picked.size_bytes,
                    picked.modified_unix_ms,
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        match done {
            Some(done) => self.attachment_content_tree(&done),
            None => Ok(None),
        }
    }

    /// One `Preparing` row made `Available` under a tree read from its source. `false` when the
    /// row is no longer `Preparing` (the managed copy made for it is then removed again).
    fn complete_prepared_attachment(
        &self,
        message_id: &str,
        file_name: &str,
        source_path: &Path,
        source_metadata: AttachmentSourceMetadata,
        tree: &omni_crypto::ContentTreeLayer,
        now_unix_ms: i64,
    ) -> OmniResult<bool> {
        let profile_id = self.resolve_transition_profile_id()?;
        let content_id_hex = hex::encode(tree.id);
        let file_size_bytes = tree.size as i64;
        let should_copy_into_managed_blob = file_size_bytes <= MANAGED_ATTACHMENT_COPY_MAX_BYTES;
        let (local_blob_path, local_blob_managed, local_source_metadata, managed_blob_created) =
            if should_copy_into_managed_blob {
                let managed_blob_path =
                    self.managed_blob_path_for_attachment(&content_id_hex, file_name)?;
                let managed_blob_created =
                    self.copy_into_managed_blob_if_needed(source_path, &managed_blob_path)?;
                let managed_metadata = Self::attachment_source_metadata(&managed_blob_path)?;
                (managed_blob_path, true, managed_metadata, managed_blob_created)
            } else {
                (source_path.to_path_buf(), false, source_metadata, false)
            };

        let updated = self.connection.execute(
            "
            UPDATE message_attachments
            SET content_id_hex = ?2,
                file_size_bytes = ?3,
                state = ?4,
                bytes_transferred = 0,
                local_blob_path = ?5,
                local_blob_managed = ?6,
                local_source_size_bytes = ?7,
                local_source_modified_unix_ms = ?8,
                last_error = NULL,
                updated_at_unix_ms = ?9
            WHERE message_id = ?1
              AND profile_id = ?10
              AND state = ?11
            ",
            params![
                message_id,
                content_id_hex,
                file_size_bytes,
                attachment_state_to_db(AttachmentTransferState::Available),
                local_blob_path.to_string_lossy(),
                if local_blob_managed { 1_i64 } else { 0_i64 },
                local_source_metadata.size_bytes,
                local_source_metadata.modified_unix_ms,
                now_unix_ms,
                profile_id.0,
                attachment_state_to_db(AttachmentTransferState::Preparing),
            ],
        );
        match updated {
            Ok(1) => {
                self.set_attachment_content_tree(message_id, tree)?;
                Ok(true)
            }
            other => {
                if managed_blob_created {
                    let _ = best_effort_remove_file(&local_blob_path);
                }
                other.map(|_| false).map_err(map_sqlite_error)
            }
        }
    }

    /// How far the preparation of a file has read, for the bubbles («Подготовка N%») of every
    /// message waiting on that reading. `false` once none of them is `Preparing` any more — the
    /// pass stops then.
    pub fn set_attachment_preparation_progress(&self, message_id: &str, bytes_read: i64, now_unix_ms: i64) -> OmniResult<bool> {
        let profile_id = self.resolve_transition_profile_id()?;
        let preparing = attachment_state_to_db(AttachmentTransferState::Preparing);
        let updated = self
            .connection
            .execute(
                "
                UPDATE message_attachments
                SET bytes_transferred = ?2, updated_at_unix_ms = ?3
                WHERE profile_id = ?4 AND state = ?5
                  AND (local_saved_path, local_source_size_bytes, local_source_modified_unix_ms) = (
                      SELECT local_saved_path, local_source_size_bytes, local_source_modified_unix_ms
                      FROM message_attachments WHERE message_id = ?1 AND profile_id = ?4
                  )
                ",
                params![message_id, bytes_read, now_unix_ms, profile_id.0, preparing],
            )
            .map_err(map_sqlite_error)?;
        Ok(updated > 0)
    }

    /// Outgoing files still being prepared, with their source file — after a restart each source
    /// is read on from its checkpoint (once for all its messages).
    pub fn list_preparing_attachments(&self) -> OmniResult<Vec<(String, String)>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id, COALESCE(local_saved_path, '') FROM message_attachments
                WHERE profile_id = ?1 AND state = ?2
                ORDER BY created_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![profile_id.0, attachment_state_to_db(AttachmentTransferState::Preparing)],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Ends the preparation of a file that never left: the attachment `Failed` («cancelled» or why
    /// it could not be read), its message `Failed` — the queue drops it, nothing was offered to
    /// anyone. `false` when it was not being prepared.
    pub fn fail_outgoing_attachment_preparation(&self, message_id: &str, reason: &str, now_unix_ms: i64) -> OmniResult<bool> {
        let profile_id = self.resolve_transition_profile_id()?;
        let updated = self
            .connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2, last_error = ?3, bytes_transferred = 0, content_tree_blocks = NULL,
                    updated_at_unix_ms = ?4
                WHERE message_id = ?1 AND profile_id = ?5 AND state = ?6
                ",
                params![
                    message_id,
                    attachment_state_to_db(AttachmentTransferState::Failed),
                    reason,
                    now_unix_ms,
                    profile_id.0,
                    attachment_state_to_db(AttachmentTransferState::Preparing),
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated == 1 {
            self.update_message_status(message_id, MessageStatus::Failed, now_unix_ms)?;
        }
        Ok(updated == 1)
    }

    /// A received offer. The same offer delivered again (the sender's catch-up after a break, a
    /// resend whose receipt was lost) refreshes a row still `offered` only: one being downloaded,
    /// paused or saved keeps its progress, its map and its transfer.
    #[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
    pub fn store_incoming_attachment_offer(
        &self,
        message_id: &str,
        dialog_key: &str,
        file_name: &str,
        file_size_bytes: i64,
        mime_type: Option<&str>,
        content_id_hex: &str,
        content_encryption: Option<&str>,
        content_key_hex: Option<&str>,
        content_nonce_seed_hex: Option<&str>,
        content_chunk_size_bytes: Option<u32>,
        created_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let profile_id = self.resolve_transition_profile_id()?;
        let content_encryption = content_encryption.ok_or_else(|| {
            OmniError::InvalidInput("attachment content encryption mode is required".to_string())
        })?;
        let protection_mode = AttachmentMediaProtectionMode::from_db_value(content_encryption)?;
        match protection_mode {
            AttachmentMediaProtectionMode::E2eeMedia => {
                if content_key_hex.is_none() || content_nonce_seed_hex.is_none() {
                    return Err(OmniError::InvalidInput(
                        "encrypted attachments require media key and nonce seed".to_string(),
                    ));
                }
            }
        }
        if content_chunk_size_bytes.is_none() || content_chunk_size_bytes == Some(0) {
            return Err(OmniError::InvalidInput(
                "attachment content chunk size is required".to_string(),
            ));
        }
        self.connection
            .execute(
                "
                INSERT INTO message_attachments (
                    message_id,
                    dialog_key,
                    file_name,
                    file_size_bytes,
                    mime_type,
                    content_id_hex,
                    state,
                    bytes_transferred,
                    local_blob_path,
                    local_serving_enabled,
                    remote_holder_available,
                    local_saved_path,
                    temp_download_path,
                    active_transfer_id,
                    last_error,
                    content_encryption,
                    content_key_hex,
                    content_nonce_seed_hex,
                    content_chunk_size_bytes,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, NULL, 0, 1, NULL, NULL, NULL, NULL, ?8, ?9, ?10, ?11, ?12, ?13, ?14
                )
                ON CONFLICT(message_id) DO UPDATE SET
                    dialog_key = excluded.dialog_key,
                    file_name = excluded.file_name,
                    file_size_bytes = excluded.file_size_bytes,
                    mime_type = excluded.mime_type,
                    content_id_hex = excluded.content_id_hex,
                    state = excluded.state,
                    bytes_transferred = excluded.bytes_transferred,
                    remote_holder_available = excluded.remote_holder_available,
                    active_transfer_id = excluded.active_transfer_id,
                    content_encryption = excluded.content_encryption,
                    content_key_hex = excluded.content_key_hex,
                    content_nonce_seed_hex = excluded.content_nonce_seed_hex,
                    content_chunk_size_bytes = excluded.content_chunk_size_bytes,
                    last_error = excluded.last_error,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                WHERE message_attachments.state = 'offered'
                ",
                params![
                    message_id,
                    normalize_dialog_key(dialog_key),
                    file_name,
                    file_size_bytes,
                    mime_type,
                    content_id_hex,
                    attachment_state_to_db(AttachmentTransferState::Offered),
                    content_encryption,
                    content_key_hex,
                    content_nonce_seed_hex,
                    content_chunk_size_bytes,
                    created_at_unix_ms,
                    current_unix_ms(),
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not persisted", message_id))
            })
    }

    /// What the chat shows this file as; `None` = a plain file plate. The caller has already
    /// checked a received one (`photo::accept_received_photo_presentation`).
    pub fn set_attachment_presentation(
        &self,
        message_id: &str,
        presentation: Option<&omni_types::AttachmentPresentationV1>,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let json = presentation
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| OmniError::Internal(error.to_string()))?;
        self.connection
            .execute(
                "UPDATE message_attachments SET presentation_json = ?1 WHERE message_id = ?2 AND profile_id = ?3",
                params![json, message_id, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn set_attachment_content_chunk_size(
        &self,
        message_id: &str,
        chunk_size_bytes: u32,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET content_chunk_size_bytes = ?2,
                    updated_at_unix_ms = ?3
                WHERE message_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    message_id,
                    chunk_size_bytes,
                    updated_at_unix_ms,
                    profile_id.0
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| OmniError::Storage(format!("attachment '{}' was not found", message_id)))
    }

    /// Dialogs that hold at least one download still waiting for its request to go out. The queue
    /// tick walks these, so a download is not tied to whichever dialog happens to be the active
    /// transport dialog (with several linked devices that is not the chat's dialog).
    pub fn dialogs_with_pending_attachment_downloads(&self) -> OmniResult<Vec<String>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT DISTINCT dialog_key
                FROM message_attachments
                WHERE profile_id = ?1
                  AND remote_holder_available = 1
                  AND state IN ('requested', 'waiting_for_reconnect')
                ORDER BY dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut dialogs = Vec::new();
        for row in rows {
            dialogs.push(row.map_err(map_sqlite_error)?);
        }
        Ok(dialogs)
    }

    /// The 1:1 dialogs with a download in flight. A group's file is pulled by its own coordinator.
    pub fn dialogs_with_downloading_attachments(&self) -> OmniResult<Vec<String>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT DISTINCT dialog_key
                FROM message_attachments
                WHERE profile_id = ?1
                  AND state = 'downloading'
                  AND dialog_key NOT LIKE 'group:%'
                ORDER BY dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn list_pending_attachment_downloads_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Vec<AttachmentRuntimeInfo>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id
                FROM message_attachments
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND remote_holder_available = 1
                  AND state IN ('requested', 'waiting_for_reconnect')
                ORDER BY updated_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![normalize_dialog_key(dialog_key), profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sqlite_error)?;

        let mut items = Vec::new();
        for row in rows {
            if let Some(runtime) =
                self.get_attachment_runtime_info(&row.map_err(map_sqlite_error)?)?
            {
                items.push(runtime);
            }
        }

        Ok(items)
    }

    /// Downloads this dialog believes are in flight. `list_pending_attachment_downloads_for_dialog`
    /// deliberately excludes these — sending the request moves the row to `downloading`, so the
    /// pending pass never looks at it again. That is correct for "has the request gone out at all"
    /// and blind to "it went out and nothing came back", which is what this is for.
    pub fn list_downloading_attachments_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Vec<AttachmentRuntimeInfo>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id
                FROM message_attachments
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND remote_holder_available = 1
                  AND state = 'downloading'
                ORDER BY updated_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![normalize_dialog_key(dialog_key), profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sqlite_error)?;

        let mut items = Vec::new();
        for row in rows {
            if let Some(runtime) =
                self.get_attachment_runtime_info(&row.map_err(map_sqlite_error)?)?
            {
                items.push(runtime);
            }
        }

        Ok(items)
    }

    pub fn mark_attachment_download_requested(
        &self,
        message_id: &str,
        save_path: &Path,
        transfer_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        if let Some(parent) = save_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to prepare download target directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }
        let temp_download_path = self.temp_download_path_for_save_path(save_path, message_id)?;
        // What is here already, as this temp file allows (another save path is another file).
        let mut checked = existing.clone();
        checked.temp_download_path = Some(temp_download_path.clone());
        let received_bytes = self.checked_received_blocks(&checked)?.received_bytes();
        self.ensure_attachment_download_space_available(
            &temp_download_path,
            existing.attachment.file_size_bytes,
            received_bytes,
        )?;
        self.reserve_attachment_download_storage(
            &temp_download_path,
            existing.attachment.file_size_bytes,
        )?;

        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    bytes_transferred = ?3,
                    local_saved_path = ?4,
                    temp_download_path = ?5,
                    active_transfer_id = ?6,
                    last_error = NULL,
                    updated_at_unix_ms = ?7
                WHERE message_id = ?1
                  AND profile_id = ?8
                ",
                params![
                    message_id,
                    attachment_state_to_db(AttachmentTransferState::Requested),
                    received_bytes,
                    save_path.to_string_lossy(),
                    temp_download_path.to_string_lossy(),
                    transfer_id,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    /// Before a request (or a stream's start is checked): the map as the temp file allows, the
    /// progress it makes, and where the request resumes — the first block not here.
    pub fn attachment_download_resume(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<(AttachmentRuntimeInfo, i64)> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        let blocks = self.checked_received_blocks(&existing)?;
        let resume_offset = blocks.first_missing_offset();
        let received_bytes = blocks.received_bytes();
        if received_bytes == existing.attachment.bytes_transferred {
            return Ok((existing, resume_offset));
        }

        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET bytes_transferred = ?2,
                    updated_at_unix_ms = ?3
                WHERE message_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    message_id,
                    received_bytes,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        let runtime = self.get_attachment_runtime_info(message_id)?.ok_or_else(|| {
            OmniError::Storage(format!("attachment '{}' was not updated", message_id))
        })?;
        Ok((runtime, resume_offset))
    }

    pub fn save_attachment_copy_to_path(
        &self,
        message_id: &str,
        save_path: &Path,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        // TM-006: refuse to write attachments into system-protected paths.
        // Validate-only: the canonical form is checked against the blocklist,
        // but actual fs ops use the original caller-supplied path (preserves
        // user-facing path display + avoids `\\?\` extended-length prefix
        // leaking into stored metadata on Windows).
        // See docs/decisions/path-confinement-tm006.md.
        let _confined = confine_destructive_path(save_path)?;
        let runtime = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        let local_copy_path = Self::existing_local_attachment_copy_path(
            runtime.local_blob_path.as_deref(),
            runtime.requested_save_path.as_deref(),
        )
        .ok_or_else(|| OmniError::Storage("attachment is not available locally".to_string()))?;

        if let Some(parent) = save_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to prepare download target directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        if local_copy_path != save_path {
            self.copy_media_file(&local_copy_path, save_path)?;
        }

        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET local_saved_path = ?2,
                    last_error = NULL,
                    updated_at_unix_ms = ?3
                WHERE message_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    message_id,
                    save_path.to_string_lossy(),
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn mark_attachment_downloading(
        &self,
        message_id: &str,
        bytes_transferred: i64,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        self.update_attachment_state_and_progress(
            message_id,
            AttachmentTransferState::Downloading,
            bytes_transferred,
            None,
            updated_at_unix_ms,
        )
    }

    pub fn mark_attachment_waiting_for_reconnect(
        &self,
        message_id: &str,
        last_error: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        let bytes_transferred = self.checked_received_blocks(&existing)?.received_bytes();
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    bytes_transferred = ?3,
                    last_error = ?4,
                    updated_at_unix_ms = ?5
                WHERE message_id = ?1
                  AND profile_id = ?6
                ",
                params![
                    message_id,
                    attachment_state_to_db(AttachmentTransferState::WaitingForReconnect),
                    bytes_transferred,
                    last_error,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn mark_attachment_transfer_failed(
        &self,
        message_id: &str,
        state: AttachmentTransferState,
        last_error: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    bytes_transferred = ?3,
                    active_transfer_id = NULL,
                    last_error = ?4,
                    updated_at_unix_ms = ?5
                WHERE message_id = ?1
                  AND profile_id = ?6
                ",
                params![
                    message_id,
                    attachment_state_to_db(state),
                    existing.attachment.bytes_transferred,
                    last_error,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    fn mark_attachment_download_completion_failed(
        &self,
        message_id: &str,
        last_error: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.mark_attachment_transfer_failed(
            message_id,
            AttachmentTransferState::Failed,
            last_error,
            updated_at_unix_ms,
        )?;
        Ok(())
    }

    pub fn complete_attachment_download_from_temp(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        self.complete_attachment_download_from_temp_with_verified_tree(
            message_id,
            updated_at_unix_ms,
            None,
        )
    }

    /// Move a finished download into place. `verified` is the tree the receive path already checked
    /// every chunk against — then nothing is read again. Without it (a copy whose chunks were not
    /// checked one by one) the file is hashed here, which for a big file is long: never on the
    /// transfer path.
    pub fn complete_attachment_download_from_temp_with_verified_tree(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
        verified: Option<&omni_crypto::ContentTreeLayer>,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let runtime = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        let temp_download_path = runtime
            .temp_download_path
            .clone()
            .ok_or_else(|| OmniError::Storage("temporary download path is missing".to_string()))?;
        let requested_save_path = runtime
            .requested_save_path
            .clone()
            .ok_or_else(|| OmniError::Storage("requested save path is missing".to_string()))?;

        self.update_attachment_state_and_progress(
            message_id,
            AttachmentTransferState::Verifying,
            runtime.attachment.file_size_bytes,
            None,
            updated_at_unix_ms,
        )?;

        let tree = if let Some(verified) = verified {
            // An empty file has no chunk to write, so nothing ever created its `.part`; the check
            // below failed every empty-file download (QA 2026-10-17).
            if verified.size == 0 && !temp_download_path.exists() {
                if let Some(parent) = temp_download_path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        OmniError::Storage(format!("failed to create download directory: {error}"))
                    })?;
                }
                self.write_media_file(&temp_download_path, &[])?;
            }
            let file_size_bytes = match Self::attachment_source_metadata(&temp_download_path) {
                Ok(metadata) => metadata.size_bytes,
                Err(error) => {
                    self.mark_attachment_download_completion_failed(
                        message_id,
                        &format!("Проверка файла не прошла: {error}"),
                        updated_at_unix_ms,
                    )?;
                    return Err(error);
                }
            };
            if file_size_bytes != verified.size as i64 {
                self.mark_attachment_download_completion_failed(
                    message_id,
                    "Проверка файла не прошла: размер не совпадает",
                    updated_at_unix_ms,
                )?;
                return Err(OmniError::Storage(
                    "downloaded attachment size does not match the offered file".to_string(),
                ));
            }
            verified.clone()
        } else {
            match self.hash_file_content_tree(&temp_download_path, &mut |_| true) {
                Ok(tree) => tree,
                Err(error) => {
                    self.mark_attachment_download_completion_failed(
                        message_id,
                        &format!("Проверка файла не прошла: {error}"),
                        updated_at_unix_ms,
                    )?;
                    return Err(error);
                }
            }
        };
        let (content_id_hex, file_size_bytes) = (hex::encode(tree.id), tree.size as i64);
        if content_id_hex != runtime.attachment.content_id_hex {
            self.mark_attachment_download_completion_failed(
                message_id,
                "Проверка файла не прошла: хеш не совпадает",
                updated_at_unix_ms,
            )?;
            return Err(OmniError::Storage(
                "downloaded attachment hash does not match the offered file".to_string(),
            ));
        }
        if file_size_bytes != runtime.attachment.file_size_bytes {
            self.mark_attachment_download_completion_failed(
                message_id,
                "Проверка файла не прошла: размер не совпадает",
                updated_at_unix_ms,
            )?;
            return Err(OmniError::Storage(
                "downloaded attachment size does not match the offered file".to_string(),
            ));
        }

        let stale_blob_path = runtime
            .local_blob_managed
            .then(|| runtime.local_blob_path.clone())
            .flatten();
        if let Some(parent) = requested_save_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                self.mark_attachment_download_completion_failed(
                    message_id,
                    &format!("Не удалось подготовить папку назначения: {error}"),
                    updated_at_unix_ms,
                )?;
                return Err(OmniError::Storage(format!(
                    "failed to prepare download target directory '{}': {error}",
                    parent.display()
                )));
            }
        }
        let requested_save_matches_temp = temp_download_path == requested_save_path;
        if !requested_save_matches_temp {
            match fs::remove_file(&requested_save_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    self.mark_attachment_download_completion_failed(
                        message_id,
                        &format!("Не удалось заменить существующий файл: {error}"),
                        updated_at_unix_ms,
                    )?;
                    return Err(OmniError::Storage(format!(
                        "failed to replace existing attachment target '{}': {error}",
                        requested_save_path.display()
                    )));
                }
            }
            if let Err(error) = fs::rename(&temp_download_path, &requested_save_path) {
                self.mark_attachment_download_completion_failed(
                    message_id,
                    &format!("Не удалось сохранить файл: {error}"),
                    updated_at_unix_ms,
                )?;
                return Err(OmniError::Storage(format!(
                    "failed to move downloaded attachment to '{}': {error}",
                    requested_save_path.display()
                )));
            }
        }

        let persist_result = self.connection.execute(
            "
                UPDATE message_attachments
                SET state = ?2,
                    content_blocks_have = NULL,
                    bytes_transferred = file_size_bytes,
                    local_blob_path = NULL,
                    local_serving_enabled = 0,
                    local_saved_path = ?3,
                    temp_download_path = NULL,
                    active_transfer_id = NULL,
                    last_error = NULL,
                    updated_at_unix_ms = ?4
                WHERE message_id = ?1
                  AND profile_id = ?5
                ",
            params![
                message_id,
                attachment_state_to_db(AttachmentTransferState::Downloaded),
                requested_save_path.to_string_lossy(),
                updated_at_unix_ms,
                self.resolve_transition_profile_id()?.0,
            ],
        );
        if let Err(error) = persist_result {
            if !requested_save_matches_temp && !temp_download_path.exists() {
                let restore_source = if requested_save_path.exists() {
                    Some(requested_save_path.as_path())
                } else {
                    None
                };
                if let Some(source_path) = restore_source {
                    if let Some(parent) = temp_download_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = self.copy_media_file(source_path, &temp_download_path);
                }
            }
            return Err(map_sqlite_error(error));
        }
        // Kept so this copy can be served on (a group member, our other devices).
        self.set_attachment_content_tree(message_id, &tree)?;

        if let Some(stale_blob_path) = stale_blob_path {
            if stale_blob_path != requested_save_path {
                let _ = best_effort_remove_file(&stale_blob_path);
            }
        }

        // A photo is shown only if the file is what its offer said (a JPEG of that size): a
        // sender's lie makes it a plain file, never decoded by the chat.
        if let Some(presentation) = runtime.attachment.presentation.as_ref() {
            let matches = self
                .read_media_file(&requested_save_path)
                .is_ok_and(|bytes| crate::photo::downloaded_photo_matches(&bytes, presentation));
            if !matches {
                self.set_attachment_presentation(message_id, None)?;
            }
        }

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn mark_attachment_downloaded_by_remote(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET remote_holder_available = 1,
                    last_error = NULL,
                    updated_at_unix_ms = ?2
                WHERE message_id = ?1
                  AND profile_id = ?3
                ",
                params![
                    message_id,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn revoke_local_attachment_access(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        if existing.local_blob_managed {
            if let Some(local_blob_path) = existing.local_blob_path.as_ref() {
                best_effort_remove_file(local_blob_path)?;
            }
        }

        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    local_blob_path = NULL,
                    local_serving_enabled = 0,
                    active_transfer_id = NULL,
                    last_error = NULL,
                    updated_at_unix_ms = ?3
                WHERE message_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    message_id,
                    attachment_state_to_db(AttachmentTransferState::Revoked),
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn note_remote_attachment_access_revoked(
        &self,
        message_id: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        let existing = self
            .get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not found", message_id))
            })?;
        let next_state = if existing.attachment.local_blob_available {
            existing.attachment.state
        } else {
            AttachmentTransferState::Unavailable
        };
        let next_error = if existing.attachment.local_blob_available {
            None
        } else {
            Some("Файл недоступен")
        };

        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    remote_holder_available = 0,
                    active_transfer_id = NULL,
                    last_error = ?3,
                    updated_at_unix_ms = ?4
                WHERE message_id = ?1
                  AND profile_id = ?5
                ",
                params![
                    message_id,
                    attachment_state_to_db(next_state),
                    next_error,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    pub fn delete_attachment_assets_for_dialog(&self, dialog_key: &str) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let file_paths =
            self.attachment_file_paths_for_dialog_profile(&normalized_dialog_key, &profile_id)?;
        self.delete_attachment_files(file_paths)?;

        self.connection
            .execute(
                "DELETE FROM message_attachments WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn delete_attachment_assets_for_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<()> {
        let file_paths = self.attachment_file_paths_for_profile(profile_id)?;
        self.delete_attachment_files(file_paths)?;

        self.connection
            .execute(
                "DELETE FROM message_attachments WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    /// The transport lost its connection: downloads in flight wait for it again («ждёт
    /// переподключения»), and the queue asks for them anew once it is back. `streaming` keeps one
    /// whose stream still runs — through another device's link (3.3) or a group pull — or whose
    /// source is still linked out of it: paused, it was asked for again every tick, and its source
    /// sent the whole file again.
    pub fn pause_inflight_attachment_downloads(
        &self,
        updated_at_unix_ms: i64,
        streaming: &dyn Fn(&AttachmentRuntimeInfo) -> bool,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id
                FROM message_attachments
                WHERE profile_id = ?1
                  AND state = 'downloading'
                ORDER BY updated_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(map_sqlite_error)?;
        let mut message_ids = Vec::new();
        for row in rows {
            message_ids.push(row.map_err(map_sqlite_error)?);
        }
        drop(statement);

        // The state is checked again in the UPDATE: a download that finished between the SELECT
        // and here (another connection — a group pull runs beside the queue tick) is `downloaded`
        // and must stay so. Without it a whole photo sat at «ждёт переподключения» (core gate
        // 2026-10-14, `a_group_photo_is_offered_…`).
        for message_id in message_ids {
            let existing = self
                .get_attachment_runtime_info(&message_id)?
                .ok_or_else(|| {
                    OmniError::Storage(format!("attachment '{}' was not found", message_id))
                })?;
            // Finished (or failed) meanwhile: its temp file may be gone already.
            if existing.attachment.state != AttachmentTransferState::Downloading || streaming(&existing) {
                continue;
            }
            // Progress as far as it is durable: blocks synced and checked. A stream may still be
            // writing beside this; it only ever adds blocks, nothing is cut here.
            let bytes_transferred = self.checked_received_blocks(&existing)?.received_bytes();
            self.connection
                .execute(
                    "
                    UPDATE message_attachments
                    SET state = 'waiting_for_reconnect',
                        bytes_transferred = ?2,
                        updated_at_unix_ms = ?3
                    WHERE message_id = ?1
                      AND profile_id = ?4
                      AND state = 'downloading'
                    ",
                    params![
                        message_id,
                        bytes_transferred,
                        updated_at_unix_ms,
                        profile_id.0
                    ],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Count the inbound downloads of a dialog that hold a temp file: asked for and not yet done
    /// (Requested, Downloading, WaitingForReconnect). Bounds what a contact's photos may ask for on
    /// their own (TM-005). An offer nobody asked for is not counted: it is a message row, no file —
    /// counted, eight files left undownloaded in a chat refused every file after them.
    pub fn count_inbound_attachments_for_dialog(&self, dialog_key: &str) -> OmniResult<i64> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM message_attachments
                WHERE profile_id = ?1
                  AND dialog_key = ?2
                  AND state IN ('requested', 'downloading', 'waiting_for_reconnect')
                ",
                params![profile_id.0, dialog_key],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)
    }

    fn update_attachment_state_and_progress(
        &self,
        message_id: &str,
        state: AttachmentTransferState,
        bytes_transferred: i64,
        last_error: Option<&str>,
        updated_at_unix_ms: i64,
    ) -> OmniResult<AttachmentRuntimeInfo> {
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET state = ?2,
                    bytes_transferred = ?3,
                    active_transfer_id = CASE WHEN ?4 != 0 THEN NULL ELSE active_transfer_id END,
                    last_error = ?5,
                    updated_at_unix_ms = ?6
                WHERE message_id = ?1
                  AND profile_id = ?7
                ",
                params![
                    message_id,
                    attachment_state_to_db(state),
                    bytes_transferred,
                    if matches!(
                        state,
                        AttachmentTransferState::Failed | AttachmentTransferState::Unavailable
                    ) {
                        1_i64
                    } else {
                        0_i64
                    },
                    last_error,
                    updated_at_unix_ms,
                    self.resolve_transition_profile_id()?.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_attachment_runtime_info(message_id)?
            .ok_or_else(|| {
                OmniError::Storage(format!("attachment '{}' was not updated", message_id))
            })
    }

    fn attachment_file_paths_for_dialog_profile(
        &self,
        dialog_key: &str,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Vec<PathBuf>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT local_blob_path, local_blob_managed, temp_download_path
                FROM message_attachments
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, profile_id.0.as_str()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(map_sqlite_error)?;

        let mut file_paths = Vec::new();
        for row in rows {
            let (local_blob_path, local_blob_managed, temp_download_path) =
                row.map_err(map_sqlite_error)?;
            for path in [
                local_blob_managed.then_some(local_blob_path).flatten(),
                temp_download_path,
            ]
            .into_iter()
            .flatten()
            {
                file_paths.push(PathBuf::from(path));
            }
        }

        Ok(file_paths)
    }

    fn attachment_file_paths_for_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Vec<PathBuf>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT local_blob_path, local_blob_managed, temp_download_path
                FROM message_attachments
                WHERE profile_id = ?1
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(map_sqlite_error)?;

        let mut file_paths = Vec::new();
        for row in rows {
            let (local_blob_path, local_blob_managed, temp_download_path) =
                row.map_err(map_sqlite_error)?;
            for path in [
                local_blob_managed.then_some(local_blob_path).flatten(),
                temp_download_path,
            ]
            .into_iter()
            .flatten()
            {
                file_paths.push(PathBuf::from(path));
            }
        }

        Ok(file_paths)
    }

    fn delete_attachment_files(
        &self,
        file_paths: impl IntoIterator<Item = PathBuf>,
    ) -> OmniResult<()> {
        for candidate in file_paths {
            match fs::remove_file(&candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(OmniError::Storage(format!(
                        "failed to delete attachment file '{}': {error}",
                        candidate.display()
                    )));
                }
            }
        }

        Ok(())
    }
}

fn attachment_state_to_db(state: AttachmentTransferState) -> &'static str {
    match state {
        AttachmentTransferState::Preparing => "preparing",
        AttachmentTransferState::Offered => "offered",
        AttachmentTransferState::Available => "available",
        AttachmentTransferState::Requested => "requested",
        AttachmentTransferState::Downloading => "downloading",
        AttachmentTransferState::WaitingForReconnect => "waiting_for_reconnect",
        AttachmentTransferState::Verifying => "verifying",
        AttachmentTransferState::Downloaded => "downloaded",
        AttachmentTransferState::Revoked => "revoked",
        AttachmentTransferState::Unavailable => "unavailable",
        AttachmentTransferState::Failed => "failed",
    }
}

fn parse_attachment_state(value: &str) -> Result<AttachmentTransferState, OmniError> {
    match value {
        "preparing" => Ok(AttachmentTransferState::Preparing),
        "offered" => Ok(AttachmentTransferState::Offered),
        "available" => Ok(AttachmentTransferState::Available),
        "requested" => Ok(AttachmentTransferState::Requested),
        "downloading" => Ok(AttachmentTransferState::Downloading),
        "waiting_for_reconnect" => Ok(AttachmentTransferState::WaitingForReconnect),
        "verifying" => Ok(AttachmentTransferState::Verifying),
        "downloaded" => Ok(AttachmentTransferState::Downloaded),
        "revoked" => Ok(AttachmentTransferState::Revoked),
        "unavailable" => Ok(AttachmentTransferState::Unavailable),
        "failed" => Ok(AttachmentTransferState::Failed),
        other => Err(OmniError::Storage(format!(
            "unexpected attachment state '{other}' in storage"
        ))),
    }
}

fn attachment_download_required_space_bytes(file_size_bytes: i64, resume_offset_bytes: i64) -> u64 {
    let file_size_bytes = file_size_bytes.max(0) as u64;
    let resume_offset_bytes = resume_offset_bytes.max(0) as u64;
    file_size_bytes.saturating_sub(resume_offset_bytes)
}

#[cfg(windows)]
fn attachment_available_space_bytes(path: &Path) -> OmniResult<Option<u64>> {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let mut available_bytes = 0_u64;
    let mut total_bytes = 0_u64;
    let mut total_free_bytes = 0_u64;
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let succeeded = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available_bytes,
            &mut total_bytes,
            &mut total_free_bytes,
        )
    };
    if succeeded == 0 {
        return Err(OmniError::Storage(format!(
            "failed to inspect free disk space for '{}': {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(Some(available_bytes))
}

#[cfg(not(windows))]
fn attachment_available_space_bytes(_path: &Path) -> OmniResult<Option<u64>> {
    Ok(None)
}

#[cfg(windows)]
fn reserve_attachment_file_allocation(path: &Path, allocation_size_bytes: u64) -> OmniResult<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle as _;

    #[repr(C)]
    struct FileAllocationInfo {
        allocation_size: i64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetFileInformationByHandle(
            hFile: *mut c_void,
            fileInformationClass: i32,
            lpFileInformation: *const c_void,
            dwBufferSize: u32,
        ) -> i32;
    }

    const FILE_ALLOCATION_INFO_CLASS: i32 = 5;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            OmniError::Storage(format!(
                "failed to prepare attachment temp directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false) // resumable temp file: preserve any existing partial download
        .open(path)
        .map_err(|error| {
            OmniError::Storage(format!(
                "failed to open attachment temp file '{}' for allocation: {error}",
                path.display()
            ))
        })?;
    let allocation = FileAllocationInfo {
        allocation_size: allocation_size_bytes.min(i64::MAX as u64) as i64,
    };
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle().cast::<c_void>(),
            FILE_ALLOCATION_INFO_CLASS,
            (&allocation as *const FileAllocationInfo).cast::<c_void>(),
            std::mem::size_of::<FileAllocationInfo>() as u32,
        )
    };
    if result == 0 {
        return Err(OmniError::Storage(format!(
            "failed to reserve disk allocation for attachment temp file '{}': {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn reserve_attachment_file_allocation(_path: &Path, _allocation_size_bytes: u64) -> OmniResult<()> {
    Ok(())
}

fn sanitize_attachment_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            _ => ch,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string()
}

fn best_effort_remove_file(path: &Path) -> OmniResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OmniError::Storage(format!(
            "failed to delete attachment file '{}': {error}",
            path.display()
        ))),
    }
}
