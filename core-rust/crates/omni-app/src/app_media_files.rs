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

/// A recording made ready to send: the file `send … as_voice` takes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceRecordingStaged {
    pub path: String,
}

/// A voice message's sound for the shell's decoder: its Opus packets, each with its length ahead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoicePackets {
    pub packets_b64: String,
    pub duration_ms: u32,
}

fn media_open_copies_dir(db_path: &Path) -> Option<PathBuf> {
    omni_storage::ProfileFolder::Scratch.path(db_path).ok().map(|scratch| scratch.join("open"))
}

/// Plain copies handed to outside programs go when the profile stops and at the next start: a program
/// may still hold one open, and what cannot be removed now goes next time. They outlived a sign-out
/// before — plaintext of a locked profile on the disk (blind audit 2026-11-11, I8).
fn clear_media_open_copies(db_path: &Path) {
    if let Some(dir) = media_open_copies_dir(db_path) {
        let _ = fs::remove_dir_all(dir);
    }
}

impl OmniApp {
    /// A recording from the shell (Opus packets, each with its length ahead, and the outline it drew)
    /// into a file; nothing of it is on the disk before this.
    pub fn stage_voice_recording(&self, packets_b64: &str, waveform_b64: &str) -> OmniResult<VoiceRecordingStaged> {
        use base64::Engine as _;
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|_| OmniError::InvalidInput("the recording is not base64".to_string()))
        };
        let _ = self.ensure_runtime_profile_active("stage_voice_recording")?;
        let packets = omni_storage::voice::split_voice_packets(&decode(packets_b64)?)?;
        let path = self.storage.stage_voice_recording(&packets, &decode(waveform_b64)?)?;
        Ok(VoiceRecordingStaged { path: path.to_string_lossy().into_owned() })
    }

    /// A video message's recording from the shell (the MP4 and its first frame's small JPEG) into a file;
    /// nothing of it is on the disk before this.
    pub fn stage_video_note_recording(&self, video_b64: &str, preview_b64: &str) -> OmniResult<VoiceRecordingStaged> {
        use base64::Engine as _;
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|_| OmniError::InvalidInput("the recording is not base64".to_string()))
        };
        let _ = self.ensure_runtime_profile_active("stage_video_note_recording")?;
        let path = self.storage.stage_video_note_recording(&decode(video_b64)?, &decode(preview_b64)?)?;
        Ok(VoiceRecordingStaged { path: path.to_string_lossy().into_owned() })
    }

    /// The sound of a voice message: checked here, so the decoder only ever sees what passed.
    pub fn read_voice_file(&self, path: impl AsRef<str>) -> OmniResult<VoicePackets> {
        use base64::Engine as _;
        let _ = self.ensure_runtime_profile_active("read_voice_file")?;
        let bytes = self.storage.read_media_file(Path::new(path.as_ref().trim()))?;
        let file = omni_storage::voice::read_voice_ogg(&bytes)?;
        Ok(VoicePackets {
            packets_b64: base64::engine::general_purpose::STANDARD
                .encode(omni_storage::voice::join_voice_packets(&file.packets)),
            duration_ms: file.duration_ms,
        })
    }

    /// The plaintext of a media file the shell shows — one in Omni's storage (encrypted at rest) or
    /// one the user saved (plain).
    pub fn read_media_file(&self, path: impl AsRef<str>) -> OmniResult<MediaFileBytes> {
        use base64::Engine as _;
        // Only for a profile signed in: the key that opens the storage stays in the process after a
        // sign-out, and a signed-out profile's files were read through this, plaintext (blind audit
        // 2026-11-11, I2).
        let _ = self.ensure_runtime_profile_active("read_media_file")?;
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
        let _ = self.ensure_runtime_profile_active("open_media_file_copy")?;
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
        let mut destination = dir.join(&name);
        self.storage.copy_media_file(&source, &destination)?;
        // A picture opens as the picture it is, whatever the sender named it (V-18,
        // docs/decisions/received-file-safety.md): the extension of the copy follows its bytes.
        if let Some(extension) = image_extension_of(&destination) {
            let named = Path::new(&name)
                .extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
            if !named.as_deref().is_some_and(|ext| same_image_extension(ext, extension)) {
                let stem = Path::new(&name).file_stem().map(|stem| stem.to_string_lossy().into_owned());
                let renamed = dir.join(format!("{}.{extension}", stem.as_deref().unwrap_or("image")));
                fs::rename(&destination, &renamed).map_err(|error| {
                    OmniError::Storage(format!("failed to name '{}': {error}", renamed.display()))
                })?;
                destination = renamed;
            }
        }
        Ok(MediaFileOpenCopy {
            path: destination.to_string_lossy().into_owned(),
        })
    }
}

/// The extension of the image format a file's first bytes say it is, `None` when they say no image.
fn image_extension_of(path: &Path) -> Option<&'static str> {
    use std::io::Read as _;
    let mut head = [0u8; 16];
    let read = fs::File::open(path).and_then(|mut file| file.read(&mut head)).ok()?;
    image_extension_of_bytes(&head[..read])
}

fn image_extension_of_bytes(head: &[u8]) -> Option<&'static str> {
    match head {
        [0xFF, 0xD8, 0xFF, ..] => Some("jpg"),
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("png"),
        [b'G', b'I', b'F', b'8', ..] => Some("gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("webp"),
        [b'B', b'M', ..] => Some("bmp"),
        [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..] if brand.starts_with(b"avif") => Some("avif"),
        [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..]
            if [&b"heic"[..], b"heix", b"mif1", b"msf1"].iter().any(|b| brand.starts_with(b)) =>
        {
            Some("heic")
        }
        _ => None,
    }
}

/// Whether a file named `.named` already says the format `.detected` (`jpeg` is `jpg`).
fn same_image_extension(named: &str, detected: &str) -> bool {
    named == detected || matches!((named, detected), ("jpeg" | "jpe", "jpg") | ("heif", "heic"))
}

#[cfg(test)]
mod image_extension_tests {
    use super::{image_extension_of_bytes, same_image_extension};

    /// A picture is known by its bytes, never by its name: a JPEG named `x.hta` is a JPEG, and
    /// a script is no picture whatever its name says.
    #[test]
    fn a_picture_is_known_by_its_bytes() {
        assert_eq!(image_extension_of_bytes(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10]), Some("jpg"));
        assert_eq!(image_extension_of_bytes(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]), Some("png"));
        assert_eq!(image_extension_of_bytes(b"GIF89a"), Some("gif"));
        let webp = [&b"RIFF"[..], &[0; 4], b"WEBPVP8 "].concat();
        assert_eq!(image_extension_of_bytes(&webp), Some("webp"));
        let heic = [&[0, 0, 0, 0x18][..], b"ftypheic"].concat();
        assert_eq!(image_extension_of_bytes(&heic), Some("heic"));
        let avif = [&[0, 0, 0, 0x1C][..], b"ftypavif"].concat();
        assert_eq!(image_extension_of_bytes(&avif), Some("avif"));
        assert_eq!(image_extension_of_bytes(b"<html><script>"), None, "a script is no picture");
        assert_eq!(image_extension_of_bytes(&[b'M', b'Z', 0x90, 0]), None, "nor a program");
        assert_eq!(image_extension_of_bytes(&[0xFF, 0xD8]), None, "too short to tell");
        assert!(same_image_extension("jpeg", "jpg") && same_image_extension("png", "png"));
        assert!(!same_image_extension("hta", "jpg"));
    }
}
