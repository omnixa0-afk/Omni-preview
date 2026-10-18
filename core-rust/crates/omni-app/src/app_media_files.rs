// What the shell shows of a file in Omni's storage (docs/decisions/db-at-rest-encryption.md, «Вложения»):
// the storage is encrypted at rest, so the shell asks the core for the plaintext instead of opening the
// path itself — a photo in a bubble or in the viewer is decoded from these bytes in memory. A program
// outside Omni gets a plain copy in `<data>\tmp\open\` («Открыть во внешней программе»), cleared at the
// next start.
//
// NB: `include!`d into lib.rs.

/// The biggest file the shell reads whole: photos are re-encoded under 12 MiB (media-messages.md §1).
const MEDIA_FILE_READ_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A media file's plaintext for the shell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFileBytes {
    pub bytes_b64: String,
}

/// Where a plain copy for an outside program was put.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFileOpenCopy {
    pub path: String,
}

fn media_open_copies_dir(db_path: &Path) -> Option<PathBuf> {
    db_path.parent().map(|parent| parent.join("tmp").join("open"))
}

/// Plain copies handed to outside programs live until the next start: a program may still hold one
/// open at exit. Whatever cannot be removed now (still open) goes next time.
fn clear_media_open_copies(db_path: &Path) {
    if let Some(dir) = media_open_copies_dir(db_path) {
        let _ = fs::remove_dir_all(dir);
    }
}

impl OmniApp {
    /// The plaintext of a media file the shell shows — one in Omni's storage (encrypted at rest) or
    /// one the user saved (plain).
    pub fn read_media_file(&self, path: impl AsRef<str>) -> OmniResult<MediaFileBytes> {
        use base64::Engine as _;
        let path = Path::new(path.as_ref().trim());
        let len = omni_crypto::file_at_rest_len(path)?;
        if len > MEDIA_FILE_READ_MAX_BYTES {
            return Err(OmniError::InvalidInput(format!(
                "the file is too big to show ({len} bytes)"
            )));
        }
        let bytes = self.storage.read_media_file(path)?;
        Ok(MediaFileBytes {
            bytes_b64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// A path an outside program can open the file at: a file the user saved is its own path; one in
    /// Omni's storage is copied out plain into `<data>\tmp\open\<random>\<file name>`.
    pub fn open_media_file_copy(
        &self,
        path: impl AsRef<str>,
        file_name: impl AsRef<str>,
    ) -> OmniResult<MediaFileOpenCopy> {
        let source = PathBuf::from(path.as_ref().trim());
        if !self.storage.is_internal_media_path(&source) {
            return Ok(MediaFileOpenCopy {
                path: source.to_string_lossy().into_owned(),
            });
        }
        let dir = media_open_copies_dir(self.storage.db_path())
            .ok_or_else(|| OmniError::Storage("invalid database path".to_string()))?
            .join(hex::encode(omni_crypto::random_bytes(6)?));
        fs::create_dir_all(&dir).map_err(|error| {
            OmniError::Storage(format!("failed to create '{}': {error}", dir.display()))
        })?;
        let name = Path::new(file_name.as_ref().trim())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .or_else(|| source.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "file".to_string());
        let destination = dir.join(name);
        self.storage.copy_media_file(&source, &destination)?;
        Ok(MediaFileOpenCopy {
            path: destination.to_string_lossy().into_owned(),
        })
    }
}
