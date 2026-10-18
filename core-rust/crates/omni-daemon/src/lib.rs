//! Reusable daemon-serving core for Omni.
//!
//! This is the Phase-2 daemon (a line + framed command protocol with an events plane over a
//! pluggable IPC transport — localhost TCP or a Windows named pipe) factored out of the
//! `omni-node` binary so it can serve **two** consumers:
//!   * `omni-node --listen` — a standalone headless daemon owning its `OmniApp`;
//!   * the WinUI shell's **embedded daemon** (Phase 3) — serving the protocol against the
//!     `OmniApp` the shell already runs in-process behind the FFI, so an external client drives
//!     the live app and the shell reflects it (no GUI automation).
//!
//! Both share the exact same protocol, transports, and event plane. The only thing that
//! differs is *who owns the `OmniApp`* and *what `shutdown` does* — captured by the tiny
//! [`AsOmniApp`] trait and an `on_shutdown` hook. Every `handle()` dispatch and event drain is
//! taken under `OmniApp::serializer`, mirroring the omni-ffi `HandleGuard` invariant, so the
//! daemon's connection threads never touch the rusqlite `Connection` concurrently with each
//! other or with the shell's FFI calls.
//!
//! See docs/decisions/daemon-ipc-phase2.md + docs/decisions/embedded-daemon-in-shell.md.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_app::{CoreEvent, DiscoverabilityMode, OmniApp};
use omni_identity::{AccountId, ContactRelationshipState};

pub mod frame;
#[cfg(windows)]
pub mod pipe;

/// Access to the shared `OmniApp` a daemon serves. Implemented by the binary that owns the app
/// (omni-node) and by the FFI shim that borrows it behind a handle (the shell). `Send + Sync`
/// is required because the app is shared across the daemon's per-connection threads; the impl
/// asserts `Sync` under the serializer invariant (the app's `Connection` is only ever touched
/// under `OmniApp::serializer`).
pub trait AsOmniApp: AppAccess + Send + Sync {}

impl<T: AppAccess + Send + Sync> AsOmniApp for T {}

/// The app for ONE dispatch. The shell's core is not there for the process lifetime: importing a
/// backup frees it and opens another (docs/bugs/the-daemon-touched-a-freed-core.md), so the daemon
/// takes the app per call and gets nothing while there is none — never a reference kept across.
pub trait AppAccess {
    fn with_app(&self, f: &mut dyn FnMut(&OmniApp));
}

impl AppAccess for OmniApp {
    fn with_app(&self, f: &mut dyn FnMut(&OmniApp)) {
        f(self)
    }
}

/// `f` on the app, or `None` while there is no app.
fn call<S: AppAccess + ?Sized, R>(source: &S, f: impl FnOnce(&OmniApp) -> R) -> Option<R> {
    let mut f = Some(f);
    let mut out = None;
    source.with_app(&mut |app| {
        if let Some(f) = f.take() {
            out = Some(f(app));
        }
    });
    out
}

const NO_CORE: &str = "ERR the core is restarting";

/// A daemon endpoint is a Windows named pipe (vs localhost TCP) iff it has the UNC prefix
/// `\\` (e.g. `\\.\pipe\omni`). Both the server (`--listen`) and the clients dispatch on this.
#[cfg(windows)]
pub fn is_pipe_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with(r"\\")
}

/// Live event subscribers — one channel per active `watch` connection (line OR framed).
/// The pump drains the core hook once and broadcasts each `CoreEvent` to every subscriber;
/// each subscriber formats for its OWN protocol (line watch → `EVENT ...` text, framed watch
/// → a JSON event frame). Any number of clients can watch at once (no destructive contention).
pub type Subscribers = Arc<Mutex<Vec<Sender<CoreEvent>>>>;

/// Background pump: every ~250 ms it drains the core event hook (under the serializer) and
/// broadcasts each `CoreEvent` to every live `watch` subscriber. Formatting is deferred to
/// the subscriber so one pump feeds both the line and framed protocols. With no subscribers
/// the events are still drained and dropped (live-tail). Closed subscriber channels are
/// reaped here on the next broadcast. The single event source — see daemon-ipc-phase2.md.
pub fn spawn_event_pump<A: AsOmniApp + 'static>(app: Arc<A>, subscribers: Subscribers) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        let events: Vec<CoreEvent> = call(&*app, |app| {
            let _guard = app.serializer.lock();
            app.drain_events()
        })
        .unwrap_or_default();
        if events.is_empty() {
            continue;
        }
        let mut subs = subscribers.lock().unwrap_or_else(|p| p.into_inner());
        if subs.is_empty() {
            continue; // live-tail: nobody watching, drop the drained events
        }
        for event in events {
            subs.retain(|tx| tx.send(event.clone()).is_ok());
        }
    });
}

/// Format a `CoreEvent` for the line protocol's `watch` stream.
fn format_event_line(event: &CoreEvent) -> String {
    match event {
        CoreEvent::InboundMessage {
            dialog_key,
            message_id,
            body,
        } => format!("EVENT message {dialog_key} {message_id} {body}"),
        CoreEvent::LinkState { peer, up } => {
            format!("EVENT linkstate {peer} {}", if *up { "up" } else { "down" })
        }
        CoreEvent::IncomingCall {
            dialog_key,
            call_id,
        } => format!("EVENT call {dialog_key} {call_id} ringing"),
        CoreEvent::GroupChanged { group_id } => format!("EVENT group {group_id} changed"),
    }
}

/// Map a `CoreEvent` to its framed-protocol JSON event frame (the structured twin of
/// `format_event_line`).
fn event_to_frame(event: &CoreEvent) -> frame::EventFrame {
    match event {
        CoreEvent::InboundMessage {
            dialog_key,
            message_id,
            body,
        } => frame::EventFrame::Message {
            dialog: dialog_key.clone(),
            id: message_id.clone(),
            body: body.clone(),
        },
        CoreEvent::LinkState { peer, up } => frame::EventFrame::Linkstate {
            peer: peer.clone(),
            up: *up,
        },
        CoreEvent::IncomingCall {
            dialog_key,
            call_id,
        } => frame::EventFrame::Call {
            dialog: dialog_key.clone(),
            call_id: call_id.clone(),
            state: "ringing".to_string(),
        },
        CoreEvent::GroupChanged { group_id } => frame::EventFrame::Group { group_id: group_id.clone() },
    }
}

/// A bound IPC endpoint the daemon accepts client connections on. Abstracts the transport
/// (localhost TCP; a Windows named pipe with OS-ACL security) so the protocol code (`serve`)
/// and the accept loop (`run_daemon`) stay transport-agnostic. See daemon-ipc-phase2.md §IPC.
pub trait IpcListener {
    /// Accept one client and return its split read/write halves. Both move onto the
    /// connection's thread (and the write half is then shared with the event pusher), so
    /// both must be `Send` and `'static`.
    fn accept(&self) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)>;
}

/// TCP-over-localhost transport: dependency-free and cross-platform, but NOT access-
/// controlled — any local process that can reach 127.0.0.1 may drive the core. The
/// named-pipe (Windows) transport replaces this for production (OS ACL).
pub struct TcpIpcListener {
    listener: TcpListener,
}

impl TcpIpcListener {
    pub fn bind(addr: &str) -> io::Result<Self> {
        Ok(Self { listener: TcpListener::bind(addr)? })
    }

    pub fn describe(&self) -> String {
        self.listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| "127.0.0.1:?".to_string())
    }
}

impl IpcListener for TcpIpcListener {
    fn accept(&self) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        let (stream, _addr) = self.listener.accept()?;
        // Duplicate the socket so one half reads requests while the other writes responses
        // and pushes events (those two share the write half behind a mutex in serve_framed).
        let read_half = stream.try_clone()?;
        Ok((Box::new(BufReader::new(read_half)), Box::new(stream)))
    }
}

/// The transport-agnostic daemon accept loop: accept a client, serve it on its own thread
/// (so a `watch` stream and command requests can be in flight at once), repeat. When a client
/// asks the whole daemon to `shutdown`, `on_shutdown` is invoked — omni-node snapshots durable
/// state and `process::exit`s; the embedded shell daemon treats it as a no-op (the shell owns
/// its lifecycle). Generic over the `IpcListener` so it serves TCP or a named pipe unchanged.
pub fn run_daemon<A, L>(
    listener: &L,
    app: Arc<A>,
    subscribers: Subscribers,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
) where
    A: AsOmniApp + 'static,
    L: IpcListener,
{
    loop {
        let (reader, writer) = match listener.accept() {
            Ok(connection) => connection,
            Err(_) => continue, // transient accept error — keep serving
        };
        let app = Arc::clone(&app);
        let subscribers = Arc::clone(&subscribers);
        let on_shutdown = Arc::clone(&on_shutdown);
        std::thread::spawn(move || {
            if serve(&*app, reader, writer, &subscribers) {
                on_shutdown();
            }
        });
    }
}

/// Bind `endpoint` (named pipe iff `\\…`, else localhost TCP), spawn the event pump, and run
/// the accept loop (blocks). Returns `Err` only on a bind failure; otherwise it runs until
/// `on_shutdown` ends the process (omni-node) or the host process exits (embedded shell).
pub fn serve_endpoint<A: AsOmniApp + 'static>(
    endpoint: &str,
    app: Arc<A>,
    on_shutdown: Arc<dyn Fn() + Send + Sync>,
) -> io::Result<()> {
    let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
    spawn_event_pump(Arc::clone(&app), Arc::clone(&subscribers));
    #[cfg(windows)]
    if is_pipe_endpoint(endpoint) {
        let listener = pipe::PipeListener::bind(endpoint)?;
        run_daemon(&listener, app, subscribers, on_shutdown);
        return Ok(());
    }
    let listener = TcpIpcListener::bind(endpoint)?;
    run_daemon(&listener, app, subscribers, on_shutdown);
    Ok(())
}

/// Read commands line-by-line, dispatch via `handle`, write one response line each.
/// Returns true if the peer asked the whole daemon to `shutdown` (vs `quit`, which
/// just ends this connection/stream). Shared by the stdin and IPC paths. Every
/// `handle()` dispatch is taken under `app.serializer` so concurrent connection threads
/// never touch the shared rusqlite `Connection` at once (mirrors the omni-ffi
/// `HandleGuard`). `subscribers` is the fan-out registry a `watch` connection joins.
pub fn serve<S: AppAccess + ?Sized, R: BufRead, W: Write + Send + 'static>(
    app: &S,
    mut reader: R,
    mut writer: W,
    subscribers: &Subscribers,
) -> bool {
    // Read lines manually (not `reader.lines()`) so `reader` stays a movable binding we can
    // hand to `serve_framed` after the `frame` handshake.
    let mut raw = String::new();
    loop {
        raw.clear();
        match reader.read_line(&mut raw) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        // Strip a leading UTF-8 BOM (some shells prepend one to the first input line).
        let line = raw.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        // Handshake to framed mode: the REST of this connection speaks length-prefixed JSON
        // frames (request-id correlation + responses AND events on one connection). The line
        // protocol stays the default; only a client that opts in by sending `frame` switches.
        if cmd == "frame" {
            if writeln!(writer, "OK framed").is_err() {
                return false;
            }
            let _ = writer.flush();
            return serve_framed(app, reader, writer, subscribers);
        }
        if cmd == "shutdown" {
            let _ = writeln!(writer, "OK bye");
            let _ = writer.flush();
            return true;
        }
        // `watch` joins this connection to the event fan-out: the daemon's pump pushes
        // `EVENT message <dialog> <id> <body>` for each NEW message in ANY dialog, and
        // an `EVENT ping` every ~5s of silence detects a dropped client. Many clients
        // can watch at once (the pump broadcasts). Subscription is live-tail: only
        // events that arrive after `watch` are delivered (use list/poll for catch-up).
        // The client filters by `<dialog>` itself. See daemon-ipc-phase2.md §Events.
        if cmd == "watch" {
            let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
            subscribers.lock().unwrap_or_else(|p| p.into_inner()).push(tx);
            if writeln!(writer, "OK watching").is_err() {
                return false;
            }
            let _ = writer.flush();
            loop {
                match rx.recv_timeout(Duration::from_millis(5000)) {
                    Ok(event) => {
                        if writeln!(writer, "{}", format_event_line(&event)).is_err() {
                            return false; // client gone; pump reaps the channel
                        }
                        let _ = writer.flush();
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if writeln!(writer, "EVENT ping").is_err() {
                            return false;
                        }
                        let _ = writer.flush();
                    }
                    Err(RecvTimeoutError::Disconnected) => return false,
                }
            }
        }
        let response = call(app, |app| {
            let _guard = app.serializer.lock();
            handle(app, cmd, rest)
        })
        .unwrap_or_else(|| NO_CORE.to_string());
        if writeln!(writer, "{response}").is_err() {
            break;
        }
        let _ = writer.flush();
        if cmd == "quit" {
            break; // end this connection only
        }
    }
    false
}

/// Serve the framed protocol on one connection after the `frame` handshake: read
/// length-prefixed JSON request frames, dispatch each via `handle` under the serializer,
/// and write a response frame echoing the request `id` (so a client can pipeline requests
/// and match replies). A `watch` request additionally turns this connection into an event
/// tail — the daemon PUSHES event frames on the SAME socket while the client keeps sending
/// requests, which is the thing the line protocol needs a second connection for. Returns
/// true iff a client asked the whole daemon to `shutdown` (vs `quit`, which ends just this
/// connection). See frame.rs + docs/decisions/daemon-ipc-phase2.md §Protocol.
fn serve_framed<S: AppAccess + ?Sized, R: BufRead, W: Write + Send + 'static>(
    app: &S,
    mut reader: R,
    writer: W,
    subscribers: &Subscribers,
) -> bool {
    // The writer is shared between this thread (responses) and — once the client sends a
    // `watch` — an event-push thread, so a mutex keeps the two from interleaving frames at
    // the byte level on the one socket.
    let writer = Arc::new(Mutex::new(writer));
    let mut watching = false;
    loop {
        let request = match frame::read_request(&mut reader) {
            Ok(Some(request)) => request,
            Ok(None) => return false, // clean EOF — peer closed the connection
            Err(_) => return false,   // malformed frame / io error — drop the connection
        };
        if request.cmd == "shutdown" {
            let mut guard = writer.lock().unwrap_or_else(|p| p.into_inner());
            let _ = frame::write_response(&mut *guard, request.id, "OK bye");
            return true;
        }
        if request.cmd == "watch" {
            // Ack on this connection, then (first time only) subscribe + spawn the pusher.
            {
                let mut guard = writer.lock().unwrap_or_else(|p| p.into_inner());
                if frame::write_response(&mut *guard, request.id, "OK watching").is_err() {
                    return false;
                }
            }
            if !watching {
                watching = true;
                let (tx, rx) = std::sync::mpsc::channel::<CoreEvent>();
                subscribers.lock().unwrap_or_else(|p| p.into_inner()).push(tx);
                let event_writer = Arc::clone(&writer);
                std::thread::spawn(move || loop {
                    let event_frame = match rx.recv_timeout(Duration::from_millis(5000)) {
                        Ok(event) => event_to_frame(&event),
                        Err(RecvTimeoutError::Timeout) => frame::EventFrame::Ping,
                        Err(RecvTimeoutError::Disconnected) => return,
                    };
                    let mut guard = event_writer.lock().unwrap_or_else(|p| p.into_inner());
                    if frame::write_event(&mut *guard, &event_frame).is_err() {
                        return; // client gone; the pump reaps the channel once rx drops
                    }
                });
            }
            continue;
        }
        let text = call(app, |app| {
            let _guard = app.serializer.lock();
            handle(app, &request.cmd, request.arg.trim())
        })
        .unwrap_or_else(|| NO_CORE.to_string());
        {
            let mut guard = writer.lock().unwrap_or_else(|p| p.into_inner());
            if frame::write_response(&mut *guard, request.id, &text).is_err() {
                return false;
            }
        }
        if request.cmd == "quit" {
            return false; // end this connection only
        }
    }
}

/// Dispatch one command against the core. Pure request/response; the caller holds the
/// serializer lock. Returns an `OK ...` / `ERR ...` reply string.
fn handle(app: &OmniApp, cmd: &str, rest: &str) -> String {
    match cmd {
        "create-profile" => {
            let mut a = rest.splitn(2, ' ');
            let name = a.next().filter(|s| !s.is_empty()).unwrap_or("Node");
            let device = a.next().filter(|s| !s.is_empty()).unwrap_or("Device");
            match app.create_account(name, device) {
                Ok(account) => {
                    if let Some(profile) = account.local_profile_summary.as_ref() {
                        if let Err(error) = app.select_active_profile(&profile.profile_id) {
                            return format!("ERR select profile: {error}");
                        }
                    }
                    "OK".to_string()
                }
                Err(error) => format!("ERR {error}"),
            }
        }
        // Activate the saved profile (after a process relaunch on an existing dir).
        "sign-in" => match app.list_local_profiles() {
            Ok(list) => match list.profiles.first() {
                Some(profile) => match app.select_active_profile(&profile.profile_id) {
                    Ok(()) => "OK".to_string(),
                    Err(error) => format!("ERR {error}"),
                },
                None => "ERR no saved profile".to_string(),
            },
            Err(error) => format!("ERR {error}"),
        },
        "account-id" => match app.get_local_account() {
            Ok(Some(account)) => format!("OK {}", account.descriptor.account_id.0),
            Ok(None) => "ERR no account".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        "mesh-invite" => match app.generate_mesh_pairing_invite() {
            Ok(token) => format!("OK {token}"),
            Err(error) => format!("ERR {error}"),
        },
        "pair" => match app.register_mesh_pairing_invite(rest) {
            Ok(node) => format!("OK {node}"),
            Err(error) => format!("ERR {error}"),
        },
        // Mesh start WITHOUT auto-connect: the driver builds links explicitly via
        // `dial`, so the topology is deterministic (e.g. A and C never link).
        "mesh-start" => match app.start_mesh_no_autoconnect() {
            Ok(()) => "OK".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        // Mesh start WITH autoconnect. A loopback (same-host) UDP bind can only reach a
        // base relay when the carrier is the TCP relay (`OMNI_RELAY_TCP` replaces the UDP
        // socket with a TCP tunnel — see quic_interface.rs). A *pure-UDP* node bound to
        // 127.0.0.1 cannot dial a non-loopback relay at all: the dial fails with the
        // misleading "no reachable address … (no verified announce?)" — it is really a
        // non-routable local socket, NOT a missing announce. So: TCP-relay client → keep
        // the proven same-host path; otherwise bind the routable LAN like mesh-start-lan.
        "mesh-start-auto" => {
            let started = if std::env::var_os("OMNI_RELAY_TCP").is_some() {
                app.start_mesh()
            } else {
                app.start_mesh_networked()
            };
            match started {
                Ok(()) => "OK".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // Mesh start bound to the detected LAN address (real cross-host) + autoconnect.
        "mesh-start-lan" => match app.start_mesh_networked() {
            Ok(()) => "OK".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        // R1a carrier policy (axis A). `carrier-policy` reads it; `carrier-policy <mode>` sets it
        // (tcp | auto | quic); applies to a running mesh at once (`OmniApp::set_carrier_policy`).
        // dev/validation verb; the GUI has the same picker (Settings → «Для опытных»).
        "carrier-policy" => {
            if rest.trim().is_empty() {
                format!("OK {}", app.carrier_policy().as_db_value())
            } else {
                match omni_types::CarrierPolicy::from_db_value(rest.trim()) {
                    Ok(policy) => match app.set_carrier_policy(policy) {
                        Ok(()) => "OK".to_string(),
                        Err(error) => format!("ERR {error}"),
                    },
                    Err(error) => format!("ERR {error}"),
                }
            }
        }
        // The public UPnP-IGD external endpoint, if the router yielded one (cross-NAT
        // reachability diagnostic). "OK none" if not mapped / loopback / CGNAT.
        "upnp" => match app.mesh_upnp_endpoint() {
            Some(endpoint) => format!("OK {endpoint}"),
            None => "OK none".to_string(),
        },
        // This host's global IPv6 source address, if any — a pure host probe (no mesh
        // needed) for the cheapest cross-network reachability path: two global-IPv6 peers
        // dial directly, no NAT/relay. "OK none" on an IPv4-only network.
        "ipv6" => match app.mesh_global_ipv6() {
            Some(addr) => format!("OK {addr}"),
            None => "OK none".to_string(),
        },
        // Task 3 — our UPnP-mapped direct-TCP front door (advertised in beacon `tcp_direct`), if any.
        // Headless hook to verify the producer maps + exposes the TCP fallback endpoint on a live node.
        "tcp-endpoint" => match app.mesh_direct_tcp_endpoint() {
            Some(addr) => format!("OK {addr}"),
            None => "OK none".to_string(),
        },
        // Human-readable node reachability for the active mesh node (directly reachable via
        // UPnP/IPv6 vs behind NAT vs connecting) — formatted from the honest connection-state
        // DTO (the same source the shell renders); headless hook for validating a live core.
        "reachability" => match app.mesh_connection_state() {
            Ok(state) => {
                let node = &state.node;
                let line = match node.kind {
                    omni_app::NodeConnectivityKind::NotStarted => "mesh not started".to_string(),
                    omni_app::NodeConnectivityKind::Isolated => {
                        "isolated (no LAN / external address)".to_string()
                    }
                    omni_app::NodeConnectivityKind::Connecting => "connecting".to_string(),
                    omni_app::NodeConnectivityKind::OnlineBehindNat => {
                        "online, behind NAT (reach via mesh/relay)".to_string()
                    }
                    omni_app::NodeConnectivityKind::OnlineReachable if node.endpoints.is_empty() => {
                        "online, directly reachable".to_string()
                    }
                    omni_app::NodeConnectivityKind::OnlineReachable => {
                        format!("online, directly reachable: {}", node.endpoints.join(", "))
                    }
                };
                // Per-contact link + the Tier-2 connection trace (how it's connecting / where it
                // stalled) — the headless hook for validating the live trace from a real shell.
                // Single line: the daemon line-protocol is newline-delimited, so the per-contact
                // links + trace are appended with inline separators ("||" between contacts,
                // "[stage/status:detail; …]" for the trace), never "\n".
                let mut out = format!("OK {line}");
                for c in &state.contacts {
                    let link = match c.link {
                        omni_app::ContactLinkKind::OnlineDirect => "online_direct".to_string(),
                        omni_app::ContactLinkKind::OnlineViaRelay => {
                            format!("online_via_relay({} hops)", c.relay_hops.unwrap_or(0))
                        }
                        omni_app::ContactLinkKind::Searching => "searching".to_string(),
                        omni_app::ContactLinkKind::WillDeliverLater => "will_deliver_later".to_string(),
                        omni_app::ContactLinkKind::CannotReach => {
                            format!("cannot_reach({:?})", c.cannot_reach_reason)
                        }
                    };
                    out.push_str(&format!(" || {} {link}", c.account_id));
                    if !c.trace.is_empty() {
                        let steps: Vec<String> = c
                            .trace
                            .iter()
                            .map(|step| {
                                let detail = step.detail.as_deref().unwrap_or("");
                                format!("{:?}/{:?}:{detail}", step.stage, step.status)
                            })
                            .collect();
                        out.push_str(&format!(" [{}]", steps.join("; ")));
                    }
                }
                out
            }
            Err(error) => format!("ERR {error}"),
        },
        // Connection-privacy mode. `privacy` reads it; `privacy <auto|relay-only|direct-only>` sets it.
        // Relay-only forbids the direct upgrade — stay relay-mediated so a contact can't learn my IP
        // (tcp-direct follow-up d). Direct-only disables all third-party rendezvous (LAN + port-map +
        // manual exchange only).
        "privacy" => {
            if rest.trim().is_empty() {
                format!("OK {}", app.connection_privacy_mode().as_db_value())
            } else {
                match omni_app::ConnectionPrivacyMode::from_db_value(rest) {
                    Ok(mode) => match app.set_connection_privacy_mode(mode) {
                        Ok(()) => format!("OK {}", mode.as_db_value()),
                        Err(error) => format!("ERR {error}"),
                    },
                    Err(error) => format!("ERR {error}"),
                }
            }
        }
        // Model A (find-by-Omni-ID) — dev/validation equivalents of Settings → "Обнаружение по ID".
        // `omni-id` prints my shareable `omni:<hex>`; `discoverability [off|public]` gets (empty)
        // or sets findability (the rendezvous sweep publishes a signed identity beacon when public);
        // `find-omni-id <omni:hex> [petname]` fetches+verifies a peer's beacon from rendezvous and
        // sends a consent-gated contact request. See app_discoverability.rs.
        "omni-id" => match app.get_my_omni_id() {
            Ok(id) => format!("OK {id}"),
            Err(error) => format!("ERR {error}"),
        },
        "discoverability" => match rest.trim() {
            "" => format!("OK {}", app.discoverability_mode().as_db_value()),
            "off" | "public" => {
                let mode = DiscoverabilityMode::from_db_value(rest.trim());
                match app.set_discoverability_mode(mode) {
                    Ok(()) => format!("OK {}", mode.as_db_value()),
                    Err(error) => format!("ERR {error}"),
                }
            }
            other => format!("ERR unknown mode '{other}' (want off|public)"),
        },
        // Dev/validation only — GUI остаётся основным путём (Настройки → «Провайдеры рандеву»).
        // Без этой команды headless-нода вообще не могла включить DHT-ногу, то есть двухмашинный
        // стенд был обязан тестировать конфигурацию, которой у пользователя нет.
        "dht-rendezvous" => match rest.trim() {
            "" => format!("OK {}", if app.dht_rendezvous_enabled() { "on" } else { "off" }),
            mode @ ("on" | "off") => match app.set_dht_rendezvous_enabled(mode == "on") {
                Ok(()) => format!("OK {mode}"),
                Err(error) => format!("ERR {error}"),
            },
            other => format!("ERR unknown mode '{other}' (want on|off)"),
        },
        // Размер таблицы маршрутизации движка — честный признак «мы в рою», а не «нога включена».
        // Нужен стенду выживания сегмента (scripts/dht-restart-survival.ps1).
        "dht-nodes" => format!("OK {}", app.dht_rendezvous_status().node_count),
        "dht-entry-node" => match rest.trim() {
            "" => format!("OK {}", if app.dht_entry_node_enabled() { "on" } else { "off" }),
            mode @ ("on" | "off") => match app.set_dht_entry_node_enabled(mode == "on") {
                Ok(()) => format!("OK {mode}"),
                Err(error) => format!("ERR {error}"),
            },
            other => format!("ERR unknown mode '{other}' (want on|off)"),
        },
        // Public relay directory — dev/validation until the «Релеи» screen (stage 4 of
        // docs/decisions/public-relay-directory.md). `public-relay` gets the switch, `public-relay on
        // [name]` / `off` sets it; `find-relays` reads the directory, relays
        // separated by " || " (the line protocol is newline-delimited).
        "public-relay" => {
            let mut a = rest.trim().splitn(2, ' ');
            match (a.next().unwrap_or(""), a.next().map(str::trim).unwrap_or("")) {
                ("", _) => {
                    let role = app.public_relay_role();
                    format!("OK {} {}", if role.on { "on" } else { "off" }, role.name)
                }
                (mode @ ("on" | "off"), name) => match app.set_public_relay_role(mode == "on", name) {
                    Ok(()) => format!("OK {mode}"),
                    Err(error) => format!("ERR {error}"),
                },
                (other, _) => format!("ERR unknown mode '{other}' (want on [name]|off)"),
            }
        }
        "find-relays" => match app.search_public_relays() {
            Ok(result) => {
                let mut out = format!("OK {} answered={} relays={}", result.status, result.answered.join(","), result.relays.len());
                for relay in result.relays {
                    out.push_str(&format!(
                        " || {} {:?} load={} seniority={}s score={:.1} on={} udp={} tcp={}",
                        relay.node_id,
                        relay.name,
                        relay.load,
                        relay.seniority_secs,
                        relay.score,
                        relay.found_on.join(","),
                        relay.udp.join(","),
                        relay.tcp.join(",")
                    ));
                }
                out
            }
            Err(error) => format!("ERR {error}"),
        },
        // Pinned relay (stage 3): `pin-relay` — the state; `pin-relay <node> <udp> [tcp=<ip:port>]
        // [always] [hidden] [name=<name>]` — go through it; `pin-relay off` — stop.
        "pin-relay" => {
            let args: Vec<&str> = rest.split_whitespace().collect();
            match args.as_slice() {
                [] => {
                    let state = app.pinned_relay_state();
                    match state.relay {
                        None => "OK none".to_string(),
                        Some(relay) => format!(
                            "OK {} {:?} udp={} tcp={} always={} hidden={} linked={} lost_secs={}",
                            relay.node_id,
                            relay.name,
                            relay.udp,
                            relay.tcp.unwrap_or_default(),
                            relay.always,
                            relay.keep_address_hidden,
                            state.linked,
                            state.lost_secs.map_or("-".to_string(), |secs| secs.to_string())
                        ),
                    }
                }
                ["off"] => match app.unpin_relay() {
                    Ok(()) => "OK off".to_string(),
                    Err(error) => format!("ERR {error}"),
                },
                [node, udp, flags @ ..] => {
                    let relay = omni_app::PinnedRelayV1 {
                        node_id: node.to_string(),
                        udp: udp.to_string(),
                        tcp: flags.iter().find_map(|f| f.strip_prefix("tcp=")).map(str::to_string),
                        name: flags.iter().find_map(|f| f.strip_prefix("name=")).unwrap_or("").to_string(),
                        always: flags.contains(&"always"),
                        keep_address_hidden: flags.contains(&"hidden"),
                    };
                    match app.pin_relay(relay) {
                        Ok(()) => "OK pinned".to_string(),
                        Err(error) => format!("ERR {error}"),
                    }
                }
                _ => "ERR want: pin-relay [off | <node> <udp> [tcp=..] [always] [hidden] [name=..]]".to_string(),
            }
        }
        "find-omni-id" => {
            let mut a = rest.splitn(2, ' ');
            let id = a.next().unwrap_or("").trim();
            let petname = a
                .next()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Найденный по ID");
            match app.find_contact_by_omni_id(id, petname) {
                Ok(contact) => format!(
                    "OK {} {:?} {}",
                    contact.petname,
                    contact.relationship_state,
                    contact.remote_account_id.map(|a| a.0).unwrap_or_default()
                ),
                Err(error) => format!("ERR {error}"),
            }
        }
        "mesh-stop" => {
            app.stop_mesh();
            "OK".to_string()
        }
        "node-address" => match app.mesh_node_address_hex() {
            Ok(Some(hex)) => format!("OK {hex}"),
            Ok(None) => "ERR mesh not started".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        "dial" => match app.mesh_dial(rest) {
            Ok(()) => "OK".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        // Dial a node at an explicit endpoint: `dial-at <node_hex> <host:port>` (cross-NAT
        // direct connect — endpoint from a rendezvous beacon / UPnP mapping, not an announce).
        "dial-at" => {
            let mut a = rest.splitn(2, ' ');
            let node = a.next().unwrap_or("");
            let endpoint = a.next().unwrap_or("");
            match app.mesh_dial_at(node, endpoint) {
                Ok(()) => "OK".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // Dial a node over its direct-TCP door at an explicit endpoint: `dial-tcp <node_hex>
        // <host:port>`. The TCP analogue of `dial-at` — isolates the QUIC-over-TCP-wrapper path for
        // the headless reachability harness (relay-less UDP-hostile fallback). See mesh_dial_tcp_direct.
        "dial-tcp" => {
            let mut a = rest.splitn(2, ' ');
            let node = a.next().unwrap_or("");
            let endpoint = a.next().unwrap_or("");
            match app.mesh_dial_tcp_direct(node, endpoint) {
                Ok(()) => "OK".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // Force UDP demotion for a peer (`demote-udp <node_hex>`) — deterministically flips the link
        // to the direct-TCP door (the phantom-drop fail-back signal, on demand). Dev/validation twin
        // used to isolate laptop→PC delivery over the door without waiting on the carrier to throttle.
        "demote-udp" => match app.mesh_demote_udp(rest.trim()) {
            Ok(flagged) => format!("OK {flagged}"),
            Err(error) => format!("ERR {error}"),
        },
        // Meta-B: migrate a peer's ONE connection onto its direct-TCP door (`route-door <node_hex>
        // <door host:port>`) or back onto UDP (`route-udp <node_hex>`). The switch is a QUIC path
        // migration on the mux — same connection, no separate carrier. Dev/validation.
        "route-door" => {
            let mut a = rest.splitn(2, ' ');
            let node = a.next().unwrap_or("");
            let door = a.next().unwrap_or("");
            match app.mesh_route_door(node, door) {
                Ok(routed) => format!("OK {routed}"),
                Err(error) => format!("ERR {error}"),
            }
        }
        "route-udp" => match app.mesh_route_udp(rest.trim()) {
            Ok(routed) => format!("OK {routed}"),
            Err(error) => format!("ERR {error}"),
        },
        // The raw UPnP-mapped direct-TCP door a peer would dial (`door` → `OK <host:port>` or `OK
        // none`). Lets the harness learn our door without rendezvous, and watch UPnP mapping come up.
        "door" => match app.mesh_mapped_tcp_door() {
            Some(endpoint) => format!("OK {endpoint}"),
            None => "OK none".to_string(),
        },
        // Add a relay/bootstrap entry to the user-managed network-node registry: `add-relay
        // <node_hex@ip:port> [label]` (pinned) or `<ip:port>` (hint). The autoconnect worker
        // then keeps it linked (re-dials if its Link drops). The GUI has add-node; this is the
        // dev/validation equivalent. See app_mesh::add_network_node.
        "add-relay" => {
            let mut a = rest.splitn(2, ' ');
            let endpoint = a.next().unwrap_or("");
            let label = a.next().filter(|s| !s.is_empty()).unwrap_or("relay");
            match app.add_network_node(endpoint, label) {
                Ok(id) => format!("OK {id}"),
                Err(error) => format!("ERR {error}"),
            }
        }
        // R1c: flag/unflag a relay as part of MY bundle (auto-propagated to approved friends over the
        // per-pair beacon): `relay-share <id> <on|off>`. The GUI has the checkbox; this is the
        // dev/validation equivalent. See app_mesh::set_relay_node_shared.
        "relay-share" => {
            let mut a = rest.splitn(2, ' ');
            let id_str = a.next().unwrap_or("");
            let on = matches!(a.next().map(str::trim), Some("on") | Some("1") | Some("true"));
            match id_str.parse::<i64>() {
                Ok(id) => match app.set_relay_node_shared(id, on) {
                    Ok(()) => "OK".to_string(),
                    Err(error) => format!("ERR {error}"),
                },
                Err(_) => "ERR usage: relay-share <id> <on|off>".to_string(),
            }
        }
        // Remove a node from the user-managed registry by id (incl. the seeded base node — the
        // empty-bootstrap invariant holds: LAN + invite + rendezvous still work). The GUI has
        // remove-node; this is the dev/validation equivalent, used e.g. to force a rendezvous-only /
        // no-relay topology in a live test. See app_mesh::remove_network_node.
        "remove-relay" => match rest.trim().parse::<i64>() {
            Ok(id) => match app.remove_network_node(id) {
                Ok(()) => "OK".to_string(),
                Err(error) => format!("ERR {error}"),
            },
            Err(_) => "ERR usage: remove-relay <id>".to_string(),
        },
        // R1c: list the network-node registry with R1c tags — `id`, endpoint, `shared` (mine), and the
        // imported `bundle` id (short) + version. One line so the line protocol stays intact. Lets a
        // test verify a propagated relay landed on the friend. See app_mesh::list_network_nodes.
        "list-relays" => match app.list_network_nodes() {
            Ok(nodes) => {
                let mut out = String::from("OK");
                for n in nodes {
                    let bundle = n
                        .bundle_id
                        .as_deref()
                        .map(|b| &b[..b.len().min(8)])
                        .unwrap_or("-");
                    let version =
                        n.bundle_version.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string());
                    out.push_str(&format!(
                        " | id={} ep={} shared={} bundle={} v={}",
                        n.id,
                        n.endpoint.as_deref().unwrap_or("-"),
                        n.shared,
                        bundle,
                        version,
                    ));
                }
                out
            }
            Err(error) => format!("ERR {error}"),
        },
        // R1c: export MY current signed om3relay: bundle (the relays I flagged `shared`), or note that
        // I share none. Dev/debug visibility into what my beacon would carry. See my_relay_bundle_token.
        "relay-bundle" => match app.my_relay_bundle_token() {
            Ok(Some(token)) => format!("OK {token}"),
            Ok(None) => "OK (no shared relays)".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        // "код связи" reach token: `reach` exports mine; `reach <om3reach:…>` imports + dials a
        // friend's (manual / Direct-only reach to an already-paired contact).
        "reach" => {
            if rest.trim().is_empty() {
                match app.generate_reach_token() {
                    Ok(token) => format!("OK {token}"),
                    Err(error) => format!("ERR {error}"),
                }
            } else {
                match app.dial_reach_token(rest) {
                    Ok(node) => format!("OK {node}"),
                    Err(error) => format!("ERR {error}"),
                }
            }
        }
        "has-link" => match app.mesh_has_link(rest) {
            Ok(linked) => format!("OK {linked}"),
            Err(error) => format!("ERR {error}"),
        },
        // DEBUG: the blinded rendezvous topic id(s) my beacon to <account-id> uses. Two unlinked
        // contacts auto-learn each other's relays ONLY if these MATCH on both sides (same per-pair
        // secret). Diagnoses a non-converging beacon exchange. See app_rendezvous::rendezvous_topic_for_contact.
        "rendezvous-topic" => match app.rendezvous_topic_for_contact(&AccountId(rest.trim().to_string())) {
            Ok(Some(topics)) => format!("OK {}", topics.join(" ")),
            Ok(None) => "OK (no rendezvous secret)".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        // Best-path quality to a node: `direct` / `relay:<hops>` / `none`. The R1a
        // connection-quality-ladder climb verifier (rung 0/1 = relay, rung 2 = direct).
        "link-quality" => match app.mesh_link_quality(rest) {
            Ok(quality) => format!("OK {quality}"),
            Err(error) => format!("ERR {error}"),
        },
        "durable-count" => format!("OK {}", app.mesh_durable_count()),
        "approve-all" => match app.list_contacts() {
            Ok(contacts) => {
                let mut approved = 0;
                for contact in contacts {
                    if contact.relationship_state == ContactRelationshipState::Pending
                        && app.approve_contact(&contact.contact_id).is_ok()
                    {
                        approved += 1;
                    }
                }
                format!("OK {approved}")
            }
            Err(error) => format!("ERR {error}"),
        },
        "contacts" => match app.list_contacts() {
            Ok(contacts) => {
                let mut s = String::from("OK");
                for c in contacts {
                    s.push_str(&format!(
                        " [{}|{:?}|{}]",
                        c.petname,
                        c.relationship_state,
                        c.primary_dialog_key.unwrap_or_default()
                    ));
                }
                s
            }
            Err(error) => format!("ERR {error}"),
        },
        "send" => {
            let mut a = rest.splitn(2, ' ');
            let who = a.next().unwrap_or("");
            let text = a.next().unwrap_or("");
            match find_contact_dialog(app, who) {
                Ok(Some(dialog)) => match app.send_text(&dialog, text) {
                    Ok(_) => "OK".to_string(),
                    Err(error) => format!("ERR {error}"),
                },
                Ok(None) => "ERR contact not found".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // `send-file <who> <absolute path>` — headless attachment send, for validating the bulk
        // transfer lane on a real 2-host stand without the GUI. The path is taken whole (after the
        // first space) so names with spaces or commas need no quoting.
        "send-file" => {
            let mut a = rest.splitn(2, ' ');
            let who = a.next().unwrap_or("");
            let path = a.next().unwrap_or("").trim();
            if path.is_empty() {
                return "ERR usage: send-file <contact> <path>".to_string();
            }
            match find_contact_dialog(app, who) {
                Ok(Some(dialog)) => match app.send_attachment(
                    &dialog,
                    "",
                    path,
                    None,
                    omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                ) {
                    Ok(record) => format!("OK {}", record.id),
                    Err(error) => format!("ERR {error}"),
                },
                Ok(None) => "ERR contact not found".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // `recv-file <message_id> <absolute save path>` — the receiving half of `send-file`.
        // A download is an explicit act (the GUI's "save as"), so a headless run has to ask for it
        // too. Path taken whole after the first space, like send-file.
        "recv-file" => {
            let mut a = rest.splitn(2, ' ');
            let message_id = a.next().unwrap_or("");
            let path = a.next().unwrap_or("").trim();
            if message_id.is_empty() || path.is_empty() {
                return "ERR usage: recv-file <message_id> <save path>".to_string();
            }
            match app.request_attachment_download(message_id, path) {
                Ok(record) => format!("OK {}", record.id),
                Err(error) => format!("ERR {error}"),
            }
        }
        // `attachments <who>` — id/state/bytes for each attachment in a dialog, so a headless run
        // can watch a transfer progress and confirm completion without the GUI.
        "attachments" => match find_contact_dialog(app, rest) {
            Ok(Some(dialog)) => match app.list_messages(&dialog) {
                Ok(response) => {
                    let mut out = String::from("OK");
                    for message in response.messages {
                        if let Some(attachment) = message.attachment.as_ref() {
                            out.push_str(&format!(
                                " [{} {:?} {}/{}]",
                                message.id,
                                attachment.state,
                                attachment.bytes_transferred,
                                attachment.file_size_bytes
                            ));
                        }
                    }
                    out
                }
                Err(error) => format!("ERR {error}"),
            },
            Ok(None) => "ERR contact not found".to_string(),
            Err(error) => format!("ERR {error}"),
        },
        "has-message" => {
            let mut a = rest.splitn(2, ' ');
            let who = a.next().unwrap_or("");
            let text = a.next().unwrap_or("");
            match find_contact_dialog(app, who) {
                Ok(Some(dialog)) => match app.list_messages(&dialog) {
                    Ok(resp) => {
                        let found = resp.messages.iter().any(|m| m.body == text);
                        format!("OK {found}")
                    }
                    Err(error) => format!("ERR {error}"),
                },
                Ok(None) => "ERR contact not found".to_string(),
                Err(error) => format!("ERR {error}"),
            }
        }
        // Force an on-demand rendezvous reconnect to an approved contact (dev/validation twin of the
        // shell's "Подключиться сейчас" — same core entry `rendezvous_reach_contact_if_no_link`):
        // `reconnect <petname|account-substr>`. Non-blocking; dials only if not already linked. The
        // reply says whether a dial was dispatched vs nothing-to-do. Poll `has-link` / `reachability`.
        "reconnect" => {
            let who = rest.trim();
            match app.list_contacts() {
                Ok(contacts) => {
                    let acct = contacts
                        .iter()
                        .find(|c| {
                            let a =
                                c.remote_account_id.as_ref().map(|a| a.0.as_str()).unwrap_or("");
                            !who.is_empty()
                                && (c.petname == who || c.petname.contains(who) || a.contains(who))
                        })
                        .and_then(|c| c.remote_account_id.clone());
                    match acct {
                        Some(a) => match app.rendezvous_reach_contact_if_no_link(&a) {
                            Ok(true) => "OK dial dispatched".to_string(),
                            Ok(false) => "OK nothing-to-do (already linked / not eligible)".to_string(),
                            Err(error) => format!("ERR {error}"),
                        },
                        None => "ERR contact not found".to_string(),
                    }
                }
                Err(error) => format!("ERR {error}"),
            }
        }
        "quit" => "OK bye".to_string(),
        other => format!("ERR unknown command '{other}'"),
    }
}

/// Resolve a contact's primary dialog_key by a substring of its petname or
/// remote account-id (the driver passes the stable account-id).
fn find_contact_dialog(app: &OmniApp, who: &str) -> Result<Option<String>, String> {
    let contacts = app.list_contacts().map_err(|e| e.to_string())?;
    for c in contacts {
        let acct = c.remote_account_id.as_ref().map(|a| a.0.as_str()).unwrap_or("");
        if !who.is_empty() && (c.petname == who || c.petname.contains(who) || acct.contains(who)) {
            return Ok(c.primary_dialog_key);
        }
    }
    Ok(None)
}
