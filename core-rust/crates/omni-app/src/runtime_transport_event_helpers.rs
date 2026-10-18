#[derive(Debug, Clone)]
struct PendingVerificationChallenge {
    dialog_key: String,
    peer_id: String,
    remote_instance_id: Option<String>,
    remote_account_id: Option<String>,
    challenge_bytes: Vec<u8>,
    expires_at_unix_ms: i64,
}

#[derive(Default, Debug)]
struct VerificationChallengeRegistry {
    pending_by_id: HashMap<String, PendingVerificationChallenge>,
    ready_session_keys: HashSet<String>,
}

#[derive(Default, Debug, Clone)]
struct PublicProfileSyncRegistryEntry {
    descriptor_updated_at_unix_ms: i64,
    descriptor_session_created_at_unix_ms: Option<i64>,
    avatar_hash_hex: Option<String>,
    avatar_session_created_at_unix_ms: Option<i64>,
    avatar_last_attempt_at_unix_ms: Option<i64>,
    avatar_attempt_count: u8,
}

static VERIFICATION_CHALLENGES: OnceLock<Mutex<HashMap<String, VerificationChallengeRegistry>>> =
    OnceLock::new();
static PUBLIC_PROFILE_SYNCS_SENT: OnceLock<Mutex<HashMap<String, PublicProfileSyncRegistryEntry>>> =
    OnceLock::new();
// R1c: per-session high-water of MY relay-bundle version already pushed over the live channel
// (keyed `db_path::account::device` → last-sent bundle version). Prevents re-sending an unchanged
// set; a version bump (rotation) makes the next push send. Cleared per-profile by
// `clear_runtime_contact_ephemera_for_active_profile`. See docs/decisions/r1c-rotatable-white-relay.md.
static RELAY_SET_PUSH_SENT: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
#[allow(clippy::type_complexity)] // transfer-id → (offset → chunk) reorder buffers
static ATTACHMENT_REORDER_BUFFERS: OnceLock<Mutex<HashMap<String, BTreeMap<i64, Vec<u8>>>>> =
    OnceLock::new();
// Per-transfer receive state, held for the life of ONE incoming bulk stream and keyed
// `{db_path}::{transfer_id}`. Without it every 1 MB chunk redid the same fixed work — a DB lookup
// for the runtime info, a `create_dir_all`, re-deriving the content crypto material, re-opening the
// temp file and reading its block map. None of that scales with chunk size, so it was
// pure per-chunk tax on a bulk download. The cached `File` MUST be dropped before the temp file is
// moved into place (Windows blocks a move while a handle is open), which is why the Complete / Fail
// / Cancel handlers drop it first. Cleared per-profile by `clear_attachment_buffers_for_profile`.
static ATTACHMENT_RECEIVE_WRITERS: OnceLock<
    Mutex<HashMap<String, SharedAttachmentReceiveWriter>>,
> = OnceLock::new();
// Newest `created_at_unix_ms` of any session bootstrap seen from a peer, keyed
// `{db_path}::{remote_account}::{remote_device}`. Used to tell a pairing-collision ECHO (a bootstrap
// we have already seen, re-delivered by the durable backstop) from a peer that genuinely
// re-bootstrapped after a reinstall. Both values compared come from the PEER's clock, so the
// comparison is immune to clock skew between the two machines — an earlier attempt compared the
// peer's timestamp against our own and was defeated by a 4.3 s skew on a real two-machine run.
// Cleared per-profile by `clear_runtime_contact_ephemera_for_active_profile`.
static LAST_INBOUND_BOOTSTRAP_AT: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
// In-flight attachment SEND workers, keyed `{db_path}::{transfer_id}`. A duplicated/replayed
// download request must NOT spawn a second sender for a transfer already streaming: that sends the
// whole file twice (2x data + CPU) and starves the sender's UI thread. See
// docs/bugs/attachment-duplicate-send-worker.md. A worker whose write has been stuck for
// `ATTACHMENT_SEND_WORKER_STUCK_AFTER_MS` streams into a dead connection: a new request supersedes it.
static ATTACHMENT_SEND_WORKERS_IN_FLIGHT: OnceLock<Mutex<HashMap<String, Arc<AttachmentSendWorker>>>> = OnceLock::new();

/// A write blocked this long means the stream's connection is gone (a dead peer is noticed only at
/// the idle timeout, ~30 s): the receiver that came back and asked again does not wait for it. A
/// live but slow link moves a 1 MiB chunk well within it (under 70 KB/s it would not).
const ATTACHMENT_SEND_WORKER_STUCK_AFTER_MS: i64 = 15_000;

/// One in-flight send worker: whether a newer request took its place, and since when one of its
/// writes has been blocked (0 - none is). Waiting for an upload slot is not a write.
#[derive(Debug, Default)]
pub(crate) struct AttachmentSendWorker {
    superseded: std::sync::atomic::AtomicBool,
    write_blocked_since_ms: std::sync::atomic::AtomicI64,
}

impl AttachmentSendWorker {
    pub(crate) fn write_started(&self) {
        self.write_blocked_since_ms.store(current_unix_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn write_done(&self) {
        self.write_blocked_since_ms.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// A newer request for the transfer took over: this worker stops at its next chunk.
    pub(crate) fn superseded(&self) -> bool {
        self.superseded.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn stuck(&self, now_unix_ms: i64) -> bool {
        let since = self.write_blocked_since_ms.load(std::sync::atomic::Ordering::Relaxed);
        since > 0 && now_unix_ms - since >= ATTACHMENT_SEND_WORKER_STUCK_AFTER_MS
    }
}
// TM-008 (2026-05-21): per-profile sliding-window counter of NEW
// `Pending` contacts auto-created by incoming transport events. Keyed by
// `db_path`; the value is a VecDeque of Unix-ms timestamps, sorted
// ascending, automatically pruned on each access by
// `record_pending_contact_creation_under_rate_limit`. Cleared by the
// new `clear_pending_contact_rate_limits_for_profile` hook in
// `stop_runtime_context`. See
// `docs/decisions/tm-008-pending-contact-rate-limit.md`.
static PENDING_CONTACT_RATE_LIMITS:
    OnceLock<Mutex<HashMap<String, std::collections::VecDeque<i64>>>> = OnceLock::new();

// Inbound message events for the daemon / event plane to drain (push, not poll).
// The daemon's event plane: ONE db_path-keyed bounded registry holding every event
// KIND as a `CoreEvent` variant. One registry + one drain so adding a kind doesn't
// add a parallel registry. Cleared on stop_runtime_context. daemon-ipc-phase2.md.
static CORE_EVENTS: OnceLock<Mutex<HashMap<String, std::collections::VecDeque<CoreEvent>>>> =
    OnceLock::new();
const TM008_PENDING_RATE_WINDOW_MS: i64 = 60 * 60 * 1000; // 60 minutes
const TM008_PENDING_RATE_LIMIT: usize = 32;
const INBOUND_EVENTS_CAP: usize = 512;
// Two different jobs, deliberately two different numbers.
//
// `..MAX_BYTES` is an ANTI-ABUSE ceiling: a peer that streams chunks at scattered offsets, never
// closing the gap, would otherwise pile them up in RAM without bound. Hitting it is a protocol
// violation and stays a hard error.
//
// `..BACKPRESSURE_BYTES` is the honest-peer path. Out-of-order arrival is normal as soon as a
// transfer rides more than one QUIC stream (independent streams are exactly how QUIC avoids
// head-of-line blocking), so the buffer fills as a matter of course, not as an anomaly. The reader
// stops pulling frames above this mark and lets QUIC's own flow control hold the sender back, which
// is what backpressure is for. Failing a transfer because we successfully received data too early
// would be absurd -- the ceiling must never be the thing that stops a well-behaved peer, so it sits
// well above the mark at which we stop asking for more.
const ATTACHMENT_REORDER_BUFFER_MAX_BYTES: usize = 8 * 1024 * 1024;
const ATTACHMENT_REORDER_BACKPRESSURE_BYTES: usize = 4 * 1024 * 1024;
const CONTACT_APPROVAL_SIGNAL_PREFIX: &str = "__omni_contact_approved__::";
const CONTACT_REQUEST_SIGNAL_PREFIX: &str = "__omni_contact_request__::";
/// «Пересобери сессию»: приёмник отвергает наши конверты по разрыву счётчика и просит начать
/// заново. Едет тем же presence-пингом, что и сигналы запроса/одобрения контакта — ни нового кадра,
/// ни нового пути. См. docs/bugs/session-wedged-by-counter-gap.md.
const SESSION_RESYNC_SIGNAL_PREFIX: &str = "__omni_session_resync__::";
const PUBLIC_PROFILE_AVATAR_SYNC_MAX_ATTEMPTS: u8 = 3;
#[cfg(test)]
const VERIFICATION_CHALLENGE_RETRY_DELAYS_MS: [u64; 3] = [600, 1_200, 2_400];
#[cfg(not(test))]
const VERIFICATION_CHALLENGE_RETRY_DELAYS_MS: [u64; 3] = [1_000, 4_000, 10_000];
#[cfg(test)]
const CONTACT_APPROVAL_RETRY_DELAYS_MS: [u64; 8] = [
    120, 240, 480, 960, 1_920, 3_840, 7_680, 15_360,
];
#[cfg(not(test))]
const CONTACT_APPROVAL_RETRY_DELAYS_MS: [u64; 8] =
    [500, 1_000, 2_000, 4_000, 8_000, 15_000, 30_000, 60_000];
#[cfg(not(test))]
const PUBLIC_PROFILE_AVATAR_SYNC_RETRY_BACKOFF_MS: [i64; 2] = [3_000, 15_000];
#[cfg(test)]
const PUBLIC_PROFILE_AVATAR_SYNC_RETRY_BACKOFF_MS: [i64; 2] = [120, 240];

fn lock_session<'a>(session_lock: &'a Arc<Mutex<()>>) -> OmniResult<std::sync::MutexGuard<'a, ()>> {
    // Mirror of `lock_app_session_guard`: recover from poisoning instead of
    // failing forever. See docs/decisions/session-lock-recovery.md.
    Ok(session_lock.lock().unwrap_or_else(|poison| {
        SESSION_LOCK_POISON_RECOVERIES
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        poison.into_inner()
    }))
}

fn lock_runtime_storage<'a>(
    runtime_storage: &'a Arc<Mutex<Storage>>,
) -> OmniResult<std::sync::MutexGuard<'a, Storage>> {
    runtime_storage
        .lock()
        .map_err(|_| OmniError::Internal("runtime storage lock is poisoned".to_string()))
}

fn clone_active_transport(
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
) -> OmniResult<Option<TransportManager>> {
    transport_handle
        .lock()
        .map_err(|_| OmniError::Internal("transport handle lock is poisoned".to_string()))
        .map(|guard| guard.clone())
}

fn transport_contact_observation_from_bundle(
    remote_bundle: &PrekeyBundleV1,
    observed_at_unix_ms: i64,
) -> ContactObservationSnapshot {
    ContactObservationSnapshot {
        identity_key: Some(remote_bundle.account.account_id.clone()),
        observed_display_name: None,
        observed_at_unix_ms,
    }
}

fn transport_contact_needs_instance_repair(
    contact: &ContactSnapshot,
    receive_context: &ReceivedChatContext,
) -> bool {
    let remote_instance_id = receive_context.remote_instance_id.trim();
    if remote_instance_id.is_empty() {
        return false;
    }
    match contact.remote_instance_id.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(current) if current == remote_instance_id => false,
        // A placeholder (the device id stood in for the installation) of THIS device: repair it.
        Some(current)
            if receive_context
                .remote_prekey_bundle
                .as_ref()
                .is_some_and(|bundle| current == bundle.device.device_id) =>
        {
            true
        }
        // Any other installation is another device of the account: it joins the contact and
        // never moves it (docs/decisions/multi-device-equal-nodes.md).
        Some(_) => false,
    }
}

/// Is the sender a device of `contact`'s account, proven by a certificate chain
/// (docs/decisions/multi-device-equal-nodes.md)? Then it is one of the contact's devices — not a
/// stranger, and not a reinstall to move the contact onto: every installation is its own device.
fn receive_context_is_certified_device_of(
    contact: &ContactSnapshot,
    receive_context: &ReceivedChatContext,
) -> bool {
    let Some(bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return false;
    };
    bundle.device_certificate_chain_json.is_some()
        && contact
            .remote_account_id
            .as_ref()
            .is_some_and(|account| account.0 == bundle.account.account_id)
        // The chain vouches for exactly the bundle's keys. Not the whole bundle: one rebuilt from a
        // bare session bootstrap has no real signed prekey, and judging it by that made a new device
        // of an approved contact look like a stranger — a fresh contact request, the contact rebound
        // to the new device (live 2026-10-07).
        && bundle_certificate_vouches_for_its_keys(bundle).is_ok()
}

fn transport_remote_account_id(receive_context: &ReceivedChatContext) -> Option<AccountId> {
    receive_context
        .remote_prekey_bundle
        .as_ref()
        .map(|bundle| AccountId(bundle.account.account_id.clone()))
}

fn transport_receive_context_is_local_identity(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let remote_instance_id = receive_context.remote_instance_id.trim();
    if !remote_instance_id.is_empty()
        && storage.get_or_create_local_instance_id()? == remote_instance_id
    {
        return Ok(true);
    }

    let Some(remote_account_id) = transport_remote_account_id(receive_context) else {
        return Ok(false);
    };
    Ok(storage
        .get_local_account()?
        .is_some_and(|account| account.descriptor.account_id == remote_account_id))
}

fn transport_receive_context_is_local_instance(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let remote_instance_id = receive_context.remote_instance_id.trim();
    if remote_instance_id.is_empty() {
        return Ok(false);
    }
    Ok(storage.get_or_create_local_instance_id()? == remote_instance_id)
}

fn transport_contact_uses_local_identity(
    storage: &Storage,
    contact: &ContactSnapshot,
) -> OmniResult<bool> {
    let local_instance_id = storage.get_or_create_local_instance_id()?;
    if contact
        .remote_instance_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value == local_instance_id)
    {
        return Ok(true);
    }

    let Some(local_account) = storage.get_local_account()? else {
        return Ok(false);
    };
    Ok(contact
        .remote_account_id
        .as_ref()
        .is_some_and(|value| *value == local_account.descriptor.account_id))
}

fn transport_contact_matches_receive_context(
    contact: &ContactSnapshot,
    receive_context: &ReceivedChatContext,
) -> bool {
    if let Some(expected_account_id) = transport_remote_account_id(receive_context).as_ref() {
        if contact
            .remote_account_id
            .as_ref()
            .is_some_and(|value| value != expected_account_id)
        {
            return false;
        }
    }

    let expected_remote_instance_id = receive_context.remote_instance_id.trim();
    if expected_remote_instance_id.is_empty() {
        return true;
    }

    match contact.remote_instance_id.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(current) if current == expected_remote_instance_id => true,
        Some(_) if receive_context_is_certified_device_of(contact, receive_context) => true,
        Some(current) => receive_context
            .remote_prekey_bundle
            .as_ref()
            .is_some_and(|bundle| current == bundle.device.device_id),
    }
}

fn resolve_transport_contact_for_receive_context(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<Option<ContactSnapshot>> {
    if let Some(contact) = storage.resolve_contact_for_dialog(&receive_context.dialog_key)? {
        if transport_contact_matches_receive_context(&contact, receive_context)
            && !transport_contact_uses_local_identity(storage, &contact)?
        {
            return Ok(Some(contact));
        }
    }

    let Some(remote_account_id) = transport_remote_account_id(receive_context) else {
        return Ok(None);
    };
    let Some(contact) = storage.find_contact_by_remote_account_id(&remote_account_id)? else {
        return Ok(None);
    };
    if !transport_contact_matches_receive_context(&contact, receive_context)
        || transport_contact_uses_local_identity(storage, &contact)?
    {
        return Ok(None);
    }
    // Found by account, not by this device's dialog: another device of the contact. Remember its
    // dialog as one of the contact's, so everything it sends resolves to the same chat.
    if receive_context_is_certified_device_of(&contact, receive_context) {
        storage.link_dialog_to_contact(
            &contact.contact_id,
            &receive_context.dialog_key,
            false,
            current_unix_ms(),
        )?;
    }
    Ok(Some(contact))
}

pub(crate) fn clear_verification_challenges_for_profile(db_path: &str) {
    if let Ok(mut registry) = VERIFICATION_CHALLENGES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.remove(db_path);
    }
}

pub(crate) fn clear_public_profile_syncs_for_profile(db_path: &str) {
    let prefix = format!("{db_path}::");
    if let Ok(mut registry) = PUBLIC_PROFILE_SYNCS_SENT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.retain(|key, _| !key.starts_with(&prefix));
    }
}

pub(crate) fn clear_attachment_buffers_for_profile(db_path: &str) {
    let prefix = format!("{db_path}::");
    if let Ok(mut registry) = ATTACHMENT_REORDER_BUFFERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.retain(|key, _| !key.starts_with(&prefix));
    }
    if let Ok(mut registry) = ATTACHMENT_SEND_WORKERS_IN_FLIGHT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.retain(|key, _| !key.starts_with(&prefix));
    }
    // Drops the cached temp-file handles too, so a profile teardown never leaves one open.
    if let Ok(mut registry) = ATTACHMENT_RECEIVE_WRITERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.retain(|key, _| !key.starts_with(&prefix));
    }
}

/// Cached per-transfer receive state: the open temp file plus everything the chunk path used to
/// re-derive from the DB on every chunk. See `ATTACHMENT_RECEIVE_WRITERS`.
pub(crate) struct AttachmentReceiveWriter {
    pub(crate) file: omni_storage::AttachmentPartWriter,
    pub(crate) material: AttachmentContentCryptoMaterial,
    pub(crate) message_id: String,
    pub(crate) file_size_bytes: i64,
    /// Blocks here: checked and synced (the map in the DB when opened), or closed by this writer
    /// since. Chunks land in any order (stage 3: several sources) — a map, not a watermark.
    pub(crate) have: Vec<bool>,
    /// Leaves written of the blocks still open, a bit each.
    pub(crate) written_leaves: std::collections::BTreeMap<u64, u32>,
    /// Bytes here — whole blocks and the leaves written of open ones: the progress shown.
    pub(crate) progress_bytes: i64,
    /// When progress was last written to the DB, so the checkpoint cadence survives without
    /// re-reading the attachment row every chunk.
    pub(crate) checkpointed_at_unix_ms: i64,
    /// The file's identity — the root every block proof must lead to.
    pub(crate) content_id: omni_crypto::ContentHash,
    /// Leaf hashes of blocks whose proof arrived and whose chunks are still being written.
    pub(crate) block_leaves: std::collections::BTreeMap<u64, Vec<omni_crypto::ContentHash>>,
    /// Blocks closed since the last checkpoint: into the map once their data is synced.
    pub(crate) unsaved_blocks: Vec<(u64, omni_crypto::ContentHash)>,
}

impl AttachmentReceiveWriter {
    /// Every block of the file is here.
    pub(crate) fn is_complete(&self) -> bool {
        self.have.iter().all(|have| *have)
    }
}

/// The per-transfer writer, shared. One transfer has ONE temp file, ONE block map and ONE set of open
/// blocks, so every chunk of that transfer must go through the same state no matter which carrier
/// thread delivered it.
///
/// This used to be take-out-of-the-registry / put-it-back, which is only safe while exactly one
/// thread ever handles a transfer's chunks. It holds today because the bulk lane is a single
/// ordered stream read by a single reader — but the failure mode if that ever stops being true is
/// nasty and silent: the second thread takes `None`, opens the temp file a SECOND time, re-derives
/// its own block map, and the two writers then interleave seeks and writes
/// while each believes it owns the file. Sharing one mutex-guarded writer makes the invariant
/// structural instead of incidental, which is a prerequisite for spreading a transfer over several
/// streams (that is the point of doing it: independent streams are how QUIC avoids head-of-line
/// blocking, and today we decline the offer by using exactly one).
pub(crate) type SharedAttachmentReceiveWriter = Arc<Mutex<AttachmentReceiveWriter>>;

/// Fetch the shared writer for a transfer, building it with `open` on first use. The registry lock
/// is held only while looking the entry up; the caller locks the writer itself for its disk I/O.
pub(crate) fn attachment_receive_writer_or_open<F>(
    db_path: &str,
    transfer_id: &str,
    open: F,
) -> OmniResult<SharedAttachmentReceiveWriter>
where
    F: FnOnce() -> OmniResult<AttachmentReceiveWriter>,
{
    let key = format!("{db_path}::{transfer_id}");
    {
        let registry = ATTACHMENT_RECEIVE_WRITERS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| OmniError::Storage("attachment receive writer registry is poisoned".into()))?;
        if let Some(writer) = registry.get(&key) {
            return Ok(Arc::clone(writer));
        }
    }
    // Opened OUTSIDE the registry lock (it touches the DB and the filesystem), so a concurrent
    // caller may have inserted one meanwhile — in that case keep theirs and drop ours, or the two
    // would be exactly the duplicate-writer pair this type exists to prevent.
    let opened = Arc::new(Mutex::new(open()?));
    let mut registry = ATTACHMENT_RECEIVE_WRITERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| OmniError::Storage("attachment receive writer registry is poisoned".into()))?;
    if let Some(theirs) = registry.get(&key) {
        return Ok(Arc::clone(theirs));
    }
    // Nothing is cut: a block is either in the map (synced and checked) or written again whole —
    // whatever lies in the file outside the map is never trusted.
    registry.insert(key, Arc::clone(&opened));
    Ok(opened)
}

/// True while this transfer is holding so much out-of-order data that the reader should stop
/// pulling frames and let QUIC's flow control push back on the sender. See
/// `ATTACHMENT_REORDER_BACKPRESSURE_BYTES`.
pub(crate) fn attachment_reorder_buffer_wants_backpressure(db_path: &str, transfer_id: &str) -> bool {
    let key = attachment_reorder_registry_key(db_path, transfer_id);
    let Ok(registry) = lock_attachment_reorder_registry() else {
        return false;
    };
    registry
        .get(&key)
        .map(|chunks| chunks.values().map(|chunk| chunk.len()).sum::<usize>())
        .is_some_and(|buffered| buffered >= ATTACHMENT_REORDER_BACKPRESSURE_BYTES)
}

/// The cached writer of a transfer, if one is open — never opens one.
pub(crate) fn attachment_receive_writer(
    db_path: &str,
    transfer_id: &str,
) -> Option<Arc<Mutex<AttachmentReceiveWriter>>> {
    let registry = ATTACHMENT_RECEIVE_WRITERS.get_or_init(|| Mutex::new(HashMap::new())).lock().ok()?;
    registry.get(&format!("{db_path}::{transfer_id}")).map(Arc::clone)
}

/// Has this transfer's contiguous prefix reached the full file yet?
///
/// `None` means there is no cached writer to ask — either the transfer never started here or it was
/// already finalised, and in both cases the caller has nothing to wait for.
pub(crate) fn attachment_receive_is_complete(db_path: &str, transfer_id: &str) -> Option<bool> {
    let key = format!("{db_path}::{transfer_id}");
    let shared = {
        let registry = ATTACHMENT_RECEIVE_WRITERS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .ok()?;
        Arc::clone(registry.get(&key)?)
    };
    // The registry lock is released before taking the writer's, so this never contends with a chunk
    // write for longer than reading two integers.
    let writer = shared.lock().ok()?;
    Some(writer.is_complete())
}

/// Forget the cached writer so the next chunk rebuilds it from what is actually on disk. Used after
/// a write error, where the in-memory map of open blocks can no longer be trusted.
pub(crate) fn forget_attachment_receive_writer(db_path: &str, transfer_id: &str) {
    let key = format!("{db_path}::{transfer_id}");
    if let Ok(mut registry) = ATTACHMENT_RECEIVE_WRITERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.remove(&key);
    }
}

/// Close and forget the cached temp-file handle for a transfer. Must run before the temp file is
/// moved into place or removed (an open handle blocks that on Windows).
pub(crate) fn drop_attachment_receive_writer(db_path: &str, transfer_id: &str) {
    forget_attachment_receive_writer(db_path, transfer_id);
}

fn inbound_bootstrap_registry_key(db_path: &str, account_id: &str, device_id: &str) -> String {
    format!("{db_path}::{account_id}::{device_id}")
}

/// Newest bootstrap timestamp seen from this peer, in the PEER's clock. `None` until one is recorded.
pub(crate) fn last_inbound_bootstrap_created_at(
    db_path: &str,
    account_id: &str,
    device_id: &str,
) -> Option<i64> {
    let key = inbound_bootstrap_registry_key(db_path, account_id, device_id);
    LAST_INBOUND_BOOTSTRAP_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()?
        .get(&key)
        .copied()
}

/// Record a bootstrap we have now seen from this peer (whether we accepted it or ignored it as a
/// collision echo) so a later re-delivery of the SAME one is recognised.
pub(crate) fn record_inbound_bootstrap_created_at(
    db_path: &str,
    account_id: &str,
    device_id: &str,
    created_at_unix_ms: i64,
) {
    let key = inbound_bootstrap_registry_key(db_path, account_id, device_id);
    if let Ok(mut registry) = LAST_INBOUND_BOOTSTRAP_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        let newest = registry.entry(key).or_insert(created_at_unix_ms);
        *newest = (*newest).max(created_at_unix_ms);
    }
}

pub(crate) fn clear_inbound_bootstrap_marks_for_profile(db_path: &str) {
    let prefix = format!("{db_path}::");
    if let Ok(mut registry) = LAST_INBOUND_BOOTSTRAP_AT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.retain(|key, _| !key.starts_with(&prefix));
    }
}

/// Reserve an in-flight slot for an attachment send worker. Returns `true` if reserved (the caller
/// must spawn the worker and `release_attachment_send_worker` when it exits), `None` if a worker
/// for this `{db_path, transfer_id}` is already running — a duplicate download request to drop.
/// A running worker whose write is stuck (its peer went away and came back) is superseded instead:
/// it stops, and the new request is served now rather than after the dead stream times out.
pub(crate) fn reserve_attachment_send_worker(db_path: &str, transfer_id: &str) -> Option<Arc<AttachmentSendWorker>> {
    let key = format!("{db_path}::{transfer_id}");
    let worker = Arc::new(AttachmentSendWorker::default());
    let Ok(mut registry) = ATTACHMENT_SEND_WORKERS_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())).lock() else {
        // Poisoned lock: fail open (allow the send) rather than wedge all future transfers.
        return Some(worker);
    };
    if let Some(running) = registry.get(&key) {
        if !running.stuck(current_unix_ms()) {
            return None;
        }
        running.superseded.store(true, std::sync::atomic::Ordering::Relaxed);
        omni_app_trace_log(
            "attachment/send-worker",
            &format!("transfer={transfer_id} its write is stuck: superseded by the new request"),
        );
    }
    registry.insert(key, Arc::clone(&worker));
    Some(worker)
}

/// Release the in-flight slot once a send worker has finished (success or failure), so a later
/// legitimate resume of the same transfer can start a fresh worker.
/// A superseded worker leaves the slot of the one that took over alone.
pub(crate) fn release_attachment_send_worker(db_path: &str, transfer_id: &str, worker: &Arc<AttachmentSendWorker>) {
    let key = format!("{db_path}::{transfer_id}");
    if let Ok(mut registry) = ATTACHMENT_SEND_WORKERS_IN_FLIGHT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        if registry.get(&key).is_some_and(|running| Arc::ptr_eq(running, worker)) {
            registry.remove(&key);
        }
    }
}

// TM-008 (2026-05-21): admit a new auto-created Pending contact under
// the sliding-window rate limit. Returns `true` if the creation may
// proceed (and records the timestamp), `false` if the profile has
// already hit `TM008_PENDING_RATE_LIMIT` new pending contacts in the
// preceding `TM008_PENDING_RATE_WINDOW_MS`. Always prunes timestamps
// older than the window on access so the registry stays bounded.
// See `docs/decisions/tm-008-pending-contact-rate-limit.md`.
pub(crate) fn record_pending_contact_creation_under_rate_limit(
    db_path: &str,
    now_unix_ms: i64,
) -> bool {
    let Ok(mut registry) = PENDING_CONTACT_RATE_LIMITS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    else {
        // Poisoned mutex — fail open. A LOW-severity rate limit must
        // never block legitimate work because of an unrelated panic.
        return true;
    };
    let window_start = now_unix_ms.saturating_sub(TM008_PENDING_RATE_WINDOW_MS);
    let entry = registry
        .entry(db_path.to_string())
        .or_default();
    while entry.front().is_some_and(|ts| *ts < window_start) {
        entry.pop_front();
    }
    if entry.len() >= TM008_PENDING_RATE_LIMIT {
        return false;
    }
    entry.push_back(now_unix_ms);
    true
}

pub(crate) fn clear_pending_contact_rate_limits_for_profile(db_path: &str) {
    if let Ok(mut registry) = PENDING_CONTACT_RATE_LIMITS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        registry.remove(db_path);
    }
}

/// A core event surfaced for the daemon's event plane (push, not poll). One
/// registry, one drain — each KIND is a variant, so adding a kind (call signal, …)
/// doesn't add a parallel registry. See docs/decisions/daemon-ipc-phase2.md.
#[derive(Clone, Debug)]
pub enum CoreEvent {
    /// A message arrived and was stored.
    InboundMessage {
        dialog_key: String,
        message_id: String,
        body: String,
    },
    /// A mesh link to `peer` came up (`up = true`) or went down — presence.
    LinkState { peer: String, up: bool },
    /// An incoming call is ringing (a peer's CallInvite was accepted into a ringing
    /// runtime) — the UI should ring with Accept/Decline.
    IncomingCall { dialog_key: String, call_id: String },
    /// A group's membership, name or our invitations changed — the UI re-reads that group.
    GroupChanged { group_id: String },
}

fn record_core_event(db_path: &str, event: CoreEvent) {
    if let Ok(mut map) = CORE_EVENTS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        let queue = map.entry(db_path.to_string()).or_default();
        if queue.len() >= INBOUND_EVENTS_CAP {
            queue.pop_front();
        }
        queue.push_back(event);
    }
}

pub(crate) fn record_inbound_message_event(
    db_path: &str,
    dialog_key: &str,
    message_id: &str,
    body: &str,
) {
    record_core_event(
        db_path,
        CoreEvent::InboundMessage {
            dialog_key: dialog_key.to_string(),
            message_id: message_id.to_string(),
            body: body.to_string(),
        },
    );
}

pub(crate) fn record_link_state_event(db_path: &str, peer: &str, up: bool) {
    record_core_event(
        db_path,
        CoreEvent::LinkState {
            peer: peer.to_string(),
            up,
        },
    );
}

pub(crate) fn record_incoming_call_event(db_path: &str, dialog_key: &str, call_id: &str) {
    record_core_event(
        db_path,
        CoreEvent::IncomingCall {
            dialog_key: dialog_key.to_string(),
            call_id: call_id.to_string(),
        },
    );
}

pub(crate) fn record_group_changed_event(db_path: &str, group_id: &str) {
    record_core_event(db_path, CoreEvent::GroupChanged { group_id: group_id.to_string() });
}

pub(crate) fn drain_core_events(db_path: &str) -> Vec<CoreEvent> {
    CORE_EVENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|mut map| map.get_mut(db_path).map(|queue| queue.drain(..).collect()))
        .unwrap_or_default()
}

pub(crate) fn clear_core_events_for_profile(db_path: &str) {
    if let Ok(mut map) = CORE_EVENTS.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        map.remove(db_path);
    }
}

fn is_contact_approval_signal_ping_id(ping_id: &str) -> bool {
    ping_id.trim().starts_with(CONTACT_APPROVAL_SIGNAL_PREFIX)
}

fn is_contact_request_signal_ping_id(ping_id: &str) -> bool {
    ping_id.trim().starts_with(CONTACT_REQUEST_SIGNAL_PREFIX)
}

fn is_session_resync_signal_ping_id(ping_id: &str) -> bool {
    ping_id.trim().starts_with(SESSION_RESYNC_SIGNAL_PREFIX)
}

#[cfg(test)]
mod attachment_send_worker_dedup_tests {
    // Guards the fix for docs/bugs/attachment-duplicate-send-worker.md: a duplicated download
    // request must not start a second sender while one is in flight (that sends the file twice).
    #[test]
    fn reserve_dedupes_until_released() {
        let db = "test-db::attachment-send-dedup";
        let tid = "transfer-dedup-1";
        // First reservation for a transfer succeeds.
        let first = super::reserve_attachment_send_worker(db, tid).unwrap();
        // A concurrent duplicate (e.g. the same request re-delivered over a second carrier) is
        // rejected while the first worker is still in flight - and while its write is blocked
        // briefly (back-pressure on a live link).
        assert!(super::reserve_attachment_send_worker(db, tid).is_none());
        first.write_started();
        assert!(super::reserve_attachment_send_worker(db, tid).is_none());
        first.write_done();
        // A different transfer is independent.
        let other = super::reserve_attachment_send_worker(db, "transfer-dedup-2").unwrap();
        // After the worker finishes, a later legitimate resume can reserve the slot again.
        super::release_attachment_send_worker(db, tid, &first);
        let again = super::reserve_attachment_send_worker(db, tid).unwrap();
        // Its write stuck past the limit (the peer went away): the next request takes over, and the
        // stuck worker, released late, does not free the new one's slot.
        again.write_blocked_since_ms.store(
            super::current_unix_ms() - super::ATTACHMENT_SEND_WORKER_STUCK_AFTER_MS,
            std::sync::atomic::Ordering::Relaxed,
        );
        let fresh = super::reserve_attachment_send_worker(db, tid).expect("a stuck worker is superseded");
        assert!(again.superseded() && !fresh.superseded());
        super::release_attachment_send_worker(db, tid, &again);
        assert!(super::reserve_attachment_send_worker(db, tid).is_none(), "the new worker still holds it");
        super::release_attachment_send_worker(db, tid, &fresh);
        super::release_attachment_send_worker(db, "transfer-dedup-2", &other);
    }
}
