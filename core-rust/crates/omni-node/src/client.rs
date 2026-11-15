//! The daemon client side of `omni-node --connect…`: where to connect and with which token.

use std::io::{self, BufRead, Write};
use std::net::TcpStream;

/// Where a client connects, and the app dir whose `daemon.token` it may read.
pub(crate) struct Client {
    pub(crate) endpoint: String,
    pub(crate) app_dir: String,
}

impl Client {
    /// The TCP daemon's token: `OMNI_DAEMON_TOKEN`, else `<app-dir>/daemon.token`.
    fn token(&self) -> io::Result<String> {
        if let Ok(token) = std::env::var(omni_daemon::TOKEN_ENV) {
            if !token.trim().is_empty() {
                return Ok(token.trim().to_string());
            }
        }
        if self.app_dir.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no token: set {} or pass --app-dir before --connect", omni_daemon::TOKEN_ENV),
            ));
        }
        let path = std::path::Path::new(&self.app_dir).join(omni_daemon::TOKEN_FILE);
        Ok(std::fs::read_to_string(path)?.trim().to_string())
    }

    /// Connect and return split read/write halves, so the client functions are
    /// transport-agnostic (mirrors the server-side `IpcListener`): a Windows pipe path
    /// (`\\.\pipe\…`) uses the named-pipe transport, anything else is TCP, which proves the
    /// token first.
    pub(crate) fn connect(&self) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        #[cfg(windows)]
        if omni_daemon::is_pipe_endpoint(&self.endpoint) {
            return omni_daemon::pipe::connect(&self.endpoint);
        }
        let token = self.token()?;
        let stream = TcpStream::connect(&self.endpoint)?;
        // The token is proved, not sent; everything after goes sealed (omni_daemon::secure_channel).
        omni_daemon::secure_channel::connect(stream.try_clone()?, stream, &token)
    }
}
