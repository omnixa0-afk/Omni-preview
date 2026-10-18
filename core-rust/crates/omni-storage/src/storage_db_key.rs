// The database key and its wraps (docs/decisions/db-at-rest-encryption.md).
//
// Every database file is SQLCipher-encrypted under its own random 32-byte key. The key lives next to
// the file in `<db>.keys` (the database cannot hold it — it is needed to open the database):
//   - under DPAPI, so this Windows account opens the file with nothing asked;
//   - once per account whose profile lives in the file, under a key derived from that account's root
//     secret — what the recovery phrase opens when the folder came from another machine.
// The `.keys` name is a prefix sidecar of the db, so a wipe, a backup bundle and a folder copy carry it.

/// Suffix of the key file next to the database (`omni.db` → `omni.db.keys`).
const DB_KEY_SIDECAR_SUFFIX: &str = ".keys";
/// The encrypted copy a plaintext database is exported to before it replaces the original.
const DB_ENCRYPTING_SIDECAR_SUFFIX: &str = ".encrypting";
/// The plaintext original, kept until the encrypted copy has replaced it, then wiped.
const DB_PLAINTEXT_SIDECAR_SUFFIX: &str = ".plaintext-old";
const DB_KEY_FILE_VERSION: u32 = 1;
const SQLITE_PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DbKeyFile {
    version: u32,
    /// The key under DPAPI CurrentUser. `None` in a file whose DPAPI wrap belongs to another machine
    /// and was dropped, or never written (Android: the platform wrap stands in).
    #[serde(default)]
    dpapi: Option<ProtectedSecret>,
    /// The key under each account's root-derived wrap key.
    #[serde(default)]
    accounts: Vec<DbKeyAccountWrap>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DbKeyAccountWrap {
    account_id: String,
    wrapped: AtRestEncryptedBlob,
}

/// Whether a database file can be opened on this machine as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseLockState {
    /// No file yet, a plaintext file (encrypted on first open), or a key this machine can unwrap.
    Openable,
    /// Encrypted, and this Windows account cannot unwrap the key: the folder came from another machine
    /// or account. The recovery phrase of a profile in it opens it (`unlock_database_with_mnemonic`).
    NeedsRecoveryPhrase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DatabaseFileKind {
    Missing,
    Plaintext,
    Encrypted,
}

/// Keys already unwrapped in this process, by normalized database path: DPAPI once per file, not per
/// connection. Never cleared on profile stop — the key belongs to the file, not to a profile; a wipe of
/// the file drops its entry (`forget_database_key`).
fn database_key_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, [u8; 32]>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, [u8; 32]>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Serializes key creation and plaintext migration: two connections opening the same new file at once
/// must not each mint a key.
fn database_key_open_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(Default::default)
}

fn database_key_id(path: &Path) -> OmniResult<String> {
    let absolute = std::path::absolute(path).map_err(|error| {
        OmniError::Storage(format!("failed to resolve database path '{}': {error}", path.display()))
    })?;
    let mut id = absolute.to_string_lossy().to_string();
    #[cfg(windows)]
    id.make_ascii_lowercase();
    Ok(id)
}

fn database_key_sidecar(path: &Path, suffix: &str) -> OmniResult<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?;
    let mut name = file_name.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

/// The value of `PRAGMA key` for a raw 256-bit key: SQLCipher uses it as is, no KDF per open.
fn database_key_pragma(key: &[u8; 32]) -> String {
    format!("PRAGMA key = \"x'{}'\";", hex::encode_upper(key))
}

fn database_file_kind(path: &Path) -> OmniResult<DatabaseFileKind> {
    let mut header = [0_u8; 16];
    let read = match fs::File::open(path) {
        Ok(mut file) => {
            use std::io::Read as _;
            let mut total = 0;
            while total < header.len() {
                match file.read(&mut header[total..]) {
                    Ok(0) => break,
                    Ok(n) => total += n,
                    Err(error) => {
                        return Err(OmniError::Storage(format!(
                            "failed to read database header '{}': {error}",
                            path.display()
                        )))
                    }
                }
            }
            total
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(DatabaseFileKind::Missing),
        Err(error) => {
            return Err(OmniError::Storage(format!(
                "failed to open database '{}': {error}",
                path.display()
            )))
        }
    };
    Ok(match read {
        0 => DatabaseFileKind::Missing,
        _ if &header == SQLITE_PLAINTEXT_HEADER => DatabaseFileKind::Plaintext,
        _ => DatabaseFileKind::Encrypted,
    })
}

fn read_database_key_file(path: &Path) -> OmniResult<Option<DbKeyFile>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            OmniError::Storage(format!("database key file '{}' is unreadable: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OmniError::Storage(format!(
            "failed to read database key file '{}': {error}",
            path.display()
        ))),
    }
}

/// Write the key file through a temporary sibling and a rename, so a crash never leaves half a file.
fn write_database_key_file(path: &Path, file: &DbKeyFile) -> OmniResult<()> {
    let json = serde_json::to_vec_pretty(file).map_err(map_serde_error_to_storage)?;
    let temporary = database_key_sidecar(path, ".tmp")?;
    fs::write(&temporary, json).map_err(|error| {
        OmniError::Storage(format!("failed to write database key file '{}': {error}", temporary.display()))
    })?;
    fs::rename(&temporary, path).map_err(|error| {
        OmniError::Storage(format!("failed to replace database key file '{}': {error}", path.display()))
    })
}

fn unwrap_database_key_with_dpapi(file: &DbKeyFile) -> Option<[u8; 32]> {
    let bytes = unprotect_bytes(file.dpapi.as_ref()?).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

fn account_wrap_aad(account_id: &str) -> Vec<u8> {
    format!("omni-db-dek-v1|{account_id}").into_bytes()
}

/// Overwrite a file's bytes before removing it (best effort; on an SSD the old blocks may survive).
fn wipe_file_best_effort(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        if let Ok(mut file) = fs::OpenOptions::new().write(true).open(path) {
            use std::io::Write as _;
            let zeros = vec![0_u8; 64 * 1024];
            let mut left = metadata.len();
            while left > 0 {
                let chunk = left.min(zeros.len() as u64) as usize;
                if file.write_all(&zeros[..chunk]).is_err() {
                    break;
                }
                left -= chunk as u64;
            }
            let _ = file.sync_all();
        }
    }
    let _ = fs::remove_file(path);
}

/// Encrypt a plaintext database in place: export it into an encrypted sibling, swap the files, wipe the
/// plaintext. Each step leaves a state the next open recovers from (`recover_interrupted_encryption`).
fn encrypt_plaintext_database(path: &Path, key: &[u8; 32]) -> OmniResult<()> {
    let encrypting = database_key_sidecar(path, DB_ENCRYPTING_SIDECAR_SUFFIX)?;
    let plaintext_old = database_key_sidecar(path, DB_PLAINTEXT_SIDECAR_SUFFIX)?;
    let _ = fs::remove_file(&encrypting);
    {
        let plain = Connection::open(path).map_err(map_sqlite_error)?;
        plain.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(map_sqlite_error)?;
        // sqlcipher_export copies schema and rows but not user_version: carry it by hand.
        let user_version: i64 = plain
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(map_sqlite_error)?;
        plain
            .execute_batch(&format!(
                "ATTACH DATABASE {} AS encrypted KEY \"x'{}'\";
                 SELECT sqlcipher_export('encrypted');
                 PRAGMA encrypted.user_version = {user_version};
                 DETACH DATABASE encrypted;",
                sqlite_string_literal(&encrypting),
                hex::encode_upper(key),
            ))
            .map_err(map_sqlite_error)?;
        // Fold the WAL back so no plaintext page outlives the swap in `-wal`.
        let _ = plain.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    fs::rename(path, &plaintext_old).map_err(|error| {
        OmniError::Storage(format!("failed to set aside plaintext database '{}': {error}", path.display()))
    })?;
    fs::rename(&encrypting, path).map_err(|error| {
        OmniError::Storage(format!("failed to install encrypted database '{}': {error}", path.display()))
    })?;
    wipe_file_best_effort(&plaintext_old);
    for suffix in ["-wal", "-shm", "-journal"] {
        if let Ok(sidecar) = database_key_sidecar(path, suffix) {
            wipe_file_best_effort(&sidecar);
        }
    }
    Ok(())
}

/// A crash between the steps of `encrypt_plaintext_database` leaves the plaintext set aside: put it
/// back when the encrypted copy never landed, wipe it when it did.
fn recover_interrupted_encryption(path: &Path) -> OmniResult<()> {
    let plaintext_old = database_key_sidecar(path, DB_PLAINTEXT_SIDECAR_SUFFIX)?;
    if !plaintext_old.exists() {
        return Ok(());
    }
    if database_file_kind(path)? == DatabaseFileKind::Encrypted {
        wipe_file_best_effort(&plaintext_old);
    } else {
        let _ = fs::remove_file(path);
        fs::rename(&plaintext_old, path).map_err(|error| {
            OmniError::Storage(format!("failed to restore plaintext database '{}': {error}", path.display()))
        })?;
    }
    Ok(())
}

/// The key to open `path` with, creating it for a new file and encrypting a plaintext one.
fn database_key_for_open(path: &Path) -> OmniResult<[u8; 32]> {
    let id = database_key_id(path)?;
    let _guard = database_key_open_lock().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(key) = database_key_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .copied()
    {
        return Ok(key);
    }

    recover_interrupted_encryption(path)?;
    let key_path = database_key_sidecar(path, DB_KEY_SIDECAR_SUFFIX)?;
    let kind = database_file_kind(path)?;
    let key = match read_database_key_file(&key_path)? {
        Some(file) => unwrap_database_key_with_dpapi(&file).ok_or_else(|| {
            OmniError::StorageLocked(
                "this database's key is sealed to another machine or Windows account".to_string(),
            )
        })?,
        None if kind == DatabaseFileKind::Encrypted => {
            return Err(OmniError::StorageLocked(
                "the database is encrypted but its key file is missing".to_string(),
            ))
        }
        None => {
            let key = generate_at_rest_master_key()?;
            // The key is on disk before the first encrypted byte is, so a crash cannot strand data.
            write_database_key_file(
                &key_path,
                &DbKeyFile {
                    version: DB_KEY_FILE_VERSION,
                    dpapi: Some(protect_bytes(&key)?),
                    accounts: Vec::new(),
                },
            )?;
            key
        }
    };
    if kind == DatabaseFileKind::Plaintext {
        encrypt_plaintext_database(path, &key)?;
    }
    database_key_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, key);
    Ok(key)
}

/// Drop a wiped database's key from the process cache, so a new file at the same path gets a new key.
fn forget_database_key(path: &Path) {
    if let Ok(id) = database_key_id(path) {
        database_key_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }
}

/// Apply the key to a fresh connection and prove it: a wrong key surfaces only on the first read.
fn apply_database_key(connection: &Connection, key: &[u8; 32]) -> OmniResult<()> {
    connection.execute_batch(&database_key_pragma(key)).map_err(map_sqlite_error)?;
    connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0))
        .map(|_| ())
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::NotADatabase =>
            {
                OmniError::StorageLocked("the database key does not open this file".to_string())
            }
            other => map_sqlite_error(other),
        })
}

impl Storage {
    /// A second SQLite connection to the database at `path`, with its key applied — for code that needs
    /// its own connection to the same file (the MLS engine). Never open the file with a bare
    /// `rusqlite::Connection::open`: without the key every read fails with "file is not a database".
    pub fn open_keyed_connection(path: impl AsRef<Path>) -> OmniResult<Connection> {
        let path = path.as_ref();
        let key = database_key_for_open(path)?;
        let connection = Connection::open(path).map_err(map_sqlite_error)?;
        apply_database_key(&connection, &key)?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT).map_err(map_sqlite_error)?;
        Ok(connection)
    }

    /// Whether `path` opens as it is, or needs a recovery phrase first. Reads no data.
    pub fn database_lock_state(path: impl AsRef<Path>) -> OmniResult<DatabaseLockState> {
        let path = path.as_ref();
        if let Ok(id) = database_key_id(path) {
            if database_key_cache()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&id)
            {
                return Ok(DatabaseLockState::Openable);
            }
        }
        let key_path = database_key_sidecar(path, DB_KEY_SIDECAR_SUFFIX)?;
        Ok(match (read_database_key_file(&key_path)?, database_file_kind(path)?) {
            (Some(file), _) if unwrap_database_key_with_dpapi(&file).is_some() => DatabaseLockState::Openable,
            (Some(_), _) => DatabaseLockState::NeedsRecoveryPhrase,
            (None, DatabaseFileKind::Encrypted) => DatabaseLockState::NeedsRecoveryPhrase,
            (None, _) => DatabaseLockState::Openable,
        })
    }

    /// Open a database whose key this machine cannot unwrap with the recovery phrase of an account in
    /// it, and seal the key to this Windows account so the next open needs nothing.
    pub fn unlock_database_with_mnemonic(path: impl AsRef<Path>, mnemonic: &str) -> OmniResult<()> {
        let path = path.as_ref();
        let mnemonic = mnemonic.trim();
        if mnemonic.is_empty() {
            return Err(OmniError::InvalidInput("mnemonic must not be empty".to_string()));
        }
        let parsed = omni_identity::parse_mnemonic_v2(mnemonic)?;
        let account_id = omni_identity::derive_account_id_from_root(&parsed.root_secret)?;
        let key_path = database_key_sidecar(path, DB_KEY_SIDECAR_SUFFIX)?;
        let mut file = read_database_key_file(&key_path)?.ok_or_else(|| {
            OmniError::StorageLocked("the database is encrypted but its key file is missing".to_string())
        })?;
        let wrap = file
            .accounts
            .iter()
            .find(|wrap| wrap.account_id == account_id.0)
            .ok_or_else(|| {
                OmniError::InvalidInput("this recovery phrase is not one of this database's profiles".to_string())
            })?;
        let wrap_key = derive_db_key_wrap_key(parsed.root_secret.0.as_bytes(), &account_id.0)?;
        let bytes = decrypt_at_rest_bytes(&wrap_key, &account_wrap_aad(&account_id.0), &wrap.wrapped)
            .map_err(|_| OmniError::InvalidInput("the recovery phrase does not open this database".to_string()))?;
        let key = <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| OmniError::Storage("database key has the wrong length".to_string()))?;

        let connection = Connection::open(path).map_err(map_sqlite_error)?;
        apply_database_key(&connection, &key)?;
        drop(connection);

        file.dpapi = Some(protect_bytes(&key)?);
        write_database_key_file(&key_path, &file)?;
        database_key_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(database_key_id(path)?, key);
        Ok(())
    }

    /// Make sure every profile in this file whose root this machine can open has a recovery-phrase wrap
    /// of the database key. Runs at startup (`migrate`) and after a profile is created, restored or
    /// unlocked by its phrase; writes the key file only when a wrap was missing.
    pub fn ensure_database_key_wraps(&self) -> OmniResult<()> {
        let key_path = database_key_sidecar(&self.db_path, DB_KEY_SIDECAR_SUFFIX)?;
        let Some(mut file) = read_database_key_file(&key_path)? else {
            return Ok(());
        };
        let key = database_key_for_open(&self.db_path)?;
        let mut changed = false;
        for (account_id, root_secret) in self.openable_account_roots()? {
            if file.accounts.iter().any(|wrap| wrap.account_id == account_id.0) {
                continue;
            }
            let wrap_key = derive_db_key_wrap_key(root_secret.0.as_bytes(), &account_id.0)?;
            file.accounts.push(DbKeyAccountWrap {
                wrapped: encrypt_at_rest_bytes(&wrap_key, &account_wrap_aad(&account_id.0), &key)?,
                account_id: account_id.0,
            });
            changed = true;
        }
        if changed {
            write_database_key_file(&key_path, &file)?;
        }
        Ok(())
    }

    /// The account root of every profile in this file whose sealed secrets open on this machine. A
    /// profile sealed to another machine is skipped: it gets its wrap after its phrase unlocks it.
    fn openable_account_roots(&self) -> OmniResult<Vec<(AccountId, omni_identity::AccountRootSecret)>> {
        let profile_ids: Vec<String> = {
            let mut statement = self
                .connection
                .prepare("SELECT profile_id FROM profile_secrets ORDER BY profile_id")
                .map_err(map_sqlite_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sqlite_error)?;
            rows.collect::<Result<_, _>>().map_err(map_sqlite_error)?
        };
        let mut roots = Vec::new();
        for profile_id in profile_ids {
            let Some(row) = self.get_profile_secrets_row(&LocalProfileId(profile_id))? else {
                continue;
            };
            if let Ok(Some(secrets)) = load_profile_secrets_from_profile_secrets_row(&row) {
                roots.push((AccountId(row.account_id.clone()), secrets.root_secret));
            }
        }
        Ok(roots)
    }

    /// Give a backup copy written by `VACUUM INTO` (same key) the key file it needs to be opened.
    fn copy_database_key_file_to(&self, destination_db_path: &Path) -> OmniResult<()> {
        Self::copy_database_key_file(&self.db_path, destination_db_path)
    }

    /// Copy the key file of the database at `source_db_path` next to a copy of that file at
    /// `destination_db_path` — every path that moves a database file moves its key with it, or the
    /// copy is "encrypted but its key file is missing". Nothing to copy for a database without one.
    pub fn copy_database_key_file(source_db_path: &Path, destination_db_path: &Path) -> OmniResult<()> {
        let source = database_key_sidecar(source_db_path, DB_KEY_SIDECAR_SUFFIX)?;
        let Some(file) = read_database_key_file(&source)? else {
            return Ok(());
        };
        write_database_key_file(&database_key_sidecar(destination_db_path, DB_KEY_SIDECAR_SUFFIX)?, &file)
    }

    /// The key file name of a database file (`omni.db` → `omni.db.keys`), for code that packs a database
    /// with its key under a name of its own.
    pub fn database_key_file_name(db_file_name: &str) -> String {
        format!("{db_file_name}{DB_KEY_SIDECAR_SUFFIX}")
    }
}
