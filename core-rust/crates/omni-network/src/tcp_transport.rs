//! TCP relay fallback (QUIC-over-TCP) for UDP-hostile carriers. A mobile carrier that
//! rate-throttles sustained UDP flows (proven on the wire) but leaves TCP alone, so
//! a node that can't reach the relay over UDP tunnels the SAME QUIC inside a TCP connection
//! (length-framed datagrams). See docs/decisions/tcp-relay-fallback.md.
//!
//! **R2 — genuine TLS carrier:** the byte-pipe is wrapped in a real rustls TLS 1.3 handshake
//! (client `TlsConnector`, relay `TlsAcceptor`), so on the wire the flow is an ordinary HTTPS-
//! shaped TLS connection, the same as any other. The outer TLS verifier is
//! AcceptAny by design: it gives channel opacity, while the relay's IDENTITY stays enforced by
//! the in-tunnel QUIC challenge (see [`crate::cert`]), exactly as on the native-UDP path. (SPKI
//! pinning is later hardening.)
//!
//! Two halves:
//! - [`TcpQuicSocket`] — client side: a `quinn::AsyncUdpSocket` backed by one TCP connection
//!   to the relay (wrapped in genuine TLS, see above). quinn runs unmodified on top; the outer
//!   TLS provides channel opacity.
//! - [`run_tcp_bridge`] — relay side: a TCP listener that pipes each connection's framed
//!   datagrams to/from the relay's local QUIC UDP port. The relay's quinn endpoint is
//!   untouched (each bridged TCP conn appears to it as a distinct loopback UDP peer).

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use omni_net_primitives::OmniServerCertVerifier;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use rand_core::OsRng;
use rustls::pki_types::ServerName;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{sleep, timeout};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::cert::derive_cert_from_signing_key;
use crate::tcp_probe::TcpProbe;

/// Max datagram we frame over TCP. QUIC packets are MTU-bounded (~1.5 KB); cap generously.
const MAX_FRAME_LEN: usize = 65535;

/// Socket buffers of a door's TCP conn, both ends. Set, not left to the OS: a send buffer of the
/// usual 64 KB caps one connection at ~1.3 MB/s on a 50 ms path (live 2026-10-16: 2-3 MB/s over
/// the door against 20 MB/s over UDP on the same two machines). 1 MiB carries 20 MB/s at 50 ms.
const DOOR_SOCKET_BUFFER_BYTES: usize = 1024 * 1024;

/// The door bridge's UDP socket toward our own quinn: a burst of answers waits here until the bridge
/// takes it into its queue.
const BRIDGE_UDP_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// The addresses our door bridge feeds our quinn from, one per TCP conn while it lives. A connection
/// from one of them came in through our TCP door; from anywhere else, over raw UDP — the carrier
/// policy of our hops refuses one or the other (`Network::carrier_refused`).
static DOOR_BRIDGE_SOURCES: std::sync::OnceLock<StdMutex<std::collections::HashSet<SocketAddr>>> =
    std::sync::OnceLock::new();

fn door_bridge_sources() -> std::sync::MutexGuard<'static, std::collections::HashSet<SocketAddr>> {
    DOOR_BRIDGE_SOURCES
        .get_or_init(|| StdMutex::new(std::collections::HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Whether quinn's remote `addr` is our own door bridge — the connection rides TCP.
pub(crate) fn is_door_bridge_source(addr: SocketAddr) -> bool {
    door_bridge_sources().contains(&crate::mux_socket::peer_key(addr))
}

/// Holds a bridge source in `DOOR_BRIDGE_SOURCES` for the conn's life.
struct DoorBridgeSource(SocketAddr);

impl Drop for DoorBridgeSource {
    fn drop(&mut self) {
        door_bridge_sources().remove(&self.0);
    }
}

/// The door's socket buffers: `DOOR_SOCKET_BUFFER_BYTES`, or `OMNI_DOOR_SOCKET_BUFFER_BYTES` for a
/// measurement — `0` leaves them to the OS (its window auto-tuning). Read once.
fn door_socket_buffer_bytes() -> Option<usize> {
    static BYTES: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| {
        let bytes = std::env::var("OMNI_DOOR_SOCKET_BUFFER_BYTES")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .unwrap_or(DOOR_SOCKET_BUFFER_BYTES);
        tcp_trace("door/socket-buffer", &format!("bytes={bytes} (0 = the OS's)"));
        (bytes > 0).then_some(bytes)
    })
}

fn size_door_socket(stream: &TcpStream) {
    let Some(bytes) = door_socket_buffer_bytes() else {
        return;
    };
    let socket = socket2::SockRef::from(stream);
    let _ = socket.set_send_buffer_size(bytes);
    let _ = socket.set_recv_buffer_size(bytes);
}

/// Fallback server name for the outer TLS ClientHello when the caller's value is not a valid TLS server
/// name. Harmless: the relay's verifier accepts any name; identity is the in-tunnel ed25519 challenge.
const OUTER_TLS_SNI: &str = "localhost";

fn ring_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(format!("outer TLS: {e}"))
}

/// Client side of the outer carrier: a TLS 1.3 connector that accepts ANY server cert. Opacity
/// only — the relay's identity is verified inside the QUIC tunnel by the ed25519 challenge
/// ([`crate::cert`]), so we reuse the very `OmniServerCertVerifier` the QUIC layer uses (one
/// accept-any verifier, not two). SPKI pinning is later hardening (see module docs).
fn outer_tls_connector() -> io::Result<TlsConnector> {
    let config = rustls::ClientConfig::builder_with_provider(ring_provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .dangerous()
        .with_custom_certificate_verifier(OmniServerCertVerifier::new())
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Relay side of the outer carrier: a TLS 1.3 acceptor with a self-signed (throwaway identity-
/// derived) cert. Genuine TLS termination so an on-path observer sees an ordinary HTTPS handshake.
/// A bare self-signed cert; a CA-issued relay domain is later hardening.
pub(crate) fn outer_tls_acceptor() -> io::Result<TlsAcceptor> {
    let (cert, key) =
        derive_cert_from_signing_key(&SigningKey::generate(&mut OsRng)).map_err(tls_err)?;
    let config = rustls::ServerConfig::builder_with_provider(ring_provider())
        .with_safe_default_protocol_versions()
        .map_err(tls_err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(tls_err)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Place `first`, then what else already waits in `rx`, one datagram per buffer, as from `peer`.
/// Returns how many were placed. A datagram is taken off the queue only when there is a buffer for it:
/// this used to take the next one and then find the buffers full — and drop it. quinn on Windows
/// hands ONE buffer per call, so under load every other datagram that came over a TCP pipe was lost
/// (live 2026-10-16: 48% loss on a door, 1.6 MB/s through it with no queue and RTT at its floor).
fn place_datagrams(
    first: Vec<u8>,
    rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    bufs: &mut [std::io::IoSliceMut<'_>],
    meta: &mut [RecvMeta],
    peer: SocketAddr,
) -> usize {
    let mut n = 0;
    let mut next = Some(first);
    while let Some(datagram) = next.take() {
        let copy = datagram.len().min(bufs[n].len());
        bufs[n][..copy].copy_from_slice(&datagram[..copy]);
        meta[n] = RecvMeta { addr: peer, len: copy, stride: copy, ..Default::default() };
        n += 1;
        if n < bufs.len() {
            next = rx.try_recv().ok();
        }
    }
    n
}

/// What one direction of a door pipe did over the last `PIPE_STATS_EVERY`, for the trace: how much
/// went through, and how long the pipe's own end waited — a writer waiting on the TCP socket means
/// TCP is the narrow place; a reader waiting on it means nothing comes. Trace-only (`trace_enabled`).
struct PipeStats {
    label: &'static str,
    who: String,
    since: std::time::Instant,
    frames: u64,
    bytes: u64,
    waited: Duration,
    queue_max: usize,
    /// The TCP socket's counters (writing ends), when the OS gives them.
    probe: Option<TcpProbe>,
}

const PIPE_STATS_EVERY: Duration = Duration::from_secs(2);

impl PipeStats {
    fn new(label: &'static str, who: String) -> Self {
        Self {
            label,
            who,
            since: std::time::Instant::now(),
            frames: 0,
            bytes: 0,
            waited: Duration::ZERO,
            queue_max: 0,
            probe: None,
        }
    }

    /// The stats of a pipe's writing end over `stream`, with the socket's counters.
    fn writing(label: &'static str, who: String, stream: &TcpStream) -> Self {
        Self { probe: TcpProbe::of(stream), ..Self::new(label, who) }
    }

    fn add(&mut self, frames: u64, len: usize, waited: Duration, queued: usize) {
        if !trace_enabled() {
            return;
        }
        self.frames += frames;
        self.bytes += len as u64;
        self.waited += waited;
        self.queue_max = self.queue_max.max(queued);
        let window = self.since.elapsed();
        if window < PIPE_STATS_EVERY {
            return;
        }
        let secs = window.as_secs_f64();
        let tcp = self.probe.as_mut().map(TcpProbe::report).unwrap_or_default();
        tcp_trace(
            self.label,
            &format!(
                "{} frames={} bytes={} MBps={:.2} avg_frame={} wait_ms={} wait_pct={:.0} queue_max={} {tcp}",
                self.who,
                self.frames,
                self.bytes,
                self.bytes as f64 / secs / 1_048_576.0,
                self.bytes / self.frames.max(1),
                self.waited.as_millis(),
                100.0 * self.waited.as_secs_f64() / secs,
                self.queue_max,
            ),
        );
        self.since = std::time::Instant::now();
        self.frames = 0;
        self.bytes = 0;
        self.waited = Duration::ZERO;
        self.queue_max = 0;
    }
}

/// Write one datagram to a TCP stream as `u16-BE length || bytes` (the pipes write `write_frames`).
#[cfg(test)]
async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, datagram: &[u8]) -> io::Result<()> {
    if datagram.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "datagram too large to frame"));
    }
    let len = (datagram.len() as u16).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(datagram).await?;
    Ok(())
}

/// How much one write to the pipe carries at most: the datagrams waiting are framed into one buffer
/// and written at once. One TLS write per datagram, and two at that (length, then body), made a TLS
/// record and a TCP segment of 2 bytes for every datagram with Nagle off — live 2026-10-16 ~35 000
/// tiny segments in a 300 MB transfer.
const PIPE_WRITE_BATCH_BYTES: usize = 64 * 1024;

/// Frame `first` and whatever else already waits in `more` (up to `PIPE_WRITE_BATCH_BYTES`) into
/// `buf` and write it in one go. Returns the datagrams and bytes written.
async fn write_frames<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    first: Vec<u8>,
    more: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    buf: &mut Vec<u8>,
) -> io::Result<(u64, usize)> {
    buf.clear();
    let mut frames = 0u64;
    let mut bytes = 0usize;
    let mut next = Some(first);
    while let Some(datagram) = next.take() {
        if datagram.len() > MAX_FRAME_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "datagram too large to frame"));
        }
        buf.extend_from_slice(&(datagram.len() as u16).to_be_bytes());
        buf.extend_from_slice(&datagram);
        frames += 1;
        bytes += datagram.len();
        if buf.len() < PIPE_WRITE_BATCH_BYTES {
            next = more.try_recv().ok();
        }
    }
    w.write_all(buf).await?;
    Ok((frames, bytes))
}

/// Read one length-framed datagram from a TCP stream. Handles partial reads (TCP is a byte
/// stream, not message-oriented) via `read_exact`.
async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    r.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// A `quinn::AsyncUdpSocket` whose datagrams ride a single TCP connection to the relay
/// instead of UDP. The connectionless socket API is honoured by always addressing the relay
/// (a node only ever sends to the relay at the socket layer — relay-routed peers go through
/// it). Unbounded queues bridge quinn's poll API to async TCP; QUIC's own congestion control
/// bounds what is actually in flight.
///
/// **Self-heal:** a single [`supervise`] task owns the byte-pipe for the socket's whole life.
/// When the TCP conn drops (relay restart, mobile blip) it dials a fresh one with backoff and
/// resumes — *without* dropping the queues, so the socket itself never errors out. quinn's dead
/// connection idle-times-out and the mesh autoconnect redials over the healed pipe. (The native
/// UDP path self-heals for free because it is connectionless; TCP needs this explicit reconnect
/// — its absence left a relay-restarted node permanently offline. See tcp-relay-fallback.md.)
#[derive(Debug)]
pub struct TcpQuicSocket {
    /// TCP connect target (the relay's bridge port) — used only to dial the stream.
    relay: SocketAddr,
    /// The peer address quinn uses for the connection (its dial destination). All traffic
    /// rides the one TCP conn, but quinn validates that received packets come from the peer
    /// it dialed — which may differ from `relay` (e.g. dial the registry UDP `:443` while the
    /// TCP bridge is `:8443`). We learn it from the first `try_send` and report it on recv so
    /// quinn's peer address stays consistent.
    peer: StdMutex<SocketAddr>,
    local: SocketAddr,
    out_tx: mpsc::UnboundedSender<Vec<u8>>,
    in_rx: StdMutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    /// Жив ли супервизор. Снимается, когда он сдался по `GiveUpAfter`. Нужно ВЛАДЕЛЬЦУ карты
    /// туннелей: без этого запись о сдавшемся туннеле выглядит как исправная, и проверка «уже на
    /// этой двери» возвращает no-op на трупе.
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TcpQuicSocket {
    /// Connect to the relay over TCP and start the supervisor. Must run inside a tokio runtime
    /// context (it `tokio::spawn`s, like the surrounding endpoint setup). `sni` is the outer TLS
    /// server name; it falls back to the placeholder if not a valid name.
    pub fn connect(relay: SocketAddr, sni: &str) -> io::Result<Arc<Self>> {
        // Brief blocking TCP connect (one-time setup at bind, not a hot path) with a bounded timeout
        // — fail fast if the relay is unreachable. The outer TLS handshake on top runs async in the
        // supervisor (which re-does it on every reconnect); later drops are healed there.
        let std_stream = std::net::TcpStream::connect_timeout(&relay, Duration::from_secs(10))?;
        std_stream.set_nodelay(true).ok();
        std_stream.set_nonblocking(true)?;
        let stream = TcpStream::from_std(std_stream)?;
        size_door_socket(&stream);
        Self::start(relay, stream, sni, TcpReconnectPolicy::Forever)
    }

    /// Async connect with an explicit retry policy — see [`TcpReconnectPolicy`]. For a peer's door,
    /// dialed while the node is running: the connect waits up to its timeout on a door that does not
    /// answer, and must not hold a runtime thread (or anything else) while it does — a blocking
    /// connect here once stopped every peer's sends for ~8 s (docs/bugs/a-dead-door-froze-every-peers-sends.md).
    pub async fn connect_with_policy(
        relay: SocketAddr,
        sni: &str,
        policy: TcpReconnectPolicy,
    ) -> io::Result<Arc<Self>> {
        let stream = timeout(Duration::from_secs(10), TcpStream::connect(relay))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("tcp connect {relay} timed out")))??;
        stream.set_nodelay(true).ok();
        size_door_socket(&stream);
        Self::start(relay, stream, sni, policy)
    }

    /// Start the supervisor over a connected stream. Must run inside a tokio runtime context.
    fn start(
        relay: SocketAddr,
        stream: TcpStream,
        sni: &str,
        policy: TcpReconnectPolicy,
    ) -> io::Result<Arc<Self>> {
        let local = stream.local_addr()?;

        let connector = outer_tls_connector()?;
        // The relay's outer TLS is accept-any; fall back to the placeholder if the caller's value isn't a
        // valid TLS server name.
        let server_name = ServerName::try_from(sni)
            .or_else(|_| ServerName::try_from(OUTER_TLS_SNI))
            .map_err(tls_err)?
            .to_owned();

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        // One supervisor owns the byte-pipe for the socket's whole life: it TLS-handshakes the
        // initial TCP conn, pumps frames over it, and when the conn breaks dials + TLS-handshakes a
        // fresh one — keeping `out_rx`/`in_tx` alive so quinn's view of the socket survives.
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(supervise(
            relay,
            connector,
            server_name,
            out_rx,
            in_tx,
            Some(stream),
            policy,
            std::sync::Arc::clone(&alive),
        ));

        Ok(Arc::new(Self {
            relay,
            peer: StdMutex::new(relay),
            local,
            out_tx,
            in_rx: StdMutex::new(in_rx),
            alive,
        }))
    }

    /// Жив ли супервизор этой трубы. `false` — он сдался: труба больше не поднимется сама, и
    /// владелец обязан считать этот сокет отсутствующим, а не исправным.
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Initial backoff between *failed* reconnect attempts; doubles up to `MAX_RECONNECT_BACKOFF`.
/// The first reconnect after a drop is attempted immediately (no pre-sleep) — only repeated
/// failures (relay still down) back off.
const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Сколько подряд неудачных переподключений терпит ОДНОРАЗОВЫЙ туннель, прежде чем сдаться.
/// При потолке backoff в 5 с и ~10 с на неудачный TCP-connect это примерно три минуты попыток.
pub(crate) const DOOR_GIVE_UP_AFTER_FAILURES: u32 = 12;

/// Что делать, когда байтовая труба не поднимается.
///
/// Разница не косметическая, и оба варианта нужны:
///
/// * `Forever` — карьер РЕЛЕЯ. Его отсутствие однажды уже оставило узел с перезапущенным релеем
///   офлайн навсегда (см. tcp-relay-fallback.md), поэтому он обязан ждать сколько угодно.
/// * `GiveUpAfter` — туннель в ДВЕРЬ ПИРА. Дверь эфемерна: у пира за NAT порт меняется при каждом
///   перезапуске, и старый туннель после этого не оживёт никогда. Вечно долбиться в него — чистая
///   потеря: живой замер 2026-08-30 показал 6189 неудачных дозвонов в ДВА мёртвых адреса за одну
///   сессию, по попытке каждые ~15 с до самого конца. Отказ безопасен: путь отказа
///   (`route_peer_over_door`) зовётся по событию и поднимет туннель заново по текущему адресу.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpReconnectPolicy {
    Forever,
    GiveUpAfter(u32),
}

/// Steady-state deadwatch on the inbound byte-pipe: once the pipe has *proven* it carries QUIC
/// (≥1 inbound frame seen), a longer silence than this means it died — reconnect, *even though TCP
/// never reported an error*. This is the load-bearing fix for the live failure: on a mobile-CGNAT
/// carrier a relay restart left the TCP conn half-open (`CloseWait`, no FIN delivered to the app /
/// RST eaten by the NAT), so a reconnect that waits for a read EOF never fired and the node stayed
/// offline. quinn keep-alive is 10 s (`QUIC_KEEP_ALIVE_INTERVAL`), so while the relay lives the pipe
/// carries a keepalive round-trip ≥ every 10 s; 20 s (2×) tolerates one missed beat + carrier jitter.
/// (Keep this comfortably above the keep-alive interval, or a live-but-idle pipe would flap.)
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Grace for the *first* inbound frame on a freshly (re)connected pipe — much longer than the
/// steady-state deadwatch. After a relay restart the pipe reconnects within seconds, but quinn does
/// not redial a NEW QUIC connection until its OLD one to the relay is abandoned — and that is gated
/// by `QUIC_MAX_IDLE_TIMEOUT` (60 s), so the first byte over the fresh pipe may not flow for ~a
/// minute. Applying `READ_IDLE_TIMEOUT` (or any window < the QUIC idle) from connect tears the fresh
/// pipe down before that redial and flaps (observed live: ~2–3 cycles). This must therefore sit
/// COMFORTABLY ABOVE the 60 s QUIC idle so the fresh pipe survives until quinn redials; only then do
/// we switch to the tight steady-state deadwatch. Sized to comfortably exceed the 30 s
/// `QUIC_MAX_IDLE_TIMEOUT` (the redial fires once the dead connection idles out), with margin for
/// carrier jitter.
const HANDSHAKE_GRACE: Duration = Duration::from_secs(45);

/// Why a [`pump`] iteration ended.
enum PumpEnd {
    /// The `TcpQuicSocket` was dropped (its queues are gone) — stop supervising.
    SocketDropped,
    /// The TCP byte-pipe broke — reconnect and resume.
    TcpBroken,
}

/// Owns the byte-pipe for the socket's lifetime: pump over the live conn, reconnect on break.
/// The queues (`out_rx`/`in_tx`) are held across reconnects, so a transient TCP drop never
/// surfaces to quinn as a socket error — the dead QUIC connection just idle-times-out and the
/// next dial rides the healed pipe.
#[allow(clippy::too_many_arguments)]
async fn supervise(
    relay: SocketAddr,
    connector: TlsConnector,
    server_name: ServerName<'static>,
    mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    in_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut initial_tcp: Option<TcpStream>,
    policy: TcpReconnectPolicy,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let mut consecutive_failures: u32 = 0;
    // Сдаться, если политика это позволяет. Считаем ПОДРЯД идущие неудачи: одна удачная связь
    // обнуляет счётчик, поэтому труба, которая моргает, живёт дальше, а труба в исчезнувший адрес —
    // нет.
    macro_rules! give_up_if_exhausted {
        () => {
            consecutive_failures += 1;
            if let TcpReconnectPolicy::GiveUpAfter(limit) = policy {
                if consecutive_failures >= limit {
                    tcp_trace(
                        "tcp/give-up",
                        &format!("relay={relay} unreachable after {consecutive_failures} tries, stopping"),
                    );
                    alive.store(false, std::sync::atomic::Ordering::Relaxed);
                    return;
                }
            }
        };
    }
    loop {
        // Obtain a TLS byte-pipe: TLS-handshake the initial TCP conn on the first pass, otherwise
        // dial (TCP + TLS) a fresh one. A failed initial handshake falls through to the dial path.
        let tls = match initial_tcp.take() {
            Some(tcp) => match connector.connect(server_name.clone(), tcp).await {
                Ok(t) => t,
                Err(_) => {
                    tcp_trace("tcp/tls-fail", &format!("relay={relay} initial TLS handshake failed"));
                    give_up_if_exhausted!();
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                    continue;
                }
            },
            None => match reconnect(relay, &connector, &server_name).await {
                Some(t) => {
                    tcp_trace("tcp/reconnect", &format!("relay={relay} byte-pipe re-established"));
                    backoff = INITIAL_RECONNECT_BACKOFF;
                    consecutive_failures = 0;
                    t
                }
                None => {
                    tcp_trace("tcp/reconnect-fail", &format!("relay={relay} dial failed, backoff"));
                    give_up_if_exhausted!();
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_RECONNECT_BACKOFF);
                    continue;
                }
            },
        };
        let stats = PipeStats::writing("door/pipe-out", format!("to={relay}"), tls.get_ref().0);
        let (mut read_half, mut write_half) = split(tls);
        match pump(&mut out_rx, &in_tx, &mut read_half, &mut write_half, stats).await {
            PumpEnd::SocketDropped => {
                tcp_trace("tcp/socket-dropped", "TcpQuicSocket gone — supervisor exiting");
                return;
            }
            PumpEnd::TcpBroken => {
                tcp_trace("tcp/disconnect", &format!("relay={relay} byte-pipe broke, reconnecting"));
                // `initial_tcp` is None → next iteration dials immediately (no pre-sleep).
            }
        }
    }
}

/// Bridge quinn's queues to one TCP conn until either the conn breaks or the socket is dropped.
/// Borrows (not consumes) the queues, so the caller reuses them across reconnects. Writer and
/// reader race: whichever ends first decides the outcome (the other future is cancelled, which
/// only discards a partial frame on an already-dead pipe — quinn retransmits).
///
/// Break detection is two-pronged, because TCP alone is not enough on a hostile carrier:
/// (1) a read EOF/error — the clean case (relay closed, healthy network); and (2) an inbound
/// deadwatch — [`HANDSHAKE_GRACE`] for the first frame on a fresh pipe (let the QUIC redial land),
/// then [`READ_IDLE_TIMEOUT`] once traffic proves the pipe — the load-bearing case for mobile CGNAT,
/// where the relay's FIN/RST is delayed or eaten by the NAT so the pipe goes silently half-open and
/// a read-EOF would never come. Either way → `TcpBroken` → the supervisor recycles the conn.
async fn pump<R, W>(
    out_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    in_tx: &mpsc::UnboundedSender<Vec<u8>>,
    read_half: &mut R,
    write_half: &mut W,
    mut stats: PipeStats,
) -> PumpEnd
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let writer = async {
        let mut buf = Vec::with_capacity(PIPE_WRITE_BATCH_BYTES + MAX_FRAME_LEN);
        while let Some(datagram) = out_rx.recv().await {
            let started = std::time::Instant::now();
            let Ok((frames, bytes)) = write_frames(write_half, datagram, out_rx, &mut buf).await else {
                return PumpEnd::TcpBroken; // TCP write failed
            };
            stats.add(frames, bytes, started.elapsed(), out_rx.len());
        }
        PumpEnd::SocketDropped // out_tx all dropped → socket gone
    };
    let reader = async {
        // First frame gets HANDSHAKE_GRACE (a fresh pipe needs time for the mesh to redial QUIC and
        // the handshake to land); once any frame proves the pipe carries QUIC, tighten to the
        // steady-state READ_IDLE_TIMEOUT so a later silence is caught fast (no flapping).
        let mut window = HANDSHAKE_GRACE;
        loop {
            match timeout(window, read_frame(read_half)).await {
                Ok(Ok(datagram)) => {
                    window = READ_IDLE_TIMEOUT;
                    if in_tx.send(datagram).is_err() {
                        return PumpEnd::SocketDropped; // in_rx dropped → socket gone
                    }
                }
                Ok(Err(_)) => return PumpEnd::TcpBroken, // TCP closed/error (clean break)
                Err(_) => return PumpEnd::TcpBroken, // no inbound past the deadwatch → pipe dead
            }
        }
    };
    tokio::select! {
        end = writer => end,
        end = reader => end,
    }
}

/// Dial a fresh TCP conn to the relay (bounded) and TLS-handshake it. `None` on failure (caller
/// retries with backoff). The relay's bridge maps each new conn to a fresh virtual UDP peer, so a
/// reconnected node is simply a new peer to the relay's quinn — exactly right after a restart.
async fn reconnect(
    relay: SocketAddr,
    connector: &TlsConnector,
    server_name: &ServerName<'static>,
) -> Option<ClientTlsStream<TcpStream>> {
    let tcp = match timeout(Duration::from_secs(10), TcpStream::connect(relay)).await {
        Ok(Ok(stream)) => stream,
        _ => return None,
    };
    tcp.set_nodelay(true).ok();
    size_door_socket(&tcp);
    match timeout(Duration::from_secs(10), connector.connect(server_name.clone(), tcp)).await {
        Ok(Ok(tls)) => Some(tls),
        _ => None,
    }
}

/// Append a diagnostic line to the shared `OMNI_TRACE_FILE` sink, so TCP-fallback link state shows
/// up in traceA.log alongside mesh events. `pub` (not just crate) so omni-mesh-runtime's pumps can
/// share it (re-exported as `omni_network::diag_trace`).
///
/// This used to be a local copy of the append-one-line logic, one of three in the workspace. The
/// implementation now lives in `omni_net_primitives::trace`; this stays as the name the mesh code
/// already calls.
pub fn tcp_trace(category: &str, message: &str) {
    omni_net_primitives::trace_line(category, message);
}

/// Whether the `OMNI_TRACE_FILE` sink is active — lets callers skip spawning monitor
/// tasks whose only output is `tcp_trace`.
pub(crate) fn trace_enabled() -> bool {
    omni_net_primitives::trace_enabled()
}

/// `UdpPoller` that is always writable — the outbound queue is unbounded, so `try_send`
/// never needs to back-pressure. Not `WouldBlock` ever: the door rides `MuxRouter`, whose
/// writability is the UDP base's, and quinn would retry it forever (it did, live 2026-10-16: the
/// whole app hung). Nor a drop on a full queue: under BBR (the default on an unmetered link) that
/// loss is not heard — 60% of a transfer went into the void.
#[derive(Debug)]
struct AlwaysWritable;

impl UdpPoller for AlwaysWritable {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncUdpSocket for TcpQuicSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AlwaysWritable)
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        // `max_transmit_segments` == 1 → one datagram per transmit. All traffic rides the one
        // TCP conn regardless of `transmit.destination`, but record that destination as the peer
        // address so recv reports it back (quinn validates the peer address per connection).
        if let Ok(mut peer) = self.peer.lock() {
            *peer = transmit.destination;
        }
        self.out_tx
            .send(transmit.contents.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tcp relay connection closed"))
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if bufs.is_empty() {
            return Poll::Pending;
        }
        let mut rx = self
            .in_rx
            .lock()
            .map_err(|_| io::Error::other("tcp recv lock poisoned"))?;
        match rx.poll_recv(cx) {
            Poll::Ready(Some(datagram)) => {
                // Report every datagram as from the peer quinn dialed (see `peer`).
                let peer = self.peer.lock().map(|p| *p).unwrap_or(self.relay);
                Poll::Ready(Ok(place_datagrams(datagram, &mut rx, bufs, meta, peer)))
            }
            Poll::Ready(None) => {
                Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "tcp relay closed")))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    fn max_transmit_segments(&self) -> usize {
        1
    }

    fn max_receive_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// Total concurrent bridged connections the relay will hold. Each one costs ≥2 tasks + an ephemeral
/// loopback UDP socket (an fd), so an UNcapped accept loop on the public relay anchor is a
/// remotely-killable resource-exhaustion DoS. Generous for a friends-scale relay; a hard ceiling.
const MAX_BRIDGE_CONNS: usize = 1024;
/// Per-source-IP cap on concurrent bridged connections. Deliberately generous because a mobile
/// carrier CGNATs MANY legitimate users behind ONE egress IP — exactly the clients this TCP-fallback
/// anchor exists for — so a tight per-IP cap would lock them out. It only blunts a single-IP flood;
/// the absolute bound is `MAX_BRIDGE_CONNS`.
const MAX_BRIDGE_CONNS_PER_IP: usize = 64;

/// Relay side: accept TCP connections on `listen` and bridge each one to the relay's local
/// QUIC UDP socket at `quic_udp`. Each connection gets its own ephemeral loopback UDP socket,
/// so the relay's (unmodified) quinn endpoint sees every TCP peer as a distinct UDP peer. The
/// bytes are opaque (QUIC) — the bridge only moves frames. Capped (see consts) so a
/// flood can't exhaust the anchor; over-cap connections are dropped (closed), accepting continues.
pub async fn run_tcp_bridge(listen: SocketAddr, quic_udp: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(listen).await?;
    let acceptor = outer_tls_acceptor()?;
    serve_tcp_bridge(listener, quic_udp, acceptor, MAX_BRIDGE_CONNS, MAX_BRIDGE_CONNS_PER_IP).await
}

/// Like [`run_tcp_bridge`] but serves on an ALREADY-bound listener — so the caller can learn the
/// bound port (to UPnP-map + advertise it) BEFORE serving. Used by a peer standing up its own
/// direct-TCP front door (task 3): bind ephemeral → read `local_addr()` → map + advertise → serve.
/// See docs/decisions/tcp-direct-peer-fallback.md.
pub async fn run_tcp_bridge_on(listener: TcpListener, quic_udp: SocketAddr) -> io::Result<()> {
    let acceptor = outer_tls_acceptor()?;
    serve_tcp_bridge(listener, quic_udp, acceptor, MAX_BRIDGE_CONNS, MAX_BRIDGE_CONNS_PER_IP).await
}

pub(crate) async fn serve_tcp_bridge(
    listener: TcpListener,
    quic_udp: SocketAddr,
    acceptor: TlsAcceptor,
    max_total: usize,
    max_per_ip: usize,
) -> io::Result<()> {
    let total = Arc::new(Semaphore::new(max_total));
    let per_ip: Arc<StdMutex<HashMap<IpAddr, usize>>> = Arc::new(StdMutex::new(HashMap::new()));
    loop {
        let (tcp, peer) = listener.accept().await?;
        let ip = peer.ip();
        // Door reached: an inbound TCP conn actually arrived at our front door. If a dialing peer
        // reports `tcp_direct/quic-fail` while we NEVER log this, the router's forward is phantom
        // (SYN/connection not delivered inbound); if we DO log this but the peer still fails, the
        // break is above the byte layer (TLS/quinn/bridge). The make-or-break relay-less signal.
        tcp_trace("tcp/bridge-accept", &format!("inbound door conn from {peer}"));
        // Absolute bound: when all permits are out, drop the new conn (it closes on drop) and keep
        // accepting — never block the loop, so a flood is shed fast instead of stalling the anchor.
        let Ok(permit) = Arc::clone(&total).try_acquire_owned() else {
            tcp_trace("tcp/bridge-drop", &format!("at capacity {max_total}; dropped {ip}"));
            continue;
        };
        // Per-IP share (generous for CGNAT). Increment under the lock; the spawned task decrements
        // on teardown. `permit` is released here on `continue` (OwnedSemaphorePermit drops).
        {
            let mut map = per_ip.lock().unwrap_or_else(|e| e.into_inner());
            let n = map.entry(ip).or_insert(0);
            if *n >= max_per_ip {
                tcp_trace("tcp/bridge-drop", &format!("per-ip cap {max_per_ip} for {ip}"));
                continue;
            }
            *n += 1;
        }
        let per_ip_dec = Arc::clone(&per_ip);
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let _permit = permit; // held for the conn's life → released when this task ends
            let _ = bridge_one_conn(tcp, quic_udp, acceptor).await;
            let mut map = per_ip_dec.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = map.get_mut(&ip) {
                *n -= 1;
                if *n == 0 {
                    map.remove(&ip);
                }
            }
        });
    }
}

async fn bridge_one_conn(
    tcp: TcpStream,
    quic_udp: SocketAddr,
    acceptor: TlsAcceptor,
) -> io::Result<()> {
    tcp.set_nodelay(true).ok();
    size_door_socket(&tcp);
    // Diagnostic: distinguish a REAL carrier conn (completes the outer TLS, then carries framed QUIC)
    // from the ~20 s AutoNAT door-confirmation probes (a bare TCP connect that closes → TLS handshake
    // EOF, zero frames). Env-gated via `tcp_trace` (noop unless OMNI_TRACE_FILE). Low volume: one line
    // per accepted door conn + one summary when it ends. See relay-less-mobile-transport-findings.md.
    let peer = tcp.peer_addr().ok();
    // Terminate the outer TLS; below this point it is the same QUIC bytes as the native-UDP path.
    let tls = match acceptor.accept(tcp).await {
        Ok(tls) => {
            tcp_trace("tcp/bridge-tls-ok", &format!("peer={peer:?}"));
            tls
        }
        Err(e) => {
            tcp_trace("tcp/bridge-tls-fail", &format!("peer={peer:?} err={e}"));
            return Err(e);
        }
    };
    let mut out_stats = PipeStats::writing("door/bridge-out", format!("to={peer:?}"), tls.get_ref().0);
    let (mut tcp_r, mut tcp_w) = split(tls);
    // A fresh ephemeral UDP socket per TCP conn → a distinct source addr at the relay's quinn.
    // Bind UNSPECIFIED (not loopback): when quinn is bound to a specific LAN interface, `quic_udp`
    // is that LAN address, which a loopback-bound socket cannot reach. UNSPECIFIED reaches both the
    // LAN-IP quinn and a loopback quinn. See docs/decisions/tcp-direct-peer-fallback.md.
    let udp = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;
    // Room for a burst of our quinn's answers while the reader below hands them on.
    let _ = socket2::SockRef::from(&udp).set_recv_buffer_size(BRIDGE_UDP_RECV_BUFFER_BYTES);
    udp.connect(quic_udp).await?;
    // Connected, the socket knows the source address quinn sees (the interface toward `quic_udp`).
    let _source = udp.local_addr().ok().map(|source| {
        let source = crate::mux_socket::peer_key(source);
        door_bridge_sources().insert(source);
        DoorBridgeSource(source)
    });
    let udp = Arc::new(udp);

    // TCP -> UDP: framed datagrams from the client to the relay's QUIC.
    let udp_send = Arc::clone(&udp);
    let t2u = tokio::spawn(async move {
        let mut count = 0u64;
        let mut stats = PipeStats::new("door/bridge-in", format!("from={peer:?}"));
        // `read_frame` erroring = TCP closed → stop forwarding.
        loop {
            let started = std::time::Instant::now();
            let Ok(dg) = read_frame(&mut tcp_r).await else {
                break;
            };
            let waited = started.elapsed();
            count += 1;
            if udp_send.send(&dg).await.is_err() {
                break;
            }
            stats.add(1, dg.len(), waited, 0);
        }
        // Summary (env-gated): frames==0 flags a door conn that TLS-completed but carried no QUIC.
        tcp_trace("tcp/bridge-conn-end", &format!("peer={peer:?} t2u_frames={count}"));
    });

    // UDP -> TCP: the relay's QUIC replies, framed back to the client. The socket is read on its own,
    // into a queue, and the queue written to the pipe: one loop doing both stopped reading the socket
    // whenever the TCP write waited, its buffer overflowed and the datagrams were lost — 58% of what
    // our quinn sent back over the door (live 2026-10-16: 0.8 MB/s from the door's owner, 10 MB/s to
    // it). A UDP socket never pushes back, so a full buffer is a silent drop; the queue never is.
    let (u2q_tx, mut u2q_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let udp_recv = Arc::clone(&udp);
    let u2q = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        while let Ok(n) = udp_recv.recv(&mut buf).await {
            if u2q_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let u2t = async {
        let stats = &mut out_stats;
        let mut buf = Vec::with_capacity(PIPE_WRITE_BATCH_BYTES + MAX_FRAME_LEN);
        while let Some(datagram) = u2q_rx.recv().await {
            let started = std::time::Instant::now();
            let (frames, bytes) = write_frames(&mut tcp_w, datagram, &mut u2q_rx, &mut buf).await?;
            stats.add(frames, bytes, started.elapsed(), u2q_rx.len());
        }
        Ok::<(), io::Error>(())
    };

    // Whichever direction ends first tears down the conn.
    tokio::select! {
        _ = t2u => {}
        _ = u2t => {}
    }
    u2q.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert::derive_cert_from_signing_key;
    use ed25519_dalek::SigningKey;
    use omni_net_primitives::{bind_quic_udp_socket, omni_client_config, omni_server_config};
    use quinn::{Endpoint, EndpointConfig, Runtime, TokioRuntime};
    use rand_core::OsRng;

    /// What waits is written in one go — up to `PIPE_WRITE_BATCH_BYTES` — and reads back datagram by
    /// datagram; what did not fit stays queued for the next write.
    #[tokio::test]
    async fn waiting_datagrams_go_out_in_one_write_and_read_back_one_by_one() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let datagrams: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 9000]).collect();
        for datagram in &datagrams[1..] {
            tx.send(datagram.clone()).unwrap();
        }
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let mut buf = Vec::new();
        let (frames, bytes) = write_frames(&mut a, datagrams[0].clone(), &mut rx, &mut buf).await.unwrap();
        let expected = PIPE_WRITE_BATCH_BYTES.div_ceil(9002) as u64;
        assert_eq!(frames, expected, "one write takes what waits, up to the batch size");
        assert_eq!(bytes, 9000 * expected as usize);
        assert_eq!(rx.len(), 20 - expected as usize, "the rest waits for the next write");
        for datagram in &datagrams[..expected as usize] {
            assert_eq!(&read_frame(&mut b).await.unwrap(), datagram);
        }
    }

    /// Every datagram that came over the pipe reaches quinn, however few buffers it hands per call:
    /// with one buffer (quinn on Windows) a datagram is left in the queue for the next call, not
    /// taken out and dropped.
    #[test]
    fn no_datagram_is_lost_when_quinn_hands_fewer_buffers_than_wait() {
        let peer: SocketAddr = "203.0.113.9:443".parse().unwrap();
        for buffers in [1usize, 2, 3] {
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
            for i in 1..10u8 {
                tx.send(vec![i; 100]).unwrap();
            }
            let mut got = Vec::new();
            let mut first = Some(vec![0u8; 100]);
            while let Some(datagram) = first.take().or_else(|| rx.try_recv().ok()) {
                let mut storage = vec![[0u8; 200]; buffers];
                let mut meta = vec![RecvMeta::default(); buffers];
                let mut bufs: Vec<std::io::IoSliceMut<'_>> =
                    storage.iter_mut().map(|b| std::io::IoSliceMut::new(&mut b[..])).collect();
                let n = place_datagrams(datagram, &mut rx, &mut bufs, &mut meta, peer);
                assert!(n >= 1 && n <= buffers);
                for (buf, meta) in bufs.iter().zip(&meta).take(n) {
                    assert_eq!(meta.addr, peer);
                    got.push(buf[0]);
                }
            }
            assert_eq!(got, (0..10u8).collect::<Vec<_>>(), "all ten, in order, with {buffers} buffer(s)");
        }
    }

    #[tokio::test]
    async fn frame_round_trips_across_sizes_and_partial_reads() {
        // Concatenate several frames, read them back one by one over an in-memory duplex pipe.
        let (mut a, mut b) = tokio::io::duplex(64); // small buffer → forces partial reads
        let payloads: Vec<Vec<u8>> = vec![
            vec![],
            vec![1],
            (0..50u16).map(|i| i as u8).collect(),
            (0..1452u16).map(|i| (i * 7) as u8).collect(),
        ];
        let writer_payloads = payloads.clone();
        let writer = tokio::spawn(async move {
            for p in &writer_payloads {
                write_frame(&mut a, p).await.unwrap();
            }
            // keep `a` alive until all written
            a.flush().await.unwrap();
        });
        for expected in &payloads {
            let got = read_frame(&mut b).await.unwrap();
            assert_eq!(&got, expected);
        }
        writer.await.unwrap();
    }

    /// Туннель в ДВЕРЬ ПИРА обязан сдаваться, а карьер РЕЛЕЯ — нет. Разница не косметическая:
    /// карта дверей ключуется UDP-адресом пира, а он меняется вместе с портом при каждом его
    /// перезапуске, поэтому старая запись новым ключом не вытесняется и живёт до конца процесса.
    /// Пока супервизор был вечным для всех, это стоило 6189 неудачных дозвонов в ДВА мёртвых
    /// адреса за одну сессию (живой замер 2026-08-30) — по попытке каждые ~15 с, навсегда.
    ///
    /// Стенд один на обе политики, и в этом доказательство: адрес перестаёт отвечать одинаково,
    /// расходится только поведение.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_door_tunnel_gives_up_where_a_relay_carrier_keeps_trying() {
        // Голый слушатель без TLS: соединение принимается, рукопожатие проваливается — ровно то,
        // что видит труба, чей адрес уже занял кто-то другой.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });

        let door = TcpQuicSocket::connect_with_policy(
            addr,
            "localhost",
            TcpReconnectPolicy::GiveUpAfter(3),
        )
        .await
        .expect("первичный TCP-connect проходит — слушатель на месте");
        let relay = TcpQuicSocket::connect(addr, "localhost")
            .expect("первичный TCP-connect проходит — слушатель на месте");
        assert!(door.is_alive(), "туннель начинает живым");

        // Рукопожатие валится сразу, backoff 0.5 -> 1 -> 2 с, поэтому трёх неудач ждать недолго.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while door.is_alive() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !door.is_alive(),
            "туннель в дверь обязан сдаться после GiveUpAfter неудач, а не долбиться вечно"
        );
        assert!(
            relay.is_alive(),
            "карьер релея сдаваться НЕ должен: его отсутствие однажды оставило узел с              перезапущенным релеем офлайн навсегда"
        );
    }

    #[tokio::test]
    async fn read_frame_errors_on_closed_stream() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a); // close the write side
        assert!(read_frame(&mut b).await.is_err());
    }

    /// Relay-restart self-heal: when the TCP byte-pipe drops (relay bounce / mobile blip), the
    /// supervisor must transparently re-establish it so frames flow again — WITHOUT the socket
    /// itself erroring out (which would bury quinn's redial). This is the in-process proxy for
    /// the §Verification durability tick that the native-UDP path got for free (connectionless)
    /// but TCP did not: before this fix a relay-restarted node stayed permanently offline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn byte_pipe_reconnects_after_drop() {
        use std::future::poll_fn;
        use std::io::IoSliceMut;
        use std::net::{IpAddr, Ipv4Addr};

        // A minimal stand-in relay: a TCP listener whose accepted conn we drop to simulate a
        // restart — the listener stays up (same registry addr) but the per-conn pipe dies,
        // exactly as a relay bounce kills the in-flight TCP conn while the port comes back.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let socket = TcpQuicSocket::connect(addr, "localhost").unwrap();
        // The stand-in relay now terminates the outer TLS, like the real bridge (R2).
        let acceptor = outer_tls_acceptor().unwrap();

        let mk = |bytes: &'static [u8]| Transmit {
            destination: addr, // a node only ever addresses the relay at the socket layer
            ecn: None,
            contents: bytes,
            segment_size: None,
            src_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        };

        // Round 1 — first conn carries a frame each way (also exercises poll_recv).
        let (raw1, _) = listener.accept().await.unwrap();
        let mut srv1 = acceptor.accept(raw1).await.unwrap();
        socket.try_send(&mk(b"ping-1")).unwrap();
        assert_eq!(read_frame(&mut srv1).await.unwrap(), b"ping-1", "client → relay over conn 1");

        write_frame(&mut srv1, b"pong-1").await.unwrap();
        let mut got = Vec::new();
        poll_fn(|cx| {
            let mut b = [0u8; 64];
            let mut bufs = [IoSliceMut::new(&mut b)];
            let mut meta = [RecvMeta::default()];
            match socket.poll_recv(cx, &mut bufs, &mut meta) {
                Poll::Ready(Ok(n)) if n >= 1 => {
                    got.extend_from_slice(&b[..meta[0].len]);
                    Poll::Ready(Ok::<(), io::Error>(()))
                }
                Poll::Ready(Ok(_)) => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
        .unwrap();
        assert_eq!(got, b"pong-1", "relay → client over conn 1");

        // Simulate the restart: drop the relay's accepted conn → the client's pipe breaks.
        drop(srv1);

        // The supervisor must reconnect to the same listener. `accept()` only returns once it
        // has — so reaching here at all proves the socket re-dialed (no sleep / no polling).
        let (raw2, _) = listener.accept().await.unwrap();
        let mut srv2 = acceptor.accept(raw2).await.unwrap();

        // Round 2 — a fresh frame must flow over the NEW conn (the socket survived the drop).
        socket.try_send(&mk(b"ping-2")).unwrap();
        assert_eq!(read_frame(&mut srv2).await.unwrap(), b"ping-2", "client → relay over conn 2");
    }

    /// The mobile-CGNAT half-open case the live run exposed (and the drop test above MISSED): the
    /// relay vanishes but the TCP conn is never closed — no FIN/RST reaches the client — so the
    /// pipe goes silent with NO read error. The supervisor must still reconnect, driven by the
    /// inbound deadwatch. The pipe here never delivers a first frame, so it is `HANDSHAKE_GRACE`
    /// that elapses. Paused time lets it pass in virtual time (no real wait): with every task
    /// blocked on silent I/O, tokio auto-advances to the pending timer.
    #[tokio::test(start_paused = true)]
    async fn byte_pipe_reconnects_after_silent_idle() {
        use std::net::{IpAddr, Ipv4Addr};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let socket = TcpQuicSocket::connect(addr, "localhost").unwrap();
        let acceptor = outer_tls_acceptor().unwrap();

        // Accept + TLS-handshake the first conn, then hold it OPEN but SILENT — never frame, read,
        // or close it (the relay's bytes vanish while the half-open TCP lingers; no EOF will come).
        let (raw1, _) = listener.accept().await.unwrap();
        let _srv1 = acceptor.accept(raw1).await.unwrap();

        // The reader must hit the deadwatch and the supervisor must re-dial — `accept()` returning
        // is the proof. No read-EOF is ever delivered, so only the deadwatch can do it.
        let (raw2, _) = listener.accept().await.unwrap();
        let mut srv2 = acceptor.accept(raw2).await.unwrap();

        socket
            .try_send(&Transmit {
                destination: addr,
                ecn: None,
                contents: b"after-idle",
                segment_size: None,
                src_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            })
            .unwrap();
        assert_eq!(
            read_frame(&mut srv2).await.unwrap(),
            b"after-idle",
            "reconnected after a silent half-open pipe (no FIN), not just a clean drop"
        );
    }

    /// R5 DoS-cap: the relay bridge must SHED connections past its caps (drop = close the socket),
    /// never spawn unbounded — an uncapped accept loop on the public :443 anchor is a
    /// remotely-killable resource exhaustion (each conn = tasks + an ephemeral UDP fd). Checks both
    /// the total cap and the per-IP cap (loopback = one IP): with 2 held open, the 3rd is closed
    /// (EOF) while the held two stay open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_caps_shed_overflow_connections() {
        // (max_total, max_per_ip): the first pair trips the TOTAL cap, the second the PER-IP cap.
        for (max_total, max_per_ip) in [(2usize, 100usize), (100usize, 2usize)] {
            let quic_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let quic_addr = quic_udp.local_addr().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let bridge_addr = listener.local_addr().unwrap();
            let acceptor = outer_tls_acceptor().unwrap();
            tokio::spawn(async move {
                let _ = serve_tcp_bridge(listener, quic_addr, acceptor, max_total, max_per_ip).await;
            });

            // Two connections fill the cap; the bridge holds them open (it blocks reading frames).
            let mut c1 = TcpStream::connect(bridge_addr).await.unwrap();
            let _c2 = TcpStream::connect(bridge_addr).await.unwrap();
            let mut c3 = TcpStream::connect(bridge_addr).await.unwrap();

            // c3 exceeds the cap → shed → a read returns 0 (EOF) promptly.
            let mut b = [0u8; 1];
            let n = timeout(Duration::from_secs(2), c3.read(&mut b))
                .await
                .expect("c3 read resolves — it was dropped")
                .expect("c3 read ok");
            assert_eq!(n, 0, "over-cap conn must be shed (caps {max_total}/{max_per_ip})");

            // A held conn stays open: its read BLOCKS (no data, not closed) → times out.
            let mut b1 = [0u8; 1];
            assert!(
                timeout(Duration::from_millis(400), c1.read(&mut b1)).await.is_err(),
                "under-cap conn stays open (caps {max_total}/{max_per_ip})"
            );
        }
    }

    /// End-to-end: a real QUIC handshake + uni-stream over the FULL production path —
    /// client `TcpQuicSocket` (genuine TLS) ↔ TLS bridge ↔ server plain UDP (exactly how node A in
    /// TCP-fallback talks to the relay over the R2 TLS carrier). Proves TcpQuicSocket
    /// is a correct AsyncUdpSocket and the TLS bridge carries QUIC faithfully — the in-process gate
    /// before the live UDP-hostile-network test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quic_handshake_and_stream_over_tcp_bridge() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let alpn = b"omni-mesh-tcp-test";
        let runtime = Arc::new(TokioRuntime);

        // --- server: quinn over plain UDP on loopback (like the relay) ---
        let skey = SigningKey::generate(&mut OsRng);
        let (cert, key) = derive_cert_from_signing_key(&skey).unwrap();
        let server_cfg = omni_server_config(cert, key, alpn).unwrap();
        let udp = bind_quic_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_udp_addr = udp.local_addr().unwrap();
        let server_socket = runtime.wrap_udp_socket(udp).unwrap();
        let server = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(server_cfg),
            server_socket,
            runtime.clone(),
        )
        .unwrap();

        // --- bridge: TCP listener -> server's QUIC UDP (each conn = a loopback UDP peer) ---
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bridge_addr = listener.local_addr().unwrap();
        let acceptor = outer_tls_acceptor().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { break };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let _ = bridge_one_conn(tcp, server_udp_addr, acceptor).await;
                });
            }
        });

        // --- client: quinn over TcpQuicSocket (genuine TLS) to the bridge (like node A) ---
        let client_socket: Arc<dyn quinn::AsyncUdpSocket> =
            TcpQuicSocket::connect(bridge_addr, "localhost").unwrap();
        let mut client = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            client_socket,
            runtime.clone(),
        )
        .unwrap();
        client.set_default_client_config(omni_client_config(alpn).unwrap());

        // quinn 0.11 servers don't auto-respond — the app must accept the Incoming. Drive the
        // server side CONCURRENTLY with the client handshake (else they deadlock: client waits
        // for the server's response, server waits for accept()).
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming connection");
            let conn = incoming.await.expect("server accepts the connection");
            // What the carrier policy reads: this connection came in through our door, not over
            // raw UDP — and a raw-UDP address is not taken for one.
            assert!(is_door_bridge_source(conn.remote_address()), "the bridge is known as a door");
            assert!(!is_door_bridge_source(server_udp_addr), "an address of no bridge is not");
            let mut recv = conn.accept_uni().await.expect("server accepts uni stream");
            recv.read_to_end(1024).await.expect("server reads stream")
        });

        // The peer address quinn uses must match what TcpQuicSocket reports on recv (bridge_addr).
        let connecting = client.connect(bridge_addr, "localhost").unwrap();
        let timeout = std::time::Duration::from_secs(10);
        let client_conn = tokio::time::timeout(timeout, connecting)
            .await
            .expect("handshake within 10s")
            .expect("client connected over TCP bridge");

        let mut send = client_conn.open_uni().await.unwrap();
        send.write_all(b"hello-over-tcp").await.unwrap();
        send.finish().unwrap();

        let got = tokio::time::timeout(timeout, server_task)
            .await
            .expect("server task within 10s")
            .expect("server task ok");
        assert_eq!(got, b"hello-over-tcp");

        client.close(0u32.into(), b"done");
    }

}
