// Files and photos in groups — docs/decisions/media-messages.md section 3.
//
// The offer rides the group as an MLS application message (`GroupAppKindV1::Attachment`, the same
// `AttachmentOfferV1` as a 1:1 chat's, its content key included). The bytes never ride the feed —
// the feed keeps every frame at every node, non-members holding store-and-forward included. A member
// who wants the file opens mesh bulk streams to holders and asks each for some of its blocks
// (`GroupAttachmentPullV1`); a holder answers on its stream with the ordinary attachment transfer
// frames, sealed under the file's own key. A holder is the author, or any member who downloaded it —
// whole (`Storage::serve_downloaded_attachment`) or so far (the blocks in its map,
// `Storage::attachment_partial_source`) — so a file outlives its author's absence and several
// members feed one download at once (docs/decisions/attachment-content-tree.md, stage 3; who is
// asked for what: `app_download_scheduler`). A holder serves only a node that is a member of the
// group in its own MLS view: co-members need not be contacts (`group-trust-model.md`), and a pull is
// not a door into a 1:1 chat.
//
// Receiving is the 1:1 receive path's storage half (start, chunk, complete), minus the 1:1 receipt:
// a group has no pairwise session to carry one.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use omni_group_mls::MemberCard;
use omni_mesh_runtime::{MeshNodeAddress, MeshRuntime};
use omni_protocol::{
    GroupAppPayloadV1, GroupAttachmentElsewhereV1, GroupAttachmentHolderNoteV1, GroupAttachmentPullV1,
    GROUP_ATTACHMENT_NOTE_ELSEWHERE_MAX, GROUP_ATTACHMENT_NOTE_MAGIC, GROUP_ATTACHMENT_NOTE_MAX_BYTES,
    GROUP_ATTACHMENT_PULL_MAGIC, GROUP_ATTACHMENT_PULL_MAX_BYTES,
};
use omni_storage::{GroupMessageMeta, GroupRecord, Storage};
use omni_transport::{AttachmentTransferFrameV2, AttachmentTransferStream};
use omni_types::{AttachmentOfferV1, AttachmentTransferState, OmniError, OmniResult};

use crate::app_download_scheduler::{Ask, AskOutcome, PullPlan, GIVE_UP_STALLED_MS};
use crate::{current_unix_ms, omni_app_trace_log};

/// A failed pull is tried again after this, not on every 3-second tick.
const PULL_RETRY_AFTER_MS: i64 = 15_000;

// --- receive the offer -------------------------------------------------------------------------

pub(crate) fn receive_offer(
    storage: &Storage,
    group_id: &str,
    group: &GroupRecord,
    sender: &MemberCard,
    payload: &GroupAppPayloadV1,
) -> OmniResult<()> {
    let offer: AttachmentOfferV1 = serde_json::from_str(&payload.body)
        .map_err(|e| OmniError::InvalidInput(format!("group attachment offer: {e}")))?;
    omni_protocol::validate_attachment_offer_v1(&offer)?;
    if offer.message_id != payload.message_id {
        return Err(OmniError::InvalidInput("group attachment offer names another message".into()));
    }
    if storage.get_attachment_runtime_info(&offer.message_id)?.is_some() {
        return Ok(()); // a re-sent offer (rolled-back epoch): we have it
    }
    let record = storage.insert_incoming_message(
        &offer.message_id,
        &group.dialog_key,
        &offer.body,
        payload.authored_at_unix_ms,
    )?;
    storage.store_incoming_attachment_offer(
        &offer.message_id,
        &group.dialog_key,
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
    storage.insert_group_message_meta(
        group_id,
        &GroupMessageMeta {
            message_id: offer.message_id.clone(),
            sender_pub: Some(sender.member_pub.clone()),
            sender_name: sender.display_name.clone(),
            kind: "attachment".into(),
        },
    )?;
    // A photo asks for itself, within the bound of a 1:1 dialog (the pull below fetches it on the
    // next tick).
    crate::accept_received_attachment_presentation(storage, &offer)?;
    let db_path = storage.db_path().to_string_lossy().to_string();
    crate::record_inbound_message_event(&db_path, &group.dialog_key, &record.id, &offer.body);
    Ok(())
}

// --- pull what we asked for ----------------------------------------------------------------------

/// A file's pull: whether it runs, when the last one ended (spaces out the retries of one that
/// failed), and how many holders it is fetching from right now (the bubble's «источников»).
#[derive(Clone, Copy, Default)]
struct PullState {
    running: bool,
    ended_at_ms: i64,
    sources: u32,
}

/// (db path, message id) → its pull. One pull per file.
fn pulls() -> &'static Mutex<HashMap<(String, String), PullState>> {
    static PULLS: OnceLock<Mutex<HashMap<(String, String), PullState>>> = OnceLock::new();
    PULLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How many holders the pull of `message_id` fetches from right now (0 — none runs).
pub(crate) fn group_pull_sources(db_path: &str, message_id: &str) -> u32 {
    pulls()
        .lock()
        .ok()
        .and_then(|pulls| pulls.get(&(db_path.to_string(), message_id.to_string())).copied())
        .filter(|pull| pull.running)
        .map_or(0, |pull| pull.sources)
}

fn set_pull_sources(db_path: &str, message_id: &str, sources: usize) {
    if let Ok(mut pulls) = pulls().lock() {
        if let Some(pull) = pulls.get_mut(&(db_path.to_string(), message_id.to_string())) {
            pull.sources = sources as u32;
        }
    }
}

/// Forget this profile's pulls and what its copies gave out (`stop_runtime_context`).
pub(crate) fn clear_group_attachment_pulls_for_db(db_path: &str) {
    if let Ok(mut pulls) = pulls().lock() {
        pulls.retain(|(db, _), _| db != db_path);
    }
    if let Ok(mut given) = given_out().lock() {
        given.retain(|(db, _), _| db != db_path);
    }
}

/// A block a whole copy gave one member is not given again to another while that one is linked,
/// this long: the other is told to ask it (stage 4, seeding — the author's upload is about one
/// file, not one per member).
const SEED_NAMES_FOR_MS: i64 = 60_000;

/// (db path, message id) → block → the member it was last given to, and when.
type GivenOut = HashMap<(String, String), HashMap<u64, (MeshNodeAddress, i64)>>;

fn given_out() -> &'static Mutex<GivenOut> {
    static GIVEN_OUT: OnceLock<Mutex<GivenOut>> = OnceLock::new();
    GIVEN_OUT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A whole copy seeding: the blocks of `request` it gave another member moments ago — one we have
/// a link to — come back with that member (the asker is told to ask it), and the rest are given to
/// the asker now. One step under one lock: two members asking at the same moment must not both be
/// given the same blocks.
fn seed(
    db_path: &str,
    request: &GroupAttachmentPullV1,
    asker: MeshNodeAddress,
    linked: &dyn Fn(&MeshNodeAddress) -> bool,
) -> Vec<(u64, MeshNodeAddress)> {
    let now = current_unix_ms();
    let Ok(mut given) = given_out().lock() else {
        return Vec::new();
    };
    let file = given.entry((db_path.to_string(), request.message_id.clone())).or_default();
    file.retain(|_, (_, at)| now - *at < SEED_NAMES_FOR_MS);
    let mut elsewhere = Vec::new();
    for block in request.from_block..request.to_block {
        match file.get(&block) {
            Some((member, _))
                if !request.insist
                    && *member != asker
                    && elsewhere.len() < GROUP_ATTACHMENT_NOTE_ELSEWHERE_MAX
                    && linked(member) =>
            {
                elsewhere.push((block, *member));
            }
            _ => {
                file.insert(block, (asker, now));
            }
        }
    }
    elsewhere
}

/// A block map as a note carries it: block `i` is bit `i % 8` of byte `i / 8`.
fn pack_blocks(have: &[bool]) -> Vec<u8> {
    let mut bits = vec![0_u8; have.len().div_ceil(8)];
    for (block, here) in have.iter().enumerate() {
        if *here {
            bits[block / 8] |= 1 << (block % 8);
        }
    }
    bits
}

fn unpack_blocks(bits: &[u8], blocks: usize) -> Vec<bool> {
    (0..blocks).map(|block| bits.get(block / 8).is_some_and(|byte| byte & (1 << (block % 8)) != 0)).collect()
}

/// Start the pull of `message_id` unless one runs or one failed too recently.
fn begin_pull(db_path: &str, message_id: &str, now: i64) -> bool {
    let Ok(mut pulls) = pulls().lock() else {
        return false;
    };
    let key = (db_path.to_string(), message_id.to_string());
    match pulls.get(&key) {
        Some(pull) if pull.running => return false,
        Some(pull) if now - pull.ended_at_ms < PULL_RETRY_AFTER_MS => return false,
        _ => {}
    }
    pulls.insert(key, PullState { running: true, ended_at_ms: 0, sources: 0 });
    true
}

fn end_pull(db_path: &str, message_id: &str, succeeded: bool) {
    if let Ok(mut pulls) = pulls().lock() {
        let key = (db_path.to_string(), message_id.to_string());
        if succeeded {
            pulls.remove(&key);
        } else {
            pulls.insert(key, PullState { running: false, ended_at_ms: current_unix_ms(), sources: 0 });
        }
    }
}

/// The members we have a link to who may hold a group file: its author first, then the others (a
/// member who downloaded it, or part of it, hands it on).
fn linked_holders(storage: &Storage, mesh: &MeshRuntime, group: &GroupRecord, message_id: &str) -> Vec<MeshNodeAddress> {
    let Ok(members) = storage.list_group_members(&group.group_id) else {
        return Vec::new();
    };
    let author = storage
        .get_group_message_meta(&group.group_id, message_id)
        .ok()
        .flatten()
        .and_then(|meta| meta.sender_pub);
    let mut holders: Vec<(bool, MeshNodeAddress)> = members
        .iter()
        .filter(|m| m.status == "member" && m.member_pub != group.my_member_pub)
        .filter_map(|m| {
            let not_author = author.as_deref() != Some(m.member_pub.as_str());
            crate::app_groups::node_from_hex(&m.node_hint_hex).map(|node| (not_author, node))
        })
        .filter(|(_, node)| mesh.has_link(node))
        .collect();
    holders.sort_by_key(|(not_author, _)| *not_author);
    holders.into_iter().map(|(_, node)| node).collect()
}

/// The mesh tick: start a pull for every group file we asked for and are not fetching yet, when we
/// have a link to someone who may hold it.
pub(crate) fn pull_requested_group_attachments(storage: &Storage, mesh: &Arc<MeshRuntime>) {
    let Ok(groups) = storage.list_groups() else {
        return;
    };
    let db_path = storage.db_path().to_string_lossy().to_string();
    for group in groups.into_iter().filter(|g| g.my_status == "member") {
        let Ok(pending) = group_downloads_to_pull(storage, &group) else {
            continue;
        };
        for item in pending {
            let message_id = item.attachment.message_id.clone();
            if linked_holders(storage, mesh, &group, &message_id).is_empty() {
                continue;
            }
            if !begin_pull(&db_path, &message_id, current_unix_ms()) {
                continue;
            }
            let mesh = Arc::clone(mesh);
            let db = db_path.clone();
            let group = group.clone();
            let spawned = std::thread::Builder::new()
                .name("omni-group-attachment-pull".to_string())
                .spawn(move || {
                    let open_mesh = Arc::clone(&mesh);
                    let outcome = pull_from_holders(
                        &db,
                        &group.group_id,
                        &message_id,
                        |storage| linked_holders(storage, &mesh, &group, &message_id),
                        move |node| {
                            open_mesh
                                .open_bulk_stream(node)
                                .map_err(|e| OmniError::Transport(format!("group attachment pull stream: {e}")))
                        },
                    );
                    if let Err(error) = &outcome {
                        omni_app_trace_log(
                            "group/attachment-pull",
                            &format!("group={} message={message_id} failed: {error}", group.group_id),
                        );
                        if let Ok(storage) = Storage::open(&db) {
                            requeue(&storage, &message_id);
                        }
                    }
                    end_pull(&db, &message_id, outcome.is_ok());
                });
            if spawned.is_err() {
                end_pull(&db_path, &item.attachment.message_id, false);
            }
        }
    }
}

/// The group's downloads a pull may be started for: asked for, and under way. A download left
/// `downloading` with no pull at it is picked up too (`begin_pull` keeps a running one single): a
/// block of a late ask landing after its pull gave up turned the requeued row back to `downloading`,
/// and nothing pulled it again (live 2026-10-16: stuck at 6 of 19 blocks while a whole copy sat one
/// link away); after a restart the same.
pub(crate) fn group_downloads_to_pull(storage: &Storage, group: &GroupRecord) -> OmniResult<Vec<omni_storage::AttachmentRuntimeInfo>> {
    let mut downloads = storage.list_pending_attachment_downloads_for_dialog(&group.dialog_key)?;
    downloads.extend(storage.list_downloading_attachments_for_dialog(&group.dialog_key)?);
    Ok(downloads)
}

/// Back to `requested`, so the next tick after the pause tries again (from what is on disk).
fn requeue(storage: &Storage, message_id: &str) {
    let Ok(Some(runtime)) = storage.get_attachment_runtime_info(message_id) else {
        return;
    };
    crate::settle_live_attachment_receive_writer(storage, &runtime);
    if let (Some(save_path), Some(transfer_id)) =
        (runtime.requested_save_path.as_ref(), runtime.active_transfer_id.as_ref())
    {
        let _ = storage.mark_attachment_download_requested(message_id, save_path, transfer_id, current_unix_ms());
    }
}

/// How long the end of a pull waits for its asks still running to stop before moving the file.
const PULL_ASKS_STOP_WAIT: Duration = Duration::from_secs(10);

/// One ask's end: who was asked, for which blocks, its note, how it went.
type AskDone = (MeshNodeAddress, Range<u64>, Option<GroupAttachmentHolderNoteV1>, OmniResult<()>);

/// Fetch a group file from several holders at once (docs/decisions/attachment-content-tree.md,
/// stage 3): `holders` names the ones to ask (looked up again every round — links come and go),
/// `open` opens a stream to one, the plan (`app_download_scheduler`) says who is asked for which
/// blocks. Every stream writes through the transfer's one writer; the file is moved into place once
/// its map holds every block. `Ok` also when the download stopped being wanted meanwhile
/// (cancelled, saved elsewhere).
pub(crate) fn pull_from_holders<O>(
    db_path: &str,
    group_id: &str,
    message_id: &str,
    holders: impl Fn(&Storage) -> Vec<MeshNodeAddress>,
    open: O,
) -> OmniResult<()>
where
    O: Fn(MeshNodeAddress) -> OmniResult<omni_mesh_runtime::BulkStream> + Clone + Send + 'static,
{
    let storage = Storage::open(db_path)?;
    if let Some(runtime) = storage.get_attachment_runtime_info(message_id)? {
        crate::settle_live_attachment_receive_writer(&storage, &runtime);
    }
    let (runtime, _) = storage.attachment_download_resume(message_id, current_unix_ms())?;
    let transfer_id = runtime
        .active_transfer_id
        .clone()
        .ok_or_else(|| OmniError::Storage("group attachment download has no transfer id".into()))?;
    storage.mark_attachment_downloading(message_id, runtime.attachment.bytes_transferred, current_unix_ms())?;

    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel::<AskDone>();
    let mut plan = PullPlan::new();
    // When bytes last came: a pull that gets none for long gives up.
    let mut progress = (runtime.attachment.bytes_transferred, current_unix_ms());
    let wanted = |storage: &Storage| -> OmniResult<bool> {
        let row = storage.get_attachment_runtime_info(message_id)?;
        Ok(row.is_some_and(|row| {
            matches!(
                row.attachment.state,
                AttachmentTransferState::Requested
                    | AttachmentTransferState::Downloading
                    | AttachmentTransferState::WaitingForReconnect
            ) && row.active_transfer_id.as_deref() == Some(transfer_id.as_str())
        }))
    };
    let outcome: OmniResult<bool> = 'pull: loop {
        if !wanted(&storage)? {
            break Ok(false);
        }
        let have = storage.attachment_received_blocks(message_id)?.have;
        if have.iter().all(|here| *here) {
            break Ok(true);
        }
        let now = current_unix_ms();
        let bytes = storage.get_attachment_runtime_info(message_id)?.map_or(0, |row| row.attachment.bytes_transferred);
        if bytes != progress.0 {
            progress = (bytes, now);
        } else if now - progress.1 > GIVE_UP_STALLED_MS {
            break Err(OmniError::Transport("no holder sent anything for a while".into()));
        }
        let holders_now = holders(&storage);
        for Ask { holder: node, blocks: range, insist } in plan.assign(&have, &holders_now, current_unix_ms()) {
            omni_app_trace_log(
                "group/attachment-pull",
                &format!(
                    "group={group_id} message={message_id} asks {node} for blocks {}..{}{}",
                    range.start,
                    range.end,
                    if insist { " (insisting)" } else { "" }
                ),
            );
            let (db, group, message, transfer) =
                (db_path.to_string(), group_id.to_string(), message_id.to_string(), transfer_id.clone());
            let (open, stop, done) = (open.clone(), Arc::clone(&stop), done_tx.clone());
            let asked = range.clone();
            let spawned = std::thread::Builder::new().name("omni-group-attachment-ask".to_string()).spawn(move || {
                let mut note = None;
                let result = open(node).and_then(|bulk| {
                    let ask = AskOf { group_id: &group, message_id: &message, transfer_id: &transfer, blocks: asked.clone(), insist };
                    pull_range(&db, bulk, &ask, &stop, &mut note)
                });
                let _ = done.send((node, asked, note, result));
            });
            if let Err(error) = spawned {
                plan.finished(node, range, &have, AskOutcome::Failed, current_unix_ms());
                break 'pull Err(OmniError::Internal(format!("group attachment ask: {error}")));
            }
        }
        set_pull_sources(db_path, message_id, plan.sources());
        if plan.asked() == 0 {
            // No one to ask right now. With no holder at all, give up (the next tick starts again);
            // else a named member is still getting its blocks, or a holder rests — wait a moment
            // (bounded by the stall above).
            if holders_now.is_empty() || plan.nobody_has_the_rest(&have, &holders_now, current_unix_ms()) {
                break Err(OmniError::Transport("no linked holder has the blocks still missing".into()));
            }
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        match done_rx.recv_timeout(Duration::from_millis(500)) {
            Ok((node, range, note, result)) => {
                // What the holder said it has, and whom it named, before how the ask went.
                if let Some(note) = note {
                    let have = match &note.have_hex {
                        None => Some(None),
                        Some(bits) => hex::decode(bits).ok().map(|bits| Some(unpack_blocks(&bits, have.len()))),
                    };
                    let named: Vec<(u64, MeshNodeAddress)> = note
                        .elsewhere
                        .iter()
                        .filter_map(|named| crate::app_groups::node_from_hex(&named.node_hex).map(|node| (named.block, node)))
                        .collect();
                    if let Some(have) = have {
                        plan.noted(node, have, named, current_unix_ms());
                    }
                }
                // What the ask brought is counted before the plan hears how it went.
                crate::checkpoint_attachment_receive_writer_by_transfer(&storage, db_path, &transfer_id);
                let have = storage.attachment_received_blocks(message_id)?.have;
                let outcome = ask_outcome(&result);
                if let Err(error) = &result {
                    omni_app_trace_log(
                        "group/attachment-pull",
                        &format!("group={group_id} message={message_id} ask of {node} ended ({outcome:?}): {error}"),
                    );
                }
                plan.finished(node, range, &have, outcome, current_unix_ms());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(OmniError::Internal("group attachment asks vanished".into()));
            }
        }
    };

    // The asks still running stop at their next frame; the file moves only once none writes it.
    stop.store(true, Ordering::SeqCst);
    set_pull_sources(db_path, message_id, 0);
    let deadline = std::time::Instant::now() + PULL_ASKS_STOP_WAIT;
    while plan.asked() > 0 {
        match done_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok((node, range, _, result)) => plan.finished(node, range, &[], ask_outcome(&result), current_unix_ms()),
            Err(_) => break,
        }
    }
    crate::checkpoint_attachment_receive_writer_by_transfer(&storage, db_path, &transfer_id);
    if !outcome? {
        return Ok(());
    }
    crate::drop_attachment_receive_writer(db_path, &transfer_id);
    let runtime = storage
        .get_attachment_runtime_info(message_id)?
        .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?;
    if !crate::complete_received_attachment(&storage, &runtime, &transfer_id)? {
        return Err(OmniError::Storage("group attachment ended with blocks of the file missing".into()));
    }
    // Now we hold it too: hand it on to members who ask.
    if let Err(error) = storage.serve_downloaded_attachment(message_id) {
        omni_app_trace_log("group/attachment-pull", &format!("message={message_id} not served on: {error}"));
    }
    omni_app_trace_log("group/attachment-pull", &format!("group={group_id} message={message_id} done"));
    Ok(())
}

/// What an ask's end says about its holder: a block that failed its check is the holder's fault —
/// so is a frame that fails its seal (the link is authenticated end to end; only someone with the
/// file's key seals for it) — a broken stream or a refusal is not.
fn ask_outcome(result: &OmniResult<()>) -> AskOutcome {
    match result {
        Ok(()) => AskOutcome::Answered,
        Err(error) => {
            let message = error.to_string();
            if message.contains("does not match")
                || message.contains("is not a leaf")
                || message.contains("without the block's proof")
                || message.contains("authentication failed")
            {
                AskOutcome::Corrupt
            } else {
                AskOutcome::Failed
            }
        }
    }
}

/// Fetch the whole file over one stream already open to one holder (a file of a block or two).
#[cfg(test)]
pub(crate) fn pull_over(
    db_path: &str,
    bulk: omni_mesh_runtime::BulkStream,
    group_id: &str,
    message_id: &str,
) -> OmniResult<()> {
    let only = MeshNodeAddress::from_bytes([0; 20]);
    let bulk = Arc::new(Mutex::new(Some(bulk)));
    pull_from_holders(db_path, group_id, message_id, |_| vec![only], move |_| {
        bulk.lock()
            .ok()
            .and_then(|mut bulk| bulk.take())
            .ok_or_else(|| OmniError::Transport("the one stream is used".into()))
    })
}

/// What one ask asks for.
struct AskOf<'a> {
    group_id: &'a str,
    message_id: &'a str,
    transfer_id: &'a str,
    blocks: Range<u64>,
    insist: bool,
}

/// One ask: the blocks of the file from the holder at the other end of `bulk`, written through the
/// transfer's writer as they come. The holder's note comes first (`note`); then it sends the asked
/// blocks it has and ends — what did not come, the plan asks of someone else.
fn pull_range(
    db_path: &str,
    bulk: omni_mesh_runtime::BulkStream,
    ask: &AskOf<'_>,
    stop: &AtomicBool,
    note: &mut Option<GroupAttachmentHolderNoteV1>,
) -> OmniResult<()> {
    let AskOf { group_id, message_id, transfer_id, .. } = *ask;
    let storage = Storage::open(db_path)?;
    let dialog_key = storage
        .get_attachment_runtime_info(message_id)?
        .ok_or_else(|| OmniError::Storage(format!("attachment '{message_id}' was not found")))?
        .dialog_key;
    let request = GroupAttachmentPullV1 {
        group_id: group_id.to_string(),
        message_id: message_id.to_string(),
        transfer_id: transfer_id.to_string(),
        from_block: ask.blocks.start,
        to_block: ask.blocks.end,
        insist: ask.insist,
    };
    let mut send = bulk.send;
    write_pull_preamble(&mut send, &request)?;
    let mut recv = bulk.recv;
    *note = Some(read_holder_note(&mut recv)?);
    let mut stream = AttachmentTransferStream::from_halves(send, recv)?;
    let mut answering = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(frame) = stream.read_frame()? else {
            // Its note was all of it: nothing of the file there yet.
            if !answering {
                return Ok(());
            }
            return Err(OmniError::Transport("the holder closed the stream before the end".into()));
        };
        answering = true;
        // Our stream, our transfer: a frame naming any other would write into someone else's file.
        if crate::attachment_transfer_frame_transfer_id(&frame) != Some(transfer_id) {
            return Err(OmniError::Transport("a frame of another transfer on a group pull".into()));
        }
        match frame {
            AttachmentTransferFrameV2::Start(start) => crate::persist_attachment_transfer_start_frame(&storage, start)?,
            AttachmentTransferFrameV2::Chunk(chunk) => {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                crate::persist_attachment_transfer_chunk_frame(&storage, chunk, &dialog_key)?;
            }
            // The holder sent what it has of the range.
            AttachmentTransferFrameV2::Complete(_) => return Ok(()),
        }
    }
}

fn block_on<F: std::future::Future>(future: F) -> OmniResult<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| OmniError::Transport(format!("group attachment IO runtime: {e}")))?;
    Ok(runtime.block_on(future))
}

/// A holder's note, read off the head of its answer (the four bytes, a u32 length, JSON).
fn read_holder_note(recv: &mut Box<dyn tokio::io::AsyncRead + Send + Unpin>) -> OmniResult<GroupAttachmentHolderNoteV1> {
    use tokio::io::AsyncReadExt as _;
    let mut head = [0_u8; 8];
    block_on(async { recv.read_exact(&mut head).await })?
        .map_err(|e| OmniError::Transport(format!("the holder closed the stream before its note: {e}")))?;
    if head[..4] != GROUP_ATTACHMENT_NOTE_MAGIC {
        return Err(OmniError::Transport("the holder's answer has no note".into()));
    }
    let len = u32::from_le_bytes(head[4..].try_into().expect("four bytes")) as usize;
    if len == 0 || len > GROUP_ATTACHMENT_NOTE_MAX_BYTES {
        return Err(OmniError::InvalidInput("group attachment note size is invalid".into()));
    }
    let mut json = vec![0_u8; len];
    block_on(async { recv.read_exact(&mut json).await })?
        .map_err(|e| OmniError::Transport(format!("group attachment note: {e}")))?;
    let note: GroupAttachmentHolderNoteV1 =
        serde_json::from_slice(&json).map_err(|e| OmniError::InvalidInput(format!("group attachment note: {e}")))?;
    omni_protocol::validate_group_attachment_holder_note_v1(&note)?;
    Ok(note)
}

fn write_holder_note(send: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>, note: &GroupAttachmentHolderNoteV1) -> OmniResult<()> {
    use tokio::io::AsyncWriteExt as _;
    let json = serde_json::to_vec(note).map_err(|e| OmniError::Internal(e.to_string()))?;
    let mut head = Vec::with_capacity(8 + json.len());
    head.extend_from_slice(&GROUP_ATTACHMENT_NOTE_MAGIC);
    head.extend_from_slice(&(json.len() as u32).to_le_bytes());
    head.extend_from_slice(&json);
    block_on(async { send.write_all(&head).await })?
        .map_err(|e| OmniError::Transport(format!("group attachment note: {e}")))
}

fn write_pull_preamble(
    send: &mut Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    request: &GroupAttachmentPullV1,
) -> OmniResult<()> {
    use tokio::io::AsyncWriteExt as _;
    let json = serde_json::to_vec(request).map_err(|e| OmniError::Internal(e.to_string()))?;
    let mut preamble = Vec::with_capacity(8 + json.len());
    preamble.extend_from_slice(&GROUP_ATTACHMENT_PULL_MAGIC);
    preamble.extend_from_slice(&(json.len() as u32).to_le_bytes());
    preamble.extend_from_slice(&json);
    block_on(async { send.write_all(&preamble).await })?
        .map_err(|e| OmniError::Transport(format!("group attachment pull request: {e}")))
}

// --- serve what a member asks for ------------------------------------------------------------------

/// What an inbound bulk stream turned out to be.
pub(crate) enum InboundBulk {
    /// A member pulling a group file from us.
    GroupPull(GroupAttachmentPullV1, omni_mesh_runtime::BulkStream),
    /// A 1:1 transfer pushed to us; its first bytes are given back to the reader.
    Transfer(omni_mesh_runtime::BulkStream),
}

/// Read the first four bytes of an inbound bulk stream to tell a group pull from a 1:1 transfer.
pub(crate) fn classify_inbound_bulk(bulk: omni_mesh_runtime::BulkStream) -> OmniResult<InboundBulk> {
    use tokio::io::AsyncReadExt as _;
    let omni_mesh_runtime::BulkStream { send, mut recv } = bulk;
    let mut head = [0_u8; 4];
    block_on(async { recv.read_exact(&mut head).await })?
        .map_err(|e| OmniError::Transport(format!("bulk stream head: {e}")))?;
    if head != GROUP_ATTACHMENT_PULL_MAGIC {
        let recv: Box<dyn tokio::io::AsyncRead + Send + Unpin> =
            Box::new(std::io::Cursor::new(head.to_vec()).chain(recv));
        return Ok(InboundBulk::Transfer(omni_mesh_runtime::BulkStream { send, recv }));
    }
    let mut len = [0_u8; 4];
    block_on(async { recv.read_exact(&mut len).await })?
        .map_err(|e| OmniError::Transport(format!("group attachment pull length: {e}")))?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > GROUP_ATTACHMENT_PULL_MAX_BYTES {
        return Err(OmniError::InvalidInput("group attachment pull request size is invalid".into()));
    }
    let mut json = vec![0_u8; len];
    block_on(async { recv.read_exact(&mut json).await })?
        .map_err(|e| OmniError::Transport(format!("group attachment pull request: {e}")))?;
    let request: GroupAttachmentPullV1 = serde_json::from_slice(&json)
        .map_err(|e| OmniError::InvalidInput(format!("group attachment pull request: {e}")))?;
    omni_protocol::validate_group_attachment_pull_v1(&request)?;
    Ok(InboundBulk::GroupPull(request, omni_mesh_runtime::BulkStream { send, recv }))
}

/// May `peer` have this file? A member of the group in our view (its node is a member's, the link
/// proved the node key), the file one of this group's messages, and still being served here.
fn serve_allowed(storage: &Storage, peer: &MeshNodeAddress, request: &GroupAttachmentPullV1) -> OmniResult<()> {
    let group = storage
        .get_group(&request.group_id)?
        .filter(|g| g.my_status == "member")
        .ok_or_else(|| OmniError::InvalidInput("not a group of ours".into()))?;
    let is_member = storage
        .list_group_members(&request.group_id)?
        .iter()
        .filter(|m| m.status == "member" && m.member_pub != group.my_member_pub)
        .any(|m| crate::app_groups::node_from_hex(&m.node_hint_hex).as_ref() == Some(peer));
    if !is_member {
        return Err(OmniError::InvalidInput("the node is not a member of the group".into()));
    }
    let runtime = storage
        .get_attachment_runtime_info(&request.message_id)?
        .ok_or_else(|| OmniError::InvalidInput("no such attachment".into()))?;
    if runtime.dialog_key != group.dialog_key {
        return Err(OmniError::InvalidInput("the attachment is not this group's".into()));
    }
    Ok(())
}

/// Answer a pull on its own stream: the note (what we have; from a whole copy, whom to ask for the
/// blocks it just gave a member `linked` says we reach — stage 4, seeding), then the blocks. Refused
/// quietly (the stream just closes): the asker retries later and a non-member learns nothing beyond
/// "no".
pub(crate) fn serve_group_pull(
    db_path: &str,
    peer: MeshNodeAddress,
    request: GroupAttachmentPullV1,
    bulk: omni_mesh_runtime::BulkStream,
    linked: &dyn Fn(&MeshNodeAddress) -> bool,
) {
    let outcome = (|| -> OmniResult<()> {
        let storage = Storage::open(db_path)?;
        serve_allowed(&storage, &peer, &request)?;
        // The whole file, or the blocks of a download in progress.
        let whole = storage.attachment_serving_blob_path(&request.message_id).ok().flatten().is_some();
        let have = if whole {
            None
        } else {
            match storage.attachment_partial_source(&request.message_id)? {
                Some((_, have)) => Some(have),
                None => {
                    // Nothing of it here (yet): said so — a member just starting is no failure to
                    // rest from, its copy will grow.
                    let mut send = bulk.send;
                    return write_holder_note(&mut send, &GroupAttachmentHolderNoteV1 { have_hex: Some(String::new()), elsewhere: Vec::new() });
                }
            }
        };
        let Some(worker) = crate::reserve_attachment_send_worker(db_path, &request.transfer_id) else {
            return Err(OmniError::Transport("this transfer is already being served".into()));
        };
        let elsewhere = if whole { seed(db_path, &request, peer, linked) } else { Vec::new() };
        let note = GroupAttachmentHolderNoteV1 {
            have_hex: have.as_deref().map(|have| hex::encode(pack_blocks(have))),
            elsewhere: elsewhere
                .iter()
                .map(|(block, member)| GroupAttachmentElsewhereV1 { block: *block, node_hex: member.to_string() })
                .collect(),
        };
        let skip: Vec<u64> = elsewhere.iter().map(|(block, _)| *block).collect();
        omni_app_trace_log(
            "group/attachment-serve",
            &format!(
                "group={} message={} to={peer} blocks {}..{} from a {} copy at {}, {} named elsewhere{}",
                request.group_id,
                request.message_id,
                request.from_block,
                request.to_block,
                if whole { "whole" } else { "partial" },
                std::path::Path::new(db_path).parent().and_then(|dir| dir.file_name()).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
                skip.len(),
                if request.insist { " (insisted)" } else { "" }
            ),
        );
        let mut send = bulk.send;
        let sent = write_holder_note(&mut send, &note).and_then(|()| {
            let stream = AttachmentTransferStream::from_halves(send, bulk.recv)?;
            crate::send_attachment_over_sink(
                &storage,
                &request.message_id,
                &request.transfer_id,
                crate::AttachmentServeRange {
                    from_offset_bytes: (request.from_block * omni_crypto::CONTENT_TREE_BLOCK_BYTES) as i64,
                    to_block: Some(request.to_block),
                    partial_allowed: true,
                    skip,
                },
                None,
                Some(&worker),
                || Ok(crate::AttachmentChunkSink::Single(stream)),
            )
        });
        crate::release_attachment_send_worker(db_path, &request.transfer_id, &worker);
        sent
    })();
    omni_app_trace_log(
        "group/attachment-serve",
        &match &outcome {
            Ok(()) => format!("group={} message={} to={peer} sent", request.group_id, request.message_id),
            Err(error) => format!(
                "group={} message={} to={peer} refused: {error}",
                request.group_id, request.message_id
            ),
        },
    );
}
