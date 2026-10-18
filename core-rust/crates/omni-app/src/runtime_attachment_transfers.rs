/// A photo to send, re-encoded (`omni_storage::photo`): the staged JPEG, the name it goes under and
/// what the receiver shows. The photo, not the picked file, is what goes out.
struct StagedOutgoingPhoto {
    path: PathBuf,
    file_name: String,
    presentation: omni_types::AttachmentPresentationV1,
}

fn stage_outgoing_photo_if_asked(
    storage: &Storage,
    message_id: &str,
    source_file_path: &str,
    as_photo: bool,
) -> OmniResult<Option<StagedOutgoingPhoto>> {
    if !as_photo {
        return Ok(None);
    }
    let (path, presentation) = storage.stage_outgoing_photo(message_id, Path::new(source_file_path))?;
    let stem = Path::new(source_file_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("photo");
    Ok(Some(StagedOutgoingPhoto { path, file_name: format!("{stem}.jpg"), presentation }))
}

/// The file of an outgoing message (1:1 or group) into the attachment store; the message row
/// must already exist.
#[allow(clippy::too_many_arguments)]
fn register_outgoing_attachment(
    storage: &Storage,
    message_id: &str,
    dialog_key: &str,
    source_file_path: &str,
    display_file_name: Option<&str>,
    staged_photo: Option<StagedOutgoingPhoto>,
    media_protection_mode: omni_types::AttachmentMediaProtectionMode,
    created_at_unix_ms: i64,
) -> OmniResult<AttachmentRuntimeInfo> {
    let (source, file_name) = match staged_photo.as_ref() {
        Some(photo) => (photo.path.to_string_lossy().into_owned(), Some(photo.file_name.as_str())),
        None => (source_file_path.to_string(), display_file_name),
    };
    // A big file is read in the background (`app_attachment_preparation`): the message shows
    // «Подготовка N%» at once and its offer leaves when the tree is read. A photo is small.
    let source_size = omni_crypto::file_at_rest_len(std::path::Path::new(&source)).map(|len| len as i64).unwrap_or(0);
    if staged_photo.is_none() && source_size > crate::app_attachment_preparation::PREPARE_INLINE_MAX_BYTES {
        let begun = storage.begin_outgoing_attachment(
            message_id,
            dialog_key,
            &source,
            file_name,
            media_protection_mode,
            created_at_unix_ms,
        )?;
        let runtime = storage.set_attachment_content_chunk_size(
            message_id,
            choose_attachment_chunk_size_bytes(source_size, 0) as u32,
            created_at_unix_ms,
        )?;
        // The very file the row names (as stored): the key one reading of it is shared under.
        let stored_source = begun.requested_save_path.map(|path| path.to_string_lossy().into_owned());
        crate::app_attachment_preparation::spawn_attachment_preparation(
            &storage.db_path().to_string_lossy(),
            message_id,
            stored_source.as_deref().unwrap_or(&source),
        );
        return Ok(runtime);
    }
    // The chunk size is part of the offer (the receiver needs it to open the chunks), so it is
    // fixed here, before any offer is built.
    let prepared = storage
        .prepare_outgoing_attachment(
            message_id,
            dialog_key,
            &source,
            file_name,
            media_protection_mode,
            created_at_unix_ms,
        )
        .and_then(|runtime| {
            storage.set_attachment_content_chunk_size(
                message_id,
                choose_attachment_chunk_size_bytes(runtime.attachment.file_size_bytes, 0) as u32,
                created_at_unix_ms,
            )
        });
    if let Some(photo) = staged_photo.as_ref() {
        // The staged copy goes once it is in the managed blob store (or on failure).
        if !matches!(&prepared, Ok(runtime) if !runtime.local_blob_managed) {
            let _ = std::fs::remove_file(&photo.path);
        }
        if prepared.is_ok() {
            storage.set_attachment_presentation(message_id, Some(&photo.presentation))?;
            return storage
                .get_attachment_runtime_info(message_id)?
                .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")));
        }
    }
    prepared
}

fn handle_incoming_attachment_transfer_stream(
    runtime_storage: &Arc<Mutex<Storage>>,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    mut stream: AttachmentTransferStream,
    receive_context: ReceivedChatContext,
) -> OmniResult<()> {
    let runtime_storage = Arc::clone(runtime_storage);
    let transport_handle = Arc::clone(transport_handle);
    let session_lock = Arc::clone(session_lock);
    thread::Builder::new()
        .name("omni-attachment-transfer-in".to_string())
        .spawn(move || {
            // Stage accounting for the whole stream, emitted ONCE at the end. Tracing per chunk
            // fsyncs a line per megabyte, which perturbs exactly what it is trying to measure —
            // totals cost nothing and still say where the time went. `read` is time blocked on the
            // QUIC stream (network / flow control), `persist` is decrypt + disk + DB, and whatever
            // is left over is loop and wake-up overhead that neither timer covers.
            let mut frames: u64 = 0;
            let mut read_total = std::time::Duration::ZERO;
            let mut persist_total = std::time::Duration::ZERO;
            let mut backpressure_total = std::time::Duration::ZERO;
            let stream_started = std::time::Instant::now();
            // Identified from the first frame; until then there is no transfer to hold back.
            let mut backpressure_key: Option<(String, String)> = None;
            loop {
                // Do not pull another frame while this transfer is already holding a lot of
                // out-of-order data. NOT reading is the backpressure: the bytes stay in the QUIC
                // receive buffer, the window stops opening, and the sender slows down on its own.
                // The alternative -- reading anyway and erroring when the buffer overflows -- fails
                // a transfer for the crime of arriving early, which is the normal case the moment a
                // transfer spans several streams.
                if let Some((db_path, transfer_id)) = backpressure_key.as_ref() {
                    let waited = std::time::Instant::now();
                    while attachment_reorder_buffer_wants_backpressure(db_path, transfer_id) {
                        thread::sleep(std::time::Duration::from_millis(2));
                    }
                    backpressure_total += waited.elapsed();
                }
                let read_started = std::time::Instant::now();
                let frame = match stream.read_frame() {
                    Ok(Some(frame)) => frame,
                    // Clean end-of-stream: the sender finished (or cancelled) and closed.
                    Ok(None) => break,
                    Err(error) => {
                        // A mid-transfer read failure is why a `.part` stops growing; without this
                        // the transfer just stopped with nothing anywhere to say so.
                        omni_app_trace_log(
                            "attachment/stream-in",
                            &format!(
                                "dialog_key={} read failed: {error}",
                                receive_context.dialog_key
                            ),
                        );
                        break;
                    }
                };
                read_total += read_started.elapsed();
                if backpressure_key.is_none() {
                    if let Some(transfer_id) = attachment_transfer_frame_transfer_id(&frame) {
                        if let Ok(storage) = runtime_storage.lock() {
                            backpressure_key = Some((
                                storage.db_path().to_string_lossy().into_owned(),
                                transfer_id.to_string(),
                            ));
                        }
                    }
                }
                // A Complete frame says "the sender has sent everything", not "you have received
                // everything". With chunks on their own streams there is no order between the two,
                // so wait for this transfer's contiguous prefix to reach the full size before
                // letting completion run. Deliberately OUTSIDE the frame handler: that holds the
                // session and storage locks, and the chunks being waited for need them.
                if let (AttachmentTransferFrameV2::Complete(complete), Some((db_path, _))) =
                    (&frame, backpressure_key.as_ref())
                {
                    // An empty file has no chunk to wait for: the wait (it waits for a writer the
                    // first chunk makes) only ran out its whole drain timeout (QA 2026-10-17).
                    let empty = runtime_storage.lock().ok().is_some_and(|storage| {
                        storage
                            .get_attachment_runtime_info_by_active_transfer_id(&complete.transfer_id)
                            .ok()
                            .flatten()
                            .is_some_and(|runtime| runtime.attachment.file_size_bytes == 0)
                    });
                    if !empty {
                        wait_for_attachment_receive_completion(db_path, &complete.transfer_id);
                    }
                }
                let persist_started = std::time::Instant::now();
                if let Err(error) = handle_incoming_attachment_transfer_frame(
                    &runtime_storage,
                    &transport_handle,
                    &session_lock,
                    frame,
                    &receive_context,
                ) {
                    omni_app_trace_log(
                        "attachment/stream-in",
                        &format!(
                            "dialog_key={} frame rejected: {error}",
                            receive_context.dialog_key
                        ),
                    );
                    break;
                }
                persist_total += persist_started.elapsed();
                frames += 1;
            }
            // However the stream ended, what it wrote is counted: the next request resumes after it.
            if let Some((db_path, transfer_id)) = backpressure_key.as_ref() {
                if let Ok(storage) = runtime_storage.lock() {
                    checkpoint_attachment_receive_writer_by_transfer(&storage, db_path, transfer_id);
                }
            }
            let wall_ms = stream_started.elapsed().as_millis();
            let read_ms = read_total.as_millis();
            let persist_ms = persist_total.as_millis();
            // `backpressure` is time we deliberately did not read. It belongs in the breakdown or it
            // would show up as unexplained "other" and read like overhead.
            let backpressure_ms = backpressure_total.as_millis();
            omni_app_trace_log(
                "attachment/stream-in",
                &format!(
                    "dialog_key={} done frames={frames} wall={wall_ms}ms read={read_ms}ms persist={persist_ms}ms backpressure={backpressure_ms}ms other={}ms",
                    receive_context.dialog_key,
                    wall_ms.saturating_sub(read_ms + persist_ms + backpressure_ms),
                ),
            );
        })
        .map(|_| ())
        .map_err(|error| {
            OmniError::Internal(format!(
                "failed to spawn attachment transfer receive worker: {error}"
            ))
        })
}

fn spawn_attachment_transfer_sender(
    storage: &Storage,
    transport: &TransportManager,
    request: &omni_types::AttachmentDownloadRequestV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_path_buf();
    let db_key = storage.db_path().to_string_lossy().to_string();
    let transfer_id = request.transfer_id.clone();
    let message_id = request.message_id.clone();
    // Idempotency: a download request duplicated/replayed across carriers must NOT start a second
    // sender for a transfer already streaming — that sends the whole file twice (2x data + CPU) and
    // starves the sender's UI thread. Reserve first; a live worker for this transfer drops the dup.
    let Some(worker) = reserve_attachment_send_worker(&db_key, &transfer_id) else {
        omni_app_trace_log(
            "attachment/send-worker",
            &format!(
                "message={message_id} transfer={transfer_id} duplicate request ignored (already in flight)"
            ),
        );
        return Ok(());
    };
    let transport = transport.clone();
    let request = request.clone();
    let receive_context = receive_context.clone();
    let worker_db_key = db_key.clone();
    let worker_transfer_id = transfer_id.clone();
    let spawned_worker = Arc::clone(&worker);
    let spawned = thread::Builder::new()
        .name("omni-attachment-transfer-out".to_string())
        .spawn(move || {
            let result = (|| -> OmniResult<()> {
                // The user waits from «Скачать» to the first byte: say where that time goes.
                let opened = std::time::Instant::now();
                let storage = Storage::open(&db_path)?;
                omni_app_trace_log(
                    "attachment/send-worker",
                    &format!(
                        "message={} transfer={} db_open={}ms",
                        request.message_id,
                        request.transfer_id,
                        opened.elapsed().as_millis()
                    ),
                );
                match send_attachment_over_sink(
                    &storage,
                    &request.message_id,
                    &request.transfer_id,
                    AttachmentServeRange::from_offset(request.resume_offset_bytes),
                    Some(&receive_context.dialog_key),
                    Some(&spawned_worker),
                    || AttachmentChunkSink::open(&transport, &receive_context.dialog_key, &request.transfer_id),
                ) {
                    Ok(()) => Ok(()),
                    // Only a failure the receiver cannot cure by asking again is told: the file is
                    // gone, changed or withdrawn. A broken link is not — it used to be, and the
                    // notice (durable, so it arrived the moment the link was back) marked the
                    // download Failed for good: any hiccup on a bad network ended the transfer. The
                    // receiver sees the stall and asks again from where its `.part` ends.
                    Err(error) if attachment_transfer_failure_is_final(&error) => {
                        let _ = send_attachment_transfer_failure_notice(
                            &storage,
                            &transport,
                            &receive_context,
                            &request.message_id,
                            &error,
                        );
                        Err(error)
                    }
                    Err(error) => Err(error),
                }
            })();
            // Release the in-flight slot so a later legitimate resume can start a fresh worker.
            release_attachment_send_worker(&worker_db_key, &worker_transfer_id, &spawned_worker);
            // The send worker's outcome was discarded here, so an abort mid-transfer left NO trace
            // at all: the file just stopped, the queue retried ~30 s later, and the log showed
            // progress-restart-progress with no reason. Name it.
            if let Err(error) = result {
                omni_app_trace_log(
                    "attachment/send-worker",
                    &format!("message={} transfer={} aborted: {error}", request.message_id, request.transfer_id),
                );
            }
        });
    match spawned {
        Ok(_) => Ok(()),
        Err(error) => {
            // Spawn failed: release the reservation taken above so the transfer is not wedged.
            release_attachment_send_worker(&db_key, &transfer_id, &worker);
            Err(OmniError::Internal(format!(
                "failed to spawn attachment transfer send worker: {error}"
            )))
        }
    }
}

fn send_attachment_transfer_failure_notice(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    message_id: &str,
    error: &OmniError,
) -> OmniResult<()> {
    let session = load_receive_session(storage, receive_context)?;
    let (reason_code, reason_message) = attachment_transfer_failure_reason(error);
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::attachment_transfer_failed(
            message_id.to_string(),
            reason_code,
            reason_message,
            current_unix_ms(),
        ),
        SecurityPayloadKind::AttachmentTransferFailed,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    Ok(())
}

/// Whether asking again cannot help — anything else (a link that broke mid-stream) is retried by the
/// receiver.
fn attachment_transfer_failure_is_final(error: &OmniError) -> bool {
    attachment_transfer_failure_reason(error).0 != ATTACHMENT_TRANSFER_RETRYABLE_REASON
}

/// The reason code of a failure that is not about the file (see `attachment_transfer_failure_is_final`).
pub(crate) const ATTACHMENT_TRANSFER_RETRYABLE_REASON: &str = "transfer_failed";
/// A device of ours asked for a file it does not have: not about the file either — the asker goes
/// to the next source at once.
pub(crate) const ATTACHMENT_NOT_HERE_REASON: &str = "not_here";

/// Tell a device of ours that asked for `message_id` that the file is not on this one.
fn send_attachment_not_here_notice(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    message_id: &str,
) -> OmniResult<()> {
    let session = load_receive_session(storage, receive_context)?;
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::attachment_transfer_failed(
            message_id.to_string(),
            ATTACHMENT_NOT_HERE_REASON.to_string(),
            "the file is not on this device".to_string(),
            current_unix_ms(),
        ),
        SecurityPayloadKind::AttachmentTransferFailed,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    Ok(())
}

fn attachment_transfer_failure_reason(error: &OmniError) -> (&'static str, String) {
    let message = error.to_string();
    let lower_message = message.to_lowercase();
    if lower_message.contains("changed after it was offered")
        || lower_message.contains("changed while it was being prepared")
        || lower_message.contains("changed during transfer")
    {
        return (
            "source_changed",
            "Исходный файл изменился после отправки. Отправьте файл заново.".to_string(),
        );
    }
    if lower_message.contains("offered by an older version") {
        return (
            "resend",
            "Файл отправлен старой версией приложения. Отправьте его заново.".to_string(),
        );
    }
    if lower_message.contains("not available")
        || lower_message.contains("not found")
        || lower_message.contains("missing")
    {
        return (
            "file_not_found",
            "Файл недоступен у отправителя.".to_string(),
        );
    }
    if lower_message.contains("cancelled") || lower_message.contains("canceled") {
        return (
            "cancelled",
            "Передача файла была отменена отправителем.".to_string(),
        );
    }
    (ATTACHMENT_TRANSFER_RETRYABLE_REASON, message)
}

/// Streams to `target_dialog_key` — the device that asked (`attachment_requester_allowed` decided it
/// may), which for a copy of our own message is another device of ours, not the contact.
/// Which part of a file one stream carries (docs/decisions/attachment-content-tree.md, stage 3).
pub(crate) struct AttachmentServeRange {
    /// From this offset, on a leaf…
    pub(crate) from_offset_bytes: i64,
    /// …to the end of the block before this one; `None` — to the end of the file.
    pub(crate) to_block: Option<u64>,
    /// A copy still being downloaded may serve the blocks in its map (a group pull); a 1:1 transfer
    /// is served only from a whole file.
    pub(crate) partial_allowed: bool,
    /// Blocks of the range not sent: a seeding author named another member for them (stage 4).
    pub(crate) skip: Vec<u64>,
}

impl AttachmentServeRange {
    /// The rest of the file, from a 1:1 request's resume offset.
    pub(crate) fn from_offset(resume_offset_bytes: i64) -> Self {
        Self { from_offset_bytes: resume_offset_bytes, to_block: None, partial_allowed: false, skip: Vec::new() }
    }
}

/// Stream an attachment we hold into whatever `open_sink` opens: the 1:1 dialog's carrier, or the
/// bulk stream a group member opened to pull it (`app_group_attachments`). `progress_dialog_key` is
/// the 1:1 dialog whose liveness a slow write proves; a group pull has none. A copy of ours still
/// being downloaded serves, when `range` allows it, the blocks of the range it has and skips the
/// others: the asker learns what it lacks from what does not come and asks someone else.
fn send_attachment_over_sink(
    storage: &Storage,
    message_id: &str,
    transfer_id: &str,
    range: AttachmentServeRange,
    progress_dialog_key: Option<&str>,
    worker: Option<&AttachmentSendWorker>,
    open_sink: impl FnOnce() -> OmniResult<AttachmentChunkSink>,
) -> OmniResult<()> {
    let resume_offset_bytes = range.from_offset_bytes;
    let attachment = storage
        .get_attachment_runtime_info(message_id)?
        .ok_or_else(|| OmniError::Storage(format!("attachment '{}' was not found", message_id)))?;
    // A whole file, or the blocks a download of it holds so far (`have`).
    let (blob_path, have) = match storage.attachment_serving_blob_path(message_id)? {
        Some(path) => (path, None),
        None => match range.partial_allowed.then(|| storage.attachment_partial_source(message_id)).transpose()?.flatten() {
            Some((path, have)) => (path, Some(have)),
            None => return Err(OmniError::Storage("attachment is not available for download".to_string())),
        },
    };
    // The tree the offer named (docs/decisions/attachment-content-tree.md). Every block is checked
    // against it before it goes out: a source changed under us stops at the first changed block,
    // not after the whole file.
    let tree = storage.attachment_content_tree(message_id)?.ok_or_else(|| {
        OmniError::Storage(
            "attachment was offered by an older version: there is no hash tree to send it by".to_string(),
        )
    })?;
    // Read as plaintext: a copy in Omni's storage is encrypted at rest, one the user saved is not.
    let mut file = storage.open_media_file(&blob_path)?;
    use std::io::{Read, Seek, SeekFrom};

    let content_crypto = attachment_content_crypto_material(&attachment)?;
    let chunk_size_bytes = content_crypto.chunk_size_bytes as usize;
    // A chunk is a leaf of the tree — a small file is one chunk and one leaf — so each chunk is
    // checked on its own when it lands, and a resume starts on a leaf.
    let leaf_bytes = omni_crypto::CONTENT_TREE_LEAF_BYTES;
    let file_size = attachment.attachment.file_size_bytes.max(0) as u64;
    let resume = resume_offset_bytes.max(0) as u64;
    if chunk_size_bytes as u64 != leaf_bytes && (chunk_size_bytes as u64) < file_size {
        return Err(OmniError::Storage(format!(
            "attachment chunk size {chunk_size_bytes} is not a leaf of its hash tree"
        )));
    }
    // On a block: a chunk is sealed under its index alone, so the same chunk must always go out as
    // the same plaintext — and the first chunk of a block carries the block's proof, which a stream
    // starting mid-block would put on another chunk.
    if resume > file_size || (!resume.is_multiple_of(omni_crypto::CONTENT_TREE_BLOCK_BYTES) && resume != file_size) {
        return Err(OmniError::InvalidInput(format!(
            "attachment resume offset {resume} is not on a block of the file"
        )));
    }
    let started_at_unix_ms = current_unix_ms();
    let start_payload = serialize_attachment_transfer_payload(&AttachmentTransferStartPayloadV2 {
        total_size_bytes: attachment.attachment.file_size_bytes,
        content_id_hex: attachment.attachment.content_id_hex.clone(),
        chunk_size_bytes: chunk_size_bytes as u32,
        block_layer: tree.blocks.clone(),
    })?;
    let start_ciphertext = seal_attachment_transfer_payload(
        &content_crypto,
        transfer_id,
        "start",
        ATTACHMENT_TRANSFER_COUNTER_START,
        started_at_unix_ms,
        &start_payload,
    )?;

    // Where sealed chunks go. One stream is the default and is byte-for-byte the old path; more than
    // one exists because independent QUIC streams are how QUIC avoids head-of-line blocking, and a
    // single stream declines that offer. See docs/decisions/attachment-multi-stream.md.
    let sink_started = std::time::Instant::now();
    let mut chunk_sink = open_sink()?;
    omni_app_trace_log(
        "attachment/stream-out",
        &format!("transfer={transfer_id} sink_open={}ms", sink_started.elapsed().as_millis()),
    );
    chunk_sink.write_control(&AttachmentTransferFrameV2::Start(
        AttachmentTransferStartV2 {
            transfer_id: transfer_id.to_string(),
            payload: start_ciphertext,
            started_at_unix_ms,
        },
    ))?;

    let mut offset_bytes = resume as i64;
    let mut last_serving_recheck_offset = offset_bytes;
    // One of a few streams of this file at a time; the others wait their turn (C3,
    // `app_upload_slots`). The stream has started: a receiver in line sees a pause, not a refusal.
    let content_key = attachment.attachment.content_id_hex.clone();
    let send_started = std::time::Instant::now();
    let mut upload_slot = crate::app_upload_slots::acquire_upload_slot(&content_key);
    let mut waited_for_slot = send_started.elapsed();
    let mut chunks: u64 = 0;
    let mut read_total = std::time::Duration::ZERO;
    let mut hash_total = std::time::Duration::ZERO;
    let mut seal_total = std::time::Duration::ZERO;
    let mut write_total = std::time::Duration::ZERO;
    // Yielding to a call in progress; see ATTACHMENT_CALL_DUTY_PERCENT. Read once, because a knob
    // that changed mid-transfer would make the trace unreadable.
    let db_key = storage.db_path().to_string_lossy().to_string();
    let duty_percent = attachment_call_duty_percent();
    let mut yielded_to_call = std::time::Duration::ZERO;
    let block_bytes = omni_crypto::CONTENT_TREE_BLOCK_BYTES;
    let block_count = omni_crypto::content_block_count(file_size);
    for block_index in resume / block_bytes..range.to_block.unwrap_or(block_count).min(block_count) {
        let block_start = block_index * block_bytes;
        let block_end = (block_start + block_bytes).min(file_size);
        if resume >= block_end {
            continue; // an empty file, or a resume at its very end: nothing to send
        }
        if have.as_ref().is_some_and(|have| !have.get(block_index as usize).copied().unwrap_or(false))
            || range.skip.contains(&block_index)
        {
            continue; // not here yet, or another member was named for it: the asker gets it there
        }
        offset_bytes = offset_bytes.max(block_start as i64);
        // Had its slice and others wait: their turn (this stream pauses meanwhile).
        let turn_wait = std::time::Instant::now();
        upload_slot.take_turns(crate::app_upload_slots::UPLOAD_SLICE);
        waited_for_slot += turn_wait.elapsed();
        // The whole block, even on a resume from its middle: its leaves prove it, and the proof
        // must go out with the first chunk sent. Read and checked once for every stream of this
        // file that wants it about now.
        let read_started = std::time::Instant::now();
        let block = crate::app_upload_slots::shared_block(&content_key, block_index, || {
            let mut data = vec![0_u8; (block_end - block_start) as usize];
            file.seek(SeekFrom::Start(block_start))
                .and_then(|_| file.read_exact(&mut data))
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::UnexpectedEof {
                        OmniError::Storage("attachment source changed during transfer".to_string())
                    } else {
                        OmniError::Storage(format!(
                            "failed to read attachment source '{}': {error}",
                            blob_path.display()
                        ))
                    }
                })?;
            let leaves: Vec<omni_crypto::ContentHash> =
                data.chunks(leaf_bytes as usize).map(omni_crypto::content_leaf_hash).collect();
            if omni_crypto::content_tree_root(&leaves).as_ref() != tree.blocks.get(block_index as usize) {
                return Err(OmniError::Storage("attachment source changed during transfer".to_string()));
            }
            Ok(crate::app_upload_slots::SharedBlock { data, leaves })
        })?;
        read_total += read_started.elapsed();
        let hash_started = std::time::Instant::now();
        let block_buffer = &block.data;
        let mut block_proof = Some(AttachmentBlockProofV2 {
            leaves: block.leaves.clone(),
            path: tree.block_proof(block_index).ok_or_else(|| {
                OmniError::Internal(format!("attachment block {block_index} has no path in its tree"))
            })?,
        });
        hash_total += hash_started.elapsed();
        while (offset_bytes as u64) < block_end {
            // A download in progress is not "served" by a flag: its map says what it may hand on.
            if have.is_none()
                && offset_bytes > last_serving_recheck_offset
                && offset_bytes - last_serving_recheck_offset >= ATTACHMENT_SERVING_RECHECK_BYTES
            {
                let runtime = storage
                    .get_attachment_runtime_info(message_id)?
                    .ok_or_else(|| {
                        OmniError::Storage(format!("attachment '{}' was not found", message_id))
                    })?;
                if !runtime.attachment.local_serving_enabled {
                    return Err(OmniError::Storage(
                        "attachment transfer was cancelled locally".to_string(),
                    ));
                }
                last_serving_recheck_offset = offset_bytes;
            }
            let at = (offset_bytes as u64 - block_start) as usize;
            let read = (leaf_bytes as usize).min(block_buffer.len() - at);
            let data = &block_buffer[at..at + read];
            let chunk_index = attachment_chunk_counter(offset_bytes, content_crypto.chunk_size_bytes)?;
            let sent_at_unix_ms = current_unix_ms();
            let seal_started = std::time::Instant::now();
            let chunk_payload =
                serialize_attachment_transfer_chunk_payload(offset_bytes, data, block_proof.take().as_ref())?;
            let encrypted_chunk = seal_attachment_transfer_payload_owned(
                &content_crypto,
                transfer_id,
                "chunk",
                chunk_index.saturating_add(1),
                sent_at_unix_ms,
                chunk_payload,
            )?;
            seal_total += seal_started.elapsed();
            // Name a mid-transfer write failure. A stalled/aborted send is invisible otherwise: the
            // transfer just restarts from the resume offset, so the log shows progress-then-restart
            // with no reason anywhere.
            let write_started = std::time::Instant::now();
            if let Some(worker) = worker {
                worker.write_started();
            }
            let written = chunk_sink.write_chunk(AttachmentTransferChunkV2 {
                transfer_id: transfer_id.to_string(),
                chunk_index,
                payload: encrypted_chunk,
                sent_at_unix_ms,
            });
            if let Some(worker) = worker {
                worker.write_done();
                // The receiver asked again while this write hung: a newer stream serves it.
                if worker.superseded() {
                    return Err(OmniError::Transport("superseded by a newer request for this transfer".to_string()));
                }
            }
            if let Err(error) = written {
                omni_app_trace_log(
                    "attachment/chunk-out",
                    &format!(
                        "transfer={transfer_id} offset={offset_bytes} len={read} write FAILED after {}ms: {error}",
                        write_started.elapsed().as_millis()
                    ),
                );
                return Err(error);
            }
            let write_elapsed = write_started.elapsed();
            write_total += write_elapsed;
            // The write waited on the receiver's back-pressure: the peer hears us. Without this,
            // presence probes queued behind the file read as "unanswered" and the link was dropped
            // mid-transfer.
            if let Some(dialog_key) = progress_dialog_key {
                note_session_outbound_progress_for_dialog(&db_key, dialog_key, current_unix_ms());
            }
            // Hand the link back to a call in progress. Only ever idles -- the transfer keeps going,
            // just slower -- and only while a call exists, so with no call this is one registry
            // lookup and the send path is byte-for-byte what it always was.
            if call_occupies_link_for_profile(&db_key) {
                if let Some(delay) = attachment_call_yield_delay(write_elapsed, duty_percent) {
                    yielded_to_call += delay;
                    thread::sleep(delay);
                }
            }
            chunks += 1;
            offset_bytes += read as i64;
        }
    }
    // Every chunk is queued; now let the writers drain and surface the first failure among them. Do
    // this BEFORE Complete: Complete must not go out while chunks are still on the wire, because
    // streams carry no order between them and the receiver would be told the file is done while a
    // gap is still open.
    chunk_sink.finish()?;
    drop(upload_slot);
    // Time in line for a slot (C3) — before the first block, and each turn given up after a slice.
    let slot_ms = waited_for_slot.as_millis();
    // Send-side stage totals, emitted once. `write` is time blocked in the QUIC write, i.e.
    // back-pressure from the receiver (cwnd is huge and loss ~0 on a fast link), so comparing it
    // against read/hash/seal says whether this side is producing or waiting.
    let send_wall_ms = send_started.elapsed().as_millis();
    let (read_ms, hash_ms, seal_ms, write_ms, call_ms) = (
        read_total.as_millis(),
        hash_total.as_millis(),
        seal_total.as_millis(),
        write_total.as_millis(),
        // Time deliberately NOT spent sending, because a call was up. Belongs in the breakdown or a
        // transfer that yielded correctly would read as unexplained slowness.
        yielded_to_call.as_millis(),
    );
    omni_app_trace_log(
        "attachment/stream-out",
        &format!(
            "transfer={transfer_id} chunks={chunks} wall={send_wall_ms}ms read={read_ms}ms hash={hash_ms}ms seal={seal_ms}ms write={write_ms}ms yield_to_call={call_ms}ms slot_wait={slot_ms}ms other={}ms",
            send_wall_ms.saturating_sub(read_ms + hash_ms + seal_ms + write_ms + call_ms + slot_ms),
        ),
    );
    let completed_at_unix_ms = current_unix_ms();
    let complete_payload =
        serialize_attachment_transfer_payload(&AttachmentTransferCompletePayloadV2 {
            total_size_bytes: attachment.attachment.file_size_bytes,
            content_id_hex: attachment.attachment.content_id_hex.clone(),
        })?;
    let complete_ciphertext = seal_attachment_transfer_payload(
        &content_crypto,
        transfer_id,
        "complete",
        ATTACHMENT_TRANSFER_COUNTER_COMPLETE,
        completed_at_unix_ms,
        &complete_payload,
    )?;
    chunk_sink.write_control(&AttachmentTransferFrameV2::Complete(
        AttachmentTransferCompleteV2 {
            transfer_id: transfer_id.to_string(),
            payload: complete_ciphertext,
            completed_at_unix_ms,
        },
    ))?;
    chunk_sink.close()?;
    Ok(())
}

/// The transfer this frame belongs to, whatever kind it is. Used by the receive loop to key
/// backpressure before the frame is handed on and consumed.
fn attachment_transfer_frame_transfer_id(frame: &AttachmentTransferFrameV2) -> Option<&str> {
    Some(match frame {
        AttachmentTransferFrameV2::Start(start) => start.transfer_id.as_str(),
        AttachmentTransferFrameV2::Chunk(chunk) => chunk.transfer_id.as_str(),
        AttachmentTransferFrameV2::Complete(complete) => complete.transfer_id.as_str(),
    })
}

/// Share of the time a transfer may spend sending WHILE A CALL IS UP on the same profile. The rest
/// of the time it idles, which is what hands the link back to the call.
///
/// Why a duty cycle and not a rate limit: a rate has to be chosen, and any choice is wrong on some
/// path — 2 Mbit/s cripples a LAN transfer and is most of a mobile uplink. A duty cycle scales
/// itself with no measurement at all. It also cannot misbehave the way the in-flight cap did
/// (docs/bugs/uplink-goodput-retransmit.md §P1b): there is no estimate to be wrong, and no feedback
/// loop, because the idle time is derived from how long a write took and a write does not get
/// slower because we idled after the previous one.
///
/// 25% is a deliberate, blunt choice: a transfer during a call takes about four times as long, and
/// three quarters of the link is left for the call. There is no measurement behind that number
/// because there is nothing to measure — it is a policy about whose latency matters more, and while
/// someone is on a call the answer is the call.
const ATTACHMENT_CALL_DUTY_PERCENT: u32 = 25;

/// Overrides [`ATTACHMENT_CALL_DUTY_PERCENT`]; 100 disables the yielding entirely. Env-gated so the
/// effect can be A/B-measured on one build, the same discipline as every other transport knob here.
fn attachment_call_duty_percent() -> u32 {
    parse_attachment_call_duty_percent(
        std::env::var("OMNI_ATTACHMENT_CALL_DUTY_PERCENT").ok().as_deref(),
    )
}

fn parse_attachment_call_duty_percent(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|percent| (1..=100).contains(percent))
        .unwrap_or(ATTACHMENT_CALL_DUTY_PERCENT)
}

/// How long to idle after a chunk that took `write_elapsed` to hand to the transport, so that
/// sending occupies `duty_percent` of the time.
///
/// `None` when there is nothing to wait for, which keeps the no-call path free of a sleep call
/// entirely rather than sleeping zero.
fn attachment_call_yield_delay(
    write_elapsed: std::time::Duration,
    duty_percent: u32,
) -> Option<std::time::Duration> {
    if duty_percent >= 100 || write_elapsed.is_zero() {
        return None;
    }
    let idle_share = f64::from(100 - duty_percent) / f64::from(duty_percent);
    let delay = write_elapsed.mul_f64(idle_share);
    (!delay.is_zero()).then_some(delay)
}

/// How many QUIC streams one attachment transfer spreads its chunks over. Default 1 = the historical
/// single-stream path, unchanged. Env-gated so the multi-stream variant is A/B-measurable on ONE
/// build, the same discipline as `OMNI_QUIC_CC` (see docs/architecture/test-patterns.md -- goodput
/// on these paths drifts by more than any change we have made, so alternating builds is the only
/// valid comparison).
fn attachment_chunk_stream_count() -> usize {
    parse_attachment_chunk_stream_count(std::env::var("OMNI_ATTACHMENT_STREAMS").ok().as_deref())
}

/// Split out from the env read so the contract of the knob is testable without touching process
/// state: a test that sets the variable would be racing every other test in the process.
fn parse_attachment_chunk_stream_count(raw: Option<&str>) -> usize {
    raw.and_then(|raw| raw.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 16)
}

/// Where sealed chunks go on their way to the peer.
///
/// The producer must never block on the network. It reads, hashes and seals in order (that order is
/// what keeps the end-to-end SHA-256 cheap and sequential), so if it also wrote to the sockets, a
/// stalled stream would stall every other stream through it -- reintroducing exactly the head-of-line
/// coupling the extra streams exist to remove. Hence: producer -> bounded queue -> one writer thread
/// per stream. The queue being bounded is the sender's own backpressure, mirroring the receiver's.
enum AttachmentChunkSink {
    /// One stream carrying Start, every chunk and Complete. No threads, no queue, no extra copies --
    /// the old path exactly, and still the default.
    Single(AttachmentTransferStream),
    Fanned {
        /// Start and Complete only. Keeping them off the chunk streams is what lets `finish` order
        /// "every chunk is on the wire" strictly before "the transfer is complete".
        control: AttachmentTransferStream,
        /// `None` after `finish` has closed the queue.
        queue: Option<std::sync::mpsc::SyncSender<AttachmentTransferChunkV2>>,
        writers: Vec<std::thread::JoinHandle<OmniResult<()>>>,
    },
}

impl AttachmentChunkSink {
    fn open(
        transport: &TransportManager,
        dialog_key: &str,
        transfer_id: &str,
    ) -> OmniResult<Self> {
        let streams = attachment_chunk_stream_count();
        if streams == 1 {
            return Ok(Self::Single(
                transport.open_attachment_transfer_stream_for_dialog_key(dialog_key)?,
            ));
        }
        // Depth = one chunk per writer: enough that no writer idles waiting for the producer, small
        // enough that the queue cannot become a megabyte-per-slot memory sink.
        let control = transport.open_attachment_transfer_stream_for_dialog_key(dialog_key)?;
        let (queue, receiver) = std::sync::mpsc::sync_channel::<AttachmentTransferChunkV2>(streams);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut writers = Vec::with_capacity(streams);
        for index in 0..streams {
            let mut stream = transport.open_attachment_transfer_stream_for_dialog_key(dialog_key)?;
            let receiver = Arc::clone(&receiver);
            let transfer_id = transfer_id.to_string();
            let handle = thread::Builder::new()
                .name(format!("omni-attachment-out-{index}"))
                .spawn(move || -> OmniResult<()> {
                    loop {
                        // The lock is held only to take one chunk off the queue, never across the
                        // write, so writers do not serialise on each other.
                        let next = {
                            let Ok(receiver) = receiver.lock() else {
                                return Err(OmniError::Internal(
                                    "attachment chunk queue is poisoned".to_string(),
                                ));
                            };
                            receiver.recv()
                        };
                        let Ok(chunk) = next else {
                            // Queue closed: the producer is done and every chunk has been taken.
                            break;
                        };
                        stream.write_frame(&AttachmentTransferFrameV2::Chunk(chunk))?;
                    }
                    stream.close()?;
                    Ok(())
                })
                .map_err(|error| {
                    OmniError::Internal(format!(
                        "failed to spawn attachment chunk writer for transfer '{transfer_id}': {error}"
                    ))
                })?;
            writers.push(handle);
        }
        Ok(Self::Fanned {
            control,
            queue: Some(queue),
            writers,
        })
    }

    /// Start / Complete. In single-stream mode this is the same stream the chunks ride, which is
    /// what keeps the default path identical to what it always was.
    fn write_control(&mut self, frame: &AttachmentTransferFrameV2) -> OmniResult<()> {
        match self {
            Self::Single(stream) => stream.write_frame(frame),
            Self::Fanned { control, .. } => control.write_frame(frame),
        }
    }

    fn close(&mut self) -> OmniResult<()> {
        match self {
            Self::Single(stream) => stream.close(),
            Self::Fanned { control, .. } => control.close(),
        }
    }

    fn write_chunk(&mut self, chunk: AttachmentTransferChunkV2) -> OmniResult<()> {
        match self {
            Self::Single(stream) => stream.write_frame(&AttachmentTransferFrameV2::Chunk(chunk)),
            Self::Fanned { queue, .. } => {
                let Some(queue) = queue.as_ref() else {
                    return Err(OmniError::Internal(
                        "attachment chunk queue was already closed".to_string(),
                    ));
                };
                // Blocks once the queue is full, which is the intended backpressure: the producer
                // stops reading the file while the writers are behind.
                queue.send(chunk).map_err(|_| {
                    OmniError::Transport(
                        "attachment chunk writers stopped before the transfer finished".to_string(),
                    )
                })
            }
        }
    }

    /// Close the queue, wait for every writer, and return the FIRST failure. Called before the
    /// Complete frame goes out, so a chunk that never made it cannot be followed by "all done".
    fn finish(&mut self) -> OmniResult<()> {
        let Self::Fanned { queue, writers, .. } = self else {
            return Ok(());
        };
        // Dropping the sender is what ends the writers' recv loops.
        queue.take();
        let mut first_error = None;
        for handle in writers.drain(..) {
            let outcome = handle.join().unwrap_or_else(|_| {
                Err(OmniError::Internal(
                    "attachment chunk writer panicked".to_string(),
                ))
            });
            if let Err(error) = outcome {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod call_yield_tests {
    use super::{
        attachment_call_yield_delay, parse_attachment_call_duty_percent,
        ATTACHMENT_CALL_DUTY_PERCENT,
    };
    use std::time::Duration;

    #[test]
    fn a_quarter_duty_cycle_idles_three_times_as_long_as_it_sent() {
        let delay = attachment_call_yield_delay(Duration::from_millis(10), 25)
            .expect("a 25% duty cycle has to wait");

        assert_eq!(delay, Duration::from_millis(30));
    }

    #[test]
    fn the_delay_scales_with_the_path_instead_of_assuming_a_rate() {
        // The whole reason this is a duty cycle and not a rate limit: the same rule has to be
        // reasonable on a gigabit LAN and on a 6 Mbit/s uplink, and it gets there by measuring
        // nothing at all. A write that took ten times longer waits ten times longer.
        let fast = attachment_call_yield_delay(Duration::from_micros(200), 25).unwrap();
        let slow = attachment_call_yield_delay(Duration::from_micros(2_000), 25).unwrap();

        assert_eq!(slow, fast * 10);
    }

    #[test]
    fn a_hundred_percent_duty_means_never_waiting() {
        assert_eq!(
            attachment_call_yield_delay(Duration::from_millis(50), 100),
            None,
            "the documented way to switch the yielding off must actually switch it off"
        );
    }

    #[test]
    fn an_instant_write_produces_no_wait_rather_than_a_zero_sleep() {
        // The first chunks of a transfer are accepted into the stream buffer immediately, so this
        // is the common case at the start, and asking the OS to sleep for zero is pure syscall.
        assert_eq!(
            attachment_call_yield_delay(Duration::ZERO, 25),
            None
        );
    }

    #[test]
    fn the_duty_knob_falls_back_to_the_default_on_anything_it_cannot_use() {
        assert_eq!(
            parse_attachment_call_duty_percent(None),
            ATTACHMENT_CALL_DUTY_PERCENT
        );
        for raw in ["", "  ", "half", "0", "101", "-5"] {
            assert_eq!(
                parse_attachment_call_duty_percent(Some(raw)),
                ATTACHMENT_CALL_DUTY_PERCENT,
                "raw={raw:?}"
            );
        }
        assert_eq!(parse_attachment_call_duty_percent(Some(" 40 ")), 40);
        assert_eq!(parse_attachment_call_duty_percent(Some("100")), 100);
    }
}

#[cfg(test)]
mod chunk_stream_count_tests {
    use super::parse_attachment_chunk_stream_count;

    #[test]
    fn the_default_is_the_historical_single_stream_path() {
        assert_eq!(parse_attachment_chunk_stream_count(None), 1);
    }

    #[test]
    fn garbage_falls_back_to_one_stream_rather_than_to_none() {
        // A typo in the knob must not disable the transfer path -- zero streams would mean chunks
        // queued to nobody, which is worse than ignoring the setting.
        for raw in ["", "  ", "four", "-2", "0", "1.5"] {
            assert_eq!(parse_attachment_chunk_stream_count(Some(raw)), 1, "raw={raw:?}");
        }
    }

    #[test]
    fn a_value_is_taken_as_written_and_clamped_at_sixteen() {
        assert_eq!(parse_attachment_chunk_stream_count(Some(" 4 ")), 4);
        assert_eq!(parse_attachment_chunk_stream_count(Some("16")), 16);
        // Each stream is a real QUIC stream plus a writer thread; an unbounded value would spawn
        // threads by the hundred for no gain the measurements ever showed.
        assert_eq!(parse_attachment_chunk_stream_count(Some("500")), 16);
    }
}
