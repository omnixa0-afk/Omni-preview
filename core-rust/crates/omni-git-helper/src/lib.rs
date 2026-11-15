//! `--relay <127.0.0.1:port> <token> [git service…]`: connect to the core's one-shot loopback entry,
//! prove the token (one line), then carry stdin to the socket and the socket to stdout until both
//! ends are done. git closing our stdin ends the way out (the socket's write half is shut, so the
//! peer's `upload-pack` sees the end); the socket's end ends the way back and the process.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::time::Duration;

/// Wrong arguments — the caller (git, through the core) misused us.
pub const USAGE_EXIT_CODE: i32 = 2;
/// The entry could not be reached or refused the token.
pub const CONNECT_EXIT_CODE: i32 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub fn run(args: Vec<String>) -> i32 {
    let mut args = args.into_iter();
    let (Some(flag), Some(addr), Some(token)) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: omni-git-helper --relay <127.0.0.1:port> <token> [service]");
        return USAGE_EXIT_CODE;
    };
    if flag != "--relay" {
        eprintln!("omni-git-helper: unknown mode {flag}");
        return USAGE_EXIT_CODE;
    }
    // Only ever a loopback entry: the token is a secret, not something to send across a network.
    let Ok(addr) = addr.parse::<SocketAddr>() else {
        eprintln!("omni-git-helper: {addr} is not an address");
        return USAGE_EXIT_CODE;
    };
    if !addr.ip().is_loopback() {
        eprintln!("omni-git-helper: {addr} is not a loopback address");
        return USAGE_EXIT_CODE;
    }
    match relay(addr, &token, std::io::stdin(), std::io::stdout()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("omni-git-helper: {error}");
            CONNECT_EXIT_CODE
        }
    }
}

/// Carry `input` to the entry and the entry's bytes to `output` (git's stdin/stdout in the process).
pub fn relay<R: Read + Send + 'static, W: Write>(addr: SocketAddr, token: &str, mut input: R, mut output: W) -> std::io::Result<()> {
    let mut socket = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    socket.set_nodelay(true)?;
    socket.write_all(format!("{token}\n").as_bytes())?;
    let mut out_half = socket.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
        while let Ok(n) = input.read(&mut buf) {
            if n == 0 || out_half.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = out_half.shutdown(Shutdown::Write);
    });
    let mut buf = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let n = socket.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        output.write_all(&buf[..n])?;
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn the_relay_proves_its_token_then_carries_both_ways_until_both_ends_close() {
        let entry = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = entry.local_addr().unwrap();
        let core = std::thread::spawn(move || {
            let (socket, _) = entry.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut token = String::new();
            reader.read_line(&mut token).unwrap();
            let mut got = Vec::new();
            reader.read_to_end(&mut got).unwrap(); // ends when git (our input) is done
            let mut socket = socket;
            socket.write_all(b"pack bytes").unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            (token, got)
        });
        let mut output = Vec::new();
        relay(addr, "t0k3n", std::io::Cursor::new(b"want abc\n0000".to_vec()), &mut output).unwrap();
        let (token, got) = core.join().unwrap();
        assert_eq!(token, "t0k3n\n");
        assert_eq!(got, b"want abc\n0000");
        assert_eq!(output, b"pack bytes");
    }

    #[test]
    fn only_a_loopback_entry_is_accepted() {
        assert_eq!(run(vec!["--relay".into(), "10.0.0.1:9".into(), "t".into()]), USAGE_EXIT_CODE);
        assert_eq!(run(vec!["--bogus".into(), "127.0.0.1:9".into(), "t".into()]), USAGE_EXIT_CODE);
        assert_eq!(run(vec!["--relay".into()]), USAGE_EXIT_CODE);
    }
}
