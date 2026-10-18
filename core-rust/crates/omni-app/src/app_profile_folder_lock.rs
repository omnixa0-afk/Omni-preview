//! One running Omni per profile folder (docs/decisions/multi-device-equal-nodes.md, step 1.5).
//!
//! Two processes on one database are two nodes with one identity and one ratchet state — the clone
//! the device model exists to prevent, and the easiest way to get one (a second double-click while
//! Omni sits in the tray). The first `OmniApp::init` for a database takes an exclusive OS lock on a
//! file next to it; another process gets a clear error instead of a second node. The OS drops the
//! lock when the holder exits, crash included, so there is nothing stale to clean up.
//!
//! Inside ONE process the lock is shared: the core legitimately opens the same database more than
//! once (tests, helpers, an app re-initialised while the old one is still dropping).

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use omni_types::{OmniError, OmniResult};

/// The marker the shell matches to tell "already open" from other init failures.
pub const PROFILE_ALREADY_OPEN: &str = "profile_already_open";

/// Held for the life of an `OmniApp`; the last holder in the process releases the OS lock.
pub(crate) struct ProfileFolderLock {
    _file: Arc<File>,
}

fn held_locks() -> &'static Mutex<HashMap<PathBuf, Weak<File>>> {
    static HELD: OnceLock<Mutex<HashMap<PathBuf, Weak<File>>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

fn lock_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    db_path.with_file_name(name)
}

pub(crate) fn acquire(db_path: &Path) -> OmniResult<ProfileFolderLock> {
    let path = lock_path(db_path);
    let key = std::fs::canonicalize(path.parent().unwrap_or(Path::new(".")))
        .map(|dir| dir.join(path.file_name().unwrap_or_default()))
        .unwrap_or_else(|_| path.clone());
    let mut held = held_locks()
        .lock()
        .map_err(|_| OmniError::Internal("profile lock registry is poisoned".to_string()))?;
    if let Some(file) = held.get(&key).and_then(Weak::upgrade) {
        return Ok(ProfileFolderLock { _file: file });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| OmniError::Storage(format!("profile lock folder: {error}")))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| OmniError::Storage(format!("profile lock file: {error}")))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(OmniError::InvalidInput(format!(
                "{PROFILE_ALREADY_OPEN}: this profile is already open in another Omni"
            )));
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(OmniError::Storage(format!("profile lock: {error}")));
        }
    }
    let file = Arc::new(file);
    held.insert(key, Arc::downgrade(&file));
    Ok(ProfileFolderLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("omni-profile-lock-tests").join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("omni.db")
    }

    #[test]
    fn the_same_process_shares_the_lock_and_releases_it_with_the_last_holder() {
        let db = temp_db("shared");
        let first = acquire(&db).unwrap();
        let second = acquire(&db).unwrap();
        drop(first);
        drop(second);
        // Released: an independent handle can take it now (what another process would do).
        let other = OpenOptions::new().write(true).open(lock_path(&db)).unwrap();
        other.try_lock().unwrap();
    }

    #[test]
    fn another_holder_of_the_file_gets_already_open() {
        let db = temp_db("taken");
        std::fs::write(lock_path(&db), b"").unwrap();
        // Stands in for another process: a handle outside this process's registry.
        let other = OpenOptions::new().write(true).open(lock_path(&db)).unwrap();
        other.try_lock().unwrap();
        let error = acquire(&db).err().expect("the profile is taken").to_string();
        assert!(error.contains(PROFILE_ALREADY_OPEN), "{error}");
    }
}
