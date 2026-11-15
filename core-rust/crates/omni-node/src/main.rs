//! Headless Omni core — a thin driver over `omni-app` (no UI).
//!
//! One process = one node. It opens an `OmniApp` on `--app-dir`, then serves the Omni daemon
//! protocol (line + framed, with an events plane) over stdin or an IPC endpoint. The serving
//! code lives in the reusable `omni-daemon` lib (shared with the WinUI shell's embedded daemon);
//! this binary is just the CLI around it — arg parsing, an owned-`OmniApp` adapter, and the thin
//! clients. Modes:
//!   * default — stdin/stdout (the Phase 1 harness path, single-threaded);
//!   * `--listen <endpoint>` — a daemon. `<endpoint>` is a Windows named pipe (`\\.\pipe\…`,
//!     OS-ACL secured) or `host:port` (TCP). The `OmniApp` + its mesh persist across
//!     client connections; each connection is served on its own thread. `shutdown` stops the
//!     daemon (snapshot + exit); `quit` ends one connection. TCP takes a token (V-11): from
//!     `OMNI_DAEMON_TOKEN`, else a random one, written to `<app-dir>/daemon.token`; it binds
//!     loopback only unless `--allow-remote` is given (a stand driven from another host).
//!     `send-file` / `recv-file` read and write only inside `--files-dir` (default
//!     `<app-dir>/files`; V-11).
//!   * `--connect[-framed] <endpoint> <command>` / `--connect-framed-watch <endpoint>` — clients.
//!     Over TCP they send `auth <token>` first: the token from `OMNI_DAEMON_TOKEN`, else from
//!     `<app-dir>/daemon.token` when `--app-dir` comes before `--connect`.
//!
//! It drives the same core the WinUI shell drives, but scriptably and with explicit topology
//! control (`mesh-start` skips auto-connect; `dial` builds links by hand). It is also the seed of
//! a P3 volunteer relay node. See docs/decisions/headless-omni-node.md +
//! docs/decisions/daemon-ipc-phase2.md + docs/decisions/embedded-daemon-in-shell.md.

use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use omni_app::{AppInitRequest, OmniApp};
use omni_daemon::{frame, AppAccess, Subscribers};

mod client;
use client::Client;

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
    let mut allow_remote = false;
    let mut files_dir: Option<String> = None;
    let mut restore: Option<(String, String)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--app-dir" => app_dir = args.next().unwrap_or_default(),
            "--listen" => listen = args.next(),
            "--allow-remote" => allow_remote = true,
            "--files-dir" => files_dir = args.next(),
            // `--restore-backup <backup> <phrase-file>` — before opening, put the profile back from a
            // backup, the way the shell's restore does (inspection by the phrase, then import). For a
            // stand checking what a restored device does.
            "--restore-backup" => {
                restore = args.next().zip(args.next());
            }
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
        let client = Client { endpoint, app_dir };
        let code = if connect_watch {
            run_client_framed_watch(&client)
        } else if connect_framed {
            run_client_framed(&client, &connect_cmd)
        } else {
            run_client(&client, &connect_cmd)
        };
        std::process::exit(code);
    }

    if app_dir.is_empty() {
        eprintln!(
            "usage: omni-node --app-dir <dir> [--files-dir <dir>] [--restore-backup <backup> <phrase-file>] [--listen <endpoint> [--allow-remote]]  |  [--app-dir <dir>] --connect <endpoint> <command>"
        );
        std::process::exit(2);
    }
    // `--app-dir` is a directory (matching the WinUI shell); the DB lives in it as
    // omni.db, so scripts can inspect the same file the GUI runs use.
    if let Err(error) = std::fs::create_dir_all(&app_dir) {
        eprintln!("cannot create app dir {app_dir}: {error}");
        std::process::exit(1);
    }
    let db_path = format!("{}\\omni.db", app_dir.trim_end_matches(['\\', '/']));
    let files_dir = files_dir.unwrap_or_else(|| format!("{}\\files", app_dir.trim_end_matches(['\\', '/'])));
    if let Err(error) = omni_daemon::set_files_root(std::path::Path::new(&files_dir)) {
        eprintln!("cannot use the files dir {files_dir}: {error}");
        std::process::exit(1);
    }

    if let Some((backup, phrase_file)) = restore {
        let restored = std::fs::read_to_string(&phrase_file)
            .map_err(|error| format!("cannot read {phrase_file}: {error}"))
            .and_then(|phrase| {
                OmniApp::inspect_account_backup_manifest(&backup, phrase.trim()).map_err(|error| error.to_string())
            })
            .and_then(|inspection| {
                OmniApp::import_database_backup_bundle(&db_path, &backup, &inspection.inspection_token)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = restored {
            eprintln!("restore from {backup} failed: {error}");
            std::process::exit(1);
        }
        eprintln!("restored from {backup}");
    }

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
            let tcp = if is_pipe(&endpoint) {
                None
            } else {
                match omni_daemon::TcpPolicy::for_app_dir(std::path::Path::new(&app_dir), allow_remote) {
                    Ok(policy) => Some(policy),
                    Err(error) => {
                        eprintln!("cannot write the daemon token in {app_dir}: {error}");
                        std::process::exit(1);
                    }
                }
            };
            eprintln!("omni-node listening on {endpoint}");
            if let Err(error) = omni_daemon::serve_endpoint(&endpoint, app, on_shutdown, tcp) {
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

fn is_pipe(endpoint: &str) -> bool {
    #[cfg(windows)]
    return omni_daemon::is_pipe_endpoint(endpoint);
    #[cfg(not(windows))]
    {
        let _ = endpoint;
        false
    }
}

/// Thin client: connect, send one command line, print the response, return an exit
/// code (1 if the response is an `ERR`, else 0).
fn run_client(client: &Client, command: &str) -> i32 {
    let endpoint = &client.endpoint;
    let (mut reader, mut writer) = match client.connect() {
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
fn run_client_framed(client: &Client, command: &str) -> i32 {
    let endpoint = &client.endpoint;
    let (mut reader, mut writer) = match client.connect() {
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
fn run_client_framed_watch(client: &Client) -> i32 {
    let endpoint = &client.endpoint;
    let (mut reader, mut writer) = match client.connect() {
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
