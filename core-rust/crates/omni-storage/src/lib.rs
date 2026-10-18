use argon2::{Algorithm, Argon2, Params, Version};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omni_crypto::{
    build_prekey_bundle, decrypt_at_rest_bytes, derive_db_key_wrap_key, device_public_key_from_wire,
    device_public_key_to_wire, encrypt_at_rest_bytes, ephemeral_public_key_from_wire,
    ephemeral_public_key_to_wire, generate_at_rest_master_key, generate_local_key_hierarchy, identity_public_key_from_wire,
    identity_public_key_to_wire, identity_signing_public_key_from_wire,
    identity_signing_public_key_to_wire, new_replay_window_state, prekey_public_key_from_wire,
    prekey_public_key_to_wire, protect_bytes, signature_from_wire, signature_to_wire,
    unprotect_bytes, AtRestEncryptedBlob, LocalDeviceKeyPair, LocalIdentityKeyPair, OneTimePrekey,
    PrekeyBundle, ProtectedSecret, ReplayWindowState, SecretProtectionScheme, SessionAddress,
    SessionBootstrapState, SessionRecord, SignedPrekey, CRYPTO_SUITE_VERSION_CURRENT,
    DEFAULT_ONE_TIME_PREKEY_COUNT,
};
use omni_identity::{
    build_contact_snapshot, build_saved_profile_list, build_verification_baseline,
    create_account_identity_v2, create_contact_id, evaluate_trust, restore_account_identity_v2,
    validate_add_contact_input, validate_contact_observation, AccountId, AddContactInput,
    ContactCardSnapshot, ContactId, ContactObservationSnapshot,
    ContactRequestDirection, ContactSnapshot, DangerousActionKind, DangerousActionPreview,
    DatabaseBootstrapStateKind, DatabaseBootstrapSummary, DeviceId, LocalAccountProfileSummary,
    LocalAccountSnapshot, LocalDeviceSnapshot, LocalProfileFootprintSummary, LocalProfileId,
    LocalProfileSummary, PasswordProtectionSummary, PublicAccountDescriptor, PublicAccountSummary,
    SavedProfileList, VerificationBaseline, VerificationMethod,
};
use omni_messaging::{
    DialogMessageRefreshSummary, MessageEncryptionState, MessageSecurityDetails,
    SecurityPayloadKind,
};
use omni_types::{
    normalize_dialog_key, AttachmentRecord,
    AttachmentTransferState, CallDeclineReason, CallDirection, CallHistoryEvent, CallInboxSnapshot,
    CallNetworkQuality, CallOutcome, CallParticipantSummary, CallState, CallSummary, ChatDirection,
    ChatMessageRecord, DialogQueueDiagnostics, ListMessagesPageResponse, MessageStatus, OmniError, OmniResult,
    OutboundQueueRecord, OutboundQueueSnapshot,
    QueueDiagnosticsSnapshot, QueueFailureKind,
};
use rand_core::{OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::Digest;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Prefix of the error `store_session_record` returns when the session copy went stale: it was
/// read before another path wrote the session, so writing it would roll the ratchet back.
pub const STALE_SESSION_WRITE: &str = "stale session write";

/// How long a one-time prekey another peer already took still opens a bootstrap that names it
/// (`Storage::get_one_time_prekey_for_inbound_bootstrap`): a bundle spreads to many peers, and the
/// late ones keep resending a bootstrap made from it. A week — the catch-up window of a new device.
pub const ONE_TIME_PREKEY_SHARED_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const LOCAL_INSTANCE_ID_META_KEY: &str = "local_instance_id";
const PROFILE_MASTER_KEY_LEN: usize = 32;
const PASSWORD_SALT_LEN: usize = 16;
const PASSWORD_ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const PASSWORD_ARGON2_ITERATIONS: u32 = 3;
const PASSWORD_ARGON2_PARALLELISM: u32 = 1;

pub struct Storage {
    connection: Connection,
    db_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AttachmentRuntimeInfo {
    pub attachment: AttachmentRecord,
    pub dialog_key: String,
    pub local_blob_path: Option<PathBuf>,
    pub local_blob_managed: bool,
    pub temp_download_path: Option<PathBuf>,
    pub requested_save_path: Option<PathBuf>,
    pub active_transfer_id: Option<String>,
    pub content_encryption: Option<String>,
    pub content_key_hex: Option<String>,
    pub content_nonce_seed_hex: Option<String>,
    pub content_chunk_size_bytes: Option<u32>,
    pub local_source_size_bytes: Option<i64>,
    pub local_source_modified_unix_ms: Option<i64>,
}

impl AttachmentRuntimeInfo {
    /// The offer for this file — the same one a contact and our own devices get.
    pub fn offer_for(&self, message_id: &str, body: &str, created_at_unix_ms: i64) -> omni_types::AttachmentOfferV1 {
        omni_types::AttachmentOfferV1 {
            message_id: message_id.to_string(),
            body: body.to_string(),
            file_name: self.attachment.file_name.clone(),
            file_size_bytes: self.attachment.file_size_bytes,
            mime_type: self.attachment.mime_type.clone(),
            content_id_hex: self.attachment.content_id_hex.clone(),
            content_encryption: self.content_encryption.clone(),
            content_key_hex: self.content_key_hex.clone(),
            content_nonce_seed_hex: self.content_nonce_seed_hex.clone(),
            content_chunk_size_bytes: self.content_chunk_size_bytes,
            created_at_unix_ms,
            presentation: self.attachment.presentation.clone(),
        }
    }
}

/// What a group member told the group about itself (docs/decisions/group-member-profile.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMemberProfileRecord {
    pub account_id: String,
    pub version: i64,
    pub display_name: String,
    /// The photo file, if the member has one and we have it.
    pub avatar_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocalPublicProfileState {
    pub display_name: String,
    pub avatar_hash_hex: Option<String>,
    pub avatar_mime_type: Option<String>,
    pub avatar_bytes: Option<Vec<u8>>,
    pub updated_at_unix_ms: i64,
}

/// TM-006: reject destructive paths that resolve to system-protected
/// locations or that arrive in relative / non-canonical form.
///
/// The threat model assumes caller-supplied paths (from WinUI shell via FFI)
/// arrive from a user-picker, but a buggy or compromised UI could send
/// something like `..\..\Windows\system32\config.dat`. This helper:
///   1. Requires an absolute path.
///   2. Canonicalizes (resolves `..`, symlinks). If the path does not exist
///      yet (creating a file), canonicalizes the parent and appends the
///      filename — so we still detect `..` escape attempts.
///   3. Rejects paths whose canonical form falls inside a known system
///      directory or directly on a drive root (`C:\` etc.).
///
/// We deliberately do NOT enforce an allowlist (e.g. "must be inside
/// %LOCALAPPDATA%\Omni") because legitimate user-picker exports go to
/// Documents / Downloads / Desktop / external drives. A pure blocklist
/// catches the worst accidents while keeping export workflows working.
///
/// See `docs/decisions/path-confinement-tm006.md`.
pub(crate) fn confine_destructive_path(path: &Path) -> OmniResult<PathBuf> {
    if !path.is_absolute() {
        return Err(OmniError::InvalidInput(format!(
            "destructive path must be absolute, got '{}'",
            path.display()
        )));
    }

    let canonical = if path.exists() {
        fs::canonicalize(path).map_err(|error| {
            OmniError::Storage(format!(
                "failed to canonicalize destructive path '{}': {error}",
                path.display()
            ))
        })?
    } else {
        // Path doesn't exist yet (we're creating). Canonicalize parent and
        // append the filename. This still resolves `..` in parent.
        let parent = path.parent().ok_or_else(|| {
            OmniError::InvalidInput(format!(
                "destructive path '{}' has no parent directory",
                path.display()
            ))
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            OmniError::InvalidInput(format!(
                "destructive path '{}' has no file name",
                path.display()
            ))
        })?;
        // Parent might also not exist (e.g., creating nested attachments dir).
        // Walk up until we find an existing ancestor, canonicalize it, then
        // append the remaining suffix.
        let mut existing_ancestor = parent;
        let mut suffix = std::path::PathBuf::from(file_name);
        while !existing_ancestor.exists() {
            let next_name = existing_ancestor.file_name().ok_or_else(|| {
                OmniError::InvalidInput(format!(
                    "destructive path '{}' has no existing ancestor",
                    path.display()
                ))
            })?;
            let mut new_suffix = std::path::PathBuf::from(next_name);
            new_suffix.push(&suffix);
            suffix = new_suffix;
            existing_ancestor = existing_ancestor.parent().ok_or_else(|| {
                OmniError::InvalidInput(format!(
                    "destructive path '{}' walks past filesystem root",
                    path.display()
                ))
            })?;
        }
        let canonical_ancestor = fs::canonicalize(existing_ancestor).map_err(|error| {
            OmniError::Storage(format!(
                "failed to canonicalize destructive path ancestor '{}': {error}",
                existing_ancestor.display()
            ))
        })?;
        canonical_ancestor.join(suffix)
    };

    // Forbidden roots — pure blocklist, system locations only.
    #[cfg(windows)]
    let forbidden_prefixes: &[&str] = &[
        r"\Windows",
        r"\Program Files",
        r"\Program Files (x86)",
        r"\ProgramData",
    ];
    #[cfg(not(windows))]
    let forbidden_prefixes: &[&str] = &[
        "/etc", "/usr", "/bin", "/sbin", "/var", "/root", "/proc", "/sys", "/dev",
    ];

    let canonical_str = canonical.to_string_lossy().to_string();
    let comparable = {
        #[cfg(windows)]
        {
            // Strip leading `\\?\` extended-length prefix that canonicalize
            // adds on Windows, and the drive letter, so we compare on the
            // root-relative path only.
            let trimmed = canonical_str
                .strip_prefix(r"\\?\")
                .unwrap_or(&canonical_str);
            // Drop drive letter ("C:" -> "")
            if trimmed.len() >= 2 && trimmed.chars().nth(1) == Some(':') {
                trimmed[2..].to_string().to_ascii_lowercase()
            } else {
                trimmed.to_string().to_ascii_lowercase()
            }
        }
        #[cfg(not(windows))]
        {
            canonical_str.clone()
        }
    };

    for prefix in forbidden_prefixes {
        #[cfg(windows)]
        let needle = prefix.to_ascii_lowercase();
        #[cfg(not(windows))]
        let needle = prefix.to_string();
        if comparable.starts_with(&needle)
            && (comparable.len() == needle.len()
                || comparable[needle.len()..].starts_with(std::path::MAIN_SEPARATOR))
        {
            return Err(OmniError::InvalidInput(format!(
                "destructive path is in a protected system directory: '{}'",
                canonical.display()
            )));
        }
    }

    // Reject bare drive root (`C:\`) — canonicalized form on Windows is
    // `\\?\C:\` then we stripped to "" — that's the root.
    if comparable.is_empty() || comparable == "\\" || comparable == "/" {
        return Err(OmniError::InvalidInput(format!(
            "destructive path resolves to a filesystem root: '{}'",
            canonical.display()
        )));
    }

    Ok(canonical)
}

impl Storage {
    pub fn open(path: impl AsRef<Path>) -> OmniResult<Self> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to create storage directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        // SQLCipher: the key comes first, before any other statement touches the file.
        let key = database_key_for_open(path)?;
        let connection = Connection::open(path).map_err(map_sqlite_error)?;
        apply_database_key(&connection, &key)?;
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(map_sqlite_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sqlite_error)?;
        // WAL + NORMAL: no fsync per commit (only at checkpoints) and still no corruption on a crash;
        // a power cut can lose only the last transactions, which the queue re-sends anyway.
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(map_sqlite_error)?;
        // Deleted messages, contacts and keys are overwritten, not left readable in free pages.
        connection
            .pragma_update(None, "secure_delete", true)
            .map_err(map_sqlite_error)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(map_sqlite_error)?;

        Ok(Self {
            connection,
            db_path: path.to_path_buf(),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn inspect_database_bootstrap_summary(&self) -> OmniResult<DatabaseBootstrapSummary> {
        let table_count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if table_count == 0 {
            return Ok(DatabaseBootstrapSummary {
                state: DatabaseBootstrapStateKind::NewDatabase,
                supported: true,
                reset_required: false,
                can_create_new_account: true,
                can_restore_from_mnemonic: true,
                unsupported_reason: None,
            });
        }

        let has_profile_registry = self.table_exists("profile_registry")?;
        let has_profile_secrets = self.table_exists("profile_secrets")?;
        let has_profile_current_device = self.table_exists("profile_current_device")?;

        if !has_profile_registry || !has_profile_secrets || !has_profile_current_device {
            let unsupported_reason = if self.table_exists("local_account")?
                || self.table_exists("local_devices")?
            {
                Some("This local database was created by a legacy architecture that is no longer supported. Delete local data and start fresh, or restore from mnemonic.".to_string())
            } else {
                Some("This local database format is unsupported. Delete local data and start fresh, or restore from mnemonic.".to_string())
            };
            return Ok(DatabaseBootstrapSummary {
                state: if self.table_exists("local_account")? {
                    DatabaseBootstrapStateKind::UnsupportedLegacyLocalData
                } else {
                    DatabaseBootstrapStateKind::UnsupportedDatabaseFormat
                },
                supported: false,
                reset_required: true,
                can_create_new_account: true,
                can_restore_from_mnemonic: true,
                unsupported_reason,
            });
        }

        let unsupported_state = self
            .connection
            .query_row(
                "
                SELECT profile_state
                FROM profile_registry
                WHERE profile_state NOT IN ('signed_in', 'signed_out_locked', 'deleting', 'deleted')
                LIMIT 1
                ",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if let Some(profile_state) = unsupported_state {
            return Ok(DatabaseBootstrapSummary {
                state: DatabaseBootstrapStateKind::UnsupportedLegacyProfileState,
                supported: false,
                reset_required: true,
                can_create_new_account: true,
                can_restore_from_mnemonic: true,
                unsupported_reason: Some(format!(
                    "This database contains unsupported legacy profile state '{profile_state}'. Delete local data and start fresh, or restore from mnemonic."
                )),
            });
        }

        let has_profiles = self
            .connection
            .query_row("SELECT COUNT(*) FROM profile_registry", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(map_sqlite_error)?
            > 0;

        Ok(DatabaseBootstrapSummary {
            state: if has_profiles {
                DatabaseBootstrapStateKind::SupportedProfileDatabase
            } else {
                DatabaseBootstrapStateKind::NewDatabase
            },
            supported: true,
            reset_required: false,
            can_create_new_account: true,
            can_restore_from_mnemonic: true,
            unsupported_reason: None,
        })
    }

    pub fn reset_local_data(&self) -> OmniResult<()> {
        Self::delete_local_data_files(&self.db_path)
    }

    pub fn delete_local_data_at_path(path: impl AsRef<Path>) -> OmniResult<()> {
        // TM-006: refuse to delete files at protected system locations.
        // Validate-only: actual delete uses the original path (preserves
        // user-friendly paths in logs; canonical form is only used for the
        // blocklist check inside confine_destructive_path).
        // See docs/decisions/path-confinement-tm006.md.
        let _confined = confine_destructive_path(path.as_ref())?;
        Self::delete_local_data_files(path.as_ref())
    }

    /// TM-006 follow-up (2026-05-21): public wrapper around the
    /// crate-private `confine_destructive_path` helper so that
    /// destructive sites that live OUTSIDE `omni-storage` (notably
    /// `omni-app`'s backup-bundle export and the attachment download
    /// entry point in `app_chat_messaging.rs`) can validate paths
    /// without each crate having to duplicate the blocklist.
    /// Validation-only: returns Err for protected system locations
    /// (`\Windows`, `\Program Files`, `\Program Files (x86)`,
    /// `\ProgramData`, drive roots, and the *nix equivalents) and
    /// for relative or empty paths. The returned canonical PathBuf
    /// is discarded by callers that want to keep their original
    /// user-friendly path in error messages.
    /// See docs/decisions/tm-006-destructive-path-confinement.md.
    pub fn confine_destructive_path_for_callers(path: &Path) -> OmniResult<PathBuf> {
        confine_destructive_path(path)
    }

    /// Wipe everything this database owns EXCEPT the db file and its SQLite journals: the mesh
    /// transport identity, the DHT node cache, the settings sidecars, attachments and profile media.
    ///
    /// For the moment the LAST local profile is deleted. The db itself has to stay — it is open, and
    /// the app still needs it to offer onboarding — but by then it holds no profile, so what is left
    /// on disk is an empty container. These files are different: none of them is per-profile, none is
    /// held open, and left alone they outlive the identity they belong to. The transport identity is
    /// the sharp one — a profile created afterwards would silently come up as the SAME mesh node as
    /// the profile the user just deleted.
    ///
    /// TM-006: same path confinement as the full reset.
    pub fn delete_device_traces_keeping_open_database(path: impl AsRef<Path>) -> OmniResult<()> {
        let path = path.as_ref();
        let _confined = confine_destructive_path(path)?;
        let kept: Vec<PathBuf> = std::iter::once(path.to_path_buf())
            .chain(
                // .keys too: without it the kept (encrypted) database could never be opened again.
                ["-wal", "-shm", "-journal", DB_KEY_SIDECAR_SUFFIX]
                    .into_iter()
                    .filter_map(|suffix| Self::sidecar_path(path, suffix).ok()),
            )
            .collect();
        Self::delete_local_data_files_except(path, &kept)
    }

    fn delete_local_data_files(path: &Path) -> OmniResult<()> {
        Self::delete_local_data_files_except(path, &[])
    }

    fn delete_local_data_files_except(path: &Path, kept: &[PathBuf]) -> OmniResult<()> {
        let keeps_key = Self::sidecar_path(path, DB_KEY_SIDECAR_SUFFIX).is_ok_and(|key| kept.contains(&key));
        if !keeps_key {
            forget_database_key(path);
        }
        for target in Self::local_data_file_targets(path)?
            .into_iter()
            .filter(|target| !kept.contains(target))
        {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(OmniError::Storage(format!(
                        "failed to delete local data file '{}': {error}",
                        target.display()
                    )));
                }
            }
        }
        let attachments_dir = Self::attachments_root_for_db_path(path)?;
        match fs::remove_dir_all(&attachments_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(OmniError::Storage(format!(
                    "failed to delete attachments directory '{}': {error}",
                    attachments_dir.display()
                )));
            }
        }
        let profile_media_dir = Self::profile_media_root_for_db_path(path)?;
        match fs::remove_dir_all(&profile_media_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(OmniError::Storage(format!(
                    "failed to delete profile media directory '{}': {error}",
                    profile_media_dir.display()
                )));
            }
        }
        Ok(())
    }

    /// Fixed-name settings sidecars: the pre-rename layout, plus the ones the SHELL writes by a
    /// fixed name (it has an app dir, not a db path). Everything else is keyed by the db file name
    /// and gets picked up by the prefix rule in `local_data_file_targets`.
    const FIXED_NAME_SIDECARS: [&'static str; 11] = [
        "transport-mode.json",
        "node-mode.json",
        "discovery-publish-mode.json",
        "discovery-probe-mode.json",
        "discovery-policy.json",
        "discovery-network-config.json",
        "internet-routing.json",
        "stun-servers.json",
        "connection-privacy-mode.json",
        "transport-identity.key",
        "carrier-policy.json",
    ];

    /// Every file this database owns: the db itself, its SQLite journals, and its sidecars.
    ///
    /// Sidecars are enumerated by PREFIX (`<db file name>` + `-…`/`.…`) rather than by a list of
    /// names. The list was the bug: it was written once and never grew with the sidecars, so a wipe
    /// silently left behind the DHT node cache (peers we had contacted), the discoverability mode,
    /// carrier policy and the connection-privacy choice — real traces, surviving the very
    /// operation whose whole job is to leave none. A prefix rule cannot go stale: a new sidecar is
    /// covered the day it is written, with nobody having to remember this function exists.
    fn local_data_file_targets(path: &Path) -> OmniResult<Vec<PathBuf>> {
        let db_file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
        let parent = path
            .parent()
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;

        let mut targets = vec![path.to_path_buf()];
        // `-wal`/`-shm`/`-journal` are matched by the same prefix scan; listing them keeps the wipe
        // correct even if the directory can't be read.
        for suffix in ["-wal", "-shm", "-journal"] {
            targets.push(Self::sidecar_path(path, suffix)?);
        }
        for file_name in Self::FIXED_NAME_SIDECARS {
            targets.push(Self::sibling_path(path, file_name)?);
        }

        // The prefix sweep. A missing/unreadable directory means there is nothing to wipe there.
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                let owned_by_this_db = name
                    .strip_prefix(db_file_name)
                    .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'));
                if owned_by_this_db && entry.path().is_file() {
                    targets.push(entry.path());
                }
            }
        }

        targets.sort();
        targets.dedup();
        Ok(targets)
    }

    fn sidecar_path(path: &Path, suffix: &str) -> OmniResult<PathBuf> {
        let file_name = path
            .file_name()
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
        let mut sidecar_name = file_name.to_os_string();
        sidecar_name.push(suffix);
        let mut sidecar_path = path.to_path_buf();
        sidecar_path.set_file_name(sidecar_name);
        Ok(sidecar_path)
    }

    fn sibling_path(path: &Path, file_name: &str) -> OmniResult<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
        Ok(parent.join(file_name))
    }

    fn profile_media_root_for_db_path(path: &Path) -> OmniResult<PathBuf> {
        let parent = path
            .parent()
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
        Ok(parent.join("profile-media"))
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
        Ok(Self::comparable_path_for_equality(left)? == Self::comparable_path_for_equality(right)?)
    }

    /// Run the provided closure inside a single `BEGIN IMMEDIATE TRANSACTION`.
    /// Nested calls (when the connection is already inside a transaction)
    /// degrade to just running the closure, so callers can compose safely.
    ///
    /// Public so orchestration crates (`omni-app`) can batch loops of small
    /// writes into one transaction instead of paying per-statement autocommit
    /// overhead — see `docs/decisions/storage-batching.md`.
    pub fn with_immediate_transaction<T, F>(&self, operation_name: &str, f: F) -> OmniResult<T>
    where
        F: FnOnce(&Self) -> OmniResult<T>,
    {
        if !self.connection.is_autocommit() {
            return f(self);
        }

        self.connection
            .execute_batch("BEGIN IMMEDIATE TRANSACTION")
            .map_err(|error| {
                OmniError::Storage(format!(
                    "failed to begin transaction for '{operation_name}': {error}"
                ))
            })?;

        match f(self) {
            Ok(value) => {
                self.connection.execute_batch("COMMIT").map_err(|error| {
                    OmniError::Storage(format!(
                        "failed to commit transaction for '{operation_name}': {error}"
                    ))
                })?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }
}

pub mod photo;

include!("storage_db_key.rs");

include!("storage_migrations.rs");

include!("storage_ownership.rs");

include!("storage_profile_foundation.rs");

include!("storage_durable_frames.rs");
include!("storage_groups.rs");
include!("storage_remote_access.rs");

include!("storage_mesh_dialog_route.rs");
include!("storage_mesh_peer_book.rs");
include!("storage_network_nodes.rs");
include!("storage_rendezvous_providers.rs");
include!("storage_mesh_bootstrap_invites.rs");

include!("storage_attachments.rs");
include!("storage_file_at_rest.rs");

include!("storage_calls.rs");

include!("storage_history_queue.rs");
include!("storage_read_marks.rs");

include!("storage_identity_account.rs");
include!("storage_account_keys.rs");
include!("storage_device_binding.rs");
include!("storage_contact_devices.rs");
include!("storage_own_devices.rs");
include!("storage_device_revocations.rs");
include!("storage_account_key_moves.rs");

include!("storage_contact_sync.rs");
include!("storage_courier_sets.rs");
include!("storage_account_passport.rs");
include!("storage_history_sync.rs");

include!("storage_public_profile.rs");


include!("storage_crypto_sessions.rs");

include!("storage_contacts_trust.rs");
include!("storage_contacts_helpers.rs");

include!("storage_security_payloads.rs");

include!("storage_queue_internal.rs");

include!("helpers.rs");

include!("tests_temp_sweep.rs");

include!("tests.rs");
