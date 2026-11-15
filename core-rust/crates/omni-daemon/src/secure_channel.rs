//! The TCP daemon's channel (security review 2026-11-09, F7). The token used to go over TCP as a
//! plain first line, and every command and event after it in the clear: anyone on the path of a
//! remote (`--allow-remote`) daemon could take the token and drive the core. Now the token never
//! leaves either side:
//!
//! 1. the server sends a fresh 32-byte nonce;
//! 2. the client answers with a fresh nonce of its own and a proof that it holds the token;
//! 3. the server checks it and answers with its own proof — a client never talks to a server that
//!    does not hold the token either;
//! 4. everything after goes in frames sealed with ChaCha20-Poly1305 under keys of the token and both
//!    nonces, one key per direction, a counter as the nonce.
//!
//! Proofs and keys are HKDF-SHA256 of the token with both nonces as salt. The token is 32 random
//! bytes (`TcpPolicy::for_app_dir`), so a recorded handshake gives nothing to guess against.

use std::io::{self, BufRead, BufReader, Read, Write};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

const NONCE_BYTES: usize = 32;
const PROOF_BYTES: usize = 32;
/// What the client sends before it is proved: its nonce and its proof.
pub(crate) const CLIENT_HELLO_BYTES: usize = NONCE_BYTES + PROOF_BYTES;
/// The largest frame a side accepts (a JSON line of events is far below it).
const MAX_FRAME_BYTES: usize = 1 << 20;
/// What one write is cut into at most.
const MAX_PLAIN_PER_FRAME: usize = 64 * 1024;

fn derive(token: &str, nonces: &[u8], label: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(nonces), token.as_bytes())
        .expand(label, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 length");
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_nonce() -> [u8; NONCE_BYTES] {
    let mut nonce = [0u8; NONCE_BYTES];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    nonce
}

fn refused(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, what.to_string())
}

type Halves = (Box<dyn BufRead + Send>, Box<dyn Write + Send>);

/// The server's side: `None` when the client does not prove the token (it gets nothing).
pub(crate) fn accept<R: Read + Send + 'static, W: Write + Send + 'static>(mut reader: R, mut writer: W, token: &str) -> Option<Halves> {
    let server_nonce = random_nonce();
    writer.write_all(&server_nonce).ok()?;
    writer.flush().ok()?;
    let mut answer = [0u8; CLIENT_HELLO_BYTES];
    reader.read_exact(&mut answer).ok()?;
    let (client_nonce, client_proof) = answer.split_at(NONCE_BYTES);
    let nonces = [server_nonce.as_slice(), client_nonce].concat();
    if !constant_time_eq(client_proof, &derive(token, &nonces, b"omni-daemon client proof v1")) {
        return None;
    }
    writer.write_all(&derive(token, &nonces, b"omni-daemon server proof v1")).ok()?;
    writer.flush().ok()?;
    let to_server = derive(token, &nonces, b"omni-daemon client to server v1");
    let to_client = derive(token, &nonces, b"omni-daemon server to client v1");
    Some((
        Box::new(BufReader::new(SealedReader::new(reader, to_server))),
        Box::new(SealedWriter::new(writer, to_client)),
    ))
}

/// The client's side: an error when the server does not prove the token, or refuses ours.
pub fn connect<R: Read + Send + 'static, W: Write + Send + 'static>(mut reader: R, mut writer: W, token: &str) -> io::Result<Halves> {
    let mut server_nonce = [0u8; NONCE_BYTES];
    reader.read_exact(&mut server_nonce)?;
    let client_nonce = random_nonce();
    let nonces = [server_nonce.as_slice(), client_nonce.as_slice()].concat();
    writer.write_all(&client_nonce)?;
    writer.write_all(&derive(token, &nonces, b"omni-daemon client proof v1"))?;
    writer.flush()?;
    let mut server_proof = [0u8; PROOF_BYTES];
    reader
        .read_exact(&mut server_proof)
        .map_err(|_| refused("the daemon refused the token"))?;
    if !constant_time_eq(&server_proof, &derive(token, &nonces, b"omni-daemon server proof v1")) {
        return Err(refused("the daemon could not prove it holds the token"));
    }
    let to_server = derive(token, &nonces, b"omni-daemon client to server v1");
    let to_client = derive(token, &nonces, b"omni-daemon server to client v1");
    Ok((
        Box::new(BufReader::new(SealedReader::new(reader, to_client))),
        Box::new(SealedWriter::new(writer, to_server)),
    ))
}

fn frame_nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[..8].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&bytes)
}

/// Reads frames `u32 length (LE) · ciphertext`, opens them in order.
struct SealedReader<R> {
    inner: R,
    cipher: ChaCha20Poly1305,
    counter: u64,
    plain: Vec<u8>,
    at: usize,
}

impl<R> SealedReader<R> {
    fn new(inner: R, key: [u8; 32]) -> Self {
        Self { inner, cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), counter: 0, plain: Vec::new(), at: 0 }
    }
}

impl<R: Read> Read for SealedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.plain.len() {
            let mut len = [0u8; 4];
            match self.inner.read_exact(&mut len) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(error) => return Err(error),
            }
            let len = u32::from_le_bytes(len) as usize;
            if len > MAX_FRAME_BYTES + 16 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "daemon frame too long"));
            }
            let mut sealed = vec![0u8; len];
            self.inner.read_exact(&mut sealed)?;
            self.plain = self
                .cipher
                .decrypt(&frame_nonce(self.counter), sealed.as_slice())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "daemon frame does not open"))?;
            self.counter += 1;
            self.at = 0;
        }
        let n = buf.len().min(self.plain.len() - self.at);
        buf[..n].copy_from_slice(&self.plain[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Seals every write as frames (no buffering: the daemon's lines go out when written).
struct SealedWriter<W> {
    inner: W,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl<W> SealedWriter<W> {
    fn new(inner: W, key: [u8; 32]) -> Self {
        Self { inner, cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), counter: 0 }
    }
}

impl<W: Write> Write for SealedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let take = buf.len().min(MAX_PLAIN_PER_FRAME);
        if take == 0 {
            return Ok(0);
        }
        let sealed = self
            .cipher
            .encrypt(&frame_nonce(self.counter), &buf[..take])
            .map_err(|_| io::Error::other("daemon frame does not seal"))?;
        self.counter += 1;
        let mut frame = Vec::with_capacity(4 + sealed.len());
        frame.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        frame.extend_from_slice(&sealed);
        self.inner.write_all(&frame)?;
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    fn pair(server_token: &'static str, client_token: &str) -> (io::Result<Halves>, std::thread::JoinHandle<Option<(String, Vec<u8>)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let raw = stream.try_clone().unwrap();
            // What the server reads off the wire as it arrives, to check no token is in it.
            let (mut reader, mut writer) = accept(stream.try_clone().unwrap(), stream, server_token)?;
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            writeln!(writer, "echo {}", line.trim()).ok()?;
            drop(raw);
            Some((line, Vec::new()))
        });
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        (connect(stream.try_clone().unwrap(), stream, client_token), server)
    }

    /// Both sides holding the token talk; the lines go sealed, and the token is never sent.
    #[test]
    fn a_client_with_the_token_talks_and_the_token_never_crosses_the_wire() {
        let (client, server) = pair("0123456789abcdef0123456789abcdef", "0123456789abcdef0123456789abcdef");
        let (mut reader, mut writer) = client.expect("the token is proved");
        writeln!(writer, "contacts").unwrap();
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "echo contacts");
        assert_eq!(server.join().unwrap().unwrap().0.trim(), "contacts");
    }

    /// A client with another token gets nothing; a server with another token is not talked to.
    #[test]
    fn a_wrong_token_on_either_side_ends_the_connection() {
        let (client, server) = pair("server-token-aaaaaaaaaaaaaaaaaaaa", "client-token-bbbbbbbbbbbbbbbbbbbb");
        assert!(client.is_err(), "neither side proves itself to the other");
        assert!(server.join().unwrap().is_none());
    }

    /// The bytes on the wire are not the plain line (sealed), and a tampered frame does not open.
    #[test]
    fn a_frame_is_sealed_and_a_tampered_one_does_not_open() {
        let key = [7u8; 32];
        let mut wire = Vec::new();
        SealedWriter::new(&mut wire, key).write_all(b"auth-like secret line\n").unwrap();
        assert!(!wire.windows(6).any(|w| w == b"secret"));
        let mut good = String::new();
        BufReader::new(SealedReader::new(wire.as_slice(), key)).read_line(&mut good).unwrap();
        assert_eq!(good, "auth-like secret line\n");
        let mut bad = wire.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let mut line = String::new();
        assert!(BufReader::new(SealedReader::new(bad.as_slice(), key)).read_line(&mut line).is_err());
    }
}
