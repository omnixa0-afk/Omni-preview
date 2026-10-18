// History sync of 1:1 chats (docs/decisions/group-history-sync.md, «1:1 при разделе сети»;
// account-recovery-from-friends.md, Б2).
//
// Delivery already fans out: a sender hands every device of both accounts what it wrote, for seven
// days. Two holes stay: a device away longer than that, and a sender device lost before the others
// came back (what it wrote lives on only at whoever got it). So devices that hold the same chat
// compare it and hand each other what the other lacks:
//
// - our own devices — every chat, every ten minutes while linked;
// - a device restored from the phrase and each contact it meets again — their one chat, every minute
//   while the restore lasts: the contact's copy is how the conversation comes back.
//
// A chat is compared by fingerprint: a line hashes its id, author, text and author time; lines fall
// into 256 buckets by their id; a bucket's fingerprint hashes its lines in order, the chat's hashes
// the buckets. Same fingerprint — nothing to do. Otherwise the buckets go across, and only the lines
// of buckets that differ travel, newest first. Merging only adds (a line id is a line), so a summary
// lost or a frame sent twice changes nothing.
//
// Lines a contact handed over are remembered as such («со слов собеседника»): checked against the
// fingerprint the passport kept, they are verified; otherwise the chat is marked, never rewritten.

const HISTORY_SUMMARY_EVERY_MS: i64 = 10 * 60 * 1000;
const HISTORY_SUMMARY_EVERY_RECOVERING_MS: i64 = 60_000;

/// Lines per frame, by size: well under the logical event's ciphertext cap.
const HISTORY_FRAME_BYTES: usize = 48 * 1024;

/// Frames of lines sent for one bucket comparison; the rest go on the next round.
const HISTORY_FRAMES_PER_ANSWER: usize = 16;

/// One chat's buckets go to one device at most this often.
const HISTORY_BUCKETS_EVERY_MS: i64 = 30_000;

/// Contacts a restored device compares with per round.
const HISTORY_RECOVERY_CONTACTS_PER_ROUND: usize = 8;

const HISTORY_BUCKET_COUNT: usize = omni_protocol::HISTORY_SYNC_BUCKETS;

/// Per profile, in memory: when each device was last given a summary, when a chat's buckets last
/// went to it.
#[derive(Default)]
struct HistorySyncState {
    summary_at_unix_ms: HashMap<String, i64>,
    buckets_at_unix_ms: HashMap<(String, String), i64>,
}

static HISTORY_SYNC: std::sync::OnceLock<Mutex<HashMap<String, HistorySyncState>>> = std::sync::OnceLock::new();

fn with_history_sync_state<T>(db_path: &str, f: impl FnOnce(&mut HistorySyncState) -> T) -> T {
    let registry = HISTORY_SYNC.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    f(map.entry(db_path.to_string()).or_default())
}

pub(crate) fn clear_history_sync_state_for_profile(db_path: &str) {
    if let Some(registry) = HISTORY_SYNC.get() {
        registry.lock().unwrap_or_else(|poison| poison.into_inner()).remove(db_path);
    }
}

/// A chat's line as both of its sides hash it.
struct DigestLine<'a> {
    id: &'a str,
    author: &'a str,
    body: &'a str,
    created_at_unix_ms: i64,
}

fn history_line_hash(line: &DigestLine<'_>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in [line.id.as_bytes(), line.author.as_bytes()] {
        hasher.update((part.len() as u32).to_le_bytes());
        hasher.update(part);
    }
    hasher.update(Sha256::digest(line.body.as_bytes()));
    hasher.update(line.created_at_unix_ms.to_le_bytes());
    hasher.finalize().into()
}

fn history_line_bucket(id: &str) -> usize {
    Sha256::digest(id.as_bytes())[0] as usize
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatDigest {
    count: u64,
    root: [u8; 16],
    buckets: Vec<[u8; 16]>,
}

fn chat_digest<'a>(lines: impl IntoIterator<Item = DigestLine<'a>>) -> ChatDigest {
    let mut buckets: Vec<Vec<[u8; 32]>> = vec![Vec::new(); HISTORY_BUCKET_COUNT];
    let mut count = 0u64;
    for line in lines {
        buckets[history_line_bucket(line.id)].push(history_line_hash(&line));
        count += 1;
    }
    let buckets: Vec<[u8; 16]> = buckets
        .into_iter()
        .map(|mut hashes| {
            if hashes.is_empty() {
                return [0u8; 16];
            }
            hashes.sort_unstable();
            let mut hasher = Sha256::new();
            for hash in &hashes {
                hasher.update(hash);
            }
            hasher.finalize()[..16].try_into().unwrap_or([0u8; 16])
        })
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(count.to_le_bytes());
    for bucket in &buckets {
        hasher.update(bucket);
    }
    let root = hasher.finalize()[..16].try_into().unwrap_or([0u8; 16]);
    ChatDigest { count, root, buckets }
}

/// A chat on this device: the contact's account, the chat's dialog, its lines.
struct LocalChat {
    contact_account_id: AccountId,
    chat_dialog_key: String,
    lines: Vec<omni_storage::ChatHistoryLine>,
}

fn local_chat(storage: &Storage, contact_account_id: &AccountId) -> Option<LocalChat> {
    let contact = storage.find_contact_by_remote_account_id(contact_account_id).ok()??;
    if contact.relationship_state != ContactRelationshipState::Approved || contact.is_blocked {
        return None;
    }
    let dialog_key = contact
        .primary_dialog_key
        .clone()
        .unwrap_or_else(|| omni_types::dialog_key_from_account_id(&contact_account_id.0));
    let chat_dialog_key = storage.chat_dialog_key_for(&dialog_key).unwrap_or(dialog_key);
    let lines = storage.list_chat_history_lines(&chat_dialog_key).ok()?;
    Some(LocalChat { contact_account_id: contact_account_id.clone(), chat_dialog_key, lines })
}

fn local_chat_digest(own_account_id: &AccountId, chat: &LocalChat) -> ChatDigest {
    chat_digest(chat.lines.iter().map(|line| DigestLine {
        id: &line.id,
        author: if line.outgoing { own_account_id.0.as_str() } else { chat.contact_account_id.0.as_str() },
        body: &line.body,
        created_at_unix_ms: line.created_at_unix_ms,
    }))
}

/// Who is on the other end of a history frame and which chat it means. `None` for anyone we do not
/// sync with: not ours, not an approved contact, or a contact naming a chat that is not ours with it.
struct HistoryPeer {
    own_account_id: AccountId,
    /// The chat's other party on our side.
    contact_account_id: AccountId,
    /// The sender is a contact (its copy of our chat with it), not one of our devices.
    from_contact: bool,
}

fn resolve_history_peer(storage: &Storage, from: &omni_crypto::SessionAddress, chat_account_id: &str) -> Option<HistoryPeer> {
    let own_account_id = storage.get_local_account().ok()??.descriptor.account_id;
    if from.remote_account_id == own_account_id {
        if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id).ok()? {
            return None;
        }
        let contact_account_id = AccountId(chat_account_id.to_string());
        if contact_account_id == own_account_id {
            return None;
        }
        return Some(HistoryPeer { own_account_id, contact_account_id, from_contact: false });
    }
    if chat_account_id != own_account_id.0 {
        return None;
    }
    Some(HistoryPeer { own_account_id, contact_account_id: from.remote_account_id.clone(), from_contact: true })
}

/// The name of a chat on the wire: its other party as we see it — the contact, whether the frame
/// goes to one of our devices (which names the chat the same) or to the contact itself (which reads
/// its own account there).
fn chat_name_for(peer: &HistoryPeer) -> String {
    peer.contact_account_id.0.clone()
}

fn send_history_message(
    storage: &Storage,
    transport: &TransportManager,
    to_account_id: &AccountId,
    dialog_key: &str,
    message: omni_types::HistorySyncMessageV1,
) -> OmniResult<()> {
    send_account_event_to_dialog(
        storage,
        transport,
        to_account_id,
        dialog_key,
        omni_types::LogicalEventEnvelopeV1::history_sync(message),
        SecurityPayloadKind::HistorySync,
        "history sync waits",
    )
}

/// Our other devices, each given a summary of every chat at most every ten minutes. Caller holds the
/// session lock.
fn history_sync_round(storage: &Storage, transport: &TransportManager, now_unix_ms: i64) {
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    let db_path = storage.db_path().to_string_lossy().to_string();
    let mut summary: Option<Vec<omni_types::HistoryChatSummaryV1>> = None;
    for (device_id, dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
        if transport.connection_snapshot_for_dialog(&dialog_key).is_none() {
            continue;
        }
        let due = with_history_sync_state(&db_path, |state| {
            now_unix_ms - state.summary_at_unix_ms.get(&device_id.0).copied().unwrap_or(0) >= HISTORY_SUMMARY_EVERY_MS
        });
        if !due {
            continue;
        }
        let Ok(Some(_)) =
            established_session_with_own_device(storage, transport, &own_account_id, &device_id, &dialog_key, "history sync waits")
        else {
            continue;
        };
        let chats = summary.get_or_insert_with(|| own_history_summary(storage, &own_account_id));
        let mut sent = true;
        for page in chats.chunks(omni_protocol::HISTORY_SYNC_MAX_CHATS) {
            if let Err(error) = send_history_message(
                storage,
                transport,
                &own_account_id,
                &dialog_key,
                omni_types::HistorySyncMessageV1::Summary { chats: page.to_vec() },
            ) {
                omni_app_trace_log("history/sync", &format!("device={} summary not sent: {error}", device_id.0));
                sent = false;
                break;
            }
        }
        if sent {
            with_history_sync_state(&db_path, |state| state.summary_at_unix_ms.insert(device_id.0.clone(), now_unix_ms));
            omni_app_trace_log("history/sync", &format!("device={} summary of {} chat(s)", device_id.0, chats.len()));
        }
    }
}

fn own_history_summary(storage: &Storage, own_account_id: &AccountId) -> Vec<omni_types::HistoryChatSummaryV1> {
    let mut chats = Vec::new();
    for contact in storage.list_contacts().unwrap_or_default() {
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        let Some(chat) = local_chat(storage, account_id) else {
            continue;
        };
        let digest = local_chat_digest(own_account_id, &chat);
        chats.push(omni_types::HistoryChatSummaryV1 {
            chat_account_id: account_id.0.clone(),
            count: digest.count,
            root_hex: hex::encode(digest.root),
        });
    }
    chats
}

/// A restored device compares its chat with each contact it reaches, every minute while the restore
/// lasts. Caller holds the session lock.
fn history_recovery_round(storage: &Storage, transport: &TransportManager, now_unix_ms: i64) {
    let Some(recovery) = load_account_recovery(storage) else {
        return;
    };
    if !account_recovery_active(&recovery, now_unix_ms) {
        return;
    }
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    let db_path = storage.db_path().to_string_lossy().to_string();
    let mut asked = 0;
    for contact in storage.list_contacts().unwrap_or_default() {
        if asked >= HISTORY_RECOVERY_CONTACTS_PER_ROUND {
            break;
        }
        let Some(account_id) = contact.remote_account_id.clone() else {
            continue;
        };
        let key = format!("contact:{}", account_id.0);
        let due = with_history_sync_state(&db_path, |state| {
            now_unix_ms - state.summary_at_unix_ms.get(&key).copied().unwrap_or(0) >= HISTORY_SUMMARY_EVERY_RECOVERING_MS
        });
        if !due {
            continue;
        }
        let Some(chat) = local_chat(storage, &account_id) else {
            continue;
        };
        let Some(dialog_key) = reachable_session_dialog(storage, transport, &account_id) else {
            continue;
        };
        let digest = local_chat_digest(&own_account_id, &chat);
        with_history_sync_state(&db_path, |state| state.summary_at_unix_ms.insert(key.clone(), now_unix_ms));
        asked += 1;
        // From our side the chat's other party is the contact itself.
        let message = omni_types::HistorySyncMessageV1::Summary {
            chats: vec![omni_types::HistoryChatSummaryV1 {
                chat_account_id: account_id.0.clone(),
                count: digest.count,
                root_hex: hex::encode(digest.root),
            }],
        };
        if let Err(error) = send_history_message(storage, transport, &account_id, &dialog_key, message) {
            omni_app_trace_log("history/recover", &format!("contact={} not asked: {error}", account_id.0));
        }
    }
}

/// A history frame on a session. Caller holds the session lock.
fn handle_incoming_history_sync(
    storage: &Storage,
    transport: &TransportManager,
    message: &omni_types::HistorySyncMessageV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    let db_path = storage.db_path().to_string_lossy().to_string();
    let now_unix_ms = current_unix_ms();
    match message {
        omni_types::HistorySyncMessageV1::Summary { chats } => {
            for summary in chats {
                let Some(peer) = resolve_history_peer(storage, from, &summary.chat_account_id) else {
                    continue;
                };
                let Some(chat) = local_chat(storage, &peer.contact_account_id) else {
                    continue;
                };
                let digest = local_chat_digest(&peer.own_account_id, &chat);
                if hex::encode(digest.root) == summary.root_hex && digest.count == summary.count {
                    continue;
                }
                send_history_buckets(storage, transport, &db_path, from, &receive_context.dialog_key, &peer, &digest, true, now_unix_ms);
            }
            Ok(())
        }
        omni_types::HistorySyncMessageV1::Buckets { chat_account_id, buckets_hex, answer } => {
            let Some(peer) = resolve_history_peer(storage, from, chat_account_id) else {
                return Ok(());
            };
            let Some(chat) = local_chat(storage, &peer.contact_account_id) else {
                return Ok(());
            };
            let digest = local_chat_digest(&peer.own_account_id, &chat);
            let differing: Vec<bool> = digest
                .buckets
                .iter()
                .zip(buckets_hex)
                .map(|(ours, theirs)| hex::encode(ours) != *theirs)
                .collect();
            send_history_lines(storage, transport, from, &receive_context.dialog_key, &peer, &chat, &differing);
            if *answer {
                send_history_buckets(storage, transport, &db_path, from, &receive_context.dialog_key, &peer, &digest, false, now_unix_ms);
            }
            Ok(())
        }
        omni_types::HistorySyncMessageV1::Lines { chat_account_id, lines } => {
            let Some(peer) = resolve_history_peer(storage, from, chat_account_id) else {
                return Ok(());
            };
            let Some(chat) = local_chat(storage, &peer.contact_account_id) else {
                return Ok(());
            };
            let added = merge_history_lines(storage, from, &receive_context.dialog_key, &peer, &chat, lines, now_unix_ms);
            if added > 0 {
                omni_app_trace_log(
                    "history/sync",
                    &format!(
                        "chat={} +{added} line(s) from {} {}",
                        peer.contact_account_id.0,
                        if peer.from_contact { "contact" } else { "device" },
                        from.remote_device_id.0
                    ),
                );
            }
            if peer.from_contact {
                check_recovered_chat(storage, &peer, now_unix_ms);
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_history_buckets(
    storage: &Storage,
    transport: &TransportManager,
    db_path: &str,
    to: &omni_crypto::SessionAddress,
    dialog_key: &str,
    peer: &HistoryPeer,
    digest: &ChatDigest,
    answer: bool,
    now_unix_ms: i64,
) {
    let key = (to.remote_device_id.0.clone(), peer.contact_account_id.0.clone());
    let due = with_history_sync_state(db_path, |state| {
        let due = now_unix_ms - state.buckets_at_unix_ms.get(&key).copied().unwrap_or(0) >= HISTORY_BUCKETS_EVERY_MS;
        if due {
            state.buckets_at_unix_ms.insert(key.clone(), now_unix_ms);
        }
        due
    });
    if !due {
        return;
    }
    let message = omni_types::HistorySyncMessageV1::Buckets {
        chat_account_id: chat_name_for(peer),
        buckets_hex: digest.buckets.iter().map(hex::encode).collect(),
        answer,
    };
    if let Err(error) = send_history_message(storage, transport, &to.remote_account_id, dialog_key, message) {
        omni_app_trace_log("history/sync", &format!("buckets not sent: {error}"));
    }
}

/// Our lines of the buckets that differ, newest first, in frames — at most
/// [`HISTORY_FRAMES_PER_ANSWER`] of them; the rest goes on the next comparison.
fn send_history_lines(
    storage: &Storage,
    transport: &TransportManager,
    to: &omni_crypto::SessionAddress,
    dialog_key: &str,
    peer: &HistoryPeer,
    chat: &LocalChat,
    differing: &[bool],
) {
    let mut frame: Vec<omni_types::HistoryLineV1> = Vec::new();
    let mut frame_bytes = 0usize;
    let mut frames = 0usize;
    let mut sent_lines = 0usize;
    let flush = |frame: &mut Vec<omni_types::HistoryLineV1>| -> bool {
        if frame.is_empty() {
            return true;
        }
        let lines = std::mem::take(frame);
        send_history_message(
            storage,
            transport,
            &to.remote_account_id,
            dialog_key,
            omni_types::HistorySyncMessageV1::Lines { chat_account_id: chat_name_for(peer), lines },
        )
        .is_ok()
    };
    for line in &chat.lines {
        if !differing.get(history_line_bucket(&line.id)).copied().unwrap_or(true) {
            continue;
        }
        // Its file is still being prepared: handed over once it can be offered, not as text.
        if crate::app_attachment_preparation::attachment_is_preparing(storage, &line.id) {
            continue;
        }
        let author = if line.outgoing { &peer.own_account_id } else { &peer.contact_account_id };
        let attachment = history_line_attachment(storage, line);
        let size = line.body.len() + line.id.len() + author.0.len() + 64 + if attachment.is_some() { 1024 } else { 0 };
        if !frame.is_empty() && (frame_bytes + size > HISTORY_FRAME_BYTES || frame.len() >= omni_protocol::HISTORY_SYNC_MAX_LINES) {
            if !flush(&mut frame) {
                return;
            }
            frames += 1;
            frame_bytes = 0;
            if frames >= HISTORY_FRAMES_PER_ANSWER {
                break;
            }
        }
        frame.push(omni_types::HistoryLineV1 {
            message_id: line.id.clone(),
            author_account_id: author.0.clone(),
            body: line.body.clone(),
            created_at_unix_ms: line.created_at_unix_ms,
            attachment,
        });
        frame_bytes += size;
        sent_lines += 1;
    }
    if frames < HISTORY_FRAMES_PER_ANSWER {
        flush(&mut frame);
    }
    if sent_lines > 0 {
        omni_app_trace_log(
            "history/sync",
            &format!("chat={} {sent_lines} line(s) to {}", peer.contact_account_id.0, to.remote_device_id.0),
        );
    }
}

/// The offer of a file line, to hand over with it — and the file served from here from now on: our
/// own file is served already; a copy we downloaded is served on, to the chat's accounts and our own
/// devices only (`attachment_requester_allowed`), so the device taking the line can fetch it here.
fn history_line_attachment(storage: &Storage, line: &omni_storage::ChatHistoryLine) -> Option<omni_types::AttachmentOfferV1> {
    let runtime = storage.get_attachment_runtime_info(&line.id).ok().flatten()?;
    if !runtime.attachment.local_serving_enabled {
        let _ = storage.serve_downloaded_attachment(&line.id);
    }
    Some(runtime.offer_for(&line.id, &line.body, line.created_at_unix_ms))
}

/// Take the lines we lack. A line is ours or the contact's; anything else is dropped. A file line
/// brings its offer: the file is fetched from the device the line came from (`from_dialog_key`), as
/// a copy from our own device is. Returns how many were new.
fn merge_history_lines(
    storage: &Storage,
    from: &omni_crypto::SessionAddress,
    from_dialog_key: &str,
    peer: &HistoryPeer,
    chat: &LocalChat,
    lines: &[omni_types::HistoryLineV1],
    now_unix_ms: i64,
) -> usize {
    let mut added = 0;
    let source = if peer.from_contact {
        DeviceId(format!("history:{}", peer.contact_account_id.0))
    } else {
        from.remote_device_id.clone()
    };
    for line in lines {
        if storage.message_exists(&line.message_id).unwrap_or(false) {
            if peer.from_contact && line.author_account_id == peer.own_account_id.0 {
                // The contact has what we sent: it was delivered.
                let _ = storage.mark_message_delivered_if_sent(&line.message_id, now_unix_ms);
            }
            continue;
        }
        let inserted = if line.author_account_id == peer.own_account_id.0 {
            storage
                .insert_sent_copy(&line.message_id, &chat.chat_dialog_key, &line.body, line.created_at_unix_ms, &source)
                .inspect(|&inserted| {
                    if inserted && peer.from_contact {
                        let _ = storage.mark_message_delivered_if_sent(&line.message_id, now_unix_ms);
                    }
                })
        } else if line.author_account_id == peer.contact_account_id.0 {
            storage
                .insert_incoming_message(&line.message_id, &chat.chat_dialog_key, &line.body, line.created_at_unix_ms)
                .and_then(|record| {
                    // Talk coming back from another copy of the chat, not news: it does not light the counter.
                    storage.mark_dialog_read_through(&record.dialog_key, line.created_at_unix_ms)?;
                    Ok(true)
                })
        } else {
            continue;
        };
        match inserted {
            Ok(true) => {
                added += 1;
                if let Some(offer) = line.attachment.as_ref().filter(|offer| offer.message_id == line.message_id) {
                    if let Err(error) = take_history_line_attachment(storage, from_dialog_key, offer) {
                        omni_app_trace_log("history/sync", &format!("line {} file not taken: {error}", line.message_id));
                    }
                }
                if peer.from_contact {
                    let _ = storage.record_history_import(&line.message_id, &peer.contact_account_id, now_unix_ms);
                }
            }
            Ok(false) => {}
            Err(error) => omni_app_trace_log("history/sync", &format!("line {} not taken: {error}", line.message_id)),
        }
    }
    added
}

fn take_history_line_attachment(
    storage: &Storage,
    from_dialog_key: &str,
    offer: &omni_types::AttachmentOfferV1,
) -> OmniResult<()> {
    if storage.get_attachment_runtime_info(&offer.message_id)?.is_some() {
        return Ok(());
    }
    storage.store_incoming_attachment_offer(
        &offer.message_id,
        from_dialog_key,
        &offer.file_name,
        offer.file_size_bytes,
        offer.mime_type.as_deref(),
        &offer.content_id_hex,
        offer.content_encryption.as_deref(),
        offer.content_key_hex.as_deref(),
        offer.content_nonce_seed_hex.as_deref(),
        offer.content_chunk_size_bytes,
        offer.created_at_unix_ms,
    )?;
    accept_received_attachment_presentation(storage, offer)
}

/// A chat a contact handed back, against the fingerprint the passport kept for it (only lines up to
/// the time the passport counted them).
fn check_recovered_chat(storage: &Storage, peer: &HistoryPeer, now_unix_ms: i64) {
    let Some(recovery) = load_account_recovery(storage) else {
        return;
    };
    let Some((_, count, root_hex, through)) = recovery
        .chat_fingerprints
        .iter()
        .find(|(account, ..)| account == &peer.contact_account_id.0)
        .cloned()
    else {
        return;
    };
    let Some(chat) = local_chat(storage, &peer.contact_account_id) else {
        return;
    };
    let counted = LocalChat {
        contact_account_id: chat.contact_account_id.clone(),
        chat_dialog_key: chat.chat_dialog_key.clone(),
        lines: chat.lines.into_iter().filter(|line| line.created_at_unix_ms <= through).collect(),
    };
    let digest = local_chat_digest(&peer.own_account_id, &counted);
    let check = if digest.count < count {
        omni_storage::ChatHistoryCheck::Incomplete
    } else if hex::encode(digest.root) == root_hex && digest.count == count {
        omni_storage::ChatHistoryCheck::Verified
    } else {
        omni_storage::ChatHistoryCheck::Mismatch
    };
    let previous = storage.get_chat_history_check(&peer.contact_account_id).ok().flatten();
    if previous != Some(check) {
        let _ = storage.record_chat_history_check(&peer.contact_account_id, check, now_unix_ms);
        omni_app_trace_log(
            "history/recover",
            &format!("chat={} {} ({} of {count} line(s))", peer.contact_account_id.0, check.as_str(), digest.count),
        );
    }
}

/// Every chat's fingerprint for the passport: (contact account, count, root, through) — what a
/// restored device checks a contact's copy against.
fn passport_chat_fingerprints(storage: &Storage, own_account_id: &AccountId, now_unix_ms: i64) -> Vec<(String, u64, String, i64)> {
    let mut out = Vec::new();
    for contact in storage.list_contacts().unwrap_or_default() {
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        let Some(chat) = local_chat(storage, account_id) else {
            continue;
        };
        if chat.lines.is_empty() {
            continue;
        }
        let digest = local_chat_digest(own_account_id, &chat);
        out.push((account_id.0.clone(), digest.count, hex::encode(digest.root), now_unix_ms));
    }
    out.sort();
    out
}
