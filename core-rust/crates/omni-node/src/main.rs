//! Headless Omni core — a thin driver over `omni-app` (no UI).
//!
//! One process = one node. It opens an `OmniApp` on `--app-dir`, then serves the Omni daemon
//! protocol (line + framed, with an events plane) over stdin or an IPC endpoint. The serving
//! code lives in the reusable `omni-daemon` lib (shared with the WinUI shell's embedded daemon);
//! this binary is just the CLI around it — arg parsing, an owned-`OmniApp` adapter, and the thin
//! clients. Modes:
//!   * default — stdin/stdout (the Phase 1 harness path, single-threaded);
//!   * `--listen <endpoint>` — a daemon. `<endpoint>` is a Windows named pipe (`\\.\pipe\…`,
//!     OS-ACL secured) or `host:port` (localhost TCP). The `OmniApp` + its mesh persist across
//!     client connections; each connection is served on its own thread. `shutdown` stops the
//!     daemon (snapshot + exit); `quit` ends one connection.
//!   * `--connect[-framed] <endpoint> <command>` / `--connect-framed-watch <endpoint>` — clients.
//!
//! It drives the same core the WinUI shell drives, but scriptably and with explicit topology
//! control (`mesh-start` skips auto-connect; `dial` builds links by hand). It is also the seed of
//! a P3 volunteer relay node. See docs/decisions/headless-omni-node.md +
//! docs/decisions/daemon-ipc-phase2.md + docs/decisions/embedded-daemon-in-shell.md.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use omni_app::{AppInitRequest, OmniApp};
use omni_daemon::{frame, AppAccess, Subscribers};

/// Adapts an `OmniApp` this process OWNS to the daemon lib's `AsOmniApp`.
struct OwnedApp(OmniApp);

impl AppAccess for OwnedApp {
    fn with_app(&self, f: &mut dyn FnMut(&OmniApp)) {
        f(&self.0)
    }
}

// SAFETY: `OmniApp` is `Send` but not `Sync`; the daemon serializes every access through
// `OmniApp::serializer` (see omni-daemon), so no two threads touch the rusqlite `Connection`
// at once — the same invariant omni-ffi's `HandleGuard` relies on.
unsafe impl Sync for OwnedApp {}

fn main() {
    let mut app_dir = String::new();
    let mut listen: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut connect_cmd = String::new();
    let mut connect_framed = false;
    let mut connect_watch = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app-dir" => app_dir = args.next().unwrap_or_default(),
            "--listen" => listen = args.next(),
            // `--connect` uses the line protocol; `--connect-framed` does the `frame`
            // handshake then one length-prefixed JSON request (proves the framed path).
            "--connect" | "--connect-framed" => {
                connect_framed = arg == "--connect-framed";
                connect = args.next();
                // Everything after the endpoint is the single command to send.
                connect_cmd = args.by_ref().collect::<Vec<_>>().join(" ");
                break;
            }
            // `--connect-framed-watch <addr>` opens a framed connection, sends `watch`, and
            // prints every received frame's raw JSON (responses AND pushed events) — proving
            // both ride one connection. Long-lived; the caller kills it.
            "--connect-framed-watch" => {
                connect_watch = true;
                connect = args.next();
                break;
            }
            _ => {}
        }
    }

    // Thin client: connect to a daemon, send one command, print the response, exit.
    if let Some(endpoint) = connect {
        let code = if connect_watch {
            run_client_framed_watch(&endpoint)
        } else if connect_framed {
            run_client_framed(&endpoint, &connect_cmd)
        } else {
            run_client(&endpoint, &connect_cmd)
        };
        std::process::exit(code);
    }

    if app_dir.is_empty() {
        eprintln!("usage: omni-node --app-dir <dir> [--listen <endpoint>]  |  --connect <endpoint> <command>");
        std::process::exit(2);
    }
    // `--app-dir` is a directory (matching the WinUI shell); the DB lives in it as
    // omni.db, so scripts can inspect the same file the GUI runs use.
    if let Err(error) = std::fs::create_dir_all(&app_dir) {
        eprintln!("cannot create app dir {app_dir}: {error}");
        std::process::exit(1);
    }
    let db_path = format!("{}\\omni.db", app_dir.trim_end_matches(['\\', '/']));

    let app = match OmniApp::init(AppInitRequest { db_path }) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("init failed: {error}");
            std::process::exit(1);
        }
    };

    match listen {
        // Daemon mode: serve the command protocol over an IPC endpoint via the omni-daemon lib.
        // `shutdown` snapshots durable state under the serializer then exits the process.
        Some(endpoint) => {
            let app = Arc::new(OwnedApp(app));
            let shutdown_app = Arc::clone(&app);
            let on_shutdown: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _guard = shutdown_app.0.serializer.lock();
                shutdown_app.0.stop_mesh();
                std::process::exit(0);
            });
            eprintln!("omni-node listening on {endpoint}");
            if let Err(error) = omni_daemon::serve_endpoint(&endpoint, app, on_shutdown) {
                eprintln!("cannot listen on {endpoint}: {error}");
                std::process::exit(1);
            }
        }
        // Default: stdin/stdout (the Phase 1 harness path — single threaded). No event pump
        // here, so `watch` only heartbeats; stdin scripts observe state by polling.
        None => {
            let stdin = io::stdin();
            let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
            omni_daemon::serve(&app, stdin.lock(), io::stdout(), &subscribers);
            app.stop_mesh();
        }
    }
}

/// Connect to a daemon endpoint and return split read/write halves, so the client functions
/// are transport-agnostic (mirrors the server-side `IpcListener`): a Windows pipe path
/// (`\\.\pipe\…`) uses the named-pipe transport, anything else is localhost TCP.
fn connect_endpoint(endpoint: &str) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
    #[cfg(windows)]
    if omni_daemon::is_pipe_endpoint(endpoint) {
        return omni_daemon::pipe::connect(endpoint);
    }
    let stream = TcpStream::connect(endpoint)?;
    let read_half = stream.try_clone()?;
    Ok((Box::new(BufReader::new(read_half)), Box::new(stream)))
}

/// Thin client: connect, send one command line, print the response, return an exit
/// code (1 if the response is an `ERR`, else 0).
fn run_client(endpoint: &str, command: &str) -> i32 {
    let (mut reader, mut writer) = match connect_endpoint(endpoint) {
        Ok(halves) => halves,
        Err(error) => {
            eprintln!("connect {endpoint}: {error}");
            return 1;
        }
    };
    if writeln!(writer, "{command}").is_err() {
        eprintln!("write failed");
        return 1;
    }
    let _ = writer.flush();
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(_) => {
            let line = line.trim_end();
            println!("{line}");
            i32::from(line.starts_with("ERR"))
        }
        Err(error) => {
            eprintln!("read failed: {error}");
            1
        }
    }
}

/// Thin client over the FRAMED protocol: connect, send the `frame` handshake line, then one
/// length-prefixed JSON request (id 1), print the response frame's `text`, and return an exit
/// code (1 on `ERR`). Proves the framed path end-to-end without juggling binary frames in a
/// shell; the line client (`run_client`) stays the default. See omni_daemon::frame.
fn run_client_framed(endpoint: &str, command: &str) -> i32 {
    let (mut reader, mut writer) = match connect_endpoint(endpoint) {
        Ok(halves) => halves,
        Err(error) => {
            eprintln!("connect {endpoint}: {error}");
            return 1;
        }
    };
    // Handshake is a normal line; the daemon replies `OK framed`, then both speak frames.
    if writeln!(writer, "frame").is_err() {
        eprintln!("write failed");
        return 1;
    }
    let _ = writer.flush();
    let mut ack = String::new();
    if reader.read_line(&mut ack).is_err() || !ack.starts_with("OK framed") {
        eprintln!("framed handshake failed: {}", ack.trim());
        return 1;
    }
    // Split the command the same way the line protocol does: first word = cmd, rest = arg.
    let mut parts = command.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("").to_string();
    let arg = parts.next().unwrap_or("").to_string();
    if frame::write_request(&mut writer, &frame::RequestFrame { id: 1, cmd, arg }).is_err() {
        eprintln!("write failed");
        return 1;
    }
    match frame::read_response(&mut reader) {
        Ok(Some(response)) => {
            println!("{}", response.text);
            i32::from(!response.ok)
        }
        Ok(None) => {
            eprintln!("no response");
            1
        }
        Err(error) => {
            eprintln!("read failed: {error}");
            1
        }
    }
}

/// Thin client that proves responses AND pushed events ride ONE framed connection: connect,
/// handshake, send a `watch` request (id 1), then print every received frame's raw JSON —
/// the `watch` ack response first, then pushed event frames as they arrive. Long-lived
/// (loops until EOF); the caller terminates it. See run_client_framed / omni_daemon::frame.
fn run_client_framed_watch(endpoint: &str) -> i32 {
    let (mut reader, mut writer) = match connect_endpoint(endpoint) {
        Ok(halves) => halves,
        Err(error) => {
            eprintln!("connect {endpoint}: {error}");
            return 1;
        }
    };
    if writeln!(writer, "frame").is_err() {
        eprintln!("write failed");
        return 1;
    }
    let _ = writer.flush();
    let mut ack = String::new();
    if reader.read_line(&mut ack).is_err() || !ack.starts_with("OK framed") {
        eprintln!("framed handshake failed: {}", ack.trim());
        return 1;
    }
    if frame::write_request(
        &mut writer,
        &frame::RequestFrame { id: 1, cmd: "watch".to_string(), arg: String::new() },
    )
    .is_err()
    {
        eprintln!("write failed");
        return 1;
    }
    loop {
        match frame::read_json_frame(&mut reader) {
            Ok(Some(json)) => {
                println!("{json}");
                let _ = io::stdout().flush();
            }
            Ok(None) => return 0, // daemon closed the connection
            Err(error) => {
                eprintln!("read failed: {error}");
                return 1;
            }
        }
    }
}
