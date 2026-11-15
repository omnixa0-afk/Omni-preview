// The account backup file (docs/decisions/account-recovery-from-friends.md, Г).
//
// «Сохранить резервную копию» writes one file — the database and its files (attachments, profile
// media, sidecars) packed together and encrypted under the account's key (`omni_crypto::crypto_backup`).
// Only the account opens it: a device of the account, or anyone with the phrase. Restoring takes the
// file and the phrase, as before: the file is opened into a staging folder and restored from there by
// the usual path.
//
// Inside, after decryption: entries `[1][u16 path length][path][u64 size][bytes]`, then `[0]`. Paths
// are relative and checked before anything is written (no `..`, no root, only the database, its key
// file and its bundle folder). The database inside is the SQLCipher copy, so its key file travels with
// it: the DPAPI wrap opens it on the same machine, the account's wrap opens it by the phrase elsewhere.

const BACKUP_ENTRY_FILE: u8 = 1;
const BACKUP_ENTRY_END: u8 = 0;
const BACKUP_DB_ENTRY: &str = "omni.db";
const BACKUP_BUNDLE_PREFIX: &str = "omni.db.bundle/";

/// Files bigger than this in a backup are not ours (a broken file must not fill the disk).
const BACKUP_ENTRY_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// db path of the original file → the staging copy its inspection opened (`inspect` → `import`).
static OPENED_BACKUPS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, PathBuf>>> =
    std::sync::OnceLock::new();

fn opened_backups() -> &'static Mutex<std::collections::HashMap<String, PathBuf>> {
    OPENED_BACKUPS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// What a backup being opened leaves free on the disk of its staging folder.
const BACKUP_KEEP_FREE_BYTES: u64 = 256 * 1024 * 1024;

/// Staging folders an operation of this process is still writing — a save packing, a backup being
/// opened. The sweep of hour-old ones leaves them: a save of many gigabytes takes longer (audit
/// 2026-11-09). Not per profile: each operation takes its folder out when it ends ([`LiveStaging`]).
static LIVE_STAGING: std::sync::LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

/// A staging folder in use while this lives.
struct LiveStaging(PathBuf);

impl LiveStaging {
    fn new(path: &Path) -> Self {
        if let Ok(mut live) = LIVE_STAGING.lock() {
            live.insert(path.to_path_buf());
        }
        Self(path.to_path_buf())
    }
}

impl Drop for LiveStaging {
    fn drop(&mut self) {
        if let Ok(mut live) = LIVE_STAGING.lock() {
            live.remove(&self.0);
        }
    }
}

fn backup_staging_dir(near: &Path) -> PathBuf {
    let tag = hex::encode(omni_crypto::random_bytes(6).unwrap_or_default());
    near.join(format!("{}{tag}", omni_storage::BACKUP_STAGING_PREFIX))
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A file that goes unless it was kept (`keep`) — the half-written backup of a save that failed.
struct RemoveFileOnDrop(Option<PathBuf>);

impl RemoveFileOnDrop {
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn io_storage(error: std::io::Error) -> OmniError {
    OmniError::Storage(format!("backup file: {error}"))
}

fn write_backup_entry(
    writer: &mut omni_crypto::BackupFileWriter<io::BufWriter<fs::File>>,
    name: &str,
    source: &Path,
) -> OmniResult<()> {
    use std::io::Read as _;
    let size = fs::metadata(source).map_err(io_storage)?.len();
    let name = name.as_bytes();
    let mut head = vec![BACKUP_ENTRY_FILE];
    head.extend_from_slice(&(name.len() as u16).to_le_bytes());
    head.extend_from_slice(name);
    head.extend_from_slice(&size.to_le_bytes());
    writer.write(&head)?;
    let mut file = fs::File::open(source).map_err(io_storage)?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut left = size;
    while left > 0 {
        let read = file.read(&mut buffer).map_err(io_storage)?;
        if read == 0 {
            return Err(OmniError::Storage(format!("{} changed while being saved", source.display())));
        }
        let read = (read as u64).min(left) as usize;
        writer.write(&buffer[..read])?;
        left -= read as u64;
    }
    Ok(())
}

fn collect_backup_files(root: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> OmniResult<()> {
    // A bundle with no files has no folder; one that cannot be read fails the backup — a copy
    // missing files no one was told of is worse than none (audit 2026-11-09).
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_storage(error)),
    };
    let mut entries: Vec<_> = entries.collect::<Result<_, _>>().map_err(io_storage)?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if path.is_dir() {
            collect_backup_files(&path, &format!("{name}/"), out)?;
        } else if path.is_file() {
            out.push((name, path));
        }
    }
    Ok(())
}

/// A copy of the database made from a profile holds that profile alone, whatever its file name: the
/// plain copy (any name but `.omnibackup`) carried every profile of the database and their key
/// wraps, so one profile's export handed over the others (blind audit 2026-11-13, pass 23). A copy
/// of the whole install is `export_database_backup_bundle_at_path`. The files the kept profile's
/// messages name are returned.
pub(crate) fn keep_active_profile_in_copy(storage: &Storage, copy_db: &Path) -> OmniResult<Vec<PathBuf>> {
    let profile = storage
        .get_active_profile_id()?
        .ok_or_else(|| OmniError::InvalidInput("open a profile first".to_string()))?;
    crate::app_groups::keep_own_mls_in_copy(storage, &Storage::open(copy_db)?)?;
    Storage::keep_one_profile_in_copy(copy_db, &profile)
}

/// Filter the installation copy locally before writing anything to the chosen destination.
fn export_plain_profile_copy(storage: &Storage, destination: &Path) -> OmniResult<Vec<PathBuf>> {
    Storage::confine_destructive_path_for_callers(destination)?;
    let root = omni_storage::backup_staging_root(storage.db_path())?;
    remove_stale_backup_staging(&root);
    let staging = backup_staging_dir(&root);
    fs::create_dir_all(&root).map_err(io_storage)?;
    fs::create_dir(&staging).map_err(io_storage)?;
    let _live = LiveStaging::new(&staging);
    let _cleanup = RemoveOnDrop(staging.clone());
    let staged_db = staging.join(BACKUP_DB_ENTRY);
    storage.export_database_backup(&staged_db)?;
    let files = keep_active_profile_in_copy(storage, &staged_db)?;
    Storage::open(&staged_db)?.export_database_backup(destination)?;
    Ok(files)
}

/// Write the encrypted backup file of this profile at `destination`.
pub(crate) fn write_encrypted_backup(storage: &Storage, destination: &Path) -> OmniResult<()> {
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::InvalidInput("open a profile first".to_string()))?;
    let keys = storage
        .get_account_keys(&account.descriptor.account_id)?
        .ok_or_else(|| OmniError::InvalidInput("this profile has no account key chain".to_string()))?;
    let secret = storage
        .get_account_rendezvous_secret()?
        .ok_or_else(|| OmniError::InvalidInput("the account key is locked: unlock the profile first".to_string()))?;
    let key = omni_crypto::backup_file_key(&secret)?;
    let parent = destination
        .parent()
        .ok_or_else(|| OmniError::InvalidInput("backup destination has no folder".to_string()))?;
    fs::create_dir_all(parent).map_err(io_storage)?;
    // A plain copy of the database and its files, only for the time it takes to pack them. It is
    // made in the profile's own folder, never at the destination (V-25, review 2026-10-18): the
    // destination is often a cloud folder or a flash drive, and the plain sidecars of a copy that
    // broke off half way stayed there. Only the finished encrypted file reaches the destination.
    let profile_folder = omni_storage::backup_staging_root(storage.db_path())?;
    remove_stale_backup_staging(&profile_folder);
    let staging = backup_staging_dir(&profile_folder);
    // Dropped in reverse: the folder goes first, then it stops being live.
    let _live = LiveStaging::new(&staging);
    let _cleanup = RemoveOnDrop(staging.clone());
    fs::create_dir_all(&staging).map_err(io_storage)?;
    let staged_db = staging.join(BACKUP_DB_ENTRY);
    storage.export_database_backup(&staged_db)?;
    // The copy is this profile's: the install's other profiles leave it, with their key wraps, and
    // of the files only those this profile's messages name are packed (pass 14).
    let own_files = keep_active_profile_in_copy(storage, &staged_db)?;
    export_backup_bundle_contents(storage.db_path(), &staged_db, Some(&own_files))?;

    // The encrypted file is written next to its final name under a name of its own, made new here: a
    // `report.part` of the person's beside `report.omnibak` was emptied and then removed (F05, audit
    // 2026-11-09). Only the file this save made is removed when it does not finish.
    let name = destination.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let part = parent.join(format!(".{name}.{}.omni-part", hex::encode(&omni_crypto::generate_group_key()[..8])));
    let file = fs::OpenOptions::new().write(true).create_new(true).open(&part).map_err(io_storage)?;
    let part_cleanup = RemoveFileOnDrop(Some(part.clone()));
    let mut writer = omni_crypto::BackupFileWriter::new(
        io::BufWriter::new(file),
        &key,
        &account.descriptor.account_id.0,
        keys.working_certificate.version,
        current_unix_ms(),
    )?;
    write_backup_entry(&mut writer, BACKUP_DB_ENTRY, &staged_db)?;
    let key_entry = Storage::database_key_file_name(BACKUP_DB_ENTRY);
    let staged_key = staging.join(&key_entry);
    if staged_key.is_file() {
        write_backup_entry(&mut writer, &key_entry, &staged_key)?;
    }
    let mut bundle = Vec::new();
    collect_backup_files(&backup_bundle_directory(&staged_db), BACKUP_BUNDLE_PREFIX, &mut bundle)?;
    for (name, path) in &bundle {
        write_backup_entry(&mut writer, name, path)?;
    }
    writer.write(&[BACKUP_ENTRY_END])?;
    let file = writer.finish()?;
    file.into_inner()
        .map_err(|error| OmniError::Storage(format!("backup file: {error}")))?
        .sync_all()
        .map_err(io_storage)?;
    fs::rename(&part, destination).map_err(io_storage)?;
    part_cleanup.keep();
    storage.record_backup_saved(&account.descriptor.account_id, current_unix_ms())?;
    Ok(())
}

/// A relative entry path that stays inside the staging folder: the database, or a file of its bundle.
fn backup_entry_path(staging: &Path, name: &str) -> OmniResult<PathBuf> {
    let bad = || OmniError::InvalidInput("the backup file names a path outside itself".to_string());
    if name != BACKUP_DB_ENTRY
        && name != Storage::database_key_file_name(BACKUP_DB_ENTRY)
        && !name.starts_with(BACKUP_BUNDLE_PREFIX)
    {
        return Err(bad());
    }
    let mut path = staging.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':']) {
            return Err(bad());
        }
        path.push(part);
    }
    Ok(path)
}

/// Open an encrypted backup file with the phrase into a staging folder; the staged database path.
/// `None` for a file that is not an encrypted backup (an older plain database copy).
pub(crate) fn open_encrypted_backup(backup: &Path, mnemonic: &str) -> OmniResult<Option<PathBuf>> {
    use std::io::Read as _;
    let file = fs::File::open(backup).map_err(io_storage)?;
    let mut input = io::BufReader::new(file);
    let Some((header, header_json)) = omni_crypto::read_backup_file_header(&mut input)? else {
        return Ok(None);
    };
    let root = omni_identity::account_root_secret_from_mnemonic_phrase_v2(mnemonic)?;
    if omni_crypto::account_id_from_root(&root)?.0 != header.account_id {
        return Err(OmniError::InvalidInput(
            "this backup belongs to another account than the phrase".to_string(),
        ));
    }
    let secret = omni_crypto::account_secret_from_root(&root, header.working_key_version)?;
    let key = omni_crypto::backup_file_key(&secret)?;
    let mut reader = omni_crypto::BackupFileReader::new(input, &key, &header, &header_json)?;
    remove_stale_backup_staging(&std::env::temp_dir());
    let staging = backup_staging_dir(&std::env::temp_dir());
    let _live = LiveStaging::new(&staging);
    fs::create_dir_all(&staging).map_err(io_storage)?;
    let result = (|| -> OmniResult<PathBuf> {
        unpack_backup_entries(&mut reader, &staging, &omni_storage::available_space_bytes)?;
        // Read to the end: only a file whose last part opens is whole.
        let mut rest = Vec::new();
        reader
            .read_to_end(&mut rest)
            .map_err(|error| OmniError::InvalidInput(format!("the backup file is damaged: {error}")))?;
        Ok(staging.join(BACKUP_DB_ENTRY))
    })();
    match result {
        Ok(db) => {
            let original = comparable_path_for_equality(backup).unwrap_or_else(|_| backup.display().to_string());
            if let Ok(mut opened) = opened_backups().lock() {
                if let Some(previous) = opened.insert(original, db.clone()) {
                    if let Some(dir) = previous.parent() {
                        let _ = fs::remove_dir_all(dir);
                    }
                }
            }
            Ok(Some(db))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

/// Write the entries of an opened backup (`reader`, decrypted) into `staging`. Before each entry, the
/// disk of `staging` must have room for it and [`BACKUP_KEEP_FREE_BYTES`] more (`free`): a backup
/// bigger than the disk stops with a plain reason instead of filling it — the staging folder is on the
/// system disk. A name that comes twice is a damaged file, not a second copy over the first (audit
/// 2026-11-09). How many entries is not capped: each takes its bytes on the disk, and those are.
fn unpack_backup_entries(
    reader: &mut impl io::Read,
    staging: &Path,
    free: &dyn Fn(&Path) -> OmniResult<Option<u64>>,
) -> OmniResult<()> {
    use std::io::{Read as _, Write as _};
    let damaged = || OmniError::InvalidInput("the backup file is damaged".to_string());
    let broken = |error: std::io::Error| OmniError::InvalidInput(format!("the backup file is damaged: {error}"));
    let mut seen = std::collections::HashSet::new();
    loop {
        let mut kind = [0u8; 1];
        reader.read_exact(&mut kind).map_err(broken)?;
        if kind[0] == BACKUP_ENTRY_END {
            break;
        }
        if kind[0] != BACKUP_ENTRY_FILE {
            return Err(damaged());
        }
        let mut len = [0u8; 2];
        reader.read_exact(&mut len).map_err(broken)?;
        let mut name = vec![0u8; u16::from_le_bytes(len) as usize];
        reader.read_exact(&mut name).map_err(broken)?;
        let name = String::from_utf8(name).map_err(|_| damaged())?;
        let mut size = [0u8; 8];
        reader.read_exact(&mut size).map_err(broken)?;
        let size = u64::from_le_bytes(size);
        if size > BACKUP_ENTRY_MAX_BYTES || !seen.insert(name.clone()) {
            return Err(damaged());
        }
        let path = backup_entry_path(staging, &name)?;
        if let Some(available) = free(staging)? {
            if available < size.saturating_add(BACKUP_KEEP_FREE_BYTES) {
                return Err(OmniError::Storage(format!(
                    "not enough free disk space to open the backup: {size} more bytes needed, {available} free"
                )));
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_storage)?;
        }
        let mut out = io::BufWriter::new(fs::File::create(&path).map_err(io_storage)?);
        let copied = io::copy(&mut reader.by_ref().take(size), &mut out).map_err(broken)?;
        if copied != size {
            return Err(OmniError::InvalidInput("the backup file is cut short".to_string()));
        }
        out.flush().map_err(io_storage)?;
    }
    if !seen.contains(BACKUP_DB_ENTRY) {
        return Err(OmniError::InvalidInput("the backup file holds no database".to_string()));
    }
    Ok(())
}

/// A staging folder holds a plain copy: one left by a check that was never followed by a restore,
/// or by a save the app did not live to finish, goes at the next check or save, once it is an hour
/// old. One an operation of this process is still writing stays ([`LIVE_STAGING`]).
fn remove_stale_backup_staging(folder: &Path) {
    remove_backup_staging_older_than(folder, std::time::Duration::from_secs(3600));
}

fn remove_backup_staging_older_than(folder: &Path, age_past: std::time::Duration) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    let live = LIVE_STAGING.lock().map(|live| live.clone()).unwrap_or_default();
    for entry in entries.filter_map(Result::ok) {
        let stale = entry.file_name().to_string_lossy().starts_with(omni_storage::BACKUP_STAGING_PREFIX)
            && !live.contains(&entry.path())
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age >= age_past);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether `path` is an encrypted backup file (by its first bytes, not its name).
fn is_encrypted_backup(path: &Path) -> bool {
    use std::io::Read as _;
    let mut magic = [0u8; 8];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && &magic == omni_crypto::BACKUP_MAGIC
}

/// The staging copy an inspection opened for `backup`, taken out (the import removes it after).
fn take_opened_backup(backup: &Path) -> Option<PathBuf> {
    let original = comparable_path_for_equality(backup).unwrap_or_else(|_| backup.display().to_string());
    opened_backups().lock().ok()?.remove(&original)
}
