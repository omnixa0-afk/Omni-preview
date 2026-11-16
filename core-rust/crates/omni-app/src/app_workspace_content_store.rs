//! Where a files project keeps content by its SHA-256: `content` in the project's data, or the disk a
//! custodian chose for its archive. Earlier stores stay readable, and what is fetched again is moved
//! from them to the current one.
use omni_types::{OmniError, OmniResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const STORE_FILE: &str = "store.json";
// No profile data is retained here: only short local metadata updates are serialized.
static STORE_CONFIGURATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Serialize, Deserialize)]
struct Stores {
    current: PathBuf,
    previous: Vec<PathBuf>,
}

fn stores(repo: &Path) -> Stores {
    std::fs::read(repo.join(STORE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| Stores {
            current: repo.join("content"),
            previous: Vec::new(),
        })
}

/// Every store the project has had: the current one and the earlier ones.
pub(crate) fn store_roots(repo: &Path) -> Vec<PathBuf> {
    let stores = stores(repo);
    std::iter::once(stores.current).chain(stores.previous).collect()
}

pub(crate) fn store_path(repo: &Path) -> PathBuf {
    stores(repo).current
}

fn object_at(store: &Path, oid: &str) -> PathBuf {
    store
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

pub(crate) fn object_path(repo: &Path, oid: &str) -> PathBuf {
    object_at(&store_path(repo), oid)
}

pub(crate) fn existing_object_path(repo: &Path, oid: &str) -> PathBuf {
    let stores = stores(repo);
    let target = object_at(&stores.current, oid);
    if target.is_file() {
        return target;
    }
    stores
        .previous
        .iter()
        .map(|p| object_at(p, oid))
        .find(|p| p.is_file())
        .unwrap_or(target)
}

/// Remove content `oid` from every store of the project (current and earlier ones); whether any held it.
pub(crate) fn remove_object(repo: &Path, oid: &str) -> bool {
    let stores = stores(repo);
    let mut removed = false;
    for store in std::iter::once(&stores.current).chain(stores.previous.iter()) {
        removed |= std::fs::remove_file(object_at(store, oid)).is_ok();
    }
    removed
}

pub(crate) fn configure_store(repo: &Path, folder: &Path) -> OmniResult<()> {
    let _guard = STORE_CONFIGURATION
        .lock()
        .map_err(|_| OmniError::Internal("archive placement lock was poisoned".into()))?;
    if !folder.is_absolute() {
        return Err(OmniError::InvalidInput(
            "choose an absolute archive folder".into(),
        ));
    }
    omni_storage::Storage::confine_destructive_path_for_callers(folder)?;
    let mut stores = stores(repo);
    if stores.current == folder {
        return Ok(());
    }
    std::fs::create_dir_all(folder).map_err(|e| OmniError::Storage(e.to_string()))?;
    stores.previous.retain(|p| p != folder);
    if !stores.previous.contains(&stores.current) {
        stores.previous.push(stores.current.clone());
    }
    stores.current = folder.to_path_buf();
    let bytes = serde_json::to_vec(&stores).map_err(|e| OmniError::Storage(e.to_string()))?;
    let temp = repo.join(format!(
        "{STORE_FILE}.{}.new",
        hex::encode(&omni_crypto::generate_group_key()[..8])
    ));
    std::fs::write(&temp, bytes).map_err(|e| OmniError::Storage(e.to_string()))?;
    std::fs::rename(&temp, repo.join(STORE_FILE)).map_err(|e| OmniError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) fn migrate_cached_object(
    repo: &Path,
    oid: &str,
    expected_size: u64,
) -> OmniResult<bool> {
    let stores = stores(repo);
    let target = object_at(&stores.current, oid);
    if target.is_file() {
        return Ok(true);
    }
    let Some(source) = stores
        .previous
        .iter()
        .map(|p| object_at(p, oid))
        .find(|p| p.is_file())
    else {
        return Ok(false);
    };
    std::fs::create_dir_all(target.parent().expect("object parent"))
        .map_err(|e| OmniError::Storage(e.to_string()))?;
    let temp = target.with_extension(format!(
        "move-{}",
        hex::encode(&omni_crypto::generate_group_key()[..8])
    ));
    let moved = (|| -> OmniResult<()> {
        let mut input =
            std::fs::File::open(&source).map_err(|e| OmniError::Storage(e.to_string()))?;
        let mut output =
            std::fs::File::create(&temp).map_err(|e| OmniError::Storage(e.to_string()))?;
        let (mut hash, mut size) = (Sha256::new(), 0_u64);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let n = input
                .read(&mut buffer)
                .map_err(|e| OmniError::Storage(e.to_string()))?;
            if n == 0 {
                break;
            }
            size = size.saturating_add(n as u64);
            hash.update(&buffer[..n]);
            output
                .write_all(&buffer[..n])
                .map_err(|e| OmniError::Storage(e.to_string()))?;
        }
        if size != expected_size || hex::encode(hash.finalize()) != oid {
            return Err(OmniError::InvalidInput(
                "cached file content failed its hash check during archive migration".into(),
            ));
        }
        output
            .sync_all()
            .map_err(|e| OmniError::Storage(e.to_string()))?;
        drop(output);
        std::fs::rename(&temp, &target).map_err(|e| OmniError::Storage(e.to_string()))?;
        // Only immutable, hash-verified cache objects are removed; never a working file.
        if let Err(error) = std::fs::remove_file(&source) {
            crate::omni_app_trace_log(
                "workspace/history",
                &format!("old verified cache object was retained: {error}"),
            );
        }
        Ok(())
    })();
    if moved.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    moved.map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_archive_moves_verified_content_to_the_chosen_store_and_preserves_bad_source_data()
    {
        let root = std::env::temp_dir().join(format!(
            "omni-store-{}",
            hex::encode(&omni_crypto::generate_group_key()[..8])
        ));
        let repo = root.join("project.files");
        std::fs::create_dir_all(&repo).unwrap();
        let bytes = b"archive version";
        let oid = hex::encode(Sha256::digest(bytes));
        let old = object_path(&repo, &oid);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, bytes).unwrap();
        let chosen = root.join("chosen-disk/archive");
        configure_store(&repo, &chosen).unwrap();
        assert_eq!(
            existing_object_path(&repo, &oid),
            old,
            "existing content can still be served during migration"
        );
        assert!(migrate_cached_object(&repo, &oid, bytes.len() as u64).unwrap());
        let new = object_path(&repo, &oid);
        assert!(new.starts_with(&chosen));
        assert_eq!(std::fs::read(&new).unwrap(), bytes);
        assert!(!old.exists());
        let bad_oid = "b".repeat(64);
        let bad = object_at(&chosen, &bad_oid);
        std::fs::create_dir_all(bad.parent().unwrap()).unwrap();
        std::fs::write(&bad, b"corrupted").unwrap();
        configure_store(&repo, &root.join("second-disk/archive")).unwrap();
        assert!(migrate_cached_object(&repo, &bad_oid, 9).is_err());
        assert!(
            bad.is_file(),
            "a failed verification never deletes the previous object"
        );
        assert!(!object_path(&repo, &bad_oid).exists());
    }
}
