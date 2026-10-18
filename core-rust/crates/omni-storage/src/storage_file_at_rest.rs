// Files Omni keeps in its own data folder — attachments (`attachments/`) and profile media
// (`profile-media/`) — are encrypted at rest under the database key
// (docs/decisions/db-at-rest-encryption.md, «Вложения»). A path anywhere else is a place the user
// chose: written and read plain. The shell never opens these paths itself: it asks the core for the
// plaintext (`omni_media_read_file`).
//
// Readers go through `FileAtRestReader`, which reads an encrypted file and a plain one alike — files
// written before encryption keep opening while the migration seals them.
//
// NB: `include!`d into lib.rs.

/// Where a download is assembled: encrypted inside Omni's storage, plain next to a file the user saves.
pub enum AttachmentPartWriter {
    Plain(fs::File),
    Encrypted(omni_crypto::FileAtRestWriter),
}

impl AttachmentPartWriter {
    /// Write `data` at plaintext `offset` (a content-tree leaf: chunk-aligned for the encrypted kind).
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> OmniResult<()> {
        match self {
            Self::Plain(file) => {
                use std::io::{Seek as _, Write as _};
                file.seek(std::io::SeekFrom::Start(offset))
                    .map_err(|error| OmniError::Storage(format!("failed to seek attachment temp file: {error}")))?;
                file.write_all(data)
                    .map_err(|error| OmniError::Storage(format!("failed to write attachment temp file: {error}")))
            }
            Self::Encrypted(writer) => writer.write_at(offset, data),
        }
    }

    /// Make what was written durable.
    pub fn sync_data(&self) -> OmniResult<()> {
        match self {
            Self::Plain(file) => file
                .sync_data()
                .map_err(|error| OmniError::Storage(format!("failed to sync attachment temp file: {error}"))),
            Self::Encrypted(writer) => writer.sync_data(),
        }
    }
}

fn path_key_for_prefix(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut key = absolute.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    key.make_ascii_lowercase();
    if !key.ends_with('\\') {
        key.push('\\');
    }
    key
}

impl Storage {
    /// The key files at rest derive theirs from: the database key.
    fn file_at_rest_key(&self) -> OmniResult<[u8; 32]> {
        database_key_for_open(&self.db_path)
    }

    /// Whether `path` lies inside Omni's own storage of this database, where files are encrypted.
    pub fn is_internal_media_path(&self, path: &Path) -> bool {
        let file = path_key_for_prefix(path);
        self.internal_media_roots()
            .iter()
            .any(|root| file.starts_with(&path_key_for_prefix(root)))
    }

    /// The folders of Omni's own storage of this database.
    fn internal_media_roots(&self) -> Vec<PathBuf> {
        [
            Self::attachments_root_for_db_path(&self.db_path),
            Self::profile_media_root_for_db_path(&self.db_path),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// A reader of the file at `path` as plaintext — encrypted or plain.
    pub fn open_media_file(&self, path: &Path) -> OmniResult<omni_crypto::FileAtRestReader> {
        omni_crypto::FileAtRestReader::open(path, &self.file_at_rest_key()?)
    }

    /// The whole plaintext of the file at `path` (a photo, an avatar: small files).
    pub fn read_media_file(&self, path: &Path) -> OmniResult<Vec<u8>> {
        use std::io::Read as _;
        let mut reader = self.open_media_file(path)?;
        let mut bytes = Vec::with_capacity(reader.len() as usize);
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| OmniError::Storage(format!("failed to read '{}': {error}", path.display())))?;
        Ok(bytes)
    }

    /// Write `bytes` as the file at `path`: encrypted inside Omni's storage, plain elsewhere.
    pub fn write_media_file(&self, path: &Path, bytes: &[u8]) -> OmniResult<()> {
        if !self.is_internal_media_path(path) {
            return fs::write(path, bytes)
                .map_err(|error| OmniError::Storage(format!("failed to write '{}': {error}", path.display())));
        }
        use std::io::Write as _;
        let mut writer = omni_crypto::FileAtRestWriter::create(path, &self.file_at_rest_key()?)?;
        writer
            .write_all(bytes)
            .map_err(|error| OmniError::Storage(format!("failed to write '{}': {error}", path.display())))?;
        writer.finish()
    }

    /// Copy the file at `source` (encrypted or plain) to `destination`, encrypted inside Omni's
    /// storage and plain where the user chose.
    pub fn copy_media_file(&self, source: &Path, destination: &Path) -> OmniResult<()> {
        let key = self.file_at_rest_key()?;
        if self.is_internal_media_path(destination) {
            let mut reader = omni_crypto::FileAtRestReader::open(source, &key)?;
            let mut writer = omni_crypto::FileAtRestWriter::create(destination, &key)?;
            std::io::copy(&mut reader, &mut writer).map_err(|error| {
                OmniError::Storage(format!("failed to copy '{}' to '{}': {error}", source.display(), destination.display()))
            })?;
            writer.finish()
        } else {
            omni_crypto::decrypt_file_at_rest(source, destination, &key)
        }
    }

    /// The writer a download assembles its `.part` with (see [`AttachmentPartWriter`]).
    pub fn open_attachment_part_writer(&self, path: &Path) -> OmniResult<AttachmentPartWriter> {
        if self.is_internal_media_path(path) {
            self.seal_plain_part_before_writing(path)?;
            return Ok(AttachmentPartWriter::Encrypted(omni_crypto::FileAtRestWriter::open_for_offsets(
                path,
                &self.file_at_rest_key()?,
            )?));
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false) // resumable temp download: assembled at offsets, never wipe partial data
            .open(path)
            .map_err(|error| {
                OmniError::Storage(format!("failed to open attachment temp file '{}': {error}", path.display()))
            })?;
        Ok(AttachmentPartWriter::Plain(file))
    }

    /// Seal one plain file in Omni's storage in place. Steps that survive a crash between any two:
    /// the encrypted copy is written beside it (`.sealing`), the plaintext steps aside (`.plain-old`),
    /// the copy takes its name, the plaintext is overwritten and removed
    /// (`recover_interrupted_sealing` finishes or undoes an interrupted run).
    fn seal_plain_media_file(&self, path: &Path, key: &[u8; 32]) -> OmniResult<bool> {
        if omni_crypto::is_file_at_rest(path) {
            return Ok(false);
        }
        let sealing = media_sibling(path, ".sealing");
        let plain_old = media_sibling(path, ".plain-old");
        omni_crypto::encrypt_file_at_rest(path, &sealing, key)?;
        let io = |error: std::io::Error| OmniError::Storage(format!("failed to seal '{}': {error}", path.display()));
        fs::rename(path, &plain_old).map_err(io)?;
        fs::rename(&sealing, path).map_err(io)?;
        wipe_file_best_effort(&plain_old);
        Ok(true)
    }

    /// A download's `.part` in Omni's storage left plain by a build before encryption is sealed before
    /// it is written into again — its blocks already in the map stay valid.
    fn seal_plain_part_before_writing(&self, path: &Path) -> OmniResult<()> {
        if path.is_file() {
            self.seal_plain_media_file(path, &self.file_at_rest_key()?)?;
        }
        Ok(())
    }

    /// Encrypt every plain file still left in Omni's storage — attachments and profile media — written
    /// before encryption.
    /// Runs in the background at start; a file that cannot be sealed now (held open by another program)
    /// is left for the next start. `.part` files are sealed when their download resumes instead.
    /// Answers how many files were sealed.
    pub fn seal_plain_attachment_files(&self) -> OmniResult<usize> {
        let roots: Vec<PathBuf> = self.internal_media_roots().into_iter().filter(|root| root.is_dir()).collect();
        if roots.is_empty() {
            return Ok(0);
        }
        let key = self.file_at_rest_key()?;
        let mut files = Vec::new();
        for root in &roots {
            collect_media_files(root, &mut files);
        }
        recover_interrupted_sealing(&files);
        let mut files = Vec::new();
        for root in &roots {
            collect_media_files(root, &mut files);
        }
        let mut sealed = 0;
        for path in files {
            let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
            if name.ends_with(".part") || name.ends_with(".sealing") || name.ends_with(".plain-old") {
                continue;
            }
            if let Ok(true) = self.seal_plain_media_file(&path, &key) {
                sealed += 1;
            }
        }
        Ok(sealed)
    }
}

fn media_sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn collect_media_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_media_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Finish or undo sealing runs a crash cut short (see `seal_plain_media_file`).
fn recover_interrupted_sealing(files: &[PathBuf]) {
    for path in files {
        let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
        if let Some(original) = name.strip_suffix(".plain-old") {
            let original = path.with_file_name(original);
            let sealing = media_sibling(&original, ".sealing");
            if original.is_file() {
                // The sealed copy took the name; the plaintext only waits to be wiped.
                wipe_file_best_effort(path);
            } else if sealing.is_file() && fs::rename(&sealing, &original).is_ok() {
                wipe_file_best_effort(path);
            } else {
                let _ = fs::rename(path, &original);
            }
        } else if let Some(original) = name.strip_suffix(".sealing") {
            // The plaintext never stepped aside: the copy may be cut short — start that file over.
            if !media_sibling(&path.with_file_name(original), ".plain-old").exists() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

