// Files at rest (docs/decisions/db-at-rest-encryption.md, «Вложения»): what Omni keeps of attachments
// and profile media inside its own data folder is encrypted under a key derived from the database key.
// A file the user saved where they chose is never this — it stays plain.
//
// Layout: a 32-byte header — `OMNIFIL1`, a random 16-byte salt, the chunk size (u32 LE), 4 bytes
// reserved — then chunks of `FILE_AT_REST_CHUNK_BYTES` plaintext (the last one shorter), each stored
// as `[24-byte random nonce][ciphertext][16-byte tag]` (XChaCha20-Poly1305). The file key is
// HKDF(DEK, salt): a salt, not the file's name, so a `.part` renamed into place keeps opening.
//
// Chunks are addressed by plaintext offset, so a file is read from anywhere (a holder serving blocks)
// and written at any chunk-aligned offset (a download assembled out of order). Every chunk's nonce is
// fresh randomness, so writing a chunk again never reuses one. The associated data is the header and
// the chunk index: a chunk moved within a file or to another file does not open. A file cut at a chunk
// boundary is not told apart here — attachments are checked against their content tree, which is.
//
// NB: `include!`d into lib.rs.

pub const FILE_AT_REST_MAGIC: &[u8; 8] = b"OMNIFIL1";
/// Plaintext bytes per chunk. A content-tree leaf (1 MiB) is a whole number of chunks, so a leaf
/// written at its offset is a run of whole chunks.
pub const FILE_AT_REST_CHUNK_BYTES: u64 = 64 * 1024;
const FILE_AT_REST_HEADER_BYTES: u64 = 32;
const FILE_AT_REST_NONCE_BYTES: u64 = 24;
const FILE_AT_REST_TAG_BYTES: u64 = 16;
const FILE_AT_REST_CHUNK_OVERHEAD: u64 = FILE_AT_REST_NONCE_BYTES + FILE_AT_REST_TAG_BYTES;
const FILE_AT_REST_KEY_LABEL: &[u8] = b"omni-file-at-rest-v1|";

fn file_at_rest_io(error: std::io::Error) -> OmniError {
    OmniError::Storage(format!("encrypted file: {error}"))
}

fn file_at_rest_disk_chunk_bytes(chunk: u64) -> u64 {
    chunk + FILE_AT_REST_CHUNK_OVERHEAD
}

/// Plaintext length of an encrypted file of `disk_len` bytes on disk (header included).
fn file_at_rest_plain_len(disk_len: u64, chunk: u64) -> u64 {
    let body = disk_len.saturating_sub(FILE_AT_REST_HEADER_BYTES);
    let full = body / file_at_rest_disk_chunk_bytes(chunk);
    let rest = body % file_at_rest_disk_chunk_bytes(chunk);
    full * chunk + rest.saturating_sub(FILE_AT_REST_CHUNK_OVERHEAD)
}

#[derive(Clone)]
struct FileAtRestHeader {
    bytes: [u8; FILE_AT_REST_HEADER_BYTES as usize],
    chunk: u64,
    cipher: chacha20poly1305::XChaCha20Poly1305,
}

impl FileAtRestHeader {
    fn new(dek: &[u8; 32]) -> OmniResult<Self> {
        let mut bytes = [0u8; FILE_AT_REST_HEADER_BYTES as usize];
        bytes[..8].copy_from_slice(FILE_AT_REST_MAGIC);
        bytes[8..24].copy_from_slice(&random_bytes(16)?);
        bytes[24..28].copy_from_slice(&(FILE_AT_REST_CHUNK_BYTES as u32).to_le_bytes());
        Self::from_bytes(dek, bytes)
    }

    fn from_bytes(dek: &[u8; 32], bytes: [u8; FILE_AT_REST_HEADER_BYTES as usize]) -> OmniResult<Self> {
        if &bytes[..8] != FILE_AT_REST_MAGIC {
            return Err(OmniError::Storage("not an encrypted Omni file".to_string()));
        }
        let chunk = u64::from(u32::from_le_bytes(bytes[24..28].try_into().expect("4 bytes")));
        if chunk == 0 || chunk > 16 * 1024 * 1024 {
            return Err(OmniError::Storage("encrypted file has a bad chunk size".to_string()));
        }
        let mut label = FILE_AT_REST_KEY_LABEL.to_vec();
        label.extend_from_slice(&bytes[8..24]);
        let key = hkdf_expand(None, dek, &label)?;
        let cipher = <chacha20poly1305::XChaCha20Poly1305 as chacha20poly1305::KeyInit>::new_from_slice(&key)
            .map_err(|error| OmniError::Internal(format!("xchacha key init failed: {error}")))?;
        Ok(Self { bytes, chunk, cipher })
    }

    fn aad(&self, index: u64) -> Vec<u8> {
        let mut aad = self.bytes.to_vec();
        aad.extend_from_slice(&index.to_le_bytes());
        aad
    }

    fn seal(&self, index: u64, plain: &[u8]) -> OmniResult<Vec<u8>> {
        use chacha20poly1305::aead::Aead as _;
        let nonce = random_bytes(FILE_AT_REST_NONCE_BYTES as usize)?;
        let sealed = self
            .cipher
            .encrypt(
                chacha20poly1305::XNonce::from_slice(&nonce),
                AeadPayload { msg: plain, aad: &self.aad(index) },
            )
            .map_err(|_| OmniError::Internal("file chunk encryption failed".to_string()))?;
        let mut out = nonce;
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn open(&self, index: u64, stored: &[u8]) -> OmniResult<Vec<u8>> {
        use chacha20poly1305::aead::Aead as _;
        if (stored.len() as u64) < FILE_AT_REST_CHUNK_OVERHEAD {
            return Err(OmniError::Storage("encrypted file chunk is cut short".to_string()));
        }
        let (nonce, sealed) = stored.split_at(FILE_AT_REST_NONCE_BYTES as usize);
        self.cipher
            .decrypt(
                chacha20poly1305::XNonce::from_slice(nonce),
                AeadPayload { msg: sealed, aad: &self.aad(index) },
            )
            .map_err(|_| OmniError::Storage(format!("encrypted file chunk {index} does not open")))
    }

    fn disk_offset(&self, index: u64) -> u64 {
        FILE_AT_REST_HEADER_BYTES + index * file_at_rest_disk_chunk_bytes(self.chunk)
    }
}

fn read_file_at_rest_header(file: &mut std::fs::File) -> OmniResult<Option<[u8; FILE_AT_REST_HEADER_BYTES as usize]>> {
    use std::io::{Read as _, Seek as _};
    file.seek(std::io::SeekFrom::Start(0)).map_err(file_at_rest_io)?;
    let mut bytes = [0u8; FILE_AT_REST_HEADER_BYTES as usize];
    let mut read = 0;
    while read < bytes.len() {
        match file.read(&mut bytes[read..]).map_err(file_at_rest_io)? {
            0 => break,
            n => read += n,
        }
    }
    Ok((read == bytes.len() && &bytes[..8] == FILE_AT_REST_MAGIC).then_some(bytes))
}

/// Whether the file at `path` is an encrypted Omni file (by its first bytes).
pub fn is_file_at_rest(path: &std::path::Path) -> bool {
    std::fs::File::open(path)
        .ok()
        .and_then(|mut file| read_file_at_rest_header(&mut file).ok().flatten())
        .is_some()
}

/// Plaintext length of a file: an encrypted one by its layout, a plain one as is.
pub fn file_at_rest_len(path: &std::path::Path) -> OmniResult<u64> {
    let mut file = std::fs::File::open(path).map_err(file_at_rest_io)?;
    let disk_len = file.metadata().map_err(file_at_rest_io)?.len();
    Ok(match read_file_at_rest_header(&mut file)? {
        Some(bytes) => {
            let chunk = u64::from(u32::from_le_bytes(bytes[24..28].try_into().expect("4 bytes")));
            file_at_rest_plain_len(disk_len, chunk.max(1))
        }
        None => disk_len,
    })
}

/// Reads a file as plaintext from any offset: an encrypted Omni file through its chunks, any other
/// file (one the user saved, an older plain copy) as it is.
pub struct FileAtRestReader {
    file: std::fs::File,
    header: Option<FileAtRestHeader>,
    plain_len: u64,
    pos: u64,
    cached: Option<(u64, Vec<u8>)>,
}

impl FileAtRestReader {
    pub fn open(path: &std::path::Path, dek: &[u8; 32]) -> OmniResult<Self> {
        let mut file = std::fs::File::open(path).map_err(|error| {
            OmniError::Storage(format!("failed to open '{}': {error}", path.display()))
        })?;
        let disk_len = file.metadata().map_err(file_at_rest_io)?.len();
        let header = match read_file_at_rest_header(&mut file)? {
            Some(bytes) => Some(FileAtRestHeader::from_bytes(dek, bytes)?),
            None => None,
        };
        let plain_len = header
            .as_ref()
            .map_or(disk_len, |header| file_at_rest_plain_len(disk_len, header.chunk));
        Ok(Self { file, header, plain_len, pos: 0, cached: None })
    }

    pub fn len(&self) -> u64 {
        self.plain_len
    }

    pub fn is_empty(&self) -> bool {
        self.plain_len == 0
    }

    pub fn is_encrypted(&self) -> bool {
        self.header.is_some()
    }

    fn chunk(&mut self, index: u64) -> std::io::Result<&[u8]> {
        use std::io::{Read as _, Seek as _};
        let header = self.header.as_ref().expect("encrypted file");
        if self.cached.as_ref().map(|(cached, _)| *cached) != Some(index) {
            let start = index * header.chunk;
            let plain = header.chunk.min(self.plain_len.saturating_sub(start));
            let mut stored = vec![0u8; (plain + FILE_AT_REST_CHUNK_OVERHEAD) as usize];
            self.file.seek(std::io::SeekFrom::Start(header.disk_offset(index)))?;
            self.file.read_exact(&mut stored)?;
            let opened = header
                .open(index, &stored)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
            self.cached = Some((index, opened));
        }
        Ok(&self.cached.as_ref().expect("just cached").1)
    }
}

impl std::io::Read for FileAtRestReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Seek as _;
        if self.pos >= self.plain_len || buf.is_empty() {
            return Ok(0);
        }
        let Some(chunk_size) = self.header.as_ref().map(|header| header.chunk) else {
            self.file.seek(std::io::SeekFrom::Start(self.pos))?;
            let read = self.file.read(buf)?;
            self.pos += read as u64;
            return Ok(read);
        };
        let index = self.pos / chunk_size;
        let within = (self.pos % chunk_size) as usize;
        let chunk = self.chunk(index)?;
        let take = buf.len().min(chunk.len().saturating_sub(within));
        buf[..take].copy_from_slice(&chunk[within..within + take]);
        self.pos += take as u64;
        Ok(take)
    }
}

impl std::io::Seek for FileAtRestReader {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let next = match to {
            std::io::SeekFrom::Start(offset) => offset as i128,
            std::io::SeekFrom::End(delta) => self.plain_len as i128 + delta as i128,
            std::io::SeekFrom::Current(delta) => self.pos as i128 + delta as i128,
        };
        if next < 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = next as u64;
        Ok(self.pos)
    }
}

/// Writes an encrypted Omni file: in order (`std::io::Write`, then `finish`), or at chunk-aligned
/// offsets (`write_at`) — a download assembled out of order, resumed after a restart.
pub struct FileAtRestWriter {
    file: std::fs::File,
    header: FileAtRestHeader,
    pending: Vec<u8>,
    next_index: u64,
}

impl FileAtRestWriter {
    /// A new file at `path` (an existing one is replaced).
    pub fn create(path: &std::path::Path, dek: &[u8; 32]) -> OmniResult<Self> {
        use std::io::Write as _;
        let header = FileAtRestHeader::new(dek)?;
        let mut file = std::fs::File::create(path).map_err(|error| {
            OmniError::Storage(format!("failed to create '{}': {error}", path.display()))
        })?;
        file.write_all(&header.bytes).map_err(file_at_rest_io)?;
        Ok(Self { file, header, pending: Vec::new(), next_index: 0 })
    }

    /// The encrypted file at `path` to write into at offsets, created when missing. A file there that
    /// is not one of ours (a plain partial left by an older build) is started over.
    pub fn open_for_offsets(path: &std::path::Path, dek: &[u8; 32]) -> OmniResult<Self> {
        let existing = std::fs::OpenOptions::new().read(true).write(true).open(path);
        if let Ok(mut file) = existing {
            if let Some(bytes) = read_file_at_rest_header(&mut file)? {
                let header = FileAtRestHeader::from_bytes(dek, bytes)?;
                return Ok(Self { file, header, pending: Vec::new(), next_index: 0 });
            }
        }
        let writer = Self::create(path, dek)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(file_at_rest_io)?;
        Ok(Self { file, ..writer })
    }

    /// Write `data` at plaintext `offset`. The offset is a chunk boundary, and `data` is whole chunks
    /// except at the file's end.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> OmniResult<()> {
        use std::io::{Seek as _, Write as _};
        let chunk = self.header.chunk;
        if !offset.is_multiple_of(chunk) {
            return Err(OmniError::Storage(format!("encrypted write at {offset} is not chunk-aligned")));
        }
        for (i, piece) in data.chunks(chunk as usize).enumerate() {
            let index = offset / chunk + i as u64;
            let stored = self.header.seal(index, piece)?;
            self.file
                .seek(std::io::SeekFrom::Start(self.header.disk_offset(index)))
                .map_err(file_at_rest_io)?;
            self.file.write_all(&stored).map_err(file_at_rest_io)?;
        }
        Ok(())
    }

    fn flush_full_chunks(&mut self) -> OmniResult<()> {
        let chunk = self.header.chunk as usize;
        while self.pending.len() >= chunk {
            let rest = self.pending.split_off(chunk);
            let piece = std::mem::replace(&mut self.pending, rest);
            self.write_at(self.next_index * self.header.chunk, &piece)?;
            self.next_index += 1;
        }
        Ok(())
    }

    /// Make what was written durable (a download's checkpoint).
    pub fn sync_data(&self) -> OmniResult<()> {
        self.file.sync_data().map_err(file_at_rest_io)
    }

    /// Write what is left of an in-order file and make it durable.
    pub fn finish(mut self) -> OmniResult<()> {
        self.flush_full_chunks()?;
        if !self.pending.is_empty() {
            let piece = std::mem::take(&mut self.pending);
            self.write_at(self.next_index * self.header.chunk, &piece)?;
        }
        self.file.sync_all().map_err(file_at_rest_io)
    }
}

impl std::io::Write for FileAtRestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        self.flush_full_chunks()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encrypt the plain file at `source` into a new encrypted file at `destination`.
pub fn encrypt_file_at_rest(source: &std::path::Path, destination: &std::path::Path, dek: &[u8; 32]) -> OmniResult<()> {
    let mut input = std::fs::File::open(source).map_err(|error| {
        OmniError::Storage(format!("failed to open '{}': {error}", source.display()))
    })?;
    let mut writer = FileAtRestWriter::create(destination, dek)?;
    std::io::copy(&mut input, &mut writer).map_err(file_at_rest_io)?;
    writer.finish()
}

/// Copy the file at `source` (encrypted or plain) out as plaintext to `destination`.
pub fn decrypt_file_at_rest(source: &std::path::Path, destination: &std::path::Path, dek: &[u8; 32]) -> OmniResult<()> {
    let mut reader = FileAtRestReader::open(source, dek)?;
    let mut output = std::fs::File::create(destination).map_err(|error| {
        OmniError::Storage(format!("failed to create '{}': {error}", destination.display()))
    })?;
    std::io::copy(&mut reader, &mut output).map_err(file_at_rest_io)?;
    output.sync_all().map_err(file_at_rest_io)
}
