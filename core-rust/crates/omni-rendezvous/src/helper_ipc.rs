//! The line IPC between the core and its helper processes: who may talk to a helper (V-21), and
//! how much is read from either side (V-12).
//!
//! A helper listens on loopback. Without a token, any local process — and a web page, by way of
//! a request to `127.0.0.1` whose body is a command line — could make it post to the couriers and
//! the DHT on the user's behalf. So the core gives every helper it starts one random token (in
//! [`HELPER_TOKEN_ENV`]), and every connection opens with `auth <token>`; a helper drops one that
//! does not. A helper is also the untrusted, key-free side (TM-013): what it may cost the core is a
//! failed rendezvous, not the core's memory, so replies are read bounded.

use std::io::{self, BufRead, Read, Write};
use std::sync::OnceLock;

/// The variable a helper gets its connection token in.
pub const HELPER_TOKEN_ENV: &str = "OMNI_HELPER_TOKEN";

/// The longest request line a helper takes (a `publish` carries a beacon; a `watch` or a
/// `fetch-many`, a few hundred topics).
pub const HELPER_REQUEST_MAX: usize = 1024 * 1024;

/// The longest `auth` line.
const AUTH_LINE_MAX: usize = 256;

/// This process's helper token: [`HELPER_TOKEN_ENV`] when set — a helper started by the core, or
/// by a dev script — else a random one, made once. The core hands it to every helper it starts;
/// a helper that calls another (the rendezvous helper's DHT leg) has the same one.
pub fn process_helper_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| match helper_token_from_env() {
        Some(token) => token,
        None => {
            let mut bytes = [0u8; 32];
            rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        }
    })
}

/// [`HELPER_TOKEN_ENV`], when it is set and not blank.
pub fn helper_token_from_env() -> Option<String> {
    let token = std::env::var(HELPER_TOKEN_ENV).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// How long a new connection has to prove the token.
pub const AUTH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How many connections may be waiting to prove the token at once.
pub const AUTH_PENDING_MAX: usize = 32;

static AUTH_PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A place among the connections still to prove the token; given back when dropped.
pub struct AuthSlot(());

impl Drop for AuthSlot {
    fn drop(&mut self) {
        AUTH_PENDING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Take a place for a new connection BEFORE a thread is started for it — none when
/// [`AUTH_PENDING_MAX`] wait already. Any local process (another Windows user's too) can open the
/// loopback port: each connection took a thread that waited for its first line forever, so
/// connections that said nothing used the helper up (blind audit 2026-11-13, pass 20).
pub fn reserve_auth_slot() -> Option<AuthSlot> {
    use std::sync::atomic::Ordering;
    if AUTH_PENDING.fetch_add(1, Ordering::SeqCst) >= AUTH_PENDING_MAX {
        AUTH_PENDING.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    Some(AuthSlot(()))
}

/// [`accept_client`] on a TCP connection, within [`AUTH_WAIT`]; the slot is given back either way.
/// After the token the connection waits as long as the core keeps it.
pub fn accept_tcp_client<R: BufRead>(stream: &std::net::TcpStream, reader: &mut R, token: &str, slot: AuthSlot) -> bool {
    let _ = stream.set_read_timeout(Some(AUTH_WAIT));
    let accepted = accept_client(reader, token);
    let _ = stream.set_read_timeout(None);
    drop(slot);
    accepted
}

/// A request on a fresh connection: `auth <token>`, then the command line.
pub fn write_request<W: Write>(writer: &mut W, token: &str, line: &str) -> io::Result<()> {
    write!(writer, "auth {token}\n{line}\n")?;
    writer.flush()
}

/// A helper's side: the connection's first line must be `auth <token>`. Nothing is answered either
/// way — a client that cannot prove the token learns nothing, and an HTTP request (`GET /…`,
/// `POST /…`) is dropped at its first line.
pub fn accept_client<R: BufRead>(reader: &mut R, token: &str) -> bool {
    let Ok(line) = read_line_capped(reader, AUTH_LINE_MAX) else {
        return false;
    };
    let presented = line.trim().strip_prefix("auth ").unwrap_or("").trim();
    presented.len() == token.len()
        && presented.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The longest reply line the core takes from a helper. The largest legitimate one is a
/// `fetch-many` over the relay directory's shelves: a few hundred hex beacons, ~3 MiB at worst.
pub const HELPER_REPLY_MAX: usize = 8 * 1024 * 1024;

/// The longest line a helper prints to stderr before its listening line.
pub const HELPER_BANNER_LINE_MAX: usize = 64 * 1024;

/// One line (with its `\n`, if any) of at most `max` bytes. A longer line is an error, not a
/// truncated reply: a cut-off list of beacons must not pass for the whole one. `Ok("")` is EOF.
pub fn read_line_capped<R: BufRead>(reader: &mut R, max: usize) -> io::Result<String> {
    let mut line = Vec::new();
    reader.take(max as u64 + 1).read_until(b'\n', &mut line)?;
    if line.len() > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("line over {max} bytes")));
    }
    String::from_utf8(line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blind audit 2026-11-13, pass 20: every connection took a thread that waited for its first
    /// line with no end, so connections that said nothing used a helper up. Only so many may wait
    /// to prove the token; a place comes back when one is done.
    #[test]
    fn only_so_many_connections_wait_to_prove_the_token() {
        let held: Vec<_> = (0..AUTH_PENDING_MAX).map(|_| reserve_auth_slot().expect("a place")).collect();
        assert!(reserve_auth_slot().is_none(), "no place past the limit");
        drop(held);
        assert!(reserve_auth_slot().is_some(), "places come back");
    }

    #[test]
    fn a_line_within_the_cap_is_read_whole_and_the_next_one_stays() {
        let mut input = io::Cursor::new(b"OK abc\nnext\n".to_vec());
        assert_eq!(read_line_capped(&mut input, 7).unwrap(), "OK abc\n");
        assert_eq!(read_line_capped(&mut input, 7).unwrap(), "next\n");
        assert_eq!(read_line_capped(&mut input, 7).unwrap(), "");
    }

    /// V-21: a connection reaches a helper's commands only after `auth <token>`; a wrong token, a
    /// command straight away, or an HTTP request is dropped at the first line.
    #[test]
    fn a_helper_takes_only_a_client_that_proved_the_token() {
        let mut request = Vec::new();
        write_request(&mut request, "t0ken", "nodes").unwrap();
        let mut reader = io::Cursor::new(request);
        assert!(accept_client(&mut reader, "t0ken"));
        assert_eq!(read_line_capped(&mut reader, HELPER_REQUEST_MAX).unwrap(), "nodes\n");

        for first in ["auth wrong\nnodes\n", "nodes\n", "auth \nnodes\n", "auth t0ke\n", "POST / HTTP/1.1\r\n\r\nnodes\n"] {
            assert!(!accept_client(&mut io::Cursor::new(first.as_bytes().to_vec()), "t0ken"), "{first:?}");
        }
    }

    /// V-12: a reply longer than the cap is refused after reading at most cap + 1 bytes,
    /// however long the helper keeps writing.
    #[test]
    fn a_line_over_the_cap_is_an_error_after_reading_only_the_cap() {
        let mut input = io::Cursor::new(vec![b'a'; 1 << 20]);
        let error = read_line_capped(&mut input, 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(input.position(), 1025);
    }
}
