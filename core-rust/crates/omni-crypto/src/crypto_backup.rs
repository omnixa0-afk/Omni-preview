// The account backup file (docs/decisions/account-recovery-from-friends.md, Г): everything on this
// device — the database and the files — in one file, encrypted so that only the account opens it.
//
// The key is derived from the account's rendezvous secret (the working key's): every device of the
// account has it, and a device that knows nothing but the phrase derives it again. The header says
// which working key version wrote the file, so a backup made after a key move opens with the phrase
// too (the phrase gives every version).
//
// Layout: `OMNIBAK1`, the header JSON (length-prefixed, plain: the account and the version, so a file
// of another account is told before anything is decrypted), then chunks of at most 1 MB, each sealed
// with ChaCha20-Poly1305 under a nonce of a random 8-byte prefix and the chunk number, the header's
// hash, the number and a "last" flag as associated data. A cut, reordered or swapped chunk does not
// open; a file cut before its last chunk is told apart from a whole one.
//
// NB: `include!`d into lib.rs.

pub const BACKUP_MAGIC: &[u8; 8] = b"OMNIBAK1";
const BACKUP_KEY_LABEL: &[u8] = b"omni-account-backup-file-v1";
const BACKUP_CHUNK_BYTES: usize = 1024 * 1024;
const BACKUP_HEADER_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupFileHeaderV1 {
    pub account_id: String,
    pub working_key_version: u64,
    pub created_at_unix_ms: i64,
    /// Random, per file: the chunk nonces' prefix.
    pub nonce_prefix_hex: String,
}

/// The key that seals backup files of the account behind `account_secret`.
pub fn backup_file_key(account_secret: &[u8; 32]) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, account_secret, BACKUP_KEY_LABEL)
}

fn backup_chunk_nonce(prefix: &[u8; 8], index: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(prefix);
    nonce[8..].copy_from_slice(&index.to_be_bytes());
    nonce
}

fn backup_chunk_aad(header_hash: &[u8; 32], index: u32, last: bool) -> Vec<u8> {
    let mut aad = header_hash.to_vec();
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(u8::from(last));
    aad
}

/// Writes a backup file: the header, then whatever is written, sealed chunk by chunk.
/// [`BackupFileWriter::finish`] seals the last chunk — a file not finished does not open.
pub struct BackupFileWriter<W: std::io::Write> {
    out: W,
    cipher: ChaCha20Poly1305,
    prefix: [u8; 8],
    header_hash: [u8; 32],
    index: u32,
    buffer: Vec<u8>,
}

impl<W: std::io::Write> BackupFileWriter<W> {
    pub fn new(mut out: W, key: &[u8; 32], account_id: &str, working_key_version: u64, now_unix_ms: i64) -> OmniResult<Self> {
        let prefix: [u8; 8] = random_bytes(8)?
            .try_into()
            .map_err(|_| OmniError::Internal("nonce prefix".to_string()))?;
        let header = BackupFileHeaderV1 {
            account_id: account_id.to_string(),
            working_key_version,
            created_at_unix_ms: now_unix_ms,
            nonce_prefix_hex: hex::encode(prefix),
        };
        let header_json = serde_json::to_vec(&header).map_err(|error| OmniError::Internal(error.to_string()))?;
        let io = |error: std::io::Error| OmniError::Storage(format!("backup file: {error}"));
        out.write_all(BACKUP_MAGIC).map_err(io)?;
        out.write_all(&(header_json.len() as u32).to_le_bytes()).map_err(io)?;
        out.write_all(&header_json).map_err(io)?;
        Ok(Self {
            out,
            cipher: <ChaCha20Poly1305 as chacha20poly1305::KeyInit>::new(key.into()),
            prefix,
            header_hash: sha256(&header_json),
            index: 0,
            buffer: Vec::with_capacity(BACKUP_CHUNK_BYTES),
        })
    }

    pub fn write(&mut self, mut bytes: &[u8]) -> OmniResult<()> {
        while !bytes.is_empty() {
            let room = BACKUP_CHUNK_BYTES - self.buffer.len();
            let take = room.min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == BACKUP_CHUNK_BYTES {
                self.seal_chunk(false)?;
            }
        }
        Ok(())
    }

    fn seal_chunk(&mut self, last: bool) -> OmniResult<()> {
        let nonce = backup_chunk_nonce(&self.prefix, self.index);
        let aad = backup_chunk_aad(&self.header_hash, self.index, last);
        let sealed = self
            .cipher
            .encrypt(ChaChaNonce::from_slice(&nonce), AeadPayload { msg: &self.buffer, aad: &aad })
            .map_err(|_| OmniError::Internal("backup chunk did not seal".to_string()))?;
        let io = |error: std::io::Error| OmniError::Storage(format!("backup file: {error}"));
        self.out.write_all(&(sealed.len() as u32).to_le_bytes()).map_err(io)?;
        self.out.write_all(&sealed).map_err(io)?;
        self.buffer.clear();
        self.index = self
            .index
            .checked_add(1)
            .ok_or_else(|| OmniError::InvalidInput("backup too large".to_string()))?;
        Ok(())
    }

    pub fn finish(mut self) -> OmniResult<W> {
        self.seal_chunk(true)?;
        self.out
            .flush()
            .map_err(|error| OmniError::Storage(format!("backup file: {error}")))?;
        Ok(self.out)
    }
}

/// The header of a backup file — read before anything is decrypted. `None` for a file that is not
/// one (an older plain database copy, say).
pub fn read_backup_file_header(input: &mut impl std::io::Read) -> OmniResult<Option<(BackupFileHeaderV1, Vec<u8>)>> {
    let mut magic = [0u8; 8];
    if input.read_exact(&mut magic).is_err() || &magic != BACKUP_MAGIC {
        return Ok(None);
    }
    let mut len = [0u8; 4];
    input
        .read_exact(&mut len)
        .map_err(|_| OmniError::InvalidInput("the backup file is cut short".to_string()))?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > BACKUP_HEADER_MAX_BYTES {
        return Err(OmniError::InvalidInput("the backup file header is malformed".to_string()));
    }
    let mut header_json = vec![0u8; len];
    input
        .read_exact(&mut header_json)
        .map_err(|_| OmniError::InvalidInput("the backup file is cut short".to_string()))?;
    let header: BackupFileHeaderV1 = serde_json::from_slice(&header_json)
        .map_err(|_| OmniError::InvalidInput("the backup file header is malformed".to_string()))?;
    Ok(Some((header, header_json)))
}

/// Reads the plaintext of a backup file after its header, chunk by chunk; fails on the first chunk
/// that does not open and on a file that ends before its last chunk.
pub struct BackupFileReader<R: std::io::Read> {
    input: R,
    cipher: ChaCha20Poly1305,
    prefix: [u8; 8],
    header_hash: [u8; 32],
    index: u32,
    plain: Vec<u8>,
    position: usize,
    done: bool,
}

impl<R: std::io::Read> BackupFileReader<R> {
    pub fn new(input: R, key: &[u8; 32], header: &BackupFileHeaderV1, header_json: &[u8]) -> OmniResult<Self> {
        let prefix: [u8; 8] = hex::decode(&header.nonce_prefix_hex)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| OmniError::InvalidInput("the backup file header is malformed".to_string()))?;
        Ok(Self {
            input,
            cipher: <ChaCha20Poly1305 as chacha20poly1305::KeyInit>::new(key.into()),
            prefix,
            header_hash: sha256(header_json),
            index: 0,
            plain: Vec::new(),
            position: 0,
            done: false,
        })
    }

    fn open_next(&mut self) -> std::io::Result<()> {
        let invalid = |message: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_string());
        let mut len = [0u8; 4];
        self.input
            .read_exact(&mut len)
            .map_err(|_| invalid("the backup file ends before its last part"))?;
        let len = u32::from_le_bytes(len) as usize;
        if len > BACKUP_CHUNK_BYTES + 16 {
            return Err(invalid("a backup file part is too large"));
        }
        let mut sealed = vec![0u8; len];
        self.input
            .read_exact(&mut sealed)
            .map_err(|_| invalid("the backup file ends before its last part"))?;
        let nonce = backup_chunk_nonce(&self.prefix, self.index);
        for last in [false, true] {
            let aad = backup_chunk_aad(&self.header_hash, self.index, last);
            if let Ok(plain) = self
                .cipher
                .decrypt(ChaChaNonce::from_slice(&nonce), AeadPayload { msg: &sealed, aad: &aad })
            {
                self.plain = plain;
                self.position = 0;
                self.done = last;
                self.index += 1;
                return Ok(());
            }
        }
        Err(invalid("the backup file does not open with this account's key, or was changed"))
    }
}

impl<R: std::io::Read> std::io::Read for BackupFileReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.position == self.plain.len() {
            if self.done {
                return Ok(0);
            }
            self.open_next()?;
        }
        let take = (self.plain.len() - self.position).min(out.len());
        out[..take].copy_from_slice(&self.plain[self.position..self.position + take]);
        self.position += take;
        Ok(take)
    }
}

/// The account secret of working key `version`, from the phrase's root secret — how a device that
/// knows only the phrase opens a backup file.
pub fn account_secret_from_root(root_secret: &AccountRootSecret, version: u64) -> OmniResult<[u8; 32]> {
    AccountMasterKey::derive(root_secret)?.working_key(version)?.account_rendezvous_secret()
}

/// The account id the phrase's root secret belongs to.
pub fn account_id_from_root(root_secret: &AccountRootSecret) -> OmniResult<AccountId> {
    Ok(AccountMasterKey::derive(root_secret)?.public_key().account_id())
}

#[cfg(test)]
mod backup_file_tests {
    use super::*;
    use std::io::Read as _;

    fn sealed(plain: &[u8], key: &[u8; 32]) -> Vec<u8> {
        let mut writer = BackupFileWriter::new(Vec::new(), key, "acc", 1, 0).unwrap();
        writer.write(plain).unwrap();
        writer.finish().unwrap()
    }

    fn opened(file: &[u8], key: &[u8; 32]) -> std::io::Result<Vec<u8>> {
        let mut input = file;
        let (header, json) = read_backup_file_header(&mut input).unwrap().unwrap();
        let mut out = Vec::new();
        BackupFileReader::new(input, key, &header, &json).unwrap().read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn a_backup_of_several_parts_opens_whole_and_not_cut_at_a_part_boundary() {
        let key = backup_file_key(&[7u8; 32]).unwrap();
        let plain: Vec<u8> = (0..BACKUP_CHUNK_BYTES * 2 + 5).map(|i| (i % 251) as u8).collect();
        let file = sealed(&plain, &key);
        assert_eq!(opened(&file, &key).unwrap(), plain);
        // Cut right after the first two parts: what is left opens part by part, but has no last one.
        let header_len = 8 + 4 + u32::from_le_bytes(file[8..12].try_into().unwrap()) as usize;
        let part = 4 + BACKUP_CHUNK_BYTES + 16;
        assert!(opened(&file[..header_len + 2 * part], &key).is_err());
        // Another key, or the parts swapped, does not open.
        assert!(opened(&file, &backup_file_key(&[8u8; 32]).unwrap()).is_err());
        let mut swapped = file[..header_len].to_vec();
        swapped.extend_from_slice(&file[header_len + part..header_len + 2 * part]);
        swapped.extend_from_slice(&file[header_len..header_len + part]);
        swapped.extend_from_slice(&file[header_len + 2 * part..]);
        assert!(opened(&swapped, &key).is_err());
        // Exactly one part's worth still ends with a sealed last (empty) part.
        let exact = vec![1u8; BACKUP_CHUNK_BYTES];
        assert_eq!(opened(&sealed(&exact, &key), &key).unwrap(), exact);
    }
}
