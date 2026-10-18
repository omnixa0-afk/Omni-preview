use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use omni_crypto::{
    bootstrap_initiator_session, bootstrap_responder_session, device_public_key_from_wire,
    device_public_key_to_wire, ephemeral_public_key_from_wire, ephemeral_public_key_to_wire,
    establish_session, identity_public_key_from_wire, identity_public_key_to_wire,
    identity_signing_public_key_from_wire, identity_signing_public_key_to_wire,
    prekey_public_key_from_wire, prekey_public_key_to_wire, signature_from_wire, signature_to_wire,
    CRYPTO_SUITE_VERSION_CURRENT,
};
use omni_identity::{
    AccountExportManifestV1, AccountId, AddContactInput, ChatHeaderContactSummary,
    ContactCardSnapshot, ContactChatTarget, ContactId, ContactObservationSnapshot,
    ContactRequestDirection, ContactSnapshot, DangerousActionPreview, DatabaseBootstrapSummary,
    DeviceId, LocalAccountSnapshot, LocalDeviceSnapshot, LocalProfileFootprintSummary,
    LocalProfileId, LocalProfileState, LocalProfileSummary, PasswordProtectionSummary,
    ProfileGateSummary, ProfileLifecycleDomainError, ProfileLifecycleErrorCode,
    ProfileLifecycleSummary, RecoveryInputSummary, RecoveryPhraseV2, SavedProfileList,
    VerificationMethod,
};
use omni_messaging::{
    build_encrypted_attachment_offer_envelope, build_encrypted_delivery_receipt_envelope,
    build_encrypted_logical_event_envelope, build_encrypted_text_envelope, decrypt_wire_payload,
    warning_from_trust, ChatSecuritySummary, DialogDeltaCursor, DialogDeltaSnapshot,
    DialogMessageRefreshSummary, MessageEncryptionState, MessageSecurityDetails,
    SecurityPayloadKind,
};
use omni_protocol::{
    importable_contact_payload_from_verification, validate_contact_verification_payload_v1, validate_prekey_bundle_v1,
    validate_session_bootstrap_message_v1, validate_wire_envelope_v1, AccountCryptoDescriptorV1,
    AccountPublicDescriptorV1, ContactVerificationPayloadV1, DeviceCryptoDescriptorV1,
    DevicePublicDescriptorV1,
    ImportableContactPayloadV1, MyVerificationPayloadV1, OneTimePrekeyDescriptorV1, PrekeyBundleV1,
    SessionBootstrapMessageV1, ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
    DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1, PREKEY_BUNDLE_VERSION_V1,
    PUBLIC_PROFILE_AVATAR_SYNC_VERSION_V1,
    PUBLIC_PROFILE_DESCRIPTOR_SYNC_VERSION_V1, SESSION_BOOTSTRAP_MESSAGE_VERSION_V1,
};
use omni_storage::{AttachmentRuntimeInfo, Storage};
use omni_transport::{
    AttachmentTransferChunkV2, AttachmentTransferCompleteV2,
    AttachmentTransferFrameV2, AttachmentTransferStartV2,
    AttachmentTransferStream, DialogConnectionSnapshot, ReceivedChatContext,
    ReceivedTransportEvent, TransportManager, WireEnvelopeDelivery,
};
use omni_types::{
    dialog_key_from_instance_id, instance_id_from_dialog_key,
    normalize_dialog_key,
    CallDeclineReason, CallDirection,
    CallInboxSnapshot, CallNetworkQuality, CallOutcome, CallState, CallSummary, ChatMessageRecord, DeliveryReceiptV1, ListMessagesPageRequest,
    ListMessagesPageResponse, ListMessagesResponse, MessageStatus,
    OmniError, OmniResult, OutboundQueueSnapshot,
    QueueDiagnosticsSnapshot, QueueFailureKind, RealtimeMediaCapabilitiesV1,
    RealtimeMediaNegotiationSnapshot, RealtimeMediaNegotiationStatus,
    RealtimeMediaSelectionV1, RealtimeVideoStreamV1,
    RealtimeVideoPixelFormat, RealtimeVideoProfileCapabilityV1, RealtimeVideoSelectionV1,
    SendTextRequest, TransportCommandResult, TransportMode, TransportSnapshot,
    TransportState,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);
#[cfg(not(test))]
const RETRY_DELAY_MS: i64 = 1_000;
#[cfg(test)]
const RETRY_DELAY_MS: i64 = 150;
#[cfg(not(test))]
const RECEIPT_WAIT_BEFORE_RETRY_MS: i64 = 12_000;
#[cfg(test)]
const RECEIPT_WAIT_BEFORE_RETRY_MS: i64 = 4_500;
#[cfg(not(test))]
const SESSION_BOOTSTRAP_REISSUE_AFTER_MS: i64 = 2_000;
#[cfg(test)]
const SESSION_BOOTSTRAP_REISSUE_AFTER_MS: i64 = 500;
// Fast-retry budget. Beyond this we DON'T bury the message — we keep retrying with an
// exponential backoff capped at `MAX_RETRY_BACKOFF_MS`, because durable store-and-forward
// backs eventual delivery and a message to a contact must not be lost on a transient outage
// (Signal/Reticulum model). See docs/decisions/delivery-reliability-model.md.
const MAX_RETRY_ATTEMPTS: i64 = 3;
/// Increment 3 of the delivery-reliability model: once a Sent-but-unconfirmed message has gone this
/// many send attempts, ALSO back it with durable store-and-forward — a mesh `Sent` only means
/// "handed to QUIC", and over a relay whose route to the peer isn't (re)established yet (e.g. right
/// after a relay restart) it silently black-holes while the live ack-timeout retry can stall. The
/// durable reconcile then guarantees eventual delivery; the recipient dedups by message id if both
/// paths land. Fired exactly ONCE per message (== this, not ≥) so the bounded durable store isn't
/// re-filled on every retry. See docs/decisions/delivery-reliability-model.md (Increment 3).
const DURABLE_BACKSTOP_AFTER_ATTEMPTS: i64 = MAX_RETRY_ATTEMPTS;
/// Phantom-link escalation (2026-07-20). If a message is STILL unconfirmed this many attempts after
/// the durable backstop already fired, the mesh Link is almost certainly a PHANTOM — `has_link`
/// reports it up but the peer receives nothing (a mobile carrier killed the UDP flow). While that
/// dead-but-registered entry persists, BOTH the live send and the durable reconcile pour into the
/// void and the auto-connect/rendezvous re-dial skips (it thinks we're linked). At this threshold we
/// drop the Link once, forcing a fresh dial over which the durable frame finally reconciles. Set
/// comfortably past the durable backstop so a merely-slow (not phantom) link isn't dropped
/// needlessly; a genuinely-live link just coalesces back on the next dial. Fired ONCE (== this).
const DROP_PHANTOM_LINK_AFTER_ATTEMPTS: i64 = DURABLE_BACKSTOP_AFTER_ATTEMPTS + 4;
#[cfg(not(test))]
const MAX_RETRY_BACKOFF_MS: i64 = 60_000;
#[cfg(test)]
const MAX_RETRY_BACKOFF_MS: i64 = 2_000;
#[cfg(not(test))]
const QUEUE_WORKER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
const QUEUE_WORKER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
/// Name a failed transport receive-dispatch instead of dropping it on the floor.
///
/// Every branch of the receive callback used to swallow its handler's `Err` with `let _ = …`, so a
/// chunk that failed to persist, an envelope that failed to decrypt, or a media datagram that was
/// refused vanished with NO trace — the live hunt for a 0-byte `.part` that kept receiving chunks
/// had literally nothing to read. The handlers stay best-effort (one bad frame must never kill the
/// callback), but the failure is now NAMED in the trace.
fn trace_receive_dispatch<T>(event: &str, result: OmniResult<T>) {
    if let Err(error) = result {
        omni_app_trace_log("recv/dispatch-error", &format!("event={event} error={error}"));
    }
}

// The pre-mesh discovery sidecars (`node-mode.json`, `discovery-*.json`) are no longer carried: the
// discovery they configured is gone (docs/decisions/no-friend-tier.md, part 2). An old bundle that
// still has them restores fine — they are simply not copied.
const BACKUP_BUNDLE_SIDECAR_FILE_NAMES: [&str; 1] = ["transport-mode.json"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInitRequest {
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResponse {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatThreadProjection {
    pub contact: ContactSnapshot,
    pub last_message: Option<ChatMessageRecord>,
    /// Incoming lines this device has not read (omni-storage `storage_read_marks.rs`).
    #[serde(default)]
    pub unread_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBackupInspection {
    pub manifest: AccountExportManifestV1,
    pub inspection_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SanitizedLocalAccountSnapshot {
    pub descriptor: omni_identity::PublicAccountDescriptor,
    pub current_device_id: DeviceId,
    pub restored_from_recovery: bool,
    pub account_representation_version: u16,
    pub public_summary: Option<omni_identity::PublicAccountSummary>,
    pub local_profile_summary: Option<omni_identity::LocalAccountProfileSummary>,
    pub public_avatar_hash_hex: Option<String>,
    pub public_avatar_mime_type: Option<String>,
    pub public_avatar_image_path: Option<String>,
    pub public_profile_updated_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateAccountResponse {
    pub account: SanitizedLocalAccountSnapshot,
    pub initial_recovery_phrase_v2: RecoveryPhraseV2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AccountBackupInspectionTokenV1 {
    version: u16,
    canonical_backup_db_path: String,
    backup_db_size_bytes: u64,
    backup_db_last_modified_unix_ms: i64,
    backup_db_sha256_hex: String,
    bundle_fingerprint_sha256_hex: String,
    bundle_files: Vec<BackupBundleFileFingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BackupBundleFileFingerprint {
    relative_path: String,
    size_bytes: u64,
    last_modified_unix_ms: i64,
    sha256_hex: String,
}

pub struct OmniApp {
    storage: Storage,
    transport: TransportManager,
    transport_handle: Arc<Mutex<Option<TransportManager>>>,
    /// The SHARED storage handle the transport receive callbacks use. Held so inbound paths that
    /// are wired later (the mesh bulk/attachment plane) serialize on the SAME lock as the legacy
    /// receive dispatch instead of racing it through a second connection.
    runtime_event_storage: Arc<Mutex<Storage>>,
    realtime_media_inbound: Arc<RealtimeMediaInboundSinks>,
    /// The shell's injector for remote screen control (docs/decisions/remote-screen-control.md).
    screen_control_sink: Arc<app_screen_control::ScreenControlSinkSlot>,
    /// Mesh overlay transport (omni-network via the sync facade). Started on
    /// demand; carries realtime media over a verified per-peer QUIC Link
    /// instead of the legacy signaling-driven session. См.
    /// docs/decisions/mesh-transport-integration-seam.md.
    /// Outer `Arc` so the mesh `PeerCarrier` (installed into the transport) can
    /// share the same handle. The inner `Arc<MeshRuntime>` lets callers clone the
    /// runtime out under a brief lock and then run the (blocking) dial/send off
    /// the lock — so a slow dial never stalls the live media/wire send path that
    /// needs the same handle.
    mesh: Arc<Mutex<Option<Arc<MeshRuntime>>>>,
    /// dialog_key ↔ peer NodeAddress for active mesh links (both directions).
    mesh_dialogs: Arc<Mutex<MeshDialogMap>>,
    /// Per-peer `ReceivedChatContext` built from the mesh SessionHello, used to
    /// dispatch that peer's subsequent inbound wire frames.
    mesh_node_ctx: Arc<Mutex<std::collections::HashMap<MeshNodeAddress, ReceivedChatContext>>>,
    /// Peers we dialed locally over the mesh (vs accepted) — drives
    /// `initiated_locally` when registering the connection from their hello.
    mesh_locally_dialed: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    /// NodeAddresses imported from a friend's invite that we want to *pair*
    /// with over the mesh (we have their pubkey but not yet a contact). The
    /// auto-connect worker dials these; the on_wire SessionHello handler then
    /// creates the outgoing-pending contact + sends the contact request and
    /// drops the entry. See docs/decisions/pairing-over-mesh-adoption.md.
    mesh_pairing_targets: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    /// Outstanding FIRST-contact bootstrap rendezvous, keyed by the invite's nonce hex. A
    /// fresh invite (inviter) or an imported one (importer) seeds the same blinded bootstrap
    /// topic; the auto-connect worker publishes our endpoints there + dials the other side
    /// until the link forms (importer entry) or the invite expires. In-memory for v1 — the
    /// inviter keeps the app open until the friend opens the invite; persistence is a follow-up
    /// (TODO 2026-09: survive restart within the invite TTL). See invite-bootstrap-rendezvous.md.
    mesh_bootstrap_invites: Arc<Mutex<std::collections::HashMap<String, MeshBootstrapInvite>>>,
    /// Stop signal for the mesh auto-connect worker (spawned by `start_mesh`,
    /// torn down by `stop_mesh`). Dropping the `Sender` makes the worker's
    /// `recv_timeout` return `Disconnected`, so it exits promptly.
    mesh_autoconnect_stop: Mutex<Option<Sender<()>>>,
    /// How the running mesh was started — `(autoconnect, prefer_lan)` of `start_mesh_internal` — so
    /// a carrier change that the running mesh cannot take restarts it the same way.
    mesh_start_mode: Mutex<Option<(bool, bool)>>,
    queue_runtime: QueueRuntime,
    session_lock: Arc<Mutex<()>>,
    /// Wave 2c concurrency fix (2026-05-16): serialise all FFI entry points
    /// so concurrent `&self`-borrowed Storage calls never race the rusqlite
    /// RefCell. parking_lot ReentrantMutex allows the same thread to
    /// re-enter (some Rust paths do nested OmniApp method calls), while
    /// any other ThreadPool worker waits at the FFI boundary. Held by the
    /// `HandleGuard` returned from `require_handle` (omni-ffi).
    pub serializer: Arc<parking_lot::ReentrantMutex<()>>,
    /// One running Omni per profile folder. Last field: released after everything above is gone.
    _profile_lock: app_profile_folder_lock::ProfileFolderLock,
}

struct QueueRuntime {
    stop_tx: Sender<()>,
    join_handle: Option<JoinHandle<()>>,
}

struct StagedBackupBundleImport {
    staging_directory: PathBuf,
    staged_db_path: PathBuf,
}

impl OmniApp {
    /// Device-level state with no profile to own it is garbage, and not the harmless kind: the
    /// transport identity would hand the NEXT profile the mesh node identity of whoever was here
    /// before, so peers see the same node across a "delete everything". Deleting the last profile
    /// clears it (`wipe_device_traces_if_no_profiles_remain`), but that only covers deletes made by
    /// a build that has that code — it does nothing for a footprint left by an older build, an
    /// interrupted delete, or a hand-copied folder. So the invariant is enforced where it can't be
    /// dodged: at EVERY start. No profiles ⇒ no device state.
    ///
    /// Ordering matters twice: this must run BEFORE the settings sidecars are read (they are part of
    /// what it wipes) and BEFORE the transport identity is created below — otherwise it would delete
    /// the key just minted and the process would run as a node no file backs.
    ///
    /// Best-effort: a locked leftover must not make the app refuse to start.
    fn wipe_orphaned_device_traces(storage: &Storage, db_path: &str) {
        match storage.list_local_profiles() {
            Ok(list) if list.profiles.is_empty() => {
                if let Err(error) = Storage::delete_device_traces_keeping_open_database(db_path) {
                    omni_app_trace_log(
                        "startup/device-traces",
                        &format!("could not wipe orphaned device state: {error}"),
                    );
                }
            }
            Ok(_) => {}
            Err(error) => omni_app_trace_log(
                "startup/device-traces",
                &format!("could not read the profile registry: {error}"),
            ),
        }
    }

    /// Attachments written before at-rest encryption are sealed off the start path: a big store takes
    /// a while, and every reader opens plain and sealed files alike meanwhile
    /// (docs/decisions/db-at-rest-encryption.md, «Вложения»).
    fn seal_plain_attachment_files_in_background(db_path: &str) {
        let db_path = db_path.to_string();
        let _ = std::thread::Builder::new()
            .name("omni-seal-attachments".to_string())
            .spawn(move || {
                let result = Storage::open(&db_path).and_then(|storage| storage.seal_plain_attachment_files());
                match result {
                    Ok(0) => {}
                    Ok(sealed) => omni_app_trace_log("storage/seal", &format!("sealed {sealed} plain attachment files")),
                    Err(error) => omni_app_trace_log("storage/seal", &format!("sealing plain attachment files failed: {error}")),
                }
            });
    }

    pub fn init(request: AppInitRequest) -> OmniResult<Self> {
        let db_path = request.db_path.trim();
        if db_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "db_path must not be empty".to_string(),
            ));
        }

        let profile_lock = app_profile_folder_lock::acquire(std::path::Path::new(db_path))?;
        let storage = Storage::open(db_path)?;
        let bootstrap_summary = storage.inspect_database_bootstrap_summary()?;
        if !bootstrap_summary.supported {
            return Err(unsupported_bootstrap_error(&bootstrap_summary));
        }
        storage.migrate()?;
        storage.healthcheck()?;
        storage.ensure_local_crypto_foundation(current_unix_ms())?;
        Self::wipe_orphaned_device_traces(&storage, db_path);
        Self::seal_plain_attachment_files_in_background(db_path);
        clear_media_open_copies(std::path::Path::new(db_path));

        let local_instance_id = storage.get_or_create_local_instance_id()?;
        let local_device_name = local_device_name();
        let transport_mode = load_transport_mode(db_path)?;
        let runtime_event_storage = Arc::new(Mutex::new(Storage::open(db_path)?));
        runtime_event_storage
            .lock()
            .map_err(|_| OmniError::Internal("runtime event storage lock is poisoned".to_string()))?
            .healthcheck()?;
        let transport_handle = Arc::new(Mutex::new(None::<TransportManager>));
        let transport_handle_for_callback = Arc::clone(&transport_handle);
        let runtime_event_storage_for_callback = Arc::clone(&runtime_event_storage);
        let realtime_media_inbound = Arc::new(RealtimeMediaInboundSinks::default());
        let realtime_media_inbound_for_callback = Arc::clone(&realtime_media_inbound);
        let runtime_event_db_path_for_callback = db_path.to_string();
        let session_lock = Arc::new(Mutex::new(()));
        let session_lock_for_callback = Arc::clone(&session_lock);
        let transport = TransportManager::new_with_mode(
            local_instance_id,
            local_device_name,
            transport_mode,
            move |event| match event {
                ReceivedTransportEvent::SessionActivated(receive_context) => {
                    trace_receive_dispatch(
                        "session_activated",
                        handle_session_activated(
                            &runtime_event_storage_for_callback,
                            &transport_handle_for_callback,
                            &session_lock_for_callback,
                            &receive_context,
                        ),
                    );
                }
                ReceivedTransportEvent::SessionClosed(receive_context) => {
                    clear_realtime_media_runtime_for_context(
                        &runtime_event_db_path_for_callback,
                        &receive_context,
                    );
                    trace_receive_dispatch(
                        "session_closed",
                        handle_session_closed(
                            &runtime_event_storage_for_callback,
                            &transport_handle_for_callback,
                            &receive_context,
                        ),
                    );
                }
                ReceivedTransportEvent::WireEnvelope(envelope, receive_context) => {
                    trace_receive_dispatch(
                        "wire_envelope",
                        handle_incoming_wire_envelope(
                            &runtime_event_storage_for_callback,
                            &transport_handle_for_callback,
                            &session_lock_for_callback,
                            envelope,
                            &receive_context,
                        ),
                    );
                }
                ReceivedTransportEvent::AttachmentTransferFrame(frame, receive_context) => {
                    trace_receive_dispatch(
                        "attachment_transfer_frame",
                        handle_incoming_attachment_transfer_frame(
                            &runtime_event_storage_for_callback,
                            &transport_handle_for_callback,
                            &session_lock_for_callback,
                            frame,
                            &receive_context,
                        ),
                    );
                }
                ReceivedTransportEvent::AttachmentTransferStream(stream, receive_context) => {
                    trace_receive_dispatch(
                        "attachment_transfer_stream",
                        handle_incoming_attachment_transfer_stream(
                            &runtime_event_storage_for_callback,
                            &transport_handle_for_callback,
                            &session_lock_for_callback,
                            stream,
                            receive_context,
                        ),
                    );
                }
                ReceivedTransportEvent::RealtimeMediaDatagram(payload, receive_context) => {
                    trace_receive_dispatch(
                        "realtime_media_datagram",
                        push_incoming_realtime_media_datagram(
                            &realtime_media_inbound_for_callback,
                            &runtime_event_db_path_for_callback,
                            &receive_context,
                            &payload,
                        ),
                    );
                }
            },
        );
        *transport_handle
            .lock()
            .map_err(|_| OmniError::Internal("transport handle lock is poisoned".to_string()))? =
            Some(transport.clone());

        let (queue_stop_tx, queue_stop_rx) = mpsc::channel();
        let queue_db_path = db_path.to_string();
        let queue_transport = transport.clone();
        let queue_session_lock = Arc::clone(&session_lock);
        let queue_handle = thread::spawn(move || {
            run_queue_worker(
                queue_db_path,
                queue_transport,
                queue_stop_rx,
                queue_session_lock,
            );
        });

        let app = Self {
            storage,
            transport,
            transport_handle,
            runtime_event_storage,
            realtime_media_inbound,
            screen_control_sink: Arc::default(),
            mesh: Arc::new(Mutex::new(None)),
            mesh_dialogs: Arc::new(Mutex::new(MeshDialogMap::default())),
            mesh_node_ctx: Arc::new(Mutex::new(std::collections::HashMap::new())),
            mesh_locally_dialed: Arc::new(Mutex::new(std::collections::HashSet::new())),
            mesh_pairing_targets: Arc::new(Mutex::new(std::collections::HashSet::new())),
            mesh_bootstrap_invites: Arc::new(Mutex::new(std::collections::HashMap::new())),
            mesh_autoconnect_stop: Mutex::new(None),
            mesh_start_mode: Mutex::new(None),
            queue_runtime: QueueRuntime {
                stop_tx: queue_stop_tx,
                join_handle: Some(queue_handle),
            },
            session_lock,
            serializer: Arc::new(parking_lot::ReentrantMutex::new(())),
            _profile_lock: profile_lock,
        };
        app.sync_transport_local_identity()?;
        Ok(app)
    }

    pub fn inspect_database_bootstrap(
        request: AppInitRequest,
    ) -> OmniResult<DatabaseBootstrapSummary> {
        let db_path = request.db_path.trim();
        if db_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "db_path must not be empty".to_string(),
            ));
        }
        let storage = Storage::open(db_path)?;
        storage.inspect_database_bootstrap_summary()
    }

    /// Open a data folder from another machine with the recovery phrase of a profile in it, before
    /// `init`: the database key and that profile's root are sealed to this machine, so neither the
    /// next start nor that profile asks for the phrase again.
    pub fn unlock_database_with_mnemonic(request: AppInitRequest, mnemonic: &str) -> OmniResult<()> {
        let db_path = request.db_path.trim();
        if db_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "db_path must not be empty".to_string(),
            ));
        }
        Storage::unlock_database_with_mnemonic(db_path, mnemonic)?;
        Storage::open(db_path)?.adopt_profile_root_with_mnemonic(mnemonic)
    }

    pub fn export_current_database_backup(
        &self,
        destination_path: impl AsRef<str>,
    ) -> OmniResult<()> {
        self.storage
            .export_database_backup(destination_path.as_ref())
    }

    pub fn export_current_database_backup_bundle(
        &self,
        destination_path: impl AsRef<Path>,
    ) -> OmniResult<()> {
        let destination_path = destination_path.as_ref();
        ensure_distinct_paths(
            self.storage.db_path(),
            destination_path,
            "backup destination must not match the current database",
        )?;
        // `.omnibackup` — the encrypted file (Г); any other name — the plain copy the dev tools use.
        if destination_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("omnibackup"))
        {
            return write_encrypted_backup(&self.storage, destination_path);
        }
        self.storage.export_database_backup(destination_path)?;
        export_backup_bundle_contents(self.storage.db_path(), destination_path)
    }

    pub fn export_database_backup_bundle_at_path(
        db_path: impl AsRef<str>,
        destination_path: impl AsRef<Path>,
    ) -> OmniResult<()> {
        let db_path = db_path.as_ref().trim();
        if db_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "db_path must not be empty".to_string(),
            ));
        }

        let source_db_path = Path::new(db_path);
        if !source_db_path.exists() {
            return Ok(());
        }

        let destination_path = destination_path.as_ref();
        ensure_distinct_paths(
            source_db_path,
            destination_path,
            "backup destination must not match the current database",
        )?;
        let storage = Storage::open(source_db_path)?;
        storage.export_database_backup(destination_path)?;
        export_backup_bundle_contents(source_db_path, destination_path)
    }

    pub fn import_database_backup_bundle(
        db_path: impl AsRef<str>,
        backup_db_path: impl AsRef<Path>,
        inspection_token: impl AsRef<str>,
    ) -> OmniResult<()> {
        let destination_path = Path::new(db_path.as_ref().trim());
        if db_path.as_ref().trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "db_path must not be empty".to_string(),
            ));
        }

        let backup_db_path = backup_db_path.as_ref();
        if !backup_db_path.exists() {
            return Err(OmniError::InvalidInput(format!(
                "backup database '{}' was not found",
                backup_db_path.display()
            )));
        }
        ensure_distinct_paths(
            destination_path,
            backup_db_path,
            "backup path must not match the destination database",
        )?;
        validate_account_backup_inspection_token(backup_db_path, inspection_token.as_ref())?;

        // An encrypted file was opened by its inspection (it had the phrase); restore from that copy.
        let opened = if is_encrypted_backup(backup_db_path) {
            Some(take_opened_backup(backup_db_path).ok_or_else(|| {
                OmniError::InvalidInput("check the backup file with the phrase first".to_string())
            })?)
        } else {
            None
        };
        let source_db_path = opened.as_deref().unwrap_or(backup_db_path);
        let _opened_cleanup = opened
            .as_deref()
            .and_then(Path::parent)
            .map(|dir| RemoveOnDrop(dir.to_path_buf()));

        let staged_import = create_staged_backup_bundle_import(destination_path, source_db_path)?;
        let import_result = (|| -> OmniResult<()> {
            validate_staged_backup_bundle_import(&staged_import.staged_db_path)?;
            Storage::delete_local_data_at_path(destination_path)?;
            copy_local_storage_bundle(&staged_import.staged_db_path, destination_path)
        })();
        let cleanup_result = cleanup_staged_backup_bundle_import(&staged_import);
        import_result?;
        cleanup_result
    }

    pub fn inspect_account_backup_manifest(
        backup_db_path: impl AsRef<str>,
        mnemonic: impl AsRef<str>,
    ) -> OmniResult<AccountBackupInspection> {
        // An encrypted file opens with the phrase into a staging copy, inspected like a plain one.
        let opened = open_encrypted_backup(Path::new(backup_db_path.as_ref()), mnemonic.as_ref())?;
        let inspected = opened
            .clone()
            .unwrap_or_else(|| PathBuf::from(backup_db_path.as_ref()));
        let manifest = Storage::inspect_account_backup_manifest(&inspected, mnemonic.as_ref());
        if manifest.is_err() {
            if let Some(dir) = opened.as_deref().and_then(Path::parent) {
                let _ = take_opened_backup(Path::new(backup_db_path.as_ref()));
                let _ = fs::remove_dir_all(dir);
            }
        }
        let manifest = manifest?;
        let inspection_token =
            build_account_backup_inspection_token(Path::new(backup_db_path.as_ref()))?;
        Ok(AccountBackupInspection {
            manifest,
            inspection_token,
        })
    }

    pub fn ping(&self) -> PingResponse {
        PingResponse {
            ok: true,
            message: "pong from rust core".to_string(),
        }
    }
}

impl QueueRuntime {
    fn shutdown(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl Drop for QueueRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Стоимость канала — платформенный факт от оболочки; решает выбор контроллера перегрузки.
/// Транзитом из `omni-transport`, чтобы FFI не тянул сетевые примитивы напрямую.
pub use omni_transport::{
    congestion_preference, congestion_preference_name, effective_congestion_control,
    parse_congestion_preference, set_congestion_preference, set_link_cost, CongestionChoice,
    CongestionPreference, LinkCost,
};

include!("app_realtime_media.rs");

include!("app_connection_trace.rs");

include!("app_mesh.rs");

include!("app_rendezvous.rs");

include!("app_discoverability.rs");

include!("app_dht_rendezvous.rs");

include!("app_calls.rs");

include!("app_backup_file.rs");

include!("app_media_files.rs");

mod app_profile_folder_lock;
mod app_attachment_preparation;
mod app_upload_slots;
mod app_download_scheduler;
pub use app_profile_folder_lock::PROFILE_ALREADY_OPEN;
mod app_groups;
mod app_group_attachments;
mod app_group_history;
mod app_group_branches;
mod app_group_rejoin;
mod app_name_discovery;
mod app_relay_directory;
mod app_pinned_relay;
pub use app_pinned_relay::{PinnedRelayStateV1, PinnedRelayV1};
mod app_publish_plans;
pub use app_publish_plans::{Publication, PublishBlockV1, PublishPlanV1, PublishPlanViewV1, PublishPlansV1};
pub use app_relay_directory::{PublicRelayRoleV1, PublicRelayV1, RelaySearchResultV1};
mod app_group_member_profiles;
pub use app_name_discovery::{FoundByNameV1, NameSearchResultV1};
pub use app_groups::{GroupInviteView, GroupMemberView, GroupMessageView, GroupSummary};

mod app_group_calls;
pub use app_group_calls::{GroupCallNotice, GroupCallSnapshot};

mod app_screen_control;
pub use app_screen_control::{CallScreenControlState, ScreenControlDelivery, ScreenControlSink};

mod app_remote_access;

mod app_helper_job;
mod app_nat_class;
mod app_peer_door;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransportModeSidecar {
    #[serde(default)]
    transport_mode: TransportMode,
}

fn load_transport_mode(db_path: &str) -> OmniResult<TransportMode> {
    let sidecar_path = transport_mode_sidecar_path(db_path);
    if !sidecar_path.exists() {
        return Ok(TransportMode::Quic);
    }

    let raw = std::fs::read_to_string(&sidecar_path).map_err(|error| {
        OmniError::Storage(format!(
            "failed to read transport mode sidecar '{}': {error}",
            sidecar_path.display()
        ))
    })?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(TransportMode::Quic);
    }

    if let Ok(sidecar) = serde_json::from_str::<TransportModeSidecar>(trimmed) {
        return Ok(sidecar.transport_mode);
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(transport_mode) = value
            .get("transport_mode")
            .and_then(|transport_mode| transport_mode.as_str())
        {
            return TransportMode::from_db_value(transport_mode);
        }
    }

    if let Ok(legacy_mode) = TransportMode::from_db_value(trimmed) {
        return Ok(legacy_mode);
    }

    Err(OmniError::InvalidInput(format!(
        "failed to parse transport mode sidecar '{}'",
        sidecar_path.display()
    )))
}

fn transport_mode_sidecar_path(db_path: &str) -> PathBuf {
    Path::new(db_path).with_file_name("transport-mode.json")
}

fn comparable_path_for_equality(path: &Path) -> OmniResult<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                OmniError::Storage(format!(
                    "failed to resolve current directory for '{}': {error}",
                    path.display()
                ))
            })?
            .join(path)
    };
    let normalized = fs::canonicalize(&absolute).unwrap_or(absolute);
    let mut comparable = normalized.to_string_lossy().to_string();
    #[cfg(windows)]
    comparable.make_ascii_lowercase();
    Ok(comparable)
}

fn paths_refer_to_same_location(left: &Path, right: &Path) -> OmniResult<bool> {
    Ok(comparable_path_for_equality(left)? == comparable_path_for_equality(right)?)
}

fn ensure_distinct_paths(left: &Path, right: &Path, message: &str) -> OmniResult<()> {
    if paths_refer_to_same_location(left, right)? {
        return Err(OmniError::InvalidInput(message.to_string()));
    }
    Ok(())
}

/// A per-profile setting file next to the profile database: `<db file name>.<suffix>` in the
/// same directory (`omni.db` when the path has no file name). One place for the rule — it used
/// to be copied into every setting that needed a file.
pub(crate) fn profile_sidecar_path(db_path: &str, suffix: &str) -> PathBuf {
    let path = Path::new(db_path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("omni.db");
    path.with_file_name(format!("{file_name}.{suffix}"))
}

fn export_backup_bundle_contents(source_db_path: &Path, backup_db_path: &Path) -> OmniResult<()> {
    // TM-006 follow-up (2026-05-21): the bundle sibling directory
    // (`<backup_db_path>.bundle/`) is about to be recursively
    // deleted and re-created. Without confinement a caller-supplied
    // `backup_db_path = C:\Windows\foo.db` would resolve to
    // `C:\Windows\foo.db.bundle/` being wiped + re-populated.
    // Validation-only: the actual ops keep the original path.
    // See docs/decisions/tm-006-destructive-path-confinement.md.
    Storage::confine_destructive_path_for_callers(backup_db_path)?;
    let bundle_directory = backup_bundle_directory(backup_db_path);
    delete_directory_if_exists(&bundle_directory)?;

    let mut bundle_created = false;
    let source_attachments_directory = attachments_directory_for_db_path(source_db_path)?;
    if source_attachments_directory.exists() {
        copy_directory_recursive(
            &source_attachments_directory,
            &bundle_directory.join("attachments"),
        )?;
        bundle_created = true;
    }

    for (source_sidecar_path, bundle_file_name) in backup_bundle_sidecar_sources(source_db_path)? {
        if !source_sidecar_path.exists() {
            continue;
        }

        copy_file_with_parent(
            &source_sidecar_path,
            &bundle_directory.join(bundle_file_name),
        )?;
        bundle_created = true;
    }

    if !bundle_created {
        delete_directory_if_exists(&bundle_directory)?;
    }

    Ok(())
}

fn import_backup_bundle_contents(
    backup_db_path: &Path,
    destination_db_path: &Path,
) -> OmniResult<()> {
    let bundle_directory = backup_bundle_directory(backup_db_path);
    if !bundle_directory.exists() {
        return Ok(());
    }

    let bundle_attachments_directory = bundle_directory.join("attachments");
    if bundle_attachments_directory.exists() {
        copy_directory_recursive(
            &bundle_attachments_directory,
            &attachments_directory_for_db_path(destination_db_path)?,
        )?;
    }

    for bundle_file_name in BACKUP_BUNDLE_SIDECAR_FILE_NAMES {
        let bundled_path = bundle_directory.join(bundle_file_name);
        if !bundled_path.exists() {
            continue;
        }

        copy_file_with_parent(
            &bundled_path,
            &sibling_path(destination_db_path, bundle_file_name)?,
        )?;
    }

    Ok(())
}

fn create_staged_backup_bundle_import(
    destination_db_path: &Path,
    backup_db_path: &Path,
) -> OmniResult<StagedBackupBundleImport> {
    let staging_directory = create_backup_bundle_import_staging_directory(destination_db_path)?;
    let staged_db_path = staging_directory.join(
        destination_db_path
            .file_name()
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?,
    );
    copy_file_with_parent(backup_db_path, &staged_db_path)?;
    Storage::copy_database_key_file(backup_db_path, &staged_db_path)?;
    import_backup_bundle_contents(backup_db_path, &staged_db_path)?;

    Ok(StagedBackupBundleImport {
        staging_directory,
        staged_db_path,
    })
}

fn create_backup_bundle_import_staging_directory(
    destination_db_path: &Path,
) -> OmniResult<PathBuf> {
    let parent = destination_db_path
        .parent()
        .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
    let staging_directory = parent.join(format!(
        ".omni-import-staging-{}-{}",
        current_unix_ms(),
        MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&staging_directory).map_err(|error| {
        OmniError::Storage(format!(
            "failed to create backup import staging directory '{}': {error}",
            staging_directory.display()
        ))
    })?;
    Ok(staging_directory)
}

fn validate_staged_backup_bundle_import(staged_db_path: &Path) -> OmniResult<()> {
    let storage = Storage::open(staged_db_path)?;
    let bootstrap_summary = storage.inspect_database_bootstrap_summary()?;
    if !bootstrap_summary.supported {
        return Err(unsupported_bootstrap_error(&bootstrap_summary));
    }
    storage.migrate()?;
    storage.healthcheck()?;
    if storage.list_local_profiles()?.profiles.is_empty() {
        return Err(OmniError::InvalidInput(
            "backup database does not contain any saved local profiles".to_string(),
        ));
    }
    Ok(())
}

fn copy_local_storage_bundle(source_db_path: &Path, destination_db_path: &Path) -> OmniResult<()> {
    copy_file_with_parent(source_db_path, destination_db_path)?;
    Storage::copy_database_key_file(source_db_path, destination_db_path)?;

    let source_attachments_directory = attachments_directory_for_db_path(source_db_path)?;
    if source_attachments_directory.exists() {
        copy_directory_recursive(
            &source_attachments_directory,
            &attachments_directory_for_db_path(destination_db_path)?,
        )?;
    }

    for file_name in BACKUP_BUNDLE_SIDECAR_FILE_NAMES {
        let source_path = sibling_path(source_db_path, file_name)?;
        if source_path.exists() {
            copy_file_with_parent(&source_path, &sibling_path(destination_db_path, file_name)?)?;
        }
    }

    Ok(())
}

fn cleanup_staged_backup_bundle_import(staged_import: &StagedBackupBundleImport) -> OmniResult<()> {
    Storage::delete_local_data_at_path(&staged_import.staged_db_path)?;
    delete_directory_if_exists(&staged_import.staging_directory)
}

fn backup_bundle_directory(backup_db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bundle", backup_db_path.display()))
}

fn attachments_directory_for_db_path(db_path: &Path) -> OmniResult<PathBuf> {
    let parent = db_path
        .parent()
        .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
    Ok(parent.join("attachments"))
}

fn backup_bundle_sidecar_sources(
    source_db_path: &Path,
) -> OmniResult<Vec<(PathBuf, &'static str)>> {
    let db_path = source_db_path.to_string_lossy();
    Ok(vec![
        (transport_mode_sidecar_path(&db_path), "transport-mode.json"),
    ])
}

fn build_account_backup_inspection_token(backup_db_path: &Path) -> OmniResult<String> {
    let inspection = inspect_account_backup_bundle(backup_db_path)?;
    let raw = serde_json::to_vec(&inspection).map_err(|error| {
        OmniError::Storage(format!(
            "failed to encode backup inspection token for '{}': {error}",
            backup_db_path.display()
        ))
    })?;
    Ok(BASE64_URL_SAFE_NO_PAD.encode(raw))
}

fn validate_account_backup_inspection_token(
    backup_db_path: &Path,
    inspection_token: &str,
) -> OmniResult<()> {
    let trimmed_token = inspection_token.trim();
    if trimmed_token.is_empty() {
        return Err(OmniError::InvalidInput(
            "inspection_token must not be empty".to_string(),
        ));
    }

    let decoded = BASE64_URL_SAFE_NO_PAD
        .decode(trimmed_token)
        .map_err(|error| {
            OmniError::InvalidInput(format!("inspection_token is not valid base64: {error}"))
        })?;
    let expected: AccountBackupInspectionTokenV1 =
        serde_json::from_slice(&decoded).map_err(|error| {
            OmniError::InvalidInput(format!("inspection_token is not valid JSON: {error}"))
        })?;
    let actual = inspect_account_backup_bundle(backup_db_path)?;
    if expected != actual {
        return Err(OmniError::InvalidInput(
            "backup bundle changed after inspection".to_string(),
        ));
    }

    Ok(())
}

fn inspect_account_backup_bundle(
    backup_db_path: &Path,
) -> OmniResult<AccountBackupInspectionTokenV1> {
    let metadata = fs::metadata(backup_db_path).map_err(|error| {
        OmniError::Storage(format!(
            "failed to inspect backup database '{}': {error}",
            backup_db_path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(OmniError::InvalidInput(format!(
            "backup database '{}' is not a file",
            backup_db_path.display()
        )));
    }

    let canonical_backup_db_path = comparable_path_for_equality(backup_db_path)?;
    let backup_db_size_bytes = metadata.len();
    let backup_db_last_modified_unix_ms = file_last_modified_unix_ms(backup_db_path, &metadata)?;
    let backup_db_sha256_hex = hash_file_sha256_hex(backup_db_path)?;
    let bundle_files =
        collect_backup_bundle_file_fingerprints(&backup_bundle_directory(backup_db_path))?;
    let bundle_fingerprint_sha256_hex = hash_backup_bundle_fingerprint(
        &canonical_backup_db_path,
        backup_db_size_bytes,
        backup_db_last_modified_unix_ms,
        &backup_db_sha256_hex,
        &bundle_files,
    );

    Ok(AccountBackupInspectionTokenV1 {
        version: 1,
        canonical_backup_db_path,
        backup_db_size_bytes,
        backup_db_last_modified_unix_ms,
        backup_db_sha256_hex,
        bundle_fingerprint_sha256_hex,
        bundle_files,
    })
}

fn collect_backup_bundle_file_fingerprints(
    bundle_directory: &Path,
) -> OmniResult<Vec<BackupBundleFileFingerprint>> {
    if !bundle_directory.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_backup_bundle_file_fingerprints_recursive(
        bundle_directory,
        bundle_directory,
        &mut files,
    )?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_backup_bundle_file_fingerprints_recursive(
    bundle_root: &Path,
    current_directory: &Path,
    files: &mut Vec<BackupBundleFileFingerprint>,
) -> OmniResult<()> {
    for entry in fs::read_dir(current_directory).map_err(|error| {
        OmniError::Storage(format!(
            "failed to enumerate backup bundle directory '{}': {error}",
            current_directory.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            OmniError::Storage(format!(
                "failed to enumerate backup bundle directory '{}': {error}",
                current_directory.display()
            ))
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_backup_bundle_file_fingerprints_recursive(bundle_root, &path, files)?;
            continue;
        }

        let metadata = entry.metadata().map_err(|error| {
            OmniError::Storage(format!(
                "failed to inspect bundled file '{}': {error}",
                path.display()
            ))
        })?;
        let relative_path = path
            .strip_prefix(bundle_root)
            .map_err(|error| {
                OmniError::Storage(format!(
                    "failed to build relative path for bundled file '{}': {error}",
                    path.display()
                ))
            })?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");

        files.push(BackupBundleFileFingerprint {
            relative_path,
            size_bytes: metadata.len(),
            last_modified_unix_ms: file_last_modified_unix_ms(&path, &metadata)?,
            sha256_hex: hash_file_sha256_hex(&path)?,
        });
    }

    Ok(())
}

fn hash_file_sha256_hex(path: &Path) -> OmniResult<String> {
    let mut file = fs::File::open(path).map_err(|error| {
        OmniError::Storage(format!(
            "failed to open '{}' for hashing: {error}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8_192];

    loop {
        let read = io::Read::read(&mut file, &mut buffer).map_err(|error| {
            OmniError::Storage(format!(
                "failed to read '{}' for hashing: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn hash_backup_bundle_fingerprint(
    canonical_backup_db_path: &str,
    backup_db_size_bytes: u64,
    backup_db_last_modified_unix_ms: i64,
    backup_db_sha256_hex: &str,
    bundle_files: &[BackupBundleFileFingerprint],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_backup_db_path.as_bytes());
    hasher.update(b"\n");
    hasher.update(backup_db_size_bytes.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(backup_db_last_modified_unix_ms.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(backup_db_sha256_hex.as_bytes());
    hasher.update(b"\n");

    for file in bundle_files {
        hasher.update(file.relative_path.as_bytes());
        hasher.update(b"\n");
        hasher.update(file.size_bytes.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(file.last_modified_unix_ms.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(file.sha256_hex.as_bytes());
        hasher.update(b"\n");
    }

    hex::encode(hasher.finalize())
}

fn file_last_modified_unix_ms(path: &Path, metadata: &fs::Metadata) -> OmniResult<i64> {
    let modified = metadata.modified().map_err(|error| {
        OmniError::Storage(format!(
            "failed to read last-modified timestamp for '{}': {error}",
            path.display()
        ))
    })?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        OmniError::Storage(format!(
            "last-modified timestamp for '{}' predates unix epoch: {error}",
            path.display()
        ))
    })?;
    Ok(duration.as_millis() as i64)
}

fn delete_directory_if_exists(path: &Path) -> OmniResult<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OmniError::Storage(format!(
            "failed to delete directory '{}': {error}",
            path.display()
        ))),
    }
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> OmniResult<()> {
    fs::create_dir_all(destination).map_err(|error| {
        OmniError::Storage(format!(
            "failed to create directory '{}': {error}",
            destination.display()
        ))
    })?;

    for entry in fs::read_dir(source).map_err(|error| {
        OmniError::Storage(format!(
            "failed to enumerate directory '{}': {error}",
            source.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            OmniError::Storage(format!(
                "failed to enumerate directory '{}': {error}",
                source.display()
            ))
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else {
            copy_file_with_parent(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

fn copy_file_with_parent(source: &Path, destination: &Path) -> OmniResult<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create directory '{}': {error}",
                parent.display()
            ))
        })?;
    }

    fs::copy(source, destination).map_err(|error| {
        OmniError::Storage(format!(
            "failed to copy '{}' to '{}': {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

fn sibling_path(path: &Path, file_name: &str) -> OmniResult<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
    Ok(parent.join(file_name))
}

include!("app_transport_discovery.rs");

include!("app_security_projections.rs");

include!("app_profile_lifecycle.rs");

include!("app_onboarding_account.rs");


include!("app_crypto_bootstrap.rs");

include!("trace_log.rs");

include!("app_contacts_trust.rs");

include!("app_chat_projection.rs");

include!("app_chat_messaging.rs");

include!("runtime.rs");

#[cfg(test)]
use omni_storage::sweep_stale_test_temp;

include!("tests.rs");

fn unsupported_bootstrap_error(summary: &DatabaseBootstrapSummary) -> OmniError {
    let message = summary
        .unsupported_reason
        .clone()
        .unwrap_or_else(|| "unsupported legacy local data".to_string());
    match summary.state {
        omni_identity::DatabaseBootstrapStateKind::UnsupportedLegacyLocalData => {
            OmniError::UnsupportedLegacyLocalData(message)
        }
        omni_identity::DatabaseBootstrapStateKind::UnsupportedDatabaseFormat => {
            OmniError::UnsupportedDatabaseFormat(message)
        }
        omni_identity::DatabaseBootstrapStateKind::UnsupportedLegacyProfileState => {
            OmniError::UnsupportedLegacyProfileState(message)
        }
        _ => OmniError::Storage(message),
    }
}
