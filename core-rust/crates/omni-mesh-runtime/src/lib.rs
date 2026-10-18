// omni-mesh-runtime — synchronous facade over the async omni-network mesh.
//
// omni-network is tokio/async; omni-app (and the FFI it backs) are synchronous.
// This crate owns a tokio runtime + a `Network`, runs the background loops
// (announce, accept, per-link inbound pumps), and exposes a small *blocking*
// API so omni-app can drive the mesh the same way it drives omni-transport —
// without tokio leaking into the otherwise-sync orchestration layer.
//
// Decision + seam: docs/decisions/mesh-transport-integration-seam.md.
// What the mesh itself does: docs/architecture/networking-overlay.md.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use omni_network::{
    DurableDataFrame, Interface, Link, MulticastConfig, MulticastInterface, Network,
    QuicOverUdpInterface,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

/// Re-export so omni-app maps contacts ↔ addresses without depending on
/// omni-network directly.
pub use omni_network::NodeAddress as MeshNodeAddress;
pub use omni_network::{pack_ipv4_peers, DhtProxyOp as MeshDhtProxyOp};
/// Re-export the public connectivity-state types so omni-app builds the honest
/// connection-state DTO without depending on omni-network directly.
pub use omni_network::{
    BulkStream, InterfaceKind as MeshInterfaceKind, LinkQuality as MeshLinkQuality,
    PeerLinkFacts as MeshPeerLinkFacts, ReachabilityState as MeshReachabilityState,
};

/// Called for each inbound media datagram, tagged with the verified peer it
/// came from. omni-app maps the peer → dialog_key and pushes into its
/// existing realtime-media queue.
pub type MeshMediaCallback = Arc<dyn Fn(MeshNodeAddress, Vec<u8>) + Send + Sync>;

/// Called for each inbound reliable frame (chat / call-control wire-envelope),
/// tagged with the verified peer. omni-app decodes it + reconstructs a
/// ReceivedChatContext and dispatches it like a legacy transport event.
pub type MeshWireCallback = Arc<dyn Fn(MeshNodeAddress, Vec<u8>) + Send + Sync>;

/// Builds the app-level hello blob (a serialized `SessionHelloV1`) to send as
/// the first reliable frame when a Link comes up, given the verified peer. The
/// facade stays identity-agnostic — it only ships the bytes omni-app produces.
pub type MeshHelloMaker = Arc<dyn Fn(MeshNodeAddress) -> Vec<u8> + Send + Sync>;

/// Called when a peer becomes fully unreachable (its last mesh path was pruned), so
/// the app can tear down stale session state (a stale verified-connection would
/// otherwise make the next message try a dead link instead of going durable).
pub type MeshPeerDownCallback = Arc<dyn Fn(MeshNodeAddress) + Send + Sync>;

/// A DHT-proxy request a peer asked us to run on our local DHT (R7 Phase 2 mode b). The app gates
/// (opt-in) + runs the BEP44 op on its `omni-dht-helper` and returns the result. The facade fills the
/// routing (`from`/`req_id`) and ships the response. See docs/decisions/dht-over-peer.md.
#[derive(Debug, Clone)]
pub struct MeshDhtProxyRequest {
    /// The requester (the response is routed back to it).
    pub from: MeshNodeAddress,
    /// Correlator echoed in the response.
    pub req_id: u64,
    /// BEP44 put (`value` is the sealed beacon) / get, or BEP 5 `get_peers` (the info-hash in the
    /// first 20 bytes of `key`).
    pub op: omni_network::DhtProxyOp,
    /// The per-bucket `dht_key` seed (the helper derives the keypair).
    pub key: [u8; 32],
    /// The BEP44 slot salt.
    pub salt: Vec<u8>,
    /// The sealed beacon to publish for a put (empty for a get).
    pub value: Vec<u8>,
}

/// The app's answer to a [`MeshDhtProxyRequest`]: whether the op succeeded + the get result.
#[derive(Debug, Clone, Default)]
pub struct MeshDhtProxyResult {
    pub ok: bool,
    /// The get result (empty for a put / an empty slot).
    pub value: Vec<u8>,
}

/// Called when a peer asks US to run a DHT op on its behalf (DHT-as-a-service over the mesh). The app
/// serves it (opt-in gate → local `omni-dht-helper` IPC) and returns the result; the facade routes the
/// response back. Runs on a blocking worker (the IPC blocks), never the async pump. `None` = we do not
/// act as a DHT proxy (a plain client). See docs/decisions/dht-over-peer.md.
pub type MeshDhtProxyCallback = Arc<dyn Fn(MeshDhtProxyRequest) -> MeshDhtProxyResult + Send + Sync>;

/// Called when a DIRECT neighbour asks us for the in-segment DHT bootstrap addresses we know (step B).
/// The app returns its configured bootstrap candidates (gated on DHT-rendezvous-enabled); the facade
/// ships them back on the link. `None` = we don't share (default off). The answer is independent of who
/// asks — the gate is "am I sharing", not "who". See docs/decisions/dht-bootstrap-mesh-propagation.md.
pub type MeshDhtBootstrapCallback = Arc<dyn Fn() -> Vec<std::net::SocketAddr> + Send + Sync>;

/// Called when a verified peer opens a BULK stream (attachment transfer) on its link. The app owns
/// the stream from here: it reads/writes the transfer protocol on its own worker and drops the
/// stream to close it.
///
/// Runs on a dedicated OS thread, NOT a tokio worker — the app reads a multi-gigabyte transfer
/// synchronously off this stream, which must never occupy a runtime worker (the same reason
/// `on_wire` is moved off the workers; see `MeshRuntime::start`).
pub type MeshBulkStreamCallback = Arc<dyn Fn(MeshNodeAddress, BulkStream) + Send + Sync>;

/// Inbound callbacks for the two planes multiplexed on one verified mesh Link
/// (one QUIC session per peer): reliable wire frames + unreliable media, plus
/// an optional app-handshake hello sent on link-up.
///
/// `on_wire` / `make_hello` are optional: while the omni-app session handshake
/// (which reconstructs a `ReceivedChatContext` from the exchanged hellos) is
/// not yet wired, callers pass `None` — the reliable-plane pump isn't spawned
/// and no hello is sent (no silent drop). Outbound `send_wire` is always
/// available.
#[derive(Clone)]
pub struct MeshCallbacks {
    pub on_wire: Option<MeshWireCallback>,
    pub on_media: MeshMediaCallback,
    pub make_hello: Option<MeshHelloMaker>,
    /// Optional: fired when a peer's last mesh path drops (proactive teardown).
    pub on_peer_down: Option<MeshPeerDownCallback>,
    /// Optional: fired when a peer asks us to run a DHT op on its behalf (DHT-over-peer, mode b).
    /// `None` = not a DHT proxy. See docs/decisions/dht-over-peer.md.
    pub on_dht_proxy_request: Option<MeshDhtProxyCallback>,
    /// Optional: fired when a direct neighbour asks for our DHT bootstrap seeds (step B). `None` = we
    /// don't share. See docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub on_dht_bootstrap_request: Option<MeshDhtBootstrapCallback>,
    /// Optional: fired when a peer opens a BULK stream (attachment transfer) on its verified link.
    /// `None` = we don't accept bulk transfers over the mesh (the stream is dropped, which closes
    /// it). See [`MeshBulkStreamCallback`].
    pub on_bulk_stream: Option<MeshBulkStreamCallback>,
}

/// Default cadence for re-broadcasting our announce so peers' soft-state
/// entries don't expire. Shorter than the 5-min announce TTL.
pub const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(20);

/// Metadata hygiene — idle-loop jitter (network audit 2026-07-12, finding 2). The maintenance loops
/// below (announce, reconcile, UPnP refresh, AutoNAT) all slept EXACTLY `announce_interval`, so an
/// on-path observer saw a metronome: a packet burst every 20.000 s, clock-aligned and identical on
/// every node. That regular beat is a fingerprint on its own — it survives encryption, and it says
/// "Omni" without reading a byte. Each sleep is now drawn from `[interval, interval + 25%]`, and
/// **re-drawn every cycle** — a fixed per-node offset would only move the metronome, not remove it.
///
/// Upper bound stays far inside the 5-min announce TTL (20 s → ≤25 s), so soft state never expires
/// between beats. Mirrors `app_mesh.rs::jittered_rendezvous_sweep_interval_secs` (R7 publish jitter).
const IDLE_LOOP_JITTER_PERCENT: u32 = 25;

/// One jittered maintenance interval: `base + rand(0..=base*25%)`. `OsRng` is not doing anything
/// security-sensitive here (this is timing hygiene, not a secret) — it just needs to be
/// unpredictable enough that the gaps don't form a pattern of their own.
fn jittered(base: Duration) -> Duration {
    use rand_core::{OsRng, RngCore};
    let span_ms = (base.as_millis() as u64).saturating_mul(u64::from(IDLE_LOOP_JITTER_PERCENT)) / 100;
    if span_ms == 0 {
        return base;
    }
    base + Duration::from_millis(OsRng.next_u64() % (span_ms + 1))
}

/// How many linked peers to ask for DHT bootstrap seeds per attempt (step B). We only need one working
/// seed to join the swarm, so keep the fan-out small. See docs/decisions/dht-bootstrap-mesh-propagation.md.
pub const DHT_BOOT_SEED_PEER_FANOUT: usize = 3;

/// How many announce intervals to skip between re-confirmations of a TCP front door that is ALREADY
/// confirmed on the SAME candidate.
///
/// The AutoNAT loop asked for a door confirmation every interval regardless of what it already knew,
/// which on a live mobile link meant a verifier TCP-connecting to our door roughly every 25 s
/// forever, plus the radio wake that goes with it, to re-learn an unchanged answer. Byte-wise this is
/// pennies; the objection is that it is repeated work with a known result, and on a phone every
/// avoidable wake is battery (docs/bugs/uplink-goodput-retransmit.md §P3).
///
/// Not skipped forever, because a UPnP lease can lapse and a NAT can remap: at eight intervals the
/// re-check still lands every few minutes, far inside the hour-long UPnP lease. Never skipped while
/// the answer is still UNKNOWN — that first confirmation gates advertising the door at all, and its
/// latency was already worth an explicit fix once.
const TCP_DOOR_RECONFIRM_SKIPPED_INTERVALS: u32 = 8;

/// One cycle of the door re-confirmation budget: given whether the door is already confirmed on the
/// current candidate and how many cycles are still to be skipped, say whether to probe now and what
/// the budget becomes. Split out of the AutoNAT loop so the policy can be tested without a network.
fn next_door_reconfirm_step(already_confirmed: bool, skips_left: u32) -> (bool, u32) {
    if already_confirmed && skips_left > 0 {
        return (false, skips_left - 1);
    }
    // Confirmed → earn a fresh skip budget. Unknown → no budget at all: an unconfirmed door is
    // probed every cycle until it answers, because nothing can be advertised until it does.
    let next = if already_confirmed {
        TCP_DOOR_RECONFIRM_SKIPPED_INTERVALS
    } else {
        0
    };
    (true, next)
}

#[derive(Debug, Error)]
pub enum MeshRuntimeError {
    #[error("failed to build tokio runtime: {0}")]
    Runtime(String),
    #[error("failed to bind QUIC interface: {0}")]
    QuicBind(String),
    #[error("dial failed: {0}")]
    Dial(String),
    #[error("no active mesh link to peer")]
    NoLink,
    #[error("send failed: {0}")]
    Send(String),
    #[error("invalid Ed25519 public key: {0}")]
    InvalidPubkey(String),
}

/// The carrier of our own hops (see `MeshConfig::carrier_policy`).
pub use omni_network::CarrierPreference;

/// Configuration for starting the mesh.
pub struct MeshConfig {
    /// Account Ed25519 authority signing key (the mesh identity). NodeAddress
    /// derives from its public half.
    pub signing_key_bytes: [u8; 32],
    /// Where the QUIC interface binds. Use `127.0.0.1:0` for same-host tests;
    /// a LAN address for cross-host (address selection is a later refinement).
    pub quic_bind: SocketAddr,
    /// IP_MULTICAST_LOOP — true so two instances on one host see each other.
    pub multicast_loopback: bool,
    /// Local interface (by IPv4) to send/join multicast on. `UNSPECIFIED` = the OS
    /// default route, which is fine on a single-homed host but ambiguous on a
    /// multi-homed one (e.g. a box with a Docker/Hyper-V virtual adapter) — set it
    /// to the real LAN IP for cross-host discovery there.
    pub multicast_iface: Ipv4Addr,
    /// Concrete IPv4 to ADVERTISE in announces, decoupled from `quic_bind` — so a node can
    /// bind dual-stack `[::]` (listening on v4 + v6) yet still announce a dialable LAN v4 and
    /// drive UPnP off it. `None` → advertise the bound `quic_addr` as-is (same-host/loopback).
    /// The live bound port is filled in after bind.
    pub advertise_ipv4: Option<Ipv4Addr>,
    /// Concrete NATIVE global IPv6 to ADVERTISE alongside v4 when bound dual-stack (`None`
    /// on an IPv4-only host). Lets a dual-stack peer dial us directly with no NAT — the
    /// cheapest cross-network reachability path. See docs/decisions/mesh-network-binding.md.
    pub advertise_ipv6: Option<Ipv6Addr>,
    /// How often to re-broadcast our announce.
    pub announce_interval: Duration,
    /// The carrier of our own hops — which carrier(s) `start()` binds and how they are used
    /// (`select_carriers`, `omni_network::CarrierPreference`). See docs/decisions/connection-quality-ladder.md.
    pub carrier_policy: omni_types::CarrierPolicy,
    /// The relay's TLS-over-TCP bridge address (rung-0 carrier target), from the
    /// `network_nodes` registry — the relay hop in `Tcp` and `Auto`; `None` → no TCP relay carrier.
    pub relay_tcp_endpoint: Option<SocketAddr>,
    /// Opt in as a DHT proxy (DHT-as-a-service over the mesh, R7 Phase 2 mode b): advertise the
    /// `ANNOUNCE_CAP_DHT_PROXY` role so peers may delegate `dht_get/put_over_peer` to us, and serve
    /// those requests on our local `omni-dht-helper`. Default false (a plain client). The app sets it
    /// from the DHT-proxy opt-in. See docs/decisions/dht-over-peer.md.
    pub dht_proxy: bool,
    /// Opt in as a **friend-proxy** (Axis 2): advertise `ANNOUNCE_CAP_FRIEND_RELAY` so friends can
    /// discover us as a willing carrier. *Whom* we actually carry is gated by per-friend consent
    /// (`consent_to_relay_for`), NOT this flag. Default false. See docs/decisions/friend-proxy-by-consent.md.
    pub friend_relay: bool,
    /// Fixed port for the direct-TCP front door (`serve_tcp_bridge`), so a user WITH router access can
    /// port-forward a KNOWN port and get a rock-solid relay-less inbound path — the ephemeral UPnP port
    /// (`None`) proved unreliable live (mobile peer's `tcp/reconnect-fail` to a phantom-forwarded map).
    /// `None` = ephemeral bind (rely on UPnP). `Some(p)` = bind that port; falls back to ephemeral if
    /// the port is already taken (e.g. a second same-host instance). See tcp-direct-peer-fallback.md.
    pub direct_tcp_bind_port: Option<u16>,
    /// The QUIC port to try FIRST when `quic_bind` asks for any port (`:0`) — the app passes the
    /// port of the previous run. A peer that remembered our address (its peer book, a beacon still
    /// on a courier) then reaches us after a restart with no rendezvous at all; with a random port
    /// every restart killed every address anyone knew, and reconnecting waited for the free couriers
    /// (live 2026-10-03: 25–331 s for the PC, docs/bugs/reconnect-hangs-on-public-couriers.md).
    /// Taken (a second instance, another app) → falls back to `quic_bind` as given. Ignored when
    /// `quic_bind` names a port of its own.
    pub preferred_quic_port: Option<u16>,
    /// The direct-TCP door port of the previous run, tried first when no FIXED door port is set
    /// (`direct_tcp_bind_port`) — the same reason as `preferred_quic_port`: a peer that remembered our
    /// door (its beacon copy, `PEER_DOOR_REGISTRY`) still reaches it after a restart, and UPnP asks the
    /// router for external = internal, so the mapping stays the same too. Taken → any port.
    pub preferred_direct_tcp_port: Option<u16>,
}

/// Whether the mesh may ask the router for port mappings (UPnP-IGD / NAT-PMP, both carriers).
/// `OMNI_PORTMAP=off` turns it off — a diagnostic switch that lets a two-machine stand model the
/// common case of two NATs WITHOUT a forwarded port (the case a relay exists for) on routers that
/// do speak UPnP. Default: on.
fn portmap_enabled() -> bool {
    std::env::var("OMNI_PORTMAP").map(|v| v.trim() != "off").unwrap_or(true)
}

impl MeshConfig {
    /// Same-host defaults: loopback QUIC + multicast loopback on, suitable for
    /// the two-instance audio acceptance test on one machine. For real cross-host
    /// the caller overrides `quic_bind` (to a LAN address, so the announced QUIC
    /// address is dialable) and optionally `multicast_iface`.
    pub fn same_host(signing_key_bytes: [u8; 32]) -> Self {
        Self {
            signing_key_bytes,
            quic_bind: "127.0.0.1:0".parse().expect("valid loopback addr"),
            multicast_loopback: true,
            multicast_iface: Ipv4Addr::UNSPECIFIED,
            advertise_ipv4: None,
            advertise_ipv6: None,
            announce_interval: DEFAULT_ANNOUNCE_INTERVAL,
            carrier_policy: omni_types::CarrierPolicy::Auto,
            relay_tcp_endpoint: None,
            dht_proxy: false,
            friend_relay: false,
            direct_tcp_bind_port: None,
            preferred_quic_port: None,
            preferred_direct_tcp_port: None,
        }
    }
}

/// How long a carrier switch waits for the relay's TCP endpoint before deciding a mesh restart
/// would not bind its carrier either (`MeshRuntime::switch_carrier_policy`).
const RELAY_TCP_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// R1a connection-quality ladder — pick the QUIC carrier interface(s) `start()` binds for `policy`:
///  - `Quic` → the native UDP carrier alone (`bind_udp`), no TCP fallback.
///  - `Tcp` → the same two carriers as `Auto` (the native socket is what routes a peer over its TCP
///    door), but the network keeps every hop of ours on TCP (`Network::set_carrier_preference`): no
///    raw UDP dial, no punch, no climb, no additive UDP link to the relay. No relay TCP endpoint → the
///    native socket alone, for doors.
///  - `Auto` + a known relay TCP endpoint → BOTH carriers (dual-carrier coexistence, STEP 2b-2),
///    ordered `[TCP-relay, UDP]`. The relay autoconnect dials EACH carrier at its OWN endpoint
///    (`MeshRuntime::dial_relay_dual_carrier`), so the two A↔R links land DISTINCT neighbours and the
///    climber (`Network::upgrade_relay_carriers`) can climb relayed peers TCP→UDP and fail back.
///  - `Auto` with no relay TCP endpoint → today's env-compatible single `bind()` (honours
///    `OMNI_RELAY_TCP`), byte-identical to the proven path.
///
/// Extracted as a free fn so the dispatch is unit-testable. The `Auto+Some` dual case is the ONLY
/// branch that yields two carriers; `dual_carrier_relay_tcp` in `start()` mirrors this condition to
/// know whether to drive the dual-carrier autoconnect. See docs/decisions/connection-quality-ladder.md.
///
/// `is_relay_server` (we run the `OMNI_TCP_BRIDGE` listener) SHORT-CIRCUITS to the native UDP server
/// carrier alone: a relay must NOT bind a TCP-CLIENT carrier to a relay endpoint, because for the
/// base-relay-points-at-self config that means dialing our OWN bridge — which `start()` only stands up
/// AFTER carrier setup, so the connect is refused and (via `?`) would fail the WHOLE mesh-start (the
/// headless-relay `Auto` regression: `bind QUIC: ECONNREFUSED`). A relay reaches the mesh on its native
/// UDP server carrier and serves throttled clients via the bridge; it is not a TCP client of itself.
/// This makes the old `carrier-policy quic_forced` relay workaround unnecessary. See
/// docs/bugs/relay-presence-false-online-dead-peer.md §"Relay-start REGRESSION".
fn carrier_preference_of(policy: omni_types::CarrierPolicy) -> CarrierPreference {
    match policy {
        omni_types::CarrierPolicy::Auto => CarrierPreference::Auto,
        omni_types::CarrierPolicy::Tcp => CarrierPreference::Tcp,
        omni_types::CarrierPolicy::Quic => CarrierPreference::Quic,
    }
}

fn select_carriers(
    policy: omni_types::CarrierPolicy,
    quic_bind: SocketAddr,
    relay_tcp_endpoint: Option<SocketAddr>,
    is_relay_server: bool,
    signing: &SigningKey,
) -> std::io::Result<Vec<Arc<QuicOverUdpInterface>>> {
    use omni_types::CarrierPolicy;
    if is_relay_server {
        return Ok(vec![QuicOverUdpInterface::bind_udp(quic_bind, signing)?]);
    }
    let carriers = match policy {
        CarrierPolicy::Quic => {
            vec![QuicOverUdpInterface::bind_udp(quic_bind, signing)?]
        }
        CarrierPolicy::Tcp if relay_tcp_endpoint.is_none() => {
            vec![QuicOverUdpInterface::bind_udp(quic_bind, signing)?]
        }
        CarrierPolicy::Auto | CarrierPolicy::Tcp => match relay_tcp_endpoint {
            Some(relay) => {
                // The UDP carrier (LAN + direct hole-punch + rendezvous) MUST come up regardless — bind
                // it first, unconditionally.
                let udp = QuicOverUdpInterface::bind_udp(quic_bind, signing)?;
                // The relay TCP carrier is a BEST-EFFORT rung-0 anchor. Its bind does a BLOCKING
                // `connect_timeout` (tcp_transport.rs), so an unreachable relay used to fail the WHOLE
                // mesh start via `?` — a single dead relay killed ALL connectivity (the "failed to bind
                // QUIC interface: connection timed out" freeze; see docs/bugs/mesh-start-dead-relay-fatal.md).
                // Treat its failure as NON-FATAL: run UDP-only; the relay is reached over UDP, and its TCP
                // carrier only comes with the next mesh start (interfaces are bound at start only — a
                // carrier switch restarts the mesh for it when the relay answers on TCP again,
                // `MeshRuntime::switch_carrier_policy`). Preserves the invariant "works with an empty
                // bootstrap".
                match QuicOverUdpInterface::bind_tcp_relay(relay, signing) {
                    Ok(tcp_relay) => vec![tcp_relay, udp], // relay reachable → dual carrier, relay rung-0 first
                    Err(error) => {
                        eprintln!(
                            "mesh: relay TCP carrier bind failed ({error}); starting UDP-only (relay {relay} unreachable)"
                        );
                        vec![udp]
                    }
                }
            }
            None => vec![QuicOverUdpInterface::bind(quic_bind, signing)?],
        },
    };
    Ok(carriers)
}

type LinkMap = Arc<Mutex<HashMap<MeshNodeAddress, Arc<Link>>>>;

/// Running mesh service. Drop tears down the runtime (and all loops).
pub struct MeshRuntime {
    /// `ManuallyDrop` so `Drop` can `shutdown_background()` it instead of letting the tokio `Runtime`'s
    /// own Drop BLOCK on in-flight `spawn_blocking` tasks (a relay reconnect's blocking `connect_timeout`
    /// to a dead relay) — which froze app close. See docs/bugs/mesh-start-dead-relay-fatal.md.
    runtime: std::mem::ManuallyDrop<Runtime>,
    network: Arc<Network>,
    local_address: MeshNodeAddress,
    quic_addr: SocketAddr,
    links: LinkMap,
    callbacks: MeshCallbacks,
    background: Mutex<Vec<JoinHandle<()>>>,
    /// The public external endpoint of our router port mapping — via UPnP-IGD or, if the
    /// router speaks that instead, NAT-PMP — once established (None if not mapped / CGNAT).
    /// This is the address a peer behind a DIFFERENT NAT can actually dial; exposed so the
    /// rendezvous beacon advertises it (the mesh announce is LAN-local). See
    /// docs/decisions/serverless-bootstrap-rendezvous.md.
    mapped_external: Arc<Mutex<Option<SocketAddr>>>,
    /// The RAW (unconfirmed) UPnP/NAT-PMP-mapped TCP front-door endpoint — the TCP analogue of
    /// `mapped_external`. Distinct from `confirmed_tcp_address` (AutoNAT-validated): this is what
    /// the router claims it forwards, BEFORE a verifier proved it. Exposed so the beacon can
    /// advertise the TCP front door OPTIMISTICALLY on first contact (no verifier exists yet — the
    /// chicken-and-egg the UDP side already solves in `upnp_beacon_candidate`), which is the only
    /// relay-less path for a peer on a UDP-hostile carrier (a mobile carrier that drops UDP). See
    /// docs/decisions/tcp-direct-peer-fallback.md + relay-less-first-contact-validate-before-advertise.md.
    mapped_tcp_external: Arc<Mutex<Option<SocketAddr>>>,
    /// The user-configured FIXED direct-TCP front-door port (`MeshConfig::direct_tcp_bind_port`), or
    /// `None` for the ephemeral/UPnP default. When set, the user has manually port-forwarded this TCP
    /// port on their router, so the beacon advertises `reflexive_ip:port` DIRECTLY — no UPnP mapping
    /// needed (the whole point: UPnP forwarding proved unreliable live). See tcp-direct-peer-fallback.md.
    configured_direct_tcp_port: Option<u16>,
    /// The port the direct-TCP door actually listens on this run (`None` = no door).
    direct_tcp_port: Option<u16>,
    /// R1a connection-quality ladder — the relay TLS-over-TCP bridge endpoint we bound a dual TCP
    /// carrier for (`Some` only in dual-carrier Auto). The autoconnect dials THIS relay over both
    /// carriers (`dial_relay_dual_carrier`); other registry relays stay single-carrier UDP. See
    /// docs/decisions/connection-quality-ladder.md.
    dual_carrier_relay_tcp: Option<SocketAddr>,
    /// The relay's TCP bridge endpoint from the config (`None` on a relay server, which is no TCP
    /// client of a relay): a live switch to TCP or Auto needs the TCP carrier bound to it.
    relay_tcp_endpoint: Option<SocketAddr>,
    /// The two roles we advertise, kept so either setter recomputes the whole capabilities bitfield
    /// without clobbering the other bit. The DHT-proxy role follows our own Mainline: the app turns it
    /// off while our engine sees no swarm, so a friend does not pick a proxy that cannot serve
    /// ([`set_dht_proxy_advertised`](Self::set_dht_proxy_advertised)).
    dht_proxy_advertised: std::sync::atomic::AtomicBool,
    friend_relay_advertised: std::sync::atomic::AtomicBool,
    /// When our carrier was last switched on this mesh (`switch_carrier_policy`): the links it closed
    /// are redialed over the new carrier in seconds, and a rendezvous for them meanwhile only costs
    /// the free providers (`carrier_switch_settling`).
    carrier_switched_at: Mutex<Option<std::time::Instant>>,
}

/// How long after a carrier switch the links it closed are left to the redial before a rendezvous
/// looks for them: the door redial takes ~2-3 s on the stand.
pub const CARRIER_SWITCH_SETTLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Durable store-and-forward TTL for a 1:1 message: held this long for an offline
/// recipient before GC. Generous so non-overlapping online windows still bridge.
const DURABLE_TTL_SECS: u32 = 7 * 24 * 60 * 60;

/// Upper bound on one reliable send (`send_wire` / `send_durable`). A QUIC uni-stream open can
/// park INDEFINITELY when the peer's MAX_STREAMS credit updates can't reach us (asymmetric mobile
/// flow-throttle eats peer→us packets while our sends keep opening streams) — and a blocking send
/// facade re-entered from the wire pump then parks the RECEIVE plane too. Bounded: a link that
/// can't accept a send in this window is effectively dead for the live path; callers already
/// treat a send error as "fall back to durable/retry".
const RELIABLE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// UPnP port-mapping lease (seconds); refreshed at ~80% so the mapping never lapses.
const MESH_UPNP_LEASE_SECS: u32 = 60 * 60;

/// Whether an IPv4 is genuinely internet-routable — so a UPnP mapping's external address
/// is worth advertising. A private/CGNAT external means inbound from the internet is still
/// blocked by an upstream NAT, so the mapping doesn't help a remote peer reach us.
fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_documentation()
        || ip.is_broadcast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000) // 100.64.0.0/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || (o[0] & 240 == 240))
}

/// Best-effort: ask the local router (UPnP-IGD) to forward a UDP port to our mesh QUIC
/// `internal_port` on `lan_ip`, returning the **public** external endpoint to advertise.
/// `None` if no IGD gateway answers, the request fails, or the external IP isn't
/// internet-public (CGNAT — inbound still blocked upstream). This makes a node behind a
/// UPnP-capable router directly dialable even on an otherwise-symmetric NAT, so the peer
/// connects OUT to it without a relay. See docs/decisions/serverless-bootstrap-rendezvous.md.
async fn map_upnp(
    lan_ip: Ipv4Addr,
    internal_port: u16,
    protocol: igd_next::PortMappingProtocol,
) -> Option<SocketAddr> {
    let options = igd_next::SearchOptions {
        bind_addr: SocketAddr::new(IpAddr::V4(lan_ip), 0),
        timeout: Some(Duration::from_secs(4)),
        ..Default::default()
    };
    // Instrumented (silent `.ok()?` swallowed the reason live): the automatic UPnP door is the
    // relay-less target path, so each failure point must be visible in the harness (daemon stderr).
    let gateway = match igd_next::aio::tokio::search_gateway(options).await {
        Ok(gateway) => gateway,
        Err(error) => {
            eprintln!("[omni-upnp] {protocol:?}: no IGD gateway on {lan_ip} ({error})");
            return None;
        }
    };
    let local = SocketAddr::new(IpAddr::V4(lan_ip), internal_port);
    // Request a STABLE external port EQUAL to the internal one (AddPortMapping) rather than letting
    // the router pick (AddAnyPortMapping): some routers hand out a NEW external port on every
    // refresh, so the door we advertised goes stale within one announce cycle and the peer's dial
    // times out (live harness finding: router rotated 57169→54046→46919→… every cycle). A fixed
    // external port keeps the advertised door valid across refreshes. Fall back to any-port only if
    // the specific port is rejected (already mapped / router policy).
    let external = match gateway
        .add_port(protocol, internal_port, local, MESH_UPNP_LEASE_SECS, "Omni mesh")
        .await
    {
        Ok(()) => match gateway.get_external_ip().await {
            Ok(ip) => SocketAddr::new(ip, internal_port),
            Err(error) => {
                eprintln!("[omni-upnp] {protocol:?}: mapped {internal_port} but get_external_ip failed ({error})");
                return None;
            }
        },
        Err(add_error) => match gateway
            .get_any_address(protocol, local, MESH_UPNP_LEASE_SECS, "Omni mesh")
            .await
        {
            Ok(external) => {
                eprintln!("[omni-upnp] {protocol:?}: fixed-port add_port({internal_port}) rejected ({add_error}); router chose {external} (may rotate)");
                external
            }
            Err(error) => {
                eprintln!("[omni-upnp] {protocol:?}: AddPortMapping+AddAnyPortMapping both failed ({error})");
                return None;
            }
        },
    };
    match external.ip() {
        IpAddr::V4(ip) if is_public_ipv4(ip) => {
            eprintln!("[omni-upnp] {protocol:?}: mapped {lan_ip}:{internal_port} -> {external} (public)");
            Some(external)
        }
        other => {
            eprintln!("[omni-upnp] {protocol:?}: external {other} is not internet-public (CGNAT) — withheld");
            None
        }
    }
}

/// NAT-PMP (RFC 6886) wire helpers — pure encode/decode, no I/O. NAT-PMP is the simple
/// predecessor to PCP; some routers offer it instead of UPnP-IGD (notably Apple, and
/// security setups with UPnP off), so trying it as a fallback reaches more home routers —
/// the same "any reachable path" approach a torrent client takes. The UDP exchange +
/// gateway discovery are in `map_nat_pmp_udp`. Encoding is fixed-width + big-endian.
mod nat_pmp {
    use std::net::Ipv4Addr;

    /// NAT-PMP listens on the default gateway, UDP.
    pub const PORT: u16 = 5351;
    const VERSION: u8 = 0;
    const OP_EXTERNAL_ADDR: u8 = 0;
    /// Map-request opcodes (RFC 6886): 1 = UDP, 2 = TCP. `pub` so the protocol-generic
    /// `map_nat_pmp` can pass the matching op to `parse_map_response`.
    pub const OP_MAP_UDP: u8 = 1;
    pub const OP_MAP_TCP: u8 = 2;
    /// A response op is the request op + 128.
    const RESPONSE_FLAG: u8 = 128;

    /// "Public address request" packet: `[version=0, op=0]`.
    pub fn external_address_request() -> [u8; 2] {
        [VERSION, OP_EXTERNAL_ADDR]
    }

    /// Parse the public-address response (≥12 bytes: `ver, op, result(2), epoch(4), ip(4)`).
    /// Returns the external IPv4 on result-code 0, else None.
    pub fn parse_external_address_response(buf: &[u8]) -> Option<Ipv4Addr> {
        if buf.len() < 12 || buf[0] != VERSION || buf[1] != OP_EXTERNAL_ADDR + RESPONSE_FLAG {
            return None;
        }
        if u16::from_be_bytes([buf[2], buf[3]]) != 0 {
            return None; // non-zero result code
        }
        Some(Ipv4Addr::new(buf[8], buf[9], buf[10], buf[11]))
    }

    /// "Map port" request: `ver, op, reserved(2)=0, internal(2), suggested_external(2),
    /// lifetime(4)`. `op` selects the protocol (`OP_MAP_UDP` / `OP_MAP_TCP`).
    fn map_request(op: u8, internal_port: u16, suggested_external: u16, lifetime_secs: u32) -> [u8; 12] {
        let internal = internal_port.to_be_bytes();
        let external = suggested_external.to_be_bytes();
        let lifetime = lifetime_secs.to_be_bytes();
        [
            VERSION, op, 0, 0, internal[0], internal[1], external[0], external[1],
            lifetime[0], lifetime[1], lifetime[2], lifetime[3],
        ]
    }

    /// Map a UDP port (op 1) — the QUIC endpoint.
    pub fn map_udp_request(internal_port: u16, suggested_external: u16, lifetime_secs: u32) -> [u8; 12] {
        map_request(OP_MAP_UDP, internal_port, suggested_external, lifetime_secs)
    }

    /// Map a TCP port (op 2) — the direct-TCP front door (task 3 NAT-PMP fallback).
    pub fn map_tcp_request(internal_port: u16, suggested_external: u16, lifetime_secs: u32) -> [u8; 12] {
        map_request(OP_MAP_TCP, internal_port, suggested_external, lifetime_secs)
    }

    /// Parse the map response (≥16 bytes: `ver, op, result(2), epoch(4), internal(2),
    /// external(2), lifetime(4)`). `op` is the request op (`OP_MAP_UDP` / `OP_MAP_TCP`); the
    /// response op must be `op + 128` (so a UDP response can't satisfy a TCP map and vice
    /// versa). Returns the mapped external port on result-code 0 with a matching internal port.
    pub fn parse_map_response(buf: &[u8], op: u8, expected_internal: u16) -> Option<u16> {
        if buf.len() < 16 || buf[0] != VERSION || buf[1] != op + RESPONSE_FLAG {
            return None;
        }
        if u16::from_be_bytes([buf[2], buf[3]]) != 0 {
            return None; // non-zero result code
        }
        if u16::from_be_bytes([buf[8], buf[9]]) != expected_internal {
            return None; // response is for a different internal port
        }
        Some(u16::from_be_bytes([buf[10], buf[11]]))
    }
}

/// The IPv4 default gateway (router) — needed to address NAT-PMP requests. Windows:
/// `GetBestRoute` for the default route (dest `0.0.0.0`); its next-hop is the gateway. Raw
/// `iphlpapi` (no Windows crate, per CONTRIBUTING.md §Android readiness). Other platforms return
/// None (fail-closed — the host supplies routing later), so NAT-PMP simply doesn't run there.
#[cfg(windows)]
fn detect_default_gateway_ipv4() -> Option<Ipv4Addr> {
    // MIB_IPFORWARDROW (Win32) — all DWORDs; we only read `next_hop` (network byte order).
    #[repr(C)]
    struct MibIpForwardRow {
        dest: u32,
        mask: u32,
        policy: u32,
        next_hop: u32,
        if_index: u32,
        fwd_type: u32,
        proto: u32,
        age: u32,
        next_hop_as: u32,
        metric1: u32,
        metric2: u32,
        metric3: u32,
        metric4: u32,
        metric5: u32,
    }
    #[link(name = "iphlpapi")]
    extern "system" {
        fn GetBestRoute(dest_addr: u32, source_addr: u32, best_route: *mut MibIpForwardRow) -> u32;
    }
    let mut row: MibIpForwardRow = unsafe { std::mem::zeroed() };
    // dest 0.0.0.0 → the default route; src 0 → let the stack choose. NO_ERROR == 0.
    if unsafe { GetBestRoute(0, 0, &mut row) } != 0 {
        return None;
    }
    // `next_hop` is a network-order DWORD; its native bytes are the octets in order.
    let gateway = Ipv4Addr::from(row.next_hop.to_ne_bytes());
    if gateway.is_unspecified() || gateway.is_loopback() {
        None
    } else {
        Some(gateway)
    }
}

#[cfg(not(windows))]
fn detect_default_gateway_ipv4() -> Option<Ipv4Addr> {
    None
}

/// Best-effort NAT-PMP port-map fallback (when UPnP-IGD didn't answer): ask the default
/// `gateway` to forward our `internal_port` for `protocol` (UDP = the QUIC endpoint, TCP = the
/// direct-TCP front door), returning the **public** external endpoint to advertise. `None` if
/// the gateway doesn't speak NAT-PMP, the request times out, or the external IP isn't
/// internet-public (CGNAT — inbound still blocked upstream). Mirrors `map_upnp`.
/// See docs/decisions/mesh-network-binding.md.
async fn map_nat_pmp(
    lan_ip: Ipv4Addr,
    gateway: Ipv4Addr,
    internal_port: u16,
    protocol: igd_next::PortMappingProtocol,
) -> Option<SocketAddr> {
    let socket = tokio::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(lan_ip), 0))
        .await
        .ok()?;
    socket
        .connect(SocketAddr::new(IpAddr::V4(gateway), nat_pmp::PORT))
        .await
        .ok()?;
    let mut buf = [0u8; 32];

    // 1. Learn our public IP (a non-NAT-PMP gateway never answers → timeout → None).
    socket.send(&nat_pmp::external_address_request()).await.ok()?;
    let n = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    let external_ip = nat_pmp::parse_external_address_response(&buf[..n])?;
    if !is_public_ipv4(external_ip) {
        return None; // CGNAT external — the mapping wouldn't be reachable from the internet
    }

    // 2. Map the port for the requested protocol (suggest the same external port; lifetime = the
    // refresh lease). The response op must match, so a UDP gateway reply can't satisfy a TCP map.
    let (request, op) = match protocol {
        igd_next::PortMappingProtocol::TCP => (
            nat_pmp::map_tcp_request(internal_port, internal_port, MESH_UPNP_LEASE_SECS),
            nat_pmp::OP_MAP_TCP,
        ),
        _ => (
            nat_pmp::map_udp_request(internal_port, internal_port, MESH_UPNP_LEASE_SECS),
            nat_pmp::OP_MAP_UDP,
        ),
    };
    socket.send(&request).await.ok()?;
    let n = tokio::time::timeout(Duration::from_secs(2), socket.recv(&mut buf))
        .await
        .ok()?
        .ok()?;
    let external_port = nat_pmp::parse_map_response(&buf[..n], op, internal_port)?;
    Some(SocketAddr::new(IpAddr::V4(external_ip), external_port))
}

/// This host's primary LAN IPv4 — the source address the OS would use to reach the internet RIGHT NOW.
/// Re-evaluated each announce cycle so it FOLLOWS a network change (Wi-Fi → mobile, dock, VPN): a node
/// that roamed must stop advertising the previous network's address. No packet is sent — `connect` on a
/// UDP socket only resolves the route's source. `None` on loopback/unspecified/no route.
pub fn detect_primary_lan_ipv4() -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Whether an IPv6 address is NATIVE globally-routable unicast (directly internet-reachable, no NAT).
/// Global unicast `2000::/3`, excluding documentation `2001:db8::/32` and the v6-over-v4 tunnels Teredo
/// (`2001:0::/32`) and 6to4 (`2002::/16`) — globally-routable on paper but ride flaky v4 relays, not the
/// directly-dialable native endpoint to advertise. See docs/decisions/mesh-network-binding.md.
pub fn is_global_ipv6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] & 0xe000) == 0x2000
        && !(s[0] == 0x2001 && s[1] == 0x0db8)
        && !(s[0] == 0x2001 && s[1] == 0x0000)
        && s[0] != 0x2002
}

/// This host's primary global IPv6 source address (or `None`). Re-evaluated each announce cycle (same
/// roaming reason as [`detect_primary_lan_ipv4`]); no packet sent (UDP-connect route trick to Google DNS).
pub fn detect_global_ipv6() -> Option<Ipv6Addr> {
    let socket = std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok()?;
    socket
        .connect((Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888), 53))
        .ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V6(ip) if is_global_ipv6(ip) => Some(ip),
        _ => None,
    }
}

/// Where a TCP bridge (relay or direct-TCP door) forwards decapsulated QUIC to OUR OWN quinn. quinn
/// bound to a SPECIFIC interface (e.g. the real LAN IP, forced so UPnP maps the right interface) does
/// NOT also listen on loopback, so forwarding to `127.0.0.1` black-holes the handshake. Forward to the
/// bind address itself in that case; only an unspecified/loopback bind (which DOES cover loopback) uses
/// `127.0.0.1`. Pure → unit-tested. See docs/decisions/tcp-direct-peer-fallback.md.
fn local_quic_target(quic_addr: SocketAddr) -> SocketAddr {
    if quic_addr.ip().is_unspecified() || quic_addr.ip().is_loopback() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), quic_addr.port())
    } else {
        quic_addr
    }
}

/// Assemble the QUIC addresses to ADVERTISE (announce) from the resolved advertise config +
/// the live bound port + the current UPnP external mapping. With explicit `advertise_ipv4`/
/// `advertise_ipv6` (networked / dual-stack), advertise those at the bound port — NOT the
/// bind address, which for a dual-stack socket is the undialable unspecified `[::]`. With
/// neither (same-host / loopback), fall back to the bound `quic_addr` unchanged — so the
/// proven IPv4/loopback path is byte-identical. The UPnP external (its own port) is appended
/// when present. Pure → unit-tested. See docs/decisions/mesh-network-binding.md.
fn build_advertised_addresses(
    advertise_ipv4: Option<Ipv4Addr>,
    advertise_ipv6: Option<Ipv6Addr>,
    port: u16,
    quic_addr: SocketAddr,
    mapped_external: Option<SocketAddr>,
) -> Vec<SocketAddr> {
    let mut addrs = Vec::new();
    if let Some(v4) = advertise_ipv4 {
        addrs.push(SocketAddr::new(IpAddr::V4(v4), port));
    }
    if let Some(v6) = advertise_ipv6 {
        addrs.push(SocketAddr::new(IpAddr::V6(v6), port));
    }
    if addrs.is_empty() {
        addrs.push(quic_addr);
    }
    if let Some(ext) = mapped_external {
        addrs.push(ext);
    }
    addrs
}

impl MeshRuntime {
    /// Start the mesh: bind interfaces, start the dispatcher, and spawn the
    /// announce + accept loops. Blocks until setup completes.
    pub fn start(config: MeshConfig, mut callbacks: MeshCallbacks) -> Result<Self, MeshRuntimeError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| MeshRuntimeError::Runtime(e.to_string()))?;

        // Move `on_wire` OFF the tokio workers onto a dedicated OS thread. The app's on_wire makes
        // BLOCKING mesh sends (the symmetric hello-reply's `send_wire`, receipts) — and `send_wire`
        // routes through `run_blocking`, which, when it detects it is ON a tokio worker, spawns the
        // send and blocks the worker on a channel waiting for it. Called from the wire pump (itself a
        // tokio task) that meant a worker blocked on a task the same starved runtime had to drive:
        // the `RELIABLE_SEND_TIMEOUT` never fired and the pump hung forever on frame 1 (live-stand
        // pairs #6-#9: `on_wire/hello sending hello reply (send_wire)` with no successor). Running
        // on_wire on a plain OS thread makes `run_blocking` take its off-runtime `block_on` branch,
        // which drives the timeout itself — no worker is ever blocked. A single serial executor
        // preserves frame order and matches the app's own session-lock serialization. See
        // docs/bugs/relay-less-mobile-transport-findings.md §RESOLVED.
        if let Some(real_on_wire) = callbacks.on_wire.take() {
            let (tx, rx) = std::sync::mpsc::channel::<(MeshNodeAddress, Vec<u8>)>();
            std::thread::Builder::new()
                .name("omni-on-wire".to_string())
                .spawn(move || {
                    while let Ok((peer, bytes)) = rx.recv() {
                        real_on_wire(peer, bytes);
                    }
                })
                .map_err(|e| MeshRuntimeError::Runtime(format!("on_wire executor: {e}")))?;
            let tx = std::sync::Mutex::new(tx);
            callbacks.on_wire = Some(Arc::new(move |peer, bytes| {
                // Fire-and-forget enqueue — never blocks the caller (safe on a tokio worker). A send
                // error means the executor thread is gone (runtime shutting down); drop the frame.
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send((peer, bytes));
                }
            }));
        }

        let signing = SigningKey::from_bytes(&config.signing_key_bytes);
        let links: LinkMap = Arc::new(Mutex::new(HashMap::new()));
        let announce_interval = config.announce_interval;
        let quic_bind = config.quic_bind;
        let multicast_loopback = config.multicast_loopback;
        let multicast_iface = config.multicast_iface;
        let advertise_ipv4 = config.advertise_ipv4;
        let advertise_ipv6 = config.advertise_ipv6;
        let carrier_policy = config.carrier_policy;
        let relay_tcp_endpoint = config.relay_tcp_endpoint;
        let preferred_quic_bind = config
            .preferred_quic_port
            .filter(|port| *port != 0 && quic_bind.port() == 0)
            .map(|port| SocketAddr::new(quic_bind.ip(), port));
        // Roles we advertise in our announce (R7 2e-4 + friend-proxy Axis 2), OR'd together.
        let dht_proxy_advertised = config.dht_proxy;
        let advertised_capabilities = (if config.dht_proxy {
            omni_network::ANNOUNCE_CAP_DHT_PROXY
        } else {
            0
        }) | (if config.friend_relay {
            omni_network::ANNOUNCE_CAP_FRIEND_RELAY
        } else {
            0
        });
        // A relay server runs the `OMNI_TCP_BRIDGE` listener (stood up below). It must not act as a
        // TCP-CLIENT carrier of a relay (for a base-relay-points-at-self config that means dialing its
        // OWN not-yet-listening bridge → ECONNREFUSED → whole-mesh-start failure — the headless-relay
        // `Auto` regression). So it binds the native UDP server carrier alone and drives no dual-carrier
        // autoconnect. See `select_carriers` + docs/bugs/relay-presence-false-online-dead-peer.md.
        let is_relay_server = std::env::var("OMNI_TCP_BRIDGE").is_ok();

        // R1a — the relay TCP endpoint we bind a dual TCP carrier for (Auto + a known endpoint).
        // `Some` ONLY in the dual-carrier case; mirrors `select_carriers`' `Auto+Some` branch. The
        // autoconnect uses it to know which registry relay is the rung-0 TCP anchor. Never for a relay
        // server (it doesn't dual-carrier-dial itself).
        let dual_carrier_relay_tcp = match carrier_policy {
            omni_types::CarrierPolicy::Auto | omni_types::CarrierPolicy::Tcp if !is_relay_server => {
                relay_tcp_endpoint
            }
            _ => None,
        };

        // All interface binds + spawns must happen inside the runtime context.
        let setup = runtime.block_on(async move {
            // R1a connection-quality ladder — pick the carrier(s) per policy (see `select_carriers`).
            // `is_relay_server` (computed above from `OMNI_TCP_BRIDGE`) forces the native UDP server
            // carrier alone — a relay must not bind a TCP-client carrier to its own not-yet-up bridge.
            let bind_at = |bind: SocketAddr| {
                select_carriers(
                    carrier_policy,
                    bind,
                    relay_tcp_endpoint,
                    is_relay_server,
                    &signing,
                )
            };
            // The previous run's port first (see `MeshConfig::preferred_quic_port`), any port if
            // it is taken — start never fails on it.
            let carriers = match preferred_quic_bind {
                Some(preferred) => bind_at(preferred).or_else(|error| {
                    omni_network::diag_trace(
                        "mesh/bind",
                        &format!("preferred QUIC port {} unavailable ({error}); any port", preferred.port()),
                    );
                    bind_at(quic_bind)
                }),
                None => bind_at(quic_bind),
            }
            .map_err(|e| MeshRuntimeError::QuicBind(e.to_string()))?;
            // Announce the native UDP socket (what peers dial for a direct rung-2 path); fall back to
            // the sole carrier when there is no native UDP socket.
            let announce_carrier = carriers
                .iter()
                .find(|c| c.kind() == MeshInterfaceKind::QuicOverUdp)
                .or_else(|| carriers.first())
                .cloned()
                .expect("select_carriers yields at least one carrier");
            let quic_addr = announce_carrier
                .local_addr()
                .map_err(|e| MeshRuntimeError::QuicBind(e.to_string()))?;

            // LAN neighbours are a bonus, not the way in. A network that refuses the multicast socket
            // (live 2026-10-13: a hotspot answering WSAEHOSTUNREACH, 10065) used to fail the WHOLE start
            // — no mesh, no contacts, no calls — while QUIC, the TCP front door and the couriers would
            // all have worked. Now the mesh starts without the LAN interface and says why.
            let multicast = match MulticastInterface::bind(MulticastConfig {
                loopback: multicast_loopback,
                local_iface: multicast_iface,
                ..MulticastConfig::default()
            }) {
                Ok(multicast) => Some(multicast),
                Err(error) => {
                    omni_network::diag_trace("mesh/bind", &format!("no LAN multicast ({error}); starting without LAN neighbours"));
                    None
                }
            };

            let interfaces: Vec<Arc<dyn Interface>> = carriers
                .into_iter()
                .map(|c| c as Arc<dyn Interface>)
                .chain(multicast.map(|m| m as Arc<dyn Interface>))
                .collect();
            let network = Arc::new(Network::new(signing, interfaces));
            network.set_capabilities(advertised_capabilities);
            network.set_carrier_preference(carrier_preference_of(carrier_policy));
            let local_address = network.local_address();
            network.start();

            Ok::<_, MeshRuntimeError>((network, local_address, quic_addr))
        })?;
        let (network, local_address, quic_addr) = setup;

        let mut background = Vec::new();

        // TCP relay-fallback bridge (relay role): when `OMNI_TCP_BRIDGE=host:port` is set, accept
        // TCP and pipe each connection's framed datagrams to/from our local QUIC UDP port, so
        // clients on UDP-throttling carriers reach us over TCP (the carrier leaves TCP alone). The
        // quinn endpoint is untouched — each bridged conn looks like a loopback UDP peer. See
        // docs/decisions/tcp-relay-fallback.md.
        if let Ok(bridge_listen) = std::env::var("OMNI_TCP_BRIDGE") {
            match bridge_listen.parse::<SocketAddr>() {
                Ok(listen_addr) => {
                    let quic_local = local_quic_target(quic_addr);
                    background.push(runtime.spawn(async move {
                        if let Err(e) =
                            omni_network::tcp_transport::run_tcp_bridge(listen_addr, quic_local).await
                        {
                            eprintln!("[omni-mesh] tcp relay bridge ended: {e}");
                        }
                    }));
                }
                Err(e) => eprintln!("[omni-mesh] bad OMNI_TCP_BRIDGE '{bridge_listen}': {e}"),
            }
        }

        // Direct-TCP front door (task 3): stand up `serve_tcp_bridge` in front of OUR OWN quinn so a
        // peer on a UDP-hostile carrier can reach us over TCP-direct — the SAME bridge the
        // relay runs, run by a peer. Bind ephemeral on 0.0.0.0 so a UPnP-forwarded port lands here;
        // learn the port so the UPnP loop below can map + advertise it. Best-effort: a failure just
        // means no TCP front door (UDP-direct / relay still apply). See tcp-direct-peer-fallback.md.
        // Prefer the configured fixed port (so the user can port-forward a KNOWN port on their router
        // for a reliable relay-less inbound path); if it is already taken (e.g. a second same-host
        // instance) fall back to an ephemeral port so start never fails on the front door.
        // Without a fixed port, the previous run's door port first (`preferred_direct_tcp_port`).
        let wanted_door_port = config
            .direct_tcp_bind_port
            .or(config.preferred_direct_tcp_port)
            .filter(|port| *port != 0);
        let direct_tcp_listener = match wanted_door_port {
            Some(port) => {
                std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).or_else(|e| {
                    eprintln!(
                        "[omni-mesh] direct-tcp port {port} unavailable ({e}); using ephemeral"
                    );
                    std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
                })
            }
            None => std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)),
        };
        let direct_tcp_internal: Option<u16> =
            match direct_tcp_listener {
                Ok(std_listener) => {
                    let port = std_listener.local_addr().ok().map(|a| a.port());
                    if port.is_some() {
                        std_listener.set_nonblocking(true).ok();
                        // Forward bridged QUIC to the address quinn is ACTUALLY bound to. A
                        // specific-interface bind (e.g. forced to the real LAN IP so UPnP picks the
                        // right interface) does NOT listen on loopback, so the old hardcoded
                        // 127.0.0.1 black-holed the handshake (live harness: `bridge-accept` fired but
                        // quinn `connect: timed out`). Unspecified/loopback bind covers loopback →
                        // 127.0.0.1 stays correct there. See tcp-direct-peer-fallback.md.
                        let quic_local = local_quic_target(quic_addr);
                        background.push(runtime.spawn(async move {
                            match tokio::net::TcpListener::from_std(std_listener) {
                                Ok(listener) => {
                                    if let Err(e) = omni_network::tcp_transport::run_tcp_bridge_on(
                                        listener, quic_local,
                                    )
                                    .await
                                    {
                                        eprintln!("[omni-mesh] direct-tcp front door ended: {e}");
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[omni-mesh] direct-tcp listener adopt failed: {e}")
                                }
                            }
                        }));
                    }
                    port
                }
                Err(e) => {
                    eprintln!("[omni-mesh] direct-tcp bind failed: {e}");
                    None
                }
            };
        // Peers on our LAN learn the door from our announces: without UPnP it is in no beacon.
        network.set_door_port(direct_tcp_internal.unwrap_or(0));

        // UPnP-IGD reachability (networked binds only): ask the local router to forward our
        // QUIC UDP port and advertise the public external endpoint, so a peer can dial us
        // directly even behind an otherwise-symmetric NAT (it connects OUT to us — no relay
        // needed, the "torrent-like" reachability win). Best-effort + refreshed before the
        // lease expires; loopback / unspecified / CGNAT yield nothing. See
        // docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
        // The v4 LAN address UPnP forwards to: the explicit `advertise_ipv4` (set when bound
        // dual-stack `[::]`, where `quic_addr.ip()` is the unspecified `::`), else the bound
        // v4 address (the proven networked path). loopback / unspecified / IPv4-less → skip.
        // Bound explicit-v4 fallback (when the route trick yields nothing but we bound a specific v4).
        let bound_v4 = match quic_addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
            _ => None,
        };
        let mapped_external: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let mapped_tcp_external: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        {
            let slot = Arc::clone(&mapped_external);
            let tcp_slot = Arc::clone(&mapped_tcp_external);
            let tcp_internal = direct_tcp_internal;
            let internal_port = quic_addr.port();
            if !portmap_enabled() {
                omni_network::diag_trace("mesh/upnp", "OMNI_PORTMAP=off: no router port mapping");
            }
            background.push(runtime.spawn(async move {
                if !portmap_enabled() {
                    return;
                }
                // Re-derive the LAN IPv4 EACH cycle so a network change (roaming) re-maps on the NEW
                // interface and DROPS a stale mapping from the previous network — checked at the announce
                // cadence, NOT the hour-long lease, so a roamed node stops advertising a dead endpoint
                // promptly (the home→mobile bug). No LAN IPv4 (CGNAT/mobile) → no UPnP endpoint advertised.
                let mut last_lan: Option<Ipv4Addr> = None;
                let mut last_mapped = std::time::Instant::now()
                    .checked_sub(Duration::from_secs(MESH_UPNP_LEASE_SECS as u64))
                    .unwrap_or_else(std::time::Instant::now);
                loop {
                    // Prefer the interface the QUIC socket is actually BOUND to (`bound_v4`) over the
                    // 8.8.8.8 route-trick guess: the UPnP mapping must forward to the socket's real
                    // interface, and the route trick is hijacked by a capturing VPN/proxy (live: a
                    // the VPN's tunnel adapter picked the tunnel's `172.19.0.1`, so SSDP went to a network with no
                    // IGD → no door). When bound dual-stack `[::]`/unspecified (`bound_v4` = None) we
                    // still fall back to the route trick. See tcp-direct-peer-fallback.md.
                    let lan = bound_v4.or_else(detect_primary_lan_ipv4);
                    let lease_due = last_mapped.elapsed()
                        >= Duration::from_secs(MESH_UPNP_LEASE_SECS as u64 * 4 / 5);
                    if lan != last_lan || lease_due {
                        let external = match lan {
                            // Prefer UPnP-IGD; fall back to NAT-PMP so a router with EITHER is reached.
                            // The gateway is re-detected here too (it changes on roaming).
                            Some(ip) => {
                                let mut e =
                                    map_upnp(ip, internal_port, igd_next::PortMappingProtocol::UDP)
                                        .await;
                                if e.is_none() {
                                    if let Some(gw) = detect_default_gateway_ipv4() {
                                        e = map_nat_pmp(ip, gw, internal_port, igd_next::PortMappingProtocol::UDP).await;
                                    }
                                }
                                e
                            }
                            None => None,
                        };
                        *slot.lock() = external;
                        // Task 3: map the direct-TCP front door's port too, so a peer can dial us
                        // over TCP-direct when UDP is throttled. Prefer UPnP-IGD; fall back to
                        // NAT-PMP-TCP so a router with EITHER is reached (mirrors the UDP path).
                        // Best-effort, same lease cadence as the UDP mapping.
                        let tcp_external = match (lan, tcp_internal) {
                            (Some(ip), Some(tport)) => {
                                let mut e =
                                    map_upnp(ip, tport, igd_next::PortMappingProtocol::TCP).await;
                                if e.is_none() {
                                    if let Some(gw) = detect_default_gateway_ipv4() {
                                        e = map_nat_pmp(
                                            ip,
                                            gw,
                                            tport,
                                            igd_next::PortMappingProtocol::TCP,
                                        )
                                        .await;
                                    }
                                }
                                e
                            }
                            _ => None,
                        };
                        *tcp_slot.lock() = tcp_external;
                        last_lan = lan;
                        last_mapped = std::time::Instant::now();
                    }
                    tokio::time::sleep(jittered(announce_interval)).await;
                }
            }));
        }

        // Announce loop: re-derive advertised v4/v6 EACH cycle (prefer the live route, fall back to the
        // configured hint) + the live UPnP mapping — so a roamed node advertises its CURRENT network and
        // never a stale endpoint from a previous one.
        {
            let net = Arc::clone(&network);
            let upnp = Arc::clone(&mapped_external);
            let port = quic_addr.port();
            background.push(runtime.spawn(async move {
                loop {
                    let v4 = detect_primary_lan_ipv4().or(advertise_ipv4);
                    let v6 = detect_global_ipv6().or(advertise_ipv6);
                    let addrs = build_advertised_addresses(v4, v6, port, quic_addr, *upnp.lock());
                    let _ = net.announce(addrs).await;
                    tokio::time::sleep(jittered(announce_interval)).await;
                }
            }));
        }

        // Durable reconcile + relay→direct upgrade loop: periodically re-sync
        // store-and-forward frames with all peers (so held frames propagate A→B→C over
        // time, not only on a fresh link — P4 durable delivery), AND try to hole-punch
        // relay-only peers up to a direct link (P2 — the real-NAT path; without this the
        // punch lived only in tests and a cross-network peer stayed relay-only forever).
        {
            let net = Arc::clone(&network);
            background.push(runtime.spawn(async move {
                loop {
                    tokio::time::sleep(jittered(announce_interval)).await;
                    net.reconcile_all_peers().await;
                    net.upgrade_relay_links_via_punch().await;
                    // R1a connection-quality ladder — carrier climb: register a UDP relay path
                    // alongside the TCP one when we hold both carrier links to a relay. No-op until
                    // the dual-carrier autoconnect lands (STEP 2b wiring). See connection-quality-ladder.md.
                    net.upgrade_relay_carriers();
                    // GC the soft-state path table (R3): expiry was only enforced lazily at lookup,
                    // so flood-learned (PathReply) + announce routes that went stale accumulated.
                    // Drive the periodic sweep here alongside the other maintenance.
                    net.path_table().evict_expired(std::time::Instant::now());
                }
            }));
        }

        // AutoNAT self-reachability (P2 — validate before advertise). Before we tell a contact "dial me
        // at this WAN endpoint", confirm it is ACTUALLY reachable from outside: ask a direct WAN peer
        // (the relay) to dial our UPnP mapping back (`request_dial_back` → the verifier's `DialBackProbe`
        // returns). Until confirmed, `reachability` stays Unknown and the app advertises NO WAN endpoint,
        // so a peer never chases a phantom UPnP port (the cross-NAT instability we hit live — a router
        // that reports a mapping it doesn't actually forward). Provider-independent: the verifier is a
        // peer we already hold. Without this loop the AutoNAT machinery was dead (only a test triggered
        // it). См. docs/decisions/connectivity-upgrade-via-peer-relay.md (Phase B).
        {
            let net = Arc::clone(&network);
            let upnp = Arc::clone(&mapped_external);
            let upnp_tcp = Arc::clone(&mapped_tcp_external);
            background.push(runtime.spawn(async move {
                // Intervals still to skip before re-confirming an already-confirmed door.
                // See TCP_DOOR_RECONFIRM_SKIPPED_INTERVALS.
                let mut door_reconfirm_skips_left: u32 = 0;
                loop {
                    // Only the UPnP-mapped WAN candidate needs external confirmation; LAN/loopback are
                    // dialable by definition, and a global-IPv6 candidate is validated separately.
                    let candidate = *upnp.lock();
                    if let (Some(candidate), Some(verifier)) = (candidate, net.verified_wan_peer()) {
                        let _ = net.request_dial_back(&verifier, candidate).await;
                    }
                    // Task 3 — confirm the direct-TCP front door the SAME way (the verifier TCP-
                    // connects back) BEFORE we advertise it, so we never publish a phantom TCP
                    // mapping (the live finding: a router that accepts a TCP map but doesn't forward).
                    // Reuses the same WAN verifier. `direct_tcp_endpoint` exposes only the confirmed one.
                    let tcp_candidate = *upnp_tcp.lock();
                    if let (Some(tcp_candidate), Some(verifier)) =
                        (tcp_candidate, net.verified_wan_peer())
                    {
                        // A door already confirmed on THIS candidate has nothing new to tell us, so
                        // most cycles skip the probe. A candidate that changed (UPnP remapped, NAT
                        // moved us) does not match the confirmed one and is therefore probed at once,
                        // and so is a door that has never been confirmed. P3.
                        let already_confirmed = net.confirmed_tcp_address() == Some(tcp_candidate);
                        let (probe_now, skips_left) =
                            next_door_reconfirm_step(already_confirmed, door_reconfirm_skips_left);
                        door_reconfirm_skips_left = skips_left;
                        if probe_now {
                            let _ = net.request_tcp_reachability(&verifier, tcp_candidate).await;
                        }
                    }
                    // Event-driven: re-confirm the INSTANT a neighbour is verified (a fresh WAN UDP
                    // verifier — the relay's UDP carrier — may have just come up), not a full interval
                    // later. The interval is the backstop/retry (UPnP that maps later, NAT remap,
                    // responder-role verifier). Removes the up-to-one-interval first-confirm latency the
                    // live Phase B run showed (~36 s). См. network.rs `autonat_triggered`.
                    tokio::select! {
                        _ = net.autonat_triggered() => {}
                        _ = tokio::time::sleep(jittered(announce_interval)) => {}
                    }
                }
            }));
        }

        // Event-driven relay→direct upgrade: punch a peer the INSTANT it becomes relay-only (a fresh
        // relay link came up, or a direct link dropped back to relay), instead of waiting up to a full
        // reconcile interval. The periodic sweep above stays the backstop/retry; this removes the
        // first-punch latency so direct comes up "почти сразу". См. docs/decisions/mesh-hole-punching.md.
        {
            let net = Arc::clone(&network);
            background.push(runtime.spawn(async move {
                while let Some(peer) = net.next_relay_upgrade().await {
                    net.punch_upgrade_peer(peer).await;
                }
            }));
        }

        // Accept loop: register inbound (peer-dialed) Links + pump both planes.
        {
            let net = Arc::clone(&network);
            let links = Arc::clone(&links);
            let callbacks = callbacks.clone();
            background.push(runtime.spawn(async move {
                while let Some(link) = net.accept().await {
                    register_link(link, &links, &callbacks);
                }
            }));
        }

        // Durable inbound loop: drain store-and-forward frames addressed to us
        // (delivered via reconcile) and feed each through the SAME on_wire path as
        // direct mesh wire — so a held 1:1 message arriving while we are online is
        // handled identically. P4 offline delivery. (A frame from a peer we have no
        // mesh session/ctx for is dropped by on_wire today — true never-linked
        // offline delivery needs the ctx-from-durable step, see
        // docs/notes/durable-1to1-integration-plan.md variant (b).)
        let on_wire = callbacks.on_wire.clone();
        let on_peer_down = callbacks.on_peer_down.clone();
        // Bulk plane pump (attachment transfer): hand each inbound stream to the app on its OWN OS
        // thread. Per-stream (not a serial executor like on_wire) because a transfer is read to
        // completion — serializing them would make a second file wait out the first — and off the
        // tokio workers because that read is synchronous and can run for gigabytes.
        if let Some(on_bulk_stream) = callbacks.on_bulk_stream.clone() {
            let net = Arc::clone(&network);
            background.push(runtime.spawn(async move {
                while let Ok((peer, stream)) = net.recv_bulk_stream().await {
                    let on_bulk_stream = Arc::clone(&on_bulk_stream);
                    if let Err(error) = std::thread::Builder::new()
                        .name("omni-bulk-in".to_string())
                        .spawn(move || on_bulk_stream(peer, stream))
                    {
                        omni_network::diag_trace(
                            "bulk-pump/spawn-fail",
                            &format!("peer={peer:?} err={error}"),
                        );
                    }
                }
            }));
        }

        let on_dht_proxy = callbacks.on_dht_proxy_request.clone();
        let on_dht_bootstrap = callbacks.on_dht_bootstrap_request.clone();
        if on_wire.is_some()
            || on_peer_down.is_some()
            || on_dht_proxy.is_some()
            || on_dht_bootstrap.is_some()
        {
            let net = Arc::clone(&network);
            let links_for_down = Arc::clone(&links);
            background.push(runtime.spawn(async move {
                loop {
                    if let Some(on_wire) = on_wire.as_ref() {
                        while let Some(frame) = net.try_recv_durable() {
                            // Same bracket as the wire pump's — tells the two on_wire lanes apart
                            // (a hang here stalls durable delivery; a hang there stalls live).
                            omni_network::diag_trace(
                                "durable-pump/frame",
                                &format!(
                                    "from={:?} tid={:?}",
                                    frame.from,
                                    std::thread::current().id()
                                ),
                            );
                            let (feed, seq) = (frame.feed_id, frame.seq);
                            on_wire(frame.from, frame.ciphertext);
                            // Handled: from now on it is persisted, so our high-water survives a
                            // restart and holders stop re-offering it.
                            net.note_durable_handled(feed, seq);
                            omni_network::diag_trace(
                                "durable-pump/frame-done",
                                &format!("from={:?}", frame.from),
                            );
                        }
                    }
                    // Proactive teardown: a peer whose last path dropped (close event
                    // or ping-liveness declaring it dead). Drop our cached Link to it
                    // so `has_link` and the send path stop choosing the now-dead mesh
                    // route, then let the app clear its stale session state — so the
                    // next message falls through to durable without a process restart.
                    while let Some(peer) = net.try_recv_peer_down() {
                        links_for_down.lock().remove(&peer);
                        if let Some(on_peer_down) = on_peer_down.as_ref() {
                            on_peer_down(peer);
                        }
                    }
                    // DHT-over-peer (mode b): a peer asked us to run a BEP44 op on our DHT. The serve
                    // BLOCKS (omni-dht-helper IPC), so run it off this pump on a blocking worker, then
                    // route the response back toward the requester. See docs/decisions/dht-over-peer.md.
                    if let Some(on_dht_proxy) = on_dht_proxy.as_ref() {
                        while let Some(req) = net.try_recv_dht_proxy_request() {
                            let cb = Arc::clone(on_dht_proxy);
                            let net_resp = Arc::clone(&net);
                            let from = req.from;
                            let req_id = req.req_id;
                            let info = MeshDhtProxyRequest {
                                from,
                                req_id,
                                op: req.op,
                                key: req.key,
                                salt: req.salt,
                                value: req.value,
                            };
                            tokio::spawn(async move {
                                if let Ok(result) =
                                    tokio::task::spawn_blocking(move || cb(info)).await
                                {
                                    net_resp
                                        .send_dht_proxy_response(from, req_id, result.ok, result.value)
                                        .await;
                                }
                            });
                        }
                    }
                    // DHT bootstrap seed exchange (step B): a direct neighbour asked for the in-segment
                    // DHT bootstrap addresses we know. The app returns its configured list (a cheap
                    // sidecar read, gated on DHT-rendezvous-enabled); we ship it back on the link. See
                    // docs/decisions/dht-bootstrap-mesh-propagation.md.
                    if let Some(on_dht_bootstrap) = on_dht_bootstrap.as_ref() {
                        while let Some((from, req_id)) = net.try_recv_dht_boot_request() {
                            let nodes = on_dht_bootstrap();
                            net.send_dht_boot_response(from, req_id, nodes).await;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }));
        }

        Ok(Self {
            runtime: std::mem::ManuallyDrop::new(runtime),
            network,
            local_address,
            quic_addr,
            links,
            callbacks,
            background: Mutex::new(background),
            mapped_external,
            mapped_tcp_external,
            configured_direct_tcp_port: config.direct_tcp_bind_port,
            direct_tcp_port: direct_tcp_internal,
            dual_carrier_relay_tcp,
            relay_tcp_endpoint: if is_relay_server { None } else { config.relay_tcp_endpoint },
            dht_proxy_advertised: std::sync::atomic::AtomicBool::new(dht_proxy_advertised),
            friend_relay_advertised: std::sync::atomic::AtomicBool::new(config.friend_relay),
            carrier_switched_at: Mutex::new(None),
        })
    }

    /// Friend-proxy (Axis 2) — turn the advertised `ANNOUNCE_CAP_FRIEND_RELAY` role on/off on a
    /// RUNNING mesh (peers pick it up on our next periodic announce). The app derives `advertised`
    /// from its persisted per-contact consent set (any consent → advertise), so a consent toggle
    /// takes effect without a mesh restart. Recomputes the whole bitfield to preserve the
    /// DHT-proxy bit. See docs/decisions/friend-proxy-by-consent.md.
    pub fn set_friend_relay_advertised(&self, advertised: bool) {
        self.friend_relay_advertised.store(advertised, std::sync::atomic::Ordering::Relaxed);
        self.apply_advertised_capabilities();
    }

    /// Turn the advertised DHT-proxy role on/off on a RUNNING mesh (peers pick it up on our next
    /// periodic announce). The app keeps it on only while our own Mainline sees the swarm — a proxy
    /// whose engine is dead only costs a friend a timeout. See docs/decisions/dht-over-peer.md.
    pub fn set_dht_proxy_advertised(&self, advertised: bool) {
        if self.dht_proxy_advertised.swap(advertised, std::sync::atomic::Ordering::Relaxed) != advertised {
            self.apply_advertised_capabilities();
        }
    }

    fn apply_advertised_capabilities(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let capabilities = (if self.dht_proxy_advertised.load(Relaxed) {
            omni_network::ANNOUNCE_CAP_DHT_PROXY
        } else {
            0
        }) | (if self.friend_relay_advertised.load(Relaxed) {
            omni_network::ANNOUNCE_CAP_FRIEND_RELAY
        } else {
            0
        });
        self.network.set_capabilities(capabilities);
    }

    /// The public external endpoint of our router port mapping (UPnP-IGD or NAT-PMP), if
    /// established (None until mapped, or never on loopback/CGNAT). A peer behind a
    /// different NAT dials this.
    pub fn mapped_external_endpoint(&self) -> Option<SocketAddr> {
        *self.mapped_external.lock()
    }

    /// Task 3 — our direct-TCP front door endpoint to advertise in the beacon `tcp_direct`, returned
    /// ONLY once a verifier has TCP-connected back and confirmed it (validate-before-advertise, the
    /// TCP analog of `confirmed_reachable_endpoint`) — so we never publish a phantom TCP mapping (the
    /// live finding: a router that accepts a TCP UPnP map but doesn't forward it). `None` until the
    /// UPnP map is up AND confirmed. The raw (unconfirmed) map lives in `mapped_tcp_external`, which
    /// the AutoNAT loop feeds to `request_tcp_reachability`. See docs/decisions/tcp-direct-peer-fallback.md.
    pub fn direct_tcp_endpoint(&self) -> Option<SocketAddr> {
        self.network.confirmed_tcp_address()
    }

    /// The RAW (unconfirmed) UPnP/NAT-PMP-mapped TCP front-door endpoint — the TCP analogue of
    /// [`mapped_external_endpoint`](Self::mapped_external_endpoint). Used for the OPTIMISTIC
    /// first-contact TCP advertise (`tcp_beacon_candidate`), gated on the egress-IP match so a
    /// phantom router mapping is still withheld — the relay-less path for a UDP-hostile peer that
    /// has no verifier yet to confirm the mapping. See relay-less-first-contact-validate-before-advertise.md.
    pub fn mapped_tcp_external_endpoint(&self) -> Option<SocketAddr> {
        *self.mapped_tcp_external.lock()
    }

    /// The user-configured FIXED direct-TCP front-door port, or `None` for the ephemeral/UPnP default.
    /// When set the user has manually port-forwarded this TCP port, so the beacon advertises
    /// `reflexive_ip:port` directly (no UPnP dependency). See docs/decisions/tcp-direct-peer-fallback.md.
    pub fn configured_direct_tcp_port(&self) -> Option<u16> {
        self.configured_direct_tcp_port
    }

    /// Master relay→direct **upgrade** gate (the `ConnectionPrivacyMode::RelayOnly` privacy choice).
    /// `false` ⇒ the node keeps its working relay paths and never climbs to a direct P2P link, so a
    /// contact can't learn its IP. The app forwards the persisted privacy mode here on mesh-start and
    /// on change. Gates the UDP hole-punch (inside the network) AND the rendezvous direct-dial (the
    /// worker reads `allows_direct_punch` below). See docs/decisions/connection-quality-ladder.md.
    pub fn set_allow_direct_punch(&self, allow: bool) {
        self.network.set_allow_direct_punch(allow);
    }

    /// Hold direct links only with `allowed` (the relays), or with anyone (`None`): the rest are
    /// closed now and refused when they come — those peers go on through a relay. Pairs with
    /// `set_allow_direct_punch(false)`. See omni-network `Network::keep_direct_only_with`.
    pub fn keep_direct_links_only_with(&self, allowed: Option<std::collections::HashSet<MeshNodeAddress>>) {
        let network = Arc::clone(&self.network);
        self.runtime.spawn(async move { network.keep_direct_only_with(allowed).await });
    }

    /// Whether the relay→direct upgrade is currently permitted. The rendezvous dial worker reads this
    /// live so a `RelayOnly` profile skips the direct-dial leg (UDP-direct + TCP-direct), keeping the
    /// "one setting gates both carriers" invariant of follow-up d. See tcp-direct-peer-fallback.md.
    pub fn carrier_preference(&self) -> CarrierPreference {
        self.network.carrier_preference()
    }

    /// Change the carrier of our hops on this running mesh, at once (`Network::switch_carrier_preference`:
    /// a connection the new carrier refuses closes and is redialed on it; `doors` — the doors we
    /// know of our peers, for that redial in TCP mode). `false` — not on this mesh, it needs a restart: a TCP or Auto hop to the
    /// relay rides the relay's TCP carrier, bound only at mesh start, and this mesh started without it
    /// while the relay answers on TCP now. A relay that does not answer (`RELAY_TCP_PROBE_TIMEOUT`)
    /// would fail the bind again on the restart — that only tore every link down for a mesh just like
    /// this one (stand, 2026-10-17: the PC's relay is gone) — so the switch is made here, without it.
    pub fn switch_carrier_policy(
        &self,
        policy: omni_types::CarrierPolicy,
        doors: std::collections::HashMap<MeshNodeAddress, SocketAddr>,
    ) -> bool {
        if policy != omni_types::CarrierPolicy::Quic
            && !self.network.has_interface(MeshInterfaceKind::QuicOverTcp)
        {
            if let Some(relay) = self.relay_tcp_endpoint {
                if std::net::TcpStream::connect_timeout(&relay, RELAY_TCP_PROBE_TIMEOUT).is_ok() {
                    return false;
                }
                omni_network::diag_trace(
                    "mesh/carrier-switch",
                    &format!("relay TCP {relay} does not answer — switching without its carrier, no restart"),
                );
            }
        }
        let preference = carrier_preference_of(policy);
        let net = Arc::clone(&self.network);
        let closed = self.run_blocking(async move { net.switch_carrier_preference(preference, &doors).await });
        omni_network::diag_trace("mesh/carrier-switch", &format!("preference={preference:?} closed={closed}"));
        *self.carrier_switched_at.lock() = Some(std::time::Instant::now());
        true
    }

    /// Whether the carrier was switched less than `CARRIER_SWITCH_SETTLE` ago: the links it closed
    /// are still being redialed over the new carrier, so a rendezvous for them waits. Stand
    /// 2026-10-17: every switch set off a sweep of each contact, twice in one tick, 4 couriers a fetch.
    pub fn carrier_switch_settling(&self) -> bool {
        self.carrier_switched_at
            .lock()
            .is_some_and(|at| at.elapsed() < CARRIER_SWITCH_SETTLE)
    }

    pub fn allows_direct_punch(&self) -> bool {
        self.network.allows_direct_punch()
    }

    /// Our advertised native-global-IPv6 endpoint (else None). Directly dialable with no NAT — carried
    /// in the rendezvous beacon so a dual-stack peer on another network reaches us without a relay.
    /// Re-detected LIVE each call (cheap UDP-connect route trick) so it FOLLOWS a network change rather
    /// than returning a stale global v6 from a previous network. See `detect_global_ipv6`.
    pub fn advertised_ipv6_endpoint(&self) -> Option<SocketAddr> {
        detect_global_ipv6().map(|v6| SocketAddr::new(IpAddr::V6(v6), self.quic_addr.port()))
    }

    /// Our NodeAddress (blake3 of the account Ed25519 public key).
    pub fn node_address(&self) -> MeshNodeAddress {
        self.local_address
    }

    /// The QUIC address we advertise (what peers dial).
    pub fn quic_addr(&self) -> SocketAddr {
        self.quic_addr
    }

    /// The local port the direct-TCP door listens on this run (`None` = no door) — what the app
    /// remembers for the next run's `MeshConfig::preferred_direct_tcp_port`.
    pub fn direct_tcp_port(&self) -> Option<u16> {
        self.direct_tcp_port
    }

    /// DHT-over-peer (R7 Phase 2, mode b): read a BEP44 slot via `proxy`'s DHT over the mesh. Sync
    /// wrapper over [`Network::dht_get_over_peer`] (blocks on the runtime, like `dial_at`), so the
    /// blocking rendezvous worker can use it. `Some(value)` on a good response (an empty value = empty
    /// slot); `None` on timeout / no route / proxy error. See docs/decisions/dht-over-peer.md.
    pub fn dht_get_over_peer(
        &self,
        proxy: MeshNodeAddress,
        key: [u8; 32],
        salt: Vec<u8>,
        deadline: Duration,
    ) -> Option<Vec<u8>> {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move { net.dht_get_over_peer(proxy, key, salt, deadline).await })
    }

    /// DHT-over-peer (mode b): write a BEP44 slot via `proxy`'s DHT over the mesh. Sync wrapper over
    /// [`Network::dht_put_over_peer`]; `true` if the proxy reported success. See dht-over-peer.md.
    /// BEP 5 `get_peers` via `proxy`'s DHT over the mesh (the relay directory's Mainline shelves when
    /// our own Mainline is dead). Sync wrapper over [`Network::dht_peers_over_peer`].
    pub fn dht_peers_over_peer(
        &self,
        proxy: MeshNodeAddress,
        info_hash: [u8; 20],
        deadline: Duration,
    ) -> Option<Vec<std::net::SocketAddrV4>> {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move { net.dht_peers_over_peer(proxy, info_hash, deadline).await })
    }

    pub fn dht_put_over_peer(
        &self,
        proxy: MeshNodeAddress,
        key: [u8; 32],
        salt: Vec<u8>,
        value: Vec<u8>,
        deadline: Duration,
    ) -> bool {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move { net.dht_put_over_peer(proxy, key, salt, value, deadline).await })
    }

    /// Peers to delegate DHT ops to (mode b): every linked peer that ADVERTISED the DHT-proxy role
    /// (`ANNOUNCE_CAP_DHT_PROXY`) in its verified announce (R7 2e-4), nearest first by the link's
    /// measured RTT (unmeasured last). Role-based, so we don't waste a delegate round on a peer that
    /// doesn't serve — and a peer advertises it only while its own Mainline sees the swarm. Empty if
    /// none. The caller tries them in order. See docs/decisions/dht-over-peer.md.
    pub fn dht_proxy_candidates(&self) -> Vec<MeshNodeAddress> {
        let peers: Vec<MeshNodeAddress> = self
            .links
            .lock()
            .keys()
            .filter(|peer| self.network.peer_advertises_dht_proxy(peer))
            .cloned()
            .collect();
        let mut ranked: Vec<(Option<std::time::Duration>, MeshNodeAddress)> = peers
            .into_iter()
            .map(|peer| (self.network.peer_link_facts(&peer).and_then(|facts| facts.rtt), peer))
            .collect();
        ranked.sort_by_key(|(rtt, _)| (rtt.is_none(), *rtt));
        ranked.into_iter().map(|(_, peer)| peer).collect()
    }

    /// Ask a linked `peer` for the in-segment DHT bootstrap addresses it knows (step B, requester side).
    /// Sync wrapper over [`Network::request_dht_bootstrap`] (blocks on the runtime, like the over-peer
    /// legs), so the blocking rendezvous worker can use it. Empty = no route / no answer / peer not
    /// sharing. The result is candidates to MERGE into our own bootstrap list — a lying peer can only
    /// return junk that never dials (fail-closed). See docs/decisions/dht-bootstrap-mesh-propagation.md.
    /// Hand out `card` as a public relay (`Some`), or stop (`None`). While set, this node answers
    /// «whoever you are» at its address and gives the card to whoever asks over that link — the way a
    /// searcher holding only a Mainline `ip:port` learns who the relay is. Relay directory stage 2.
    pub fn set_relay_card(&self, card: Option<Vec<u8>>) {
        self.network.set_relay_card(card);
    }

    /// Who sits at `endpoint`, and its relay card: the identity proven by the key that answered, the
    /// card exactly as handed over — the caller checks it is signed by that identity. `None` — nobody
    /// answered, not a relay, or no card in time. Leaves no link the app sees (no app hello goes to a
    /// stranger); the bare network connection idles out.
    pub fn probe_relay_at(&self, endpoint: SocketAddr, deadline: Duration) -> Option<(MeshNodeAddress, Vec<u8>)> {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move {
            let (peer, _link) = tokio::time::timeout(deadline, net.dial_unknown(endpoint)).await.ok()?.ok()?;
            let card = net.request_relay_card(peer, deadline).await?;
            Some((peer, card))
        })
    }

    pub fn request_dht_bootstrap(
        &self,
        peer: MeshNodeAddress,
        deadline: Duration,
    ) -> Vec<std::net::SocketAddr> {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move { net.request_dht_bootstrap(peer, deadline).await })
            .unwrap_or_default()
    }

    /// Linked peers to ask for DHT bootstrap seeds (step B), DHT-proxy-advertising peers first (they are
    /// definitionally in the DHT, so most likely to hold good seeds), then any other linked peer (e.g. a
    /// LAN neighbour that simply shares its configured list — the auto-LAN-seed path). Capped at
    /// `DHT_BOOT_SEED_PEER_FANOUT` — we only need one working seed. See dht-bootstrap-mesh-propagation.md.
    pub fn dht_bootstrap_seed_peers(&self) -> Vec<MeshNodeAddress> {
        let mut peers: Vec<MeshNodeAddress> = self.links.lock().keys().cloned().collect();
        // `false` (an advertiser) sorts before `true` — so proxy-advertising peers come first.
        peers.sort_by_key(|peer| !self.network.peer_advertises_dht_proxy(peer));
        peers.truncate(DHT_BOOT_SEED_PEER_FANOUT);
        peers
    }

    /// Friend-proxy (Axis 2) — opt in to carry `peer`'s traffic through us. Per-friend, off by
    /// default, revocable. Advertising the role (`MeshConfig::friend_relay`) says we're *willing*;
    /// this records *for whom*. See docs/decisions/friend-proxy-by-consent.md.
    pub fn consent_to_relay_for(&self, peer: MeshNodeAddress) {
        self.network.consent_to_relay_for(peer);
    }

    /// Withdraw a prior [`consent_to_relay_for`](Self::consent_to_relay_for) — effective immediately.
    pub fn revoke_relay_consent(&self, peer: &MeshNodeAddress) {
        self.network.revoke_relay_consent(peer);
    }

    /// Whether we've consented to friend-proxy for `peer`.
    pub fn relay_consent_for(&self, peer: &MeshNodeAddress) -> bool {
        self.network.relay_consent_for(peer)
    }

    /// Pick a friend to route our traffic through: the first LINKED peer that advertised the
    /// friend-relay role (`ANNOUNCE_CAP_FRIEND_RELAY`). `None` if none. Like `dht_proxy_candidates`, first only.
    pub fn friend_relay_candidate(&self) -> Option<MeshNodeAddress> {
        self.links
            .lock()
            .keys()
            .find(|peer| self.network.peer_advertises_friend_relay(peer))
            .cloned()
    }

    /// Friend-exit (Axis 2, Phase D) — opt in to EGRESS `peer`'s traffic through us. Distinct from
    /// [`consent_to_relay_for`](Self::consent_to_relay_for) (egress lends our uplink + IP): its own
    /// consent set, off by default, revocable. The Phase D egress leg gates on this + destination/byte
    /// caps. See docs/decisions/friend-proxy-by-consent.md (Phase D).
    pub fn consent_to_exit_for(&self, peer: MeshNodeAddress) {
        self.network.consent_to_exit_for(peer);
    }

    /// Withdraw a prior [`consent_to_exit_for`](Self::consent_to_exit_for) — effective immediately.
    pub fn revoke_exit_consent(&self, peer: &MeshNodeAddress) {
        self.network.revoke_exit_consent(peer);
    }

    /// Whether we've consented to friend-exit for `peer`.
    pub fn exit_consent_for(&self, peer: &MeshNodeAddress) -> bool {
        self.network.exit_consent_for(peer)
    }

    /// Pick a friend to egress our traffic through: the first LINKED peer that advertised the
    /// friend-exit role (`ANNOUNCE_CAP_FRIEND_EXIT`). `None` if none. Mirrors `friend_relay_candidate`.
    pub fn friend_exit_candidate(&self) -> Option<MeshNodeAddress> {
        self.links
            .lock()
            .keys()
            .find(|peer| self.network.peer_advertises_friend_exit(peer))
            .cloned()
    }

    /// Ask our consenting friend `friend` to carry `payload` (opaque, already E2E-sealed for `to`)
    /// toward `to`. The friend gates on its own consent + per-origin cap. See friend-proxy-by-consent.md.
    pub async fn carry_via_friend(
        &self,
        friend: MeshNodeAddress,
        to: MeshNodeAddress,
        payload: Vec<u8>,
    ) -> std::io::Result<()> {
        self.network.carry_via_friend(friend, to, payload).await
    }

    /// Establish a verified Link to `peer` (must have been discovered via a
    /// multicast announce already). Blocking. Idempotent: reuses an existing
    /// Link if present.
    pub fn dial(&self, peer: MeshNodeAddress) -> Result<(), MeshRuntimeError> {
        if self.links.lock().contains_key(&peer) {
            return Ok(());
        }
        let net = Arc::clone(&self.network);
        let links = Arc::clone(&self.links);
        let callbacks = self.callbacks.clone();
        self.run_blocking(async move {
            let link = net
                .dial(peer)
                .await
                .map_err(|e| MeshRuntimeError::Dial(e.to_string()))?;
            register_link(link, &links, &callbacks);
            Ok(())
        })
    }

    /// Drop the runtime's registered Link to `peer`, so subsequent `dial` / `dial_at` calls
    /// re-run the full `register_link` path (which sends a fresh SessionHello via `make_hello`).
    /// Used by the app layer when a contact is forgotten: the underlying QUIC connection may
    /// stay warm (there's no protocol shutdown handshake here), but our handshake state is
    /// zeroed so the peer must re-introduce itself on the next dial. Idempotent: returns true
    /// when a Link entry was actually removed.
    ///
    /// Deliberately does NOT close the QUIC connection or prune verified_neighbors: the mesh's
    /// `reuse_live_direct_link` will happily reuse the warm connection on the next dial and
    /// then re-emit the app hello, which is the same self-healing path a coalesced re-dial
    /// takes. See `docs/bugs/relay-pairing-approval-gap.md` for the sibling case.
    pub fn drop_link(&self, peer: &MeshNodeAddress) -> bool {
        self.links.lock().remove(peer).is_some()
    }

    /// R1a connection-quality-ladder FAIL-BACK: flag `peer`'s direct-UDP path as degraded so the mesh
    /// sends over the reliable direct-TCP door instead, for a bounded window. Called when a message
    /// stayed undelivered over the (phantom) UDP link — the asymmetric-mobile case where UDP inbound
    /// keeps the path alive-looking while outbound is dead. Unlike `drop_link` this KEEPS the link (and
    /// its TCP carrier), just deprioritises UDP — no drop/re-dial churn. See tcp-direct-peer-fallback.md.
    pub fn demote_udp_direct(&self, peer: &MeshNodeAddress) -> bool {
        self.network.demote_udp_direct(peer)
    }

    /// Meta-B fail-down: route `peer`'s single quinn connection over its direct-TCP door at `door` —
    /// the connection MIGRATES onto the door (same connection id, no separate carrier, no re-dial).
    /// `false` if the peer isn't dialed over UDP yet. See omni-network `mux_socket`.
    pub fn route_peer_over_door(
        &self,
        peer: &MeshNodeAddress,
        door: SocketAddr,
    ) -> std::io::Result<bool> {
        // Runs on the mesh runtime (not the caller's thread): establishing the door tunnel
        // `TcpQuicSocket::connect`s, which `tokio::spawn`s its supervisor — that panics outside a
        // runtime context. Same reason the sync dial facades use `run_blocking` (block_on-in-runtime fix).
        let net = Arc::clone(&self.network);
        let peer = *peer;
        self.run_blocking(async move { net.route_peer_over_door(&peer, door).await })
    }

    /// Our reflexive address from public STUN, asked from the mesh's own QUIC socket — the mapping a
    /// peer's dial reaches (a query from any other socket describes a port nobody listens on). All
    /// servers are asked at once; bounded by one `timeout`. See `omni_network::stun_socket`.
    pub fn stun_reflexive(&self) -> Option<omni_network::stun_socket::Reflexive> {
        let net = Arc::clone(&self.network);
        self.run_blocking(async move {
            net.stun_reflexive(
                omni_network::stun_socket::DEFAULT_STUN_SERVERS,
                std::time::Duration::from_secs(3),
            )
            .await
        })
    }

    /// Meta-B optimistic upgrade: route `peer` back over the UDP base (migrates back, drops the door
    /// tunnel). `false` if no direct-UDP path with a known quic address was found.
    pub fn route_peer_over_udp(&self, peer: &MeshNodeAddress) -> bool {
        self.network.route_peer_over_udp(peer)
    }

    /// Dial `peer` at endpoints learned out-of-band (serverless rendezvous), not from a
    /// multicast announce — the cross-network entry point. Idempotent like `dial`.
    /// Run `fut` to completion on the mesh runtime, SAFE from any thread — including a worker of this
    /// very runtime, where a bare `block_on` panics "Cannot start a runtime from within a runtime"
    /// (live: the rendezvous retry sweep, an async task, called `dial_peer_tcp_direct` → block_on →
    /// panic). Off-runtime → `block_on`. On-runtime → spawn onto the runtime + block the caller on a
    /// std channel; the multi-thread runtime keeps other workers free to drive the spawned task, so
    /// there is no deadlock. This is why the sync dial facades can be invoked from BOTH the FFI thread
    /// and the async sweeps. `fut` must be `'static` (spawn requires it) — dial closures already clone
    /// the Arc handles they need.
    fn run_blocking<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            self.runtime.spawn(async move {
                let _ = tx.send(fut.await);
            });
            rx.recv()
                .expect("mesh runtime dropped the dial task before completion")
        } else {
            self.runtime.block_on(fut)
        }
    }

    /// See docs/decisions/serverless-bootstrap-rendezvous.md §Integration step 4.
    pub fn dial_at(
        &self,
        peer: MeshNodeAddress,
        endpoints: Vec<SocketAddr>,
    ) -> Result<(), MeshRuntimeError> {
        let Some(over_a_route) = self.link_to_upgrade(&peer) else {
            return Ok(());
        };
        let net = Arc::clone(&self.network);
        let links = Arc::clone(&self.links);
        let callbacks = self.callbacks.clone();
        self.run_blocking(async move {
            let link = net
                .dial_at_endpoints(peer, endpoints)
                .await
                .map_err(|e| MeshRuntimeError::Dial(e.to_string()))?;
            register_dialed_link(link, over_a_route, &net, &links, &callbacks)
        })
    }

    /// Whether a dial at out-of-band endpoints has anything to do: `None` when the peer is already
    /// linked DIRECTLY, otherwise whether the link it has is a route through other nodes.
    ///
    /// A routed link is a fallback, not the destination — the rendezvous sweep dials a beacon's
    /// endpoints precisely to get off it. Returning early on ANY link made that a no-op: live
    /// 2026-10-10 the laptop accepted an invite of a profile on the PC, at once had a route to it
    /// through the PC's other node — one that delivered nothing — and the fresh beacon's endpoints
    /// were never dialed; the pairing hello went down the dead route for minutes.
    fn link_to_upgrade(&self, peer: &MeshNodeAddress) -> Option<bool> {
        if !self.links.lock().contains_key(peer) {
            return Some(false);
        }
        matches!(self.network.peer_link_quality(peer), Some(MeshLinkQuality::Relay { .. }) | None)
            .then_some(true)
    }

    /// [`dial_at`](Self::dial_at), given up after `limit`. For callers on a shared thread, where a
    /// stale address must not stall everyone queued behind it for a whole handshake timeout.
    pub fn dial_at_within(
        &self,
        peer: MeshNodeAddress,
        endpoints: Vec<SocketAddr>,
        limit: std::time::Duration,
    ) -> Result<(), MeshRuntimeError> {
        let Some(over_a_route) = self.link_to_upgrade(&peer) else {
            return Ok(());
        };
        let net = Arc::clone(&self.network);
        let links = Arc::clone(&self.links);
        let callbacks = self.callbacks.clone();
        self.run_blocking(async move {
            let link = tokio::time::timeout(limit, net.dial_at_endpoints(peer, endpoints))
                .await
                .map_err(|_| MeshRuntimeError::Dial(format!("no answer within {limit:?}")))?
                .map_err(|e| MeshRuntimeError::Dial(e.to_string()))?;
            register_dialed_link(link, over_a_route, &net, &links, &callbacks)
        })
    }

    /// TCP-direct fallback (task 3) — try to establish a DIRECT link to `peer` over one of its
    /// advertised TCP front doors (`peer_tcp_endpoints`, from the peer's beacon `tcp_direct`), using
    /// the same TLS-over-TCP wrapper as the relay fallback. The dialer-side counterpart of
    /// [`dial_at`](Self::dial_at) (which dials UDP-direct); the rendezvous sweep calls BOTH, and
    /// `path_rank` prefers the UDP-direct carrier when it comes up — TCP-direct only carries the link
    /// on a UDP-hostile carrier. First-success across the advertised endpoints; idempotent
    /// (skipped if a TCP-direct carrier path to the peer already exists). See
    /// docs/decisions/tcp-direct-peer-fallback.md.
    /// Returns whether the peer's connection was actually routed over a door: `true` = migrated,
    /// `false` = there was nothing to migrate (no live connection to this peer yet, or no door
    /// given). **`false` is NOT success** — nothing was dialed and the peer stays unreachable.
    ///
    /// This used to return `Result<()>` and threw the flag away with `.map(|_| ())`, so "migrated
    /// nothing" was indistinguishable from "migrated". On a UDP-hostile carrier that is the NORMAL
    /// case:
    /// UDP never lands, so there is never a connection to migrate, and every caller — the rendezvous
    /// sweep, find-by-Omni-ID, the `dial-tcp` harness command — reported success while zero
    /// connections existed. See docs/bugs/door-first-contact.md.
    pub fn dial_peer_tcp_direct(
        &self,
        peer: MeshNodeAddress,
        peer_tcp_endpoints: Vec<SocketAddr>,
    ) -> Result<bool, MeshRuntimeError> {
        // Meta-B: the direct-TCP door is NOT a separate carrier/connection any more — it is a route for
        // the peer's ONE quinn connection. Migrate that connection onto the peer's (first) door; if the
        // peer isn't dialed over UDP yet (`Ok(false)`), a later rendezvous sweep migrates it once
        // `dial_at` has landed the UDP connection. See omni-network `mux_socket` + tcp-direct-peer-fallback.md.
        let Some(door) = peer_tcp_endpoints.into_iter().next() else {
            return Ok(false);
        };
        // Already linked → NOTHING to do. Do not move a healthy connection onto the door here.
        //
        // The rendezvous sweep calls this every pass, so re-routing on sight meant a link that had
        // just come up over native UDP was dragged onto the TLS-over-TCP tunnel seconds later — a
        // pure downgrade, and the reason transfers crawled (measured: 22-27 s per MiB over the door
        // vs native UDP). A live GUI trace caught it exactly: `quic/incoming` from the peer's real
        // address, challenge ok, then `quic/path-migrated old=<peer public> new=<local bridge>` one
        // second on.
        //
        // The door is the FALLBACK carrier. Migrating onto it is the failover path's job
        // (`drop_phantom_link` → `route_peer_over_door`), which fires on evidence that UDP stopped
        // delivering — not on a timer.
        //
        // Linked means a live DIRECT path. A Link whose direct path a carrier switch closed stays in
        // `links` while a relay path to the peer is still listed, and reading that as «linked» left
        // the pinned relay undialed over its door for the whole TCP phase (stand 2026-10-17: 26 s
        // without a link, the call's bitrate fell to the floor for want of feedback).
        if self.links.lock().contains_key(&peer)
            && matches!(self.network.peer_link_quality(&peer), Some(MeshLinkQuality::Direct))
        {
            return Ok(true);
        }
        // No connection to migrate: ORIGINATE over the door. This is the relay-less first contact —
        // on a UDP-hostile carrier the door is the only address that is reachable at all, and
        // waiting for a UDP connection to appear (so it could be migrated) waits forever.
        let net = Arc::clone(&self.network);
        let links = Arc::clone(&self.links);
        let callbacks = self.callbacks.clone();
        self.run_blocking(async move {
            let link = net
                .dial_over_door(peer, door)
                .await
                .map_err(|e| MeshRuntimeError::Dial(e.to_string()))?;
            register_link(link, &links, &callbacks);
            Ok(true)
        })
    }

    /// R1a connection-quality ladder — establish the carrier link(s) to a registry relay. When this
    /// relay is the bound rung-0 TCP anchor (dual-carrier Auto), hold BOTH carriers
    /// (`dial_relay_dual_carrier`); otherwise today's single-carrier `dial_at` to the UDP endpoint.
    /// The app's relay-dial sites call this in place of `dial_at` so the carrier choice lives in one
    /// place. Idempotent / a no-op when the relay's link(s) are already up.
    pub fn autoconnect_network_node(
        &self,
        relay: MeshNodeAddress,
        udp_endpoint: SocketAddr,
        node_tcp_endpoint: Option<SocketAddr>,
    ) -> Result<(), MeshRuntimeError> {
        // QUIC for our hops: the relay over UDP only, whatever carrier was bound at start.
        let tcp_anchor = self
            .dual_carrier_relay_tcp
            .filter(|_| self.network.carrier_preference() != CarrierPreference::Quic);
        match (tcp_anchor, node_tcp_endpoint) {
            // This relay is the bound rung-0 TCP anchor → hold both A↔R carriers.
            (Some(bound), Some(node_tcp)) if bound == node_tcp => {
                self.dial_relay_dual_carrier(relay, udp_endpoint, node_tcp)
            }
            // Single carrier (UDP-only Auto, Quic, or a non-anchor relay): today's
            // path. `dial_at` is a no-op when already linked.
            _ => {
                if self.has_link(&relay) {
                    return Ok(());
                }
                self.dial_at(relay, vec![udp_endpoint])
            }
        }
    }

    /// R1a — hold BOTH A↔R carrier links to the rung-0 TCP anchor relay (STEP 2b-2 activation):
    ///  1. the **TCP base** (rung 0) synchronously — the relay's canonical Link (drives `has_link` /
    ///     send). Fast (the TLS-over-TCP pipe is already up from bind), so the proven ~1.5 s rung-0
    ///     connect is preserved. Skipped if the relay is already linked.
    ///  2. an **additive UDP A↔R link** (rung 1) in the BACKGROUND — a SECOND Direct path to the same
    ///     relay at a DISTINCT neighbour (its `:443`), so `Network::upgrade_relay_carriers` can climb
    ///     relayed peers onto UDP and fail back to TCP. Background because on a UDP-hostile path
    ///     this dial just times out WITHOUT touching the already-up TCP base. Idempotent (skipped if
    ///     the UDP carrier path already exists). The produced Link is discarded — only the Direct PATH
    ///     it registers matters; the identity-keyed inbox is reused, never orphaned.
    ///
    /// See docs/decisions/connection-quality-ladder.md.
    pub fn dial_relay_dual_carrier(
        &self,
        relay: MeshNodeAddress,
        udp_endpoint: SocketAddr,
        tcp_endpoint: SocketAddr,
    ) -> Result<(), MeshRuntimeError> {
        if !self.has_link(&relay) {
            let net = Arc::clone(&self.network);
            let links = Arc::clone(&self.links);
            let callbacks = self.callbacks.clone();
            let tcp_base = self.run_blocking(async move {
                let link = net
                    .dial_over_carrier(relay, &tcp_endpoint.to_string(), MeshInterfaceKind::QuicOverTcp)
                    .await
                    .map_err(|e| MeshRuntimeError::Dial(e.to_string()))?;
                register_link(link, &links, &callbacks);
                Ok::<_, MeshRuntimeError>(())
            });
            // No TCP base → the relay over UDP, the single-carrier way. Returning the TCP error here
            // left a relay whose TCP would not come up never dialed at all, UDP included (stand
            // 2026-10-14: the PC relay from a contact's bundle became the TCP anchor by being the
            // first row with a TCP address; its TCP failed at once every tick, so the profile never
            // reached the relay and its contact stayed «searching»).
            if tcp_base.is_err() {
                return self.dial_at(relay, vec![udp_endpoint]);
            }
        }
        // Additive UDP A↔R link, off the calling thread (the mesh executor times it out on a UDP-hostile path
        // without blocking the autoconnect worker). One dial in flight per tick; the
        // `has_carrier_path` guard stops re-dialing once it's up. Not when our hops are TCP.
        if self.network.carrier_preference() == omni_network::CarrierPreference::Tcp {
            return Ok(());
        }
        let net = Arc::clone(&self.network);
        self.runtime.spawn(async move {
            if !net.has_carrier_path(&relay, MeshInterfaceKind::QuicOverUdp) {
                let _ = net
                    .dial_over_carrier(relay, &udp_endpoint.to_string(), MeshInterfaceKind::QuicOverUdp)
                    .await;
            }
        });
        Ok(())
    }

    /// Send a media datagram to `peer` over its verified mesh Link (unreliable).
    pub fn send_media(&self, peer: MeshNodeAddress, payload: &[u8]) -> Result<(), MeshRuntimeError> {
        let link = self
            .links
            .lock()
            .get(&peer)
            .cloned()
            .ok_or(MeshRuntimeError::NoLink)?;
        // ONE POLL, INLINE — NOT `run_blocking`.
        //
        // The media engine calls this per packet from its own threads (the pacer; the RTCP built
        // inside the receive path, i.e. from the mesh pump's tokio task). `run_blocking` there would
        // block a tokio worker until another worker ran the send — and the send never actually
        // waits: path choice is a sync lock and quinn's `send_datagram` is synchronous. So the
        // future completes on its first poll; the runtime is entered only so tokio resources work if
        // an interface ever does await, and a still-pending send is handed to the runtime instead
        // of being waited for (a media datagram late is as good as lost). See
        // docs/bugs/media-rode-the-shells-timer.md.
        use std::future::Future as _;
        let _entered = self.runtime.enter();
        let outcome = {
            let mut send = std::pin::pin!(link.send_datagram(payload));
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            send.as_mut().poll(&mut cx)
        };
        match outcome {
            std::task::Poll::Ready(result) => {
                result.map_err(|e| MeshRuntimeError::Send(e.to_string()))
            }
            std::task::Poll::Pending => {
                let payload = payload.to_vec();
                self.runtime.spawn(async move {
                    let _ = link.send_datagram(&payload).await;
                });
                Ok(())
            }
        }
    }

    /// Send a reliable wire frame (chat / call-control envelope) to `peer` over
    /// its verified mesh Link.
    pub fn send_wire(&self, peer: MeshNodeAddress, payload: &[u8]) -> Result<(), MeshRuntimeError> {
        let link = self
            .links
            .lock()
            .get(&peer)
            .cloned()
            .ok_or(MeshRuntimeError::NoLink)?;
        let payload = payload.to_vec();
        self.run_blocking(async move {
            match tokio::time::timeout(RELIABLE_SEND_TIMEOUT, link.send(&payload)).await {
                Ok(result) => result.map_err(|e| MeshRuntimeError::Send(e.to_string())),
                Err(_) => Err(MeshRuntimeError::Send(format!(
                    "send_wire timed out after {RELIABLE_SEND_TIMEOUT:?} (no stream credit / dead carrier)"
                ))),
            }
        })
    }

    /// Open a BULK stream to `peer` over its verified mesh Link — the lane for attachment
    /// transfer.
    ///
    /// A dedicated ordered, flow-controlled stream on the peer's EXISTING connection: no second
    /// dial, no extra handshake, and it inherits whatever carrier that connection currently rides
    /// (including a TCP door it migrated onto). The transport supplies ordering and retransmission,
    /// so the caller streams straight through at constant memory — which is what makes
    /// multi-gigabyte files possible, unlike frame-by-frame sending where the receiver must hold
    /// out-of-order gaps in RAM.
    ///
    /// `NoLink` when the peer has no verified link; `Send` when no current path offers a bulk
    /// plane. Callers treat both as "no mesh lane" and fall back.
    pub fn open_bulk_stream(&self, peer: MeshNodeAddress) -> Result<BulkStream, MeshRuntimeError> {
        let link = self
            .links
            .lock()
            .get(&peer)
            .cloned()
            .ok_or(MeshRuntimeError::NoLink)?;
        self.run_blocking(async move {
            link.open_bulk_stream()
                .await
                .map_err(|e| MeshRuntimeError::Send(e.to_string()))
        })
    }

    /// Try to upgrade a door-carried peer onto direct UDP: "start reliable, then move to fast".
    ///
    /// Only meaningful while the peer is linked over its TCP door; a peer already on UDP, or with no
    /// link at all, answers `false` and nothing happens. The upgrade never risks the live link —
    /// the UDP path is added only after a successful identity challenge over it, and the door stays
    /// underneath as the fallback.
    pub fn upgrade_peer_to_udp(&self, peer: MeshNodeAddress, udp_endpoints: Vec<SocketAddr>) -> bool {
        if udp_endpoints.is_empty() || !self.links.lock().contains_key(&peer) {
            return false;
        }
        let net = Arc::clone(&self.network);
        self.run_blocking(async move {
            for udp in udp_endpoints {
                if net.upgrade_peer_to_udp(peer, udp).await {
                    return Ok::<bool, MeshRuntimeError>(true);
                }
            }
            Ok(false)
        })
        .unwrap_or(false)
    }

    /// Hold a 1:1 message for `to` in the durable store-and-forward layer and
    /// best-effort reconcile it there (P4 offline delivery). `ciphertext` is the
    /// already-E2E-encrypted wire-envelope bytes — opaque to the mesh. Held even
    /// with no path; anti-entropy delivers it once the recipient is reachable.
    /// feed_id is per-(from,to) directional; `seq` is the per-recipient monotone
    /// the CALLER persists (omni-storage `next_durable_seq`) so it survives restart.
    pub fn send_durable(
        &self,
        to: MeshNodeAddress,
        ciphertext: &[u8],
        seq: u64,
    ) -> Result<(), MeshRuntimeError> {
        let from = self.local_address;
        let mut feed_id = [0u8; 16];
        feed_id[..8].copy_from_slice(&from.as_bytes()[..8]);
        feed_id[8..].copy_from_slice(&to.as_bytes()[..8]);
        let msg_id = u64::from_le_bytes(feed_id[8..16].try_into().expect("8 bytes")) ^ seq;
        let frame = DurableDataFrame {
            to,
            from,
            feed_id,
            seq,
            msg_id,
            ttl_secs: DURABLE_TTL_SECS,
            ciphertext: ciphertext.to_vec(),
            signature: Vec::new(),
        };
        let network = Arc::clone(&self.network);
        self.run_blocking(async move {
            match tokio::time::timeout(RELIABLE_SEND_TIMEOUT, network.send_durable(frame)).await {
                Ok(result) => result.map_err(|e| MeshRuntimeError::Send(e.to_string())),
                Err(_) => Err(MeshRuntimeError::Send(format!(
                    "send_durable timed out after {RELIABLE_SEND_TIMEOUT:?} (no stream credit / dead carrier)"
                ))),
            }
        })
    }

    /// True if a verified Link to `peer` exists AND is reachable (call before routing media). The
    /// `links` map membership alone is not enough: a relay-only peer's path can be re-added (autoconnect
    /// re-dial / carrier upgrade) after the peer died, so we also require `peer_link_quality` to report
    /// a path — which, for a Relay best-path, is gated on proven-fresh end-to-end liveness. So a dead
    /// relay-reachable peer reads `has_link == false` (the honest connection-state invariant; the
    /// relay-presence-false-online fix). A Direct link is unaffected (always reachable while present).
    /// See docs/bugs/relay-presence-false-online-dead-peer.md.
    pub fn has_link(&self, peer: &MeshNodeAddress) -> bool {
        self.links.lock().contains_key(peer) && self.network.peer_link_quality(peer).is_some()
    }

    /// Quality of the current best path to `peer` — `Direct` vs `Relay { hops }` — or
    /// None if there is no path. Finer than `has_link` (binary); lets the shell render an
    /// honest per-contact link state ("online — direct" vs "online — via N hops").
    /// See docs/decisions/omni-network-model.md (honest connection-state surface).
    pub fn peer_link_quality(&self, peer: &MeshNodeAddress) -> Option<MeshLinkQuality> {
        self.network.peer_link_quality(peer)
    }

    /// Применить смену контроллера перегрузки на живом меше: пересобрать конфиги интерфейсов и
    /// закрыть соединения, которые едут на прежнем. Возвращает число закрытых соединений.
    ///
    /// Нужно вместо перезапуска меша — тот сносит дискавери, анонсы и проброс порта и стоит больше
    /// минуты без связи (замер 2026-08-31).
    pub fn refresh_congestion_control(&self) -> std::io::Result<usize> {
        self.network.refresh_congestion_control()
    }

    /// The carrier of the best path to `peer` (R1a) — distinguishes a TCP-relay (rung 0) from a
    /// UDP-relay (rung 1), which `peer_link_quality` collapses to the same `Relay { hops }`.
    pub fn peer_best_carrier(&self, peer: &MeshNodeAddress) -> Option<MeshInterfaceKind> {
        self.network.peer_best_carrier(peer)
    }

    /// Факты живого линка: адрес соединения и ИЗМЕРЕННЫЙ транспортом RTT.
    /// См. `omni_network::Network::peer_link_facts`.
    pub fn peer_link_facts(&self, peer: &MeshNodeAddress) -> Option<MeshPeerLinkFacts> {
        self.network.peer_link_facts(peer)
    }

    /// Whether `peer`'s direct connection is riding its TCP door (Meta-B mux migration) rather than
    /// native UDP — the distinction `peer_best_carrier` hides (it reports the `QuicOverUdp` interface
    /// even on the door). Lets the honest readout say `direct:door` instead of a lying `direct:udp`.
    pub fn peer_on_door(&self, peer: &MeshNodeAddress) -> bool {
        self.network.peer_on_door(peer)
    }

    /// This node's AutoNAT reachability verdict (Unknown / Reachable / Unreachable):
    /// whether a fresh peer can dial us directly. Feeds the node-state ("online &
    /// reachable" vs "online, behind NAT") of the honest connection-state surface.
    pub fn reachability_state(&self) -> MeshReachabilityState {
        self.network.reachability_state()
    }

    /// The WAN candidate AutoNAT has confirmed externally reachable (a verifier dialed it back), if
    /// any. The app advertises THIS to a contact instead of an unconfirmed UPnP/reflexive guess —
    /// so the peer dials a port we proved is open, not a phantom. См. connectivity-upgrade-via-peer-relay.md.
    pub fn confirmed_reachable_endpoint(&self) -> Option<SocketAddr> {
        self.network.reachable_address()
    }

    /// Snapshot the last-good DIRECT endpoint per peer for the warm-rejoin cache (G3): `(node,
    /// endpoint)` pairs the app persists (omni-storage `mesh_peer_book`) and, on the next start,
    /// re-dials via `dial_at` before LAN/UPnP/rendezvous discovery completes. Relay-only peers are
    /// skipped. See docs/decisions/omni-network-model.md (D7).
    pub fn snapshot_peer_book(&self) -> Vec<(MeshNodeAddress, String)> {
        self.network.snapshot_peer_book()
    }

    /// Re-export of the per-(group, sender) feed-id derivation for the app.
    pub fn group_feed_id_for(domain: &str, group_id: &str, sender_pub: &str) -> [u8; 16] {
        omni_network::group_feed_id(domain, group_id, sender_pub)
    }

    /// Subscribe to a group's synthetic mesh destination (G3): inbound durable
    /// frames addressed to it are delivered to the app's on_wire path, and each
    /// newly stored frame is gossip-pushed once to `members` with live paths.
    /// Call BEFORE `import_durable_rows` at start so persisted group frames
    /// re-deliver. Re-subscribing replaces the member push set.
    pub fn subscribe_group_destination(
        &self,
        group_id: &str,
        members: Vec<MeshNodeAddress>,
    ) {
        self.network
            .subscribe_group_destination(omni_network::group_destination_address(group_id), members);
    }

    pub fn unsubscribe_group_destination(&self, group_id: &str) {
        self.network
            .unsubscribe_group_destination(&omni_network::group_destination_address(group_id));
    }

    /// Publish one entry of a group feed (chat or ops) into the durable layer:
    /// stored locally, gossip-pushed to linked co-members, healed everywhere else
    /// by anti-entropy. `feed_id`/`seq` come from the app (seq is the persisted
    /// per-feed monotone); `ciphertext` is the opaque feed-item JSON.
    pub fn send_group_durable(
        &self,
        group_id: &str,
        feed_id: [u8; 16],
        seq: u64,
        ciphertext: &[u8],
    ) -> Result<(), MeshRuntimeError> {
        let frame = DurableDataFrame {
            to: omni_network::group_destination_address(group_id),
            from: self.local_address,
            feed_id,
            seq,
            msg_id: omni_network::group_msg_id(&feed_id, seq),
            ttl_secs: DURABLE_TTL_SECS,
            ciphertext: ciphertext.to_vec(),
            signature: Vec::new(),
        };
        let network = Arc::clone(&self.network);
        self.run_blocking(async move {
            match tokio::time::timeout(RELIABLE_SEND_TIMEOUT, network.send_durable(frame)).await {
                Ok(result) => result.map_err(|e| MeshRuntimeError::Send(e.to_string())),
                Err(_) => Err(MeshRuntimeError::Send(format!(
                    "send_group_durable timed out after {RELIABLE_SEND_TIMEOUT:?} (no stream credit / dead carrier)"
                ))),
            }
        })
    }

    /// Snapshot the store-and-forward durable frames we hold for others, as
    /// `(feed_id, seq, wire)` rows for the app to persist (omni-storage) so they
    /// survive a restart. The runtime owns the wire encode/decode so omni-app
    /// deals only with opaque bytes. P4 durable persistence.
    pub fn export_durable_rows(&self) -> Vec<(Vec<u8>, i64, Vec<u8>)> {
        self.network
            .export_durable()
            .into_iter()
            .map(|frame| {
                let feed_id = frame.feed_id.to_vec();
                let seq = frame.seq as i64;
                let wire = omni_network::NetworkFrame::DurableData(frame).encode_to_wire();
                (feed_id, seq, wire)
            })
            .collect()
    }

    /// Restore persisted durable-frame wire blobs into the store at startup.
    /// Best-effort: an undecodable blob is skipped. P4 durable persistence.
    pub fn import_durable_rows(&self, wires: Vec<Vec<u8>>) {
        let frames = wires
            .into_iter()
            .filter_map(
                |wire| match omni_network::NetworkFrame::decode_from_wire(&wire) {
                    Ok(omni_network::NetworkFrame::DurableData(frame)) => Some(frame),
                    _ => None,
                },
            )
            .collect();
        self.network.import_durable(frames);
    }

    /// Stop the dispatcher + background loops. Drop completes teardown.
    pub fn shutdown(&self) {
        // Say goodbye on the wire first, while the runtime still drives the connections. Skipped
        // when called from inside a runtime (block_on would panic there); peers then fall back to
        // their liveness prune.
        if tokio::runtime::Handle::try_current().is_err() {
            self.runtime.block_on(self.network.close_interfaces());
        }
        self.network.shutdown();
        for handle in self.background.lock().drain(..) {
            handle.abort();
        }
    }
}

impl Drop for MeshRuntime {
    fn drop(&mut self) {
        self.shutdown();
        // Take the runtime out and shut it down in the BACKGROUND (does not wait for tasks to finish).
        // Letting the `Runtime` field drop normally would block the caller until every in-flight
        // `spawn_blocking` task returned — e.g. a relay reconnect stuck in a 10 s blocking connect to a
        // dead relay — which is what froze app close. Tasks are already aborted by `shutdown()` above.
        // SAFETY: we are in `drop`; `self.runtime` is never touched again after this take.
        unsafe { std::mem::ManuallyDrop::take(&mut self.runtime) }.shutdown_background();
    }
}

/// Derive a peer's NodeAddress from its Ed25519 identity signing public key
/// (32 bytes) — the same key shared in `PrekeyBundleV1`.
pub fn node_address_from_ed25519_pubkey(
    pubkey_bytes: [u8; 32],
) -> Result<MeshNodeAddress, MeshRuntimeError> {
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|e| MeshRuntimeError::InvalidPubkey(e.to_string()))?;
    Ok(MeshNodeAddress::from_pubkey(&verifying))
}

/// Register a verified Link + spawn its inbound pumps (reliable wire plane +
/// unreliable media plane). Must run inside the tokio runtime context. The two
/// pumps share an `Arc<Link>` but lock independent inboxes, so they run
/// concurrently.
/// Register the Link a dial at endpoints produced. Over an existing ROUTED link only a direct path
/// counts: the dial falls back to the route when the endpoints do not answer, and re-registering that
/// would report a dial that reached nothing. A direct one is registered — its hello goes down the
/// direct path, which is what an invite's pairing waited for.
fn register_dialed_link(
    link: Link,
    over_a_route: bool,
    net: &Network,
    links: &LinkMap,
    callbacks: &MeshCallbacks,
) -> Result<(), MeshRuntimeError> {
    let peer = link.destination();
    if over_a_route && !matches!(net.peer_link_quality(&peer), Some(MeshLinkQuality::Direct)) {
        return Err(MeshRuntimeError::Dial(
            "the endpoints did not answer; the peer stays on its route".to_string(),
        ));
    }
    register_link(link, links, callbacks);
    Ok(())
}

fn register_link(link: Link, links: &LinkMap, callbacks: &MeshCallbacks) {
    let link = Arc::new(link);
    let peer = link.destination();
    links.lock().insert(peer, Arc::clone(&link));

    // App handshake: on link-up, send the app-provided hello as the first
    // reliable frame (the peer reads it as the first `on_wire` frame). Done in
    // the async context here — never blocks the caller.
    if let Some(make_hello) = &callbacks.make_hello {
        let hello = make_hello(peer);
        let hello_link = Arc::clone(&link);
        tokio::spawn(async move {
            let _ = hello_link.send(&hello).await;
        });
    }

    // Reliable wire plane (chat / call-control) — only pumped when a handler
    // is installed; otherwise reliable frames are left unread (no silent drop).
    if let Some(on_wire) = callbacks.on_wire.clone() {
        let wire_link = Arc::clone(&link);
        tokio::spawn(async move {
            while let Ok(bytes) = wire_link.recv().await {
                // Diagnostic bracket (OMNI_TRACE_FILE): a `pump/frame` with no matching
                // `pump/frame-done` = the app callback HUNG on this frame (the live stand's
                // receive-death signature); `pump/exit` = the inbox closed under us.
                omni_network::diag_trace(
                    "pump/frame",
                    &format!(
                        "peer={peer:?} len={} tid={:?}",
                        bytes.len(),
                        std::thread::current().id()
                    ),
                );
                on_wire(peer, bytes);
                omni_network::diag_trace("pump/frame-done", &format!("peer={peer:?}"));
            }
            omni_network::diag_trace("pump/exit", &format!("peer={peer:?}"));
        });
    }

    // Unreliable media plane.
    let on_media = Arc::clone(&callbacks.on_media);
    tokio::spawn(async move {
        while let Ok(bytes) = link.recv_datagram().await {
            on_media(peer, bytes);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use std::sync::mpsc;
    use std::time::Instant;

    fn fresh_key_bytes() -> [u8; 32] {
        SigningKey::generate(&mut OsRng).to_bytes()
    }

    /// Runs the door budget over `cycles` AutoNAT cycles and returns which ones probed.
    fn door_probe_pattern(already_confirmed: impl Fn(usize) -> bool, cycles: usize) -> Vec<bool> {
        let mut skips_left = 0;
        (0..cycles)
            .map(|cycle| {
                let (probe, next) = next_door_reconfirm_step(already_confirmed(cycle), skips_left);
                skips_left = next;
                probe
            })
            .collect()
    }

    #[test]
    fn an_unconfirmed_door_is_probed_every_single_cycle() {
        // Nothing can be advertised until the door answers, so this is the one case where the full
        // cadence is the point. A backoff here would only delay the first confirmation.
        assert_eq!(door_probe_pattern(|_| false, 5), vec![true; 5]);
    }

    #[test]
    fn a_confirmed_door_is_rechecked_only_once_every_few_cycles() {
        let pattern = door_probe_pattern(|_| true, 2 + 2 * TCP_DOOR_RECONFIRM_SKIPPED_INTERVALS as usize);

        assert!(pattern[0], "the first cycle has no budget yet and must probe");
        let skipped = TCP_DOOR_RECONFIRM_SKIPPED_INTERVALS as usize;
        assert!(
            pattern[1..=skipped].iter().all(|probe| !probe),
            "a confirmed, unchanged door must not be re-probed for {skipped} cycles: {pattern:?}"
        );
        assert!(pattern[skipped + 1], "and then re-checked: {pattern:?}");
        // Never off for good: a UPnP lease can lapse and a NAT can remap.
        assert!(
            pattern.iter().filter(|probe| **probe).count() >= 2,
            "{pattern:?}"
        );
    }

    #[test]
    fn a_door_candidate_that_changed_is_probed_immediately_mid_budget() {
        // `already_confirmed` compares the CONFIRMED address with the CURRENT candidate, so a UPnP
        // remap reads as false here — and must cut the skip budget short rather than wait it out.
        let pattern = door_probe_pattern(|cycle| cycle != 3, 6);

        assert!(pattern[0], "{pattern:?}");
        assert!(!pattern[1], "still inside the budget: {pattern:?}");
        assert!(pattern[3], "the candidate moved: {pattern:?}");
        assert!(
            pattern[4],
            "an interrupted budget starts over rather than resuming: {pattern:?}"
        );
    }

    /// The maintenance loops used to sleep EXACTLY 20 s, which is a fingerprint by itself. Two things
    /// have to hold for the jitter to actually remove the metronome: it must stay inside the announce
    /// TTL, and it must be re-drawn per cycle — a constant per-node offset would just shift the beat.
    #[test]
    fn jittered_stays_in_bounds_and_varies_between_draws() {
        let base = DEFAULT_ANNOUNCE_INTERVAL;
        let max = base + base * IDLE_LOOP_JITTER_PERCENT / 100;
        let draws: Vec<Duration> = (0..64).map(|_| jittered(base)).collect();

        for draw in &draws {
            assert!(
                *draw >= base && *draw <= max,
                "draw {draw:?} outside [{base:?}, {max:?}] — a beat may outlive the announce TTL"
            );
        }
        assert!(
            draws.iter().collect::<std::collections::HashSet<_>>().len() > 1,
            "every draw identical — that is a shifted metronome, not jitter"
        );
        // A zero-length base has no room to jitter; it must not panic (modulo by zero).
        assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test]
    async fn select_carriers_binds_the_policy_carriers() {
        use omni_network::InterfaceKind;
        let key = SigningKey::from_bytes(&fresh_key_bytes());
        let udp_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Quic → native UDP only.
        let forced =
            select_carriers(
                omni_types::CarrierPolicy::Quic,
                udp_bind,
                None,
                false,
                &key,
            )
            .unwrap();
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].kind(), InterfaceKind::QuicOverUdp);

        // Auto, no relay TCP endpoint → env-compatible single bind() = native UDP.
        let auto =
            select_carriers(
                omni_types::CarrierPolicy::Auto,
                udp_bind,
                None,
                false,
                &key,
            )
            .unwrap();
        assert_eq!(auto.len(), 1);
        assert_eq!(auto[0].kind(), InterfaceKind::QuicOverUdp);

        // A passive std TCP listener stands in for the relay bridge (the connect only needs the TCP
        // accept for kind()).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay = listener.local_addr().unwrap();

        // Tcp + a relay TCP endpoint → the relay's TCP carrier and the native socket (for doors);
        // the network keeps our hops on TCP.
        let tcp =
            select_carriers(
                omni_types::CarrierPolicy::Tcp,
                udp_bind,
                Some(relay),
                false,
                &key,
            )
            .unwrap();
        assert_eq!(tcp.len(), 2);
        assert_eq!(tcp[0].kind(), InterfaceKind::QuicOverTcp);
        assert_eq!(tcp[1].kind(), InterfaceKind::QuicOverUdp);

        // Tcp with NO relay endpoint → the native socket alone (doors only).
        let fallback =
            select_carriers(
                omni_types::CarrierPolicy::Tcp,
                udp_bind,
                None,
                false,
                &key,
            )
            .unwrap();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].kind(), InterfaceKind::QuicOverUdp);

        // Auto + a relay TCP endpoint → BOTH carriers (dual-carrier coexistence, STEP 2b-2). Each is
        // dialed at its OWN endpoint by the autoconnect, so the two A↔R links stay distinct.
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let relay2 = listener2.local_addr().unwrap();
        let dual =
            select_carriers(
                omni_types::CarrierPolicy::Auto,
                udp_bind,
                Some(relay2),
                false,
                &key,
            )
            .unwrap();
        assert_eq!(dual.len(), 2, "Auto + relay TCP endpoint → both carriers");
        let kinds: Vec<_> = dual.iter().map(|c| c.kind()).collect();
        assert!(kinds.contains(&InterfaceKind::QuicOverUdp), "native UDP carrier present");
        assert!(kinds.contains(&InterfaceKind::QuicOverTcp), "TLS-over-TCP rung-0 carrier present");

        // Regression (headless-relay `Auto` ECONNREFUSED): a RELAY SERVER must bind ONLY the native UDP
        // server carrier — never a TCP-CLIENT carrier to a relay endpoint, even on Auto with a known
        // (self) endpoint, because its own bridge listener is not up yet at carrier-setup time. So the
        // self-directed TCP connect that used to fail the whole mesh-start is simply not attempted.
        let relay_server =
            select_carriers(
                omni_types::CarrierPolicy::Auto,
                udp_bind,
                Some(relay2),
                true,
                &key,
            )
            .unwrap();
        assert_eq!(relay_server.len(), 1, "relay server binds a single carrier");
        assert_eq!(
            relay_server[0].kind(),
            InterfaceKind::QuicOverUdp,
            "relay server binds the native UDP server carrier, not a TCP-client carrier to itself"
        );
    }

    #[test]
    fn is_public_ipv4_rejects_private_cgnat_and_loopback() {
        // Worth advertising — genuinely internet-routable external addresses.
        assert!(is_public_ipv4("8.8.8.8".parse().unwrap()));
        assert!(is_public_ipv4("1.1.1.1".parse().unwrap()));
        // Not worth advertising — inbound still blocked upstream / not routable.
        assert!(!is_public_ipv4("192.168.1.10".parse().unwrap()), "private");
        assert!(!is_public_ipv4("10.0.0.5".parse().unwrap()), "private");
        assert!(!is_public_ipv4("100.64.0.1".parse().unwrap()), "CGNAT 100.64/10");
        assert!(!is_public_ipv4("127.0.0.1".parse().unwrap()), "loopback");
        assert!(!is_public_ipv4("169.254.1.1".parse().unwrap()), "link-local");
        assert!(!is_public_ipv4("0.0.0.0".parse().unwrap()), "unspecified");
        assert!(!is_public_ipv4("203.0.113.7".parse().unwrap()), "documentation");
    }

    #[test]
    fn build_advertised_addresses_decouples_advertise_from_bind() {
        let lan: Ipv4Addr = "192.168.0.12".parse().unwrap();
        let v6: Ipv6Addr = "2606:4700::1111".parse().unwrap();
        let bind: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let upnp: SocketAddr = "203.0.113.7:6000".parse().unwrap();

        // IPv4-only (proven path): advertise the LAN v4 at the bound port — NOT the bind
        // addr. With no UPnP that's exactly [lan:port], matching the old `vec![quic_addr]`.
        assert_eq!(
            build_advertised_addresses(Some(lan), None, 4444, bind, None),
            vec![SocketAddr::new(IpAddr::V4(lan), 4444)],
        );
        assert_eq!(
            build_advertised_addresses(Some(lan), None, 4444, bind, Some(upnp)),
            vec![SocketAddr::new(IpAddr::V4(lan), 4444), upnp],
        );
        // Dual-stack: advertise BOTH the LAN v4 and the global v6 at the bound port (+ UPnP).
        assert_eq!(
            build_advertised_addresses(Some(lan), Some(v6), 4444, bind, Some(upnp)),
            vec![
                SocketAddr::new(IpAddr::V4(lan), 4444),
                SocketAddr::new(IpAddr::V6(v6), 4444),
                upnp,
            ],
        );
        // Same-host / loopback (nothing to advertise): fall back to the bound addr, unchanged.
        assert_eq!(
            build_advertised_addresses(None, None, 4444, bind, None),
            vec![bind],
        );
    }

    #[test]
    fn nat_pmp_requests_and_responses_round_trip() {
        // The public-address request is the fixed 2-byte probe.
        assert_eq!(nat_pmp::external_address_request(), [0, 0]);
        // Public-address response: IP on result 0; non-zero result / wrong op / truncated → None.
        let addr_ok = [0u8, 128, 0, 0, 0, 0, 0, 1, 203, 0, 113, 7];
        assert_eq!(
            nat_pmp::parse_external_address_response(&addr_ok),
            Some(Ipv4Addr::new(203, 0, 113, 7))
        );
        let addr_err = [0u8, 128, 0, 1, 0, 0, 0, 1, 203, 0, 113, 7]; // result code 1
        assert_eq!(nat_pmp::parse_external_address_response(&addr_err), None);
        let addr_wrong_op = [0u8, 129, 0, 0, 0, 0, 0, 1, 203, 0, 113, 7];
        assert_eq!(nat_pmp::parse_external_address_response(&addr_wrong_op), None);
        assert_eq!(nat_pmp::parse_external_address_response(&[0, 128]), None);

        // Map request encodes op + internal/external ports + lifetime big-endian (3600 = 0x0E10).
        assert_eq!(
            nat_pmp::map_udp_request(0x1234, 0x5678, 3600),
            [0, 1, 0, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 0x0E, 0x10]
        );
        // The TCP map request is identical but op = 2 (the direct-TCP door's NAT-PMP fallback).
        assert_eq!(
            nat_pmp::map_tcp_request(0x1234, 0x5678, 3600),
            [0, 2, 0, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 0x0E, 0x10]
        );
        // Map response: external port on result 0 + matching internal + matching op; else None.
        let map_ok = [0u8, 129, 0, 0, 0, 0, 0, 1, 0x12, 0x34, 0x9A, 0xBC, 0, 0, 0x0E, 0x10];
        assert_eq!(nat_pmp::parse_map_response(&map_ok, nat_pmp::OP_MAP_UDP, 0x1234), Some(0x9ABC));
        assert_eq!(nat_pmp::parse_map_response(&map_ok, nat_pmp::OP_MAP_UDP, 0x0001), None); // wrong internal
        // A UDP response (op 129) must NOT satisfy a TCP map (expects op 130), and vice versa.
        assert_eq!(nat_pmp::parse_map_response(&map_ok, nat_pmp::OP_MAP_TCP, 0x1234), None);
        let map_tcp_ok = [0u8, 130, 0, 0, 0, 0, 0, 1, 0x12, 0x34, 0x9A, 0xBC, 0, 0, 0x0E, 0x10];
        assert_eq!(nat_pmp::parse_map_response(&map_tcp_ok, nat_pmp::OP_MAP_TCP, 0x1234), Some(0x9ABC));
        assert_eq!(nat_pmp::parse_map_response(&map_tcp_ok, nat_pmp::OP_MAP_UDP, 0x1234), None);
        let map_err = [0u8, 129, 0, 2, 0, 0, 0, 1, 0x12, 0x34, 0x9A, 0xBC, 0, 0, 0x0E, 0x10];
        assert_eq!(nat_pmp::parse_map_response(&map_err, nat_pmp::OP_MAP_UDP, 0x1234), None); // result code 2
        assert_eq!(nat_pmp::parse_map_response(&[0, 129], nat_pmp::OP_MAP_UDP, 1), None);
    }

    // Test helper: the return tuple bundles the callbacks + their capture channels.
    #[allow(clippy::type_complexity)]
    fn recording_callbacks(
        hello: Vec<u8>,
    ) -> (
        MeshCallbacks,
        mpsc::Receiver<(MeshNodeAddress, Vec<u8>)>,
        mpsc::Receiver<(MeshNodeAddress, Vec<u8>)>,
    ) {
        let (wire_tx, wire_rx) = mpsc::channel::<(MeshNodeAddress, Vec<u8>)>();
        let (media_tx, media_rx) = mpsc::channel::<(MeshNodeAddress, Vec<u8>)>();
        let callbacks = MeshCallbacks {
            on_wire: Some(Arc::new(move |peer, bytes| {
                let _ = wire_tx.send((peer, bytes));
            })),
            on_media: Arc::new(move |peer, bytes| {
                let _ = media_tx.send((peer, bytes));
            }),
            make_hello: Some(Arc::new(move |_peer| hello.clone())),
            on_peer_down: None,
            on_dht_proxy_request: None,
            on_dht_bootstrap_request: None,
            on_bulk_stream: None,
        };
        (callbacks, wire_rx, media_rx)
    }

    /// The mesh comes back on the port of its previous run when it is free, and on any port when it
    /// is not — a restart must keep the address peers remember, and must never fail on it.
    #[test]
    fn mesh_takes_the_preferred_port_when_free_and_any_port_when_taken() {
        // A port that is free right now: bind, read it, let go.
        let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut config = MeshConfig::same_host(fresh_key_bytes());
        config.preferred_quic_port = Some(free);
        let (callbacks, _wire, _media) = recording_callbacks(b"hello".to_vec());
        let mesh = MeshRuntime::start(config, callbacks).expect("mesh starts on the free preferred port");
        assert_eq!(mesh.quic_addr().port(), free, "the preferred port, not a random one");

        // Negative half: that port is now held (by this mesh), so a second mesh asking for it must
        // still start — elsewhere — instead of failing or sharing the socket.
        let mut second = MeshConfig::same_host(fresh_key_bytes());
        second.preferred_quic_port = Some(free);
        let (callbacks, _wire, _media) = recording_callbacks(b"hello".to_vec());
        let other = MeshRuntime::start(second, callbacks).expect("a taken preferred port is not fatal");
        assert_ne!(other.quic_addr().port(), free, "a held port is never shared");

        other.shutdown();
        mesh.shutdown();
    }

    /// A mesh that started without the relay's TCP carrier switches to TCP in place while the relay
    /// does not answer on TCP — a restart would bind no carrier either, only tear the links down —
    /// and asks for the restart once it answers.
    #[test]
    fn a_carrier_switch_restarts_for_the_relays_tcp_carrier_only_when_the_relay_answers() {
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mut config = MeshConfig::same_host(fresh_key_bytes());
        config.relay_tcp_endpoint = Some(dead);
        let (callbacks, _wire, _media) = recording_callbacks(b"hello".to_vec());
        let mesh = MeshRuntime::start(config, callbacks).expect("a dead relay is not fatal");
        assert!(!mesh.network.has_interface(MeshInterfaceKind::QuicOverTcp), "started UDP-only");

        assert!(
            mesh.switch_carrier_policy(omni_types::CarrierPolicy::Tcp, HashMap::new()),
            "the relay does not answer: switched in place, no restart"
        );
        assert_eq!(mesh.carrier_preference(), CarrierPreference::Tcp);

        let _answering = std::net::TcpListener::bind(dead).expect("the same port, now listening");
        assert!(
            !mesh.switch_carrier_policy(omni_types::CarrierPolicy::Auto, HashMap::new()),
            "the relay answers now: a restart binds its carrier"
        );
        mesh.shutdown();
    }

    /// The door comes back on the previous run's port when it is free, and on any port when it is
    /// taken — the TCP twin of the QUIC test above.
    #[test]
    fn the_door_takes_the_preferred_port_when_free_and_any_port_when_taken() {
        let free = std::net::TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let mut config = MeshConfig::same_host(fresh_key_bytes());
        config.preferred_direct_tcp_port = Some(free);
        let (callbacks, _wire, _media) = recording_callbacks(b"hello".to_vec());
        let mesh = MeshRuntime::start(config, callbacks).expect("mesh starts");
        assert_eq!(mesh.direct_tcp_port(), Some(free), "the preferred door port, not a random one");

        // Negative half: held by the first mesh, so the second one's door lands elsewhere.
        let mut second = MeshConfig::same_host(fresh_key_bytes());
        second.preferred_direct_tcp_port = Some(free);
        let (callbacks, _wire, _media) = recording_callbacks(b"hello".to_vec());
        let other = MeshRuntime::start(second, callbacks).expect("a taken door port is not fatal");
        let other_port = other.direct_tcp_port().expect("a door on some port");
        assert_ne!(other_port, free, "a held door port is never shared");

        other.shutdown();
        mesh.shutdown();
    }

    /// Two mesh runtimes on one host discover each other via multicast, dial,
    /// and exchange BOTH planes over the single verified Link: an unreliable
    /// media datagram and a reliable wire frame (chat / call-control) — the
    /// "one channel, all data" path the app rides. Skips gracefully if the host
    /// has no multicast-capable adapter.
    #[test]
    fn two_runtimes_discover_dial_and_exchange_both_planes() {
        let alice_key = fresh_key_bytes();
        let bob_key = fresh_key_bytes();

        let alice_hello = b"mesh-hello-alice".to_vec();
        let bob_hello = b"mesh-hello-bob".to_vec();
        let (alice_cb, alice_wire_rx, _alice_media_rx) = recording_callbacks(alice_hello.clone());
        let (bob_cb, bob_wire_rx, bob_media_rx) = recording_callbacks(bob_hello.clone());

        let alice = MeshRuntime::start(MeshConfig::same_host(alice_key), alice_cb)
            .expect("alice mesh starts");
        let bob =
            MeshRuntime::start(MeshConfig::same_host(bob_key), bob_cb).expect("bob mesh starts");

        let bob_address = bob.node_address();

        // Wait for Alice to discover Bob via multicast announce (best-effort —
        // skip if no multicast adapter, mirroring omni-network's own tests).
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut dialed = false;
        while Instant::now() < deadline {
            if alice.dial(bob_address).is_ok() {
                dialed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        if !dialed {
            eprintln!("SKIP: multicast discovery unavailable on this host");
            return;
        }

        // Media plane: send continuously like an audio stream (~20 ms). Early
        // packets may race ahead of Bob's inbound Link registration and drop —
        // media is loss-tolerant — so keep sending and assert one arrives. This
        // also confirms the Link is fully up on both sides.
        let media = vec![0x42u8; 160];
        let mut media_received = None;
        let media_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < media_deadline {
            alice
                .send_media(bob_address, &media)
                .expect("alice sends media");
            if let Ok(pair) = bob_media_rx.recv_timeout(Duration::from_millis(20)) {
                media_received = Some(pair);
                break;
            }
        }
        let (media_from, media_got) =
            media_received.expect("bob receives a media datagram within 5s");
        assert_eq!(media_from, alice.node_address());
        assert_eq!(media_got, media);

        // App handshake: each side sent its hello on link-up; assert the peer
        // received it as the first reliable frame.
        let (bob_hello_from, bob_hello_got) = bob_wire_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("bob receives alice's hello within 5s");
        assert_eq!(bob_hello_from, alice.node_address());
        assert_eq!(bob_hello_got, alice_hello);

        let (alice_hello_from, alice_hello_got) = alice_wire_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("alice receives bob's hello within 5s");
        assert_eq!(alice_hello_from, bob_address);
        assert_eq!(alice_hello_got, bob_hello);

        // Wire plane: a subsequent reliable frame is delivered in order after
        // the hello (no retry — reliable, ordered, same Link).
        let chat = b"chat-hello-over-mesh".to_vec();
        alice
            .send_wire(bob_address, &chat)
            .expect("alice sends wire frame");
        let (wire_from, wire_got) = bob_wire_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("bob receives the chat frame within 5s");
        assert_eq!(wire_from, alice.node_address());
        assert_eq!(wire_got, chat);

        alice.shutdown();
        bob.shutdown();
    }
}
