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
            // A plain copy out of our storage (to open it, or «Save as»): it came from someone,
            // so Windows treats it as downloaded (V-18).
            omni_crypto::decrypt_file_at_rest(source, destination, &key)?;
            mark_as_downloaded(destination);
            Ok(())
        }
    }

    /// The writer a download assembles its `.part` with (see [`AttachmentPartWriter`]).
    pub fn open_attachment_part_writer(&self, path: &Path) -> OmniResult<AttachmentPartWriter> {
        if self.is_internal_media_path(path) {
            return self.internal_part_writer(path).map(AttachmentPartWriter::Encrypted);
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

    fn internal_part_writer(&self, path: &Path) -> OmniResult<omni_crypto::FileAtRestWriter> {
        use cap_fs_ext::{FollowSymlinks, MetadataExt as _, OpenOptionsFollowExt as _};
        let key = self.file_at_rest_key()?;
        let root = self
            .internal_media_roots()
            .into_iter()
            .find(|r| path.starts_with(r))
            .ok_or_else(|| OmniError::Storage("invalid internal part path".into()))?;
        let relative = path.strip_prefix(&root).map_err(media_io)?;
        let mut dir = open_media_root(&root).map_err(media_io)?;
        for component in relative.parent().unwrap_or(Path::new("")).components() {
            let std::path::Component::Normal(name) = component else {
                return Err(media_io("invalid part directory"));
            };
            dir = open_media_child(&dir, name).map_err(media_io)?;
        }
        let name = relative
            .file_name()
            .ok_or_else(|| media_io("invalid part name"))?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        match dir.open_with(name, options.clone().create_new(true)) {
            Ok(file) => {
                return omni_crypto::FileAtRestWriter::from_empty_file(file.into_std(), &key)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(media_io(e)),
        }
        seal_media_entry(&dir, Path::new(name), &key)?;
        let file = dir.open_with(name, &options).map_err(media_io)?;
        let meta = file.metadata().map_err(media_io)?;
        if !meta.is_file() || cap_media_link(&meta) || meta.nlink() != 1 {
            return Err(media_io("unsafe partial attachment"));
        }
        omni_crypto::FileAtRestWriter::resume_file(file.into_std(), &key)
    }

    /// Migrate legacy attachments using operations relative to opened storage directories.
    pub fn seal_plain_attachment_files(&self) -> OmniResult<usize> {
        use cap_fs_ext::MetadataExt as _;
        let mut sealed = 0;
        let mut visited = std::collections::HashSet::new();
        let mut pending: Vec<_> = self
            .internal_media_roots()
            .iter()
            .filter_map(|root| open_media_root(root).ok())
            .collect();
        if pending.is_empty() {
            return Ok(0);
        }
        let key = self.file_at_rest_key()?;
        while let Some(dir) = pending.pop() {
            let meta = dir.dir_metadata().map_err(media_io)?;
            if !visited.insert((meta.dev(), meta.ino())) {
                continue;
            }
            let mut files = Vec::new();
            for entry in dir.entries().map_err(media_io)?.flatten() {
                let name = entry.file_name();
                let Ok(meta) = dir.symlink_metadata(&name) else {
                    continue;
                };
                if cap_media_link(&meta) {
                    continue;
                }
                if meta.is_dir() {
                    if let Ok(child) = open_media_child(&dir, &name) {
                        pending.push(child);
                    }
                } else if meta.is_file() {
                    files.push(PathBuf::from(name));
                }
            }
            for name in &files {
                recover_media_entry(&dir, name, &key);
            }
            // Recovery can put back a file which did not exist during enumeration.
            for entry in dir.entries().map_err(media_io)?.flatten() {
                let name = entry.file_name();
                let text = name.to_string_lossy();
                if [".part", ".sealing", ".plain-old"]
                    .iter()
                    .any(|suffix| text.ends_with(suffix))
                {
                    continue;
                }
                if seal_media_entry(&dir, Path::new(&name), &key).unwrap_or(false) {
                    sealed += 1;
                }
            }
        }
        Ok(sealed)
    }
}

fn media_io(e: impl std::fmt::Display) -> OmniError {
    OmniError::Storage(e.to_string())
}

fn media_reparse_point(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        std::os::windows::fs::MetadataExt::file_attributes(meta) & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn cap_media_link(meta: &cap_std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        cap_std::fs::MetadataExt::file_attributes(meta) & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn open_media_child(
    dir: &cap_std::fs::Dir,
    name: &std::ffi::OsStr,
) -> std::io::Result<cap_std::fs::Dir> {
    use cap_fs_ext::DirExt as _;
    let child = dir.open_dir_nofollow(name)?;
    if cap_media_link(&child.dir_metadata()?) {
        return Err(std::io::Error::other("linked media directory"));
    }
    Ok(child)
}

fn open_media_root(root: &Path) -> std::io::Result<cap_std::fs::Dir> {
    let parent = root
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let dir = cap_std::fs::Dir::open_ambient_dir(parent, cap_std::ambient_authority())?;
    open_media_child(
        &dir,
        root.file_name().ok_or(std::io::ErrorKind::InvalidInput)?,
    )
}

fn media_sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn open_regular_media(dir: &cap_std::fs::Dir, name: &Path) -> OmniResult<fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
    let meta = dir.symlink_metadata(name).map_err(media_io)?;
    if !meta.is_file() || cap_media_link(&meta) {
        return Err(media_io("not a regular media file"));
    }
    let input = dir
        .open_with(
            name,
            cap_std::fs::OpenOptions::new()
                .read(true)
                .follow(FollowSymlinks::No),
        )
        .map_err(media_io)?
        .into_std();
    if !input.metadata().map_err(media_io)?.is_file() {
        return Err(media_io("not a regular media file"));
    }
    Ok(input)
}

fn seal_media_entry(dir: &cap_std::fs::Dir, name: &Path, key: &[u8; 32]) -> OmniResult<bool> {
    let mut input = open_regular_media(dir, name)?;
    if omni_crypto::opened_file_is_at_rest(&mut input)? {
        return Ok(false);
    }
    let temp = media_sibling(name, ".sealing");
    let output = dir
        .open_with(
            &temp,
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .map_err(media_io)?;
    let mut writer = omni_crypto::FileAtRestWriter::from_empty_file(output.into_std(), key)?;
    std::io::copy(&mut input, &mut writer).map_err(media_io)?;
    writer.finish()?;
    drop(input);
    dir.rename(&temp, dir, name).map_err(media_io)?;
    Ok(true)
}

// Only discard an old plaintext when a complete, authenticated encrypted copy holds the same bytes.
fn sealed_copy_matches(
    dir: &cap_std::fs::Dir,
    sealed: &Path,
    old: &Path,
    key: &[u8; 32],
) -> OmniResult<bool> {
    use std::io::Read as _;
    let mut sealed =
        omni_crypto::FileAtRestReader::from_file(open_regular_media(dir, sealed)?, key)?;
    let mut old = open_regular_media(dir, old)?;
    let mut remaining = old.metadata().map_err(media_io)?.len();
    if !sealed.is_encrypted() || sealed.len() != remaining {
        return Ok(false);
    }
    let mut a = vec![0; 65536];
    let mut b = vec![0; 65536];
    while remaining > 0 {
        let n = remaining.min(a.len() as u64) as usize;
        sealed.read_exact(&mut a[..n]).map_err(media_io)?;
        old.read_exact(&mut b[..n]).map_err(media_io)?;
        if a[..n] != b[..n] {
            return Ok(false);
        }
        remaining -= n as u64;
    }
    Ok(true)
}

fn recover_media_entry(dir: &cap_std::fs::Dir, name: &Path, key: &[u8; 32]) {
    let Some(text) = name.to_str() else { return };
    let original = text
        .strip_suffix(".plain-old")
        .or_else(|| text.strip_suffix(".sealing"));
    let Some(original) = original
        .map(Path::new)
        .filter(|p| p.file_name() == Some(p.as_os_str()))
    else {
        return;
    };
    let regular = |path: &Path| {
        dir.symlink_metadata(path)
            .is_ok_and(|m| m.is_file() && !cap_media_link(&m))
    };
    let absent = |path: &Path| {
        dir.symlink_metadata(path)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    };
    if !regular(name) {
        return;
    }
    if text.ends_with(".plain-old") {
        let sealing = media_sibling(original, ".sealing");
        if regular(original) {
            if sealed_copy_matches(dir, original, name, key).unwrap_or(false) {
                let _ = dir.remove_file(name);
            }
        } else if absent(original) {
            if sealed_copy_matches(dir, &sealing, name, key).unwrap_or(false)
                && dir.rename(&sealing, dir, original).is_ok()
            {
                let _ = dir.remove_file(name);
            } else {
                let _ = dir.rename(name, dir, original);
            }
        }
    } else if absent(&media_sibling(original, ".plain-old")) && regular(original) {
        let _ = dir.remove_file(name);
    }
}

/// Mark a file that came from someone else as downloaded from the internet (V-18): Windows' own
/// Mark-of-the-Web, the `Zone.Identifier` stream with zone 3, so SmartScreen and Office's protected
/// view treat it as they treat a browser download. Best effort — a disk without alternate streams
/// (FAT, a network share) keeps the file unmarked; the shell's own warning for programs still holds.
pub(crate) fn mark_as_downloaded(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(error) = fs::write(&stream, b"[ZoneTransfer]
ZoneId=3
") {
            eprintln!("could not mark '{}' as downloaded: {error}", path.display());
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}
