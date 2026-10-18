// Network orchestrator — top-level public API. См.
// docs/architecture/networking-overlay.md §«Architecture layers».
//
// Phase α-3 (2026-05-28): data plane moved onto point-to-point QUIC with
// mutual identity verification. Dispatcher task reads frames from each
// interface, routes по FrameKind:
// - Announce → verify signature, store peer (pubkey + advertised QUIC
//   addresses) in peer_book, update path_table
// - ChallengeRequest → respond (sign nonce) + challenge back once (mutual auth)
// - ChallengeResponse → verify against expected NodeAddress, mark neighbor
//   verified, wake dial() or hand an incoming Link to accept()
// - Data → deliver to the Link inbox ONLY if it arrived from a verified
//   neighbor whose identity matches the frame's `from` (anti-spoof)
//
// **Two complementary tables** (not duplication — different layers):
// - `peer_book`: NodeAddress → (pubkey, advertised addresses). Answers
//   "who is this and where do I dial them" for *direct* connection setup.
// - `path_table`: NodeAddress → (next-hop interface/neighbor, cost, TTL).
//   Soft-state reachability that feeds **Phase β multi-hop routing**; not
//   consumed by α-3 direct dial.
//
// Public API:
// - `Network::new(identity, interfaces)` — construct
// - `network.local_address()` — наш NodeAddress
// - `network.start()` — spawn'ит dispatcher background task
// - `network.announce(addrs)` — broadcast signed announce через все interfaces
// - `network.dial(destination)` — QUIC-connect + mutual-verify → Link
// - `network.accept()` — receive next *verified* incoming Link
// - `network.shutdown()` — clean teardown
//
// Phase α-3 ограничения:
// - Direct (single-hop) links only — peer must be reachable at an advertised
//   address. Multi-hop forwarding через intermediate nodes — Phase β.
// - NAT hole-punching coordination — Phase β.

use crate::address::NodeAddress;
use crate::announce::Announce;
use crate::cert::{
    fresh_challenge_nonce, respond_to_challenge, verify_challenge_response, CHALLENGE_NONCE_BYTES,
};
use crate::interface::{BulkStream, InterfaceHandle, InterfaceId, InterfaceKind, NeighborId};
use crate::media_datagram::{MediaDatagram, MEDIA_DATAGRAM_DEFAULT_HOP_LIMIT};
use crate::path_table::PathTable;
use crate::durable::{FeedId, ReconcileEngine, ReconcileState};
use crate::protocol::{
    AntiEntropyHaveFrame, AntiEntropyOfferFrame, AntiEntropyRequestFrame, ChallengeRequest,
    ChallengeResponseFrame, DataFrame, DhtBootReqFrame, DhtBootRespFrame, DhtProxyOp,
    DhtProxyRequestFrame, DhtProxyResponseFrame,
    DialBackProbeFrame, DialBackRequestFrame, DurableDataFrame, FriendRelayCarryFrame,
    NetworkFrame, NetworkFrameError, ObservedAddressFrame, PathReplyFrame, PathRequestFrame,
    PeerPingFrame, PeerPongFrame, PunchRequestFrame, PunchSyncFrame, TcpReachabilityRequestFrame,
    TcpReachabilityResultFrame, RelayCardReqFrame, RelayCardRespFrame, MAX_ANTI_ENTROPY_ENTRIES,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use futures_util::stream::{FuturesUnordered, StreamExt};
use rand_core::{OsRng, RngCore};
use parking_lot::Mutex as SyncMutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// Default TTL для announce'ов которые мы publish'им. 300 секунд = 5 минут;
/// peers refresh каждые ~5 минут чтобы entries не expired.
pub const DEFAULT_ANNOUNCE_TTL_SECONDS: u32 = 300;

/// How long `dial` waits for the responder to prove its identity before
/// giving up on a candidate address. Ed25519 verify is fast even in debug;
/// the budget is for the network round-trips of the mutual handshake.
/// Per-attempt wait for a `ChallengeResponse` before re-sending the SAME challenge. Kept short so a
/// lost request/response on a flaky link (mobile CGNAT) is retried quickly rather than stalling.
const CHALLENGE_TIMEOUT: Duration = Duration::from_millis(2500);
/// How many times the dialer re-sends the challenge over the SAME connection before giving up. A
/// single packet loss must not permanently fail a dial — re-sending (same nonce, same QUIC
/// connection) rides out transient loss WITHOUT opening a churny new connection. Worst-case dial wait
/// = `CHALLENGE_TIMEOUT × CHALLENGE_SEND_ATTEMPTS`.
const CHALLENGE_SEND_ATTEMPTS: u32 = 4;

/// Happy Eyeballs "Connection Attempt Delay" (RFC 8305 §5, recommended value): how long `dial`
/// waits on one endpoint's connect before it ALSO starts the next one. A dead endpoint therefore
/// costs this much, not the QUIC handshake timeout; a live but slower one keeps running and may
/// still win.
const DIAL_ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// At most this many connects of one `dial` in flight at once (libp2p's default dial concurrency
/// factor). The peer book holds at most `MAX_PEER_ADVERTISED_ADDRESSES` per interface anyway.
const DIAL_CONCURRENCY: usize = 8;

/// How long an AutoNAT dial-back request waits for its probe before counting as a
/// verifier failure (toward the `Unreachable` verdict). Generous: covers the
/// verifier's fresh dial + round-trip.
const DIAL_BACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a door-carried connection migrated back to UDP gets to prove itself (identity challenge)
/// before it returns to its door. Well under the QUIC idle timeout, so a dead UDP leg costs a few
/// seconds of queued traffic, never the connection.
const MIGRATE_BACK_PROOF_TIMEOUT: Duration = Duration::from_secs(6);
/// Pause after a failed migrate-back, doubling up to the cap. Short enough that a UDP leg which
/// recovers (the common case: a passing outage) is back on UDP within minutes.
const MIGRATE_BACK_FIRST_PAUSE: Duration = Duration::from_secs(180);
const MIGRATE_BACK_MAX_PAUSE: Duration = Duration::from_secs(1800);

/// Task 3 — how long a verifier waits for its TCP-connect-back to a peer's front door before
/// reporting it unreachable. See docs/decisions/tcp-direct-peer-fallback.md.
const TCP_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on Data frames buffered per neighbor while its identity is still being
/// verified (mutual handshake in flight). Bounds memory against a peer that
/// floods Data before completing the handshake; overflow is dropped.
const MAX_PENDING_DATA_FRAMES: usize = 64;

/// Caps on distinct **unsolicited** origins for which we create an incoming
/// multi-hop Link on-demand (a frame from a peer we never dialed, relayed by a
/// verified neighbor). Bound injection-spam by a malicious relay: the **per-relay**
/// cap ensures a single bad verified neighbor can't fill the whole pool (it gets
/// at most `PER_RELAY`), and the **global** cap bounds total memory. The
/// app-layer "approved-contact only" gate + E2E undecryptable-drop are the other
/// two bounds (см. ADR mesh-multihop-identity-delivery.md + external review §3.4).
/// Deferring allocation entirely until app-accept (review §3.4 item 3) is the
/// end-state follow-up; idle-eviction is intentionally NOT done here — it would
/// race a live-but-idle conversation's inbox (see commit message).
const MAX_MULTIHOP_ORIGINS_TOTAL: usize = 256;
const MAX_MULTIHOP_ORIGINS_PER_RELAY: usize = 8;

/// Size of the recently-seen-frame dedup window (origin + msg_id). Bounds the
/// memory of the replay / fan-out guard; oldest entries evicted FIFO. A short
/// flood/replay window, NOT the durable layer's long-term reconciliation.
const SEEN_FRAMES_CAP: usize = 4096;

/// Initial TTL of a reactive PathRequest flood (R3): bounds both the flood radius and how many hops
/// a discovered route may be. Media/mesh relay is meant to be shallow, so a small cap keeps the flood
/// cheap. See docs/decisions/omni-network-model.md (D6).
const PATH_REQUEST_DEFAULT_HOP_LIMIT: u8 = 4;

/// How long `dial` waits for a PathReply to fill the route after flooding a PathRequest, before
/// giving up (and falling through to "no path"). Bounded so a dial never hangs on discovery.
const PATH_REQUEST_TIMEOUT: Duration = Duration::from_millis(2500);

/// Poll cadence while waiting for the discovered route to appear in the path table.
const PATH_REQUEST_POLL: Duration = Duration::from_millis(50);

/// Sliding window + cap for path-control frames (PathRequest + PathReply) ACCEPTED from one verified
/// neighbour (R3 anti-eclipse/DoS). A verified neighbour is only a route candidate, not trusted: a
/// malicious one could spam distinct-nonce requests (amplified by re-broadcast) or bogus replies
/// (path_table pollution). Capping per arrival neighbour bounds both without hurting honest use —
/// real discovery is a handful of frames per dial. Generous so bursts of legit traffic pass.
const PATH_CONTROL_RATE_WINDOW: Duration = Duration::from_secs(10);
const PATH_CONTROL_RATE_MAX: u32 = 64;

/// Global cap on durable frames a node holds (own + store-and-forward for others).
/// Bounds memory against the S&F amplification the durable ADR warns about.
const MAX_DURABLE_FRAMES: usize = 8192;
/// Per-`from` cap on stored durable frames, so one source can't monopolise the
/// global budget (fairness — extends TM-005/008 to the durable layer).
const MAX_DURABLE_FRAMES_PER_SOURCE: usize = 512;

/// After coordinating a hole-punch, how long to let the relay's `PunchSync` reach both
/// sides + both probes open the pinholes before the direct dial. P2 auto-upgrade.
const PUNCH_PROBE_GRACE: Duration = Duration::from_millis(200);

/// Which side dials in a coordinated hole-punch. QUIC has no symmetric simultaneous-open, so
/// exactly ONE side dials; the other only probes (its pinhole is what lets the dialer's Initial
/// through). See [`decide_punch_role`] + docs/decisions/mesh-hole-punching.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PunchRole {
    Dialer,
    Listener,
}

/// Elect the single hole-punch dialer deterministically: the **lower `NodeAddress` dials**, the
/// other listens. Pure + symmetric — both peers derive the same verdict from the same address pair,
/// so two sides that independently coordinate the same punch never both dial (which QUIC can't do).
fn decide_punch_role(local: &NodeAddress, peer: &NodeAddress) -> PunchRole {
    if local < peer {
        PunchRole::Dialer
    } else {
        PunchRole::Listener
    }
}

/// Friend-proxy (Axis 2) per-origin anti-abuse: even a consented friend caps how many frames it
/// carries for one origin per window — a consented peer still can't turn us into a flood amplifier.
const FRIEND_RELAY_WINDOW_MS: u64 = 60_000;
const FRIEND_RELAY_MAX_CARRIES_PER_WINDOW: usize = 600;

/// Friend-exit (Axis 2, Phase D) per-origin anti-abuse: even a consented friend caps, per window,
/// how many DISTINCT destinations it will egress toward for one origin AND the total bytes it
/// egresses for that origin — a consented peer still can't turn our uplink into an open,
/// unbounded proxy. See docs/decisions/friend-proxy-by-consent.md (Phase D).
const FRIEND_EXIT_WINDOW_MS: u64 = 60_000;
const FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW: usize = 64;
const FRIEND_EXIT_MAX_BYTES_PER_WINDOW: u64 = 32 * 1024 * 1024;

/// Per-origin friend-exit accounting within the current sliding window: which destinations we have
/// egressed toward (for the distinct-destination cap) and the running byte total, each stamped with
/// timestamps for lazy window expiry. A destination is an opaque key (the egress leg decides its
/// shape — host:port, mesh node, etc.); Phase D1 only counts. See friend-proxy-by-consent.md.
#[derive(Default)]
struct FriendExitWindow {
    /// (destination-key → last-seen ms) within the window — its len is the distinct-destination count.
    destinations: HashMap<Vec<u8>, u64>,
    /// (bytes, at-ms) egress records within the window; summed for the byte cap, GC'd by age.
    bytes: Vec<(u64, u64)>,
}

/// Mesh ping-liveness (prompt link-DOWN). A verified neighbour silent for
/// `LIVENESS_PING_AFTER` gets a `Ping`; a live peer answers `Pong` (or sends any
/// frame) and stays marked alive — so an idle-but-alive link never churns (the
/// failure mode that killed the QUIC keepalive/idle-timeout approach: keepalive
/// resets the idle timer, a short idle alone tears down live-idle links). A peer
/// that stays silent past `LIVENESS_DEAD_AFTER` (≈3 missed pings) is declared dead
/// and pruned (→ `peer_down`), surfacing offline far faster than the QUIC idle
/// timeout — so the next message falls through to durable without a process
/// restart. App-level, independent of QUIC internals. See
/// docs/decisions/headless-omni-node.md §"prompt link-down detection".
const LIVENESS_TICK: Duration = Duration::from_secs(4);
const LIVENESS_PING_AFTER: Duration = Duration::from_secs(5);
const LIVENESS_DEAD_AFTER: Duration = Duration::from_secs(17);
/// A peer that comes back over a NEW direct connection while an older one is still on record has
/// most likely restarted — its old connection is dead, but nothing tells us so (a crashed process
/// sends no close), and the path-ranking tie-break may keep choosing it. So the older direct paths
/// are pinged at once and pruned on the next liveness tick if they stayed silent this long after
/// the ping, instead of carrying frames into nowhere until `LIVENESS_DEAD_AFTER`. A live spare
/// (simultaneous open) answers and stays. See docs/bugs/receipt-after-restart-fails-to-decrypt.md.
const SUPERSEDED_PROBE_WINDOW: Duration = Duration::from_secs(2);
/// R1a fail-back window: how long a direct-UDP path stays demoted below the direct-TCP door after a
/// delivery failure over it. Long enough to ride out a sustained mobile UDP cut, short enough that a
/// UDP path that recovers (network change / roam) climbs back to preferred without a restart.
const UDP_DEMOTE_DURATION: Duration = Duration::from_secs(120);

/// End-to-end liveness for a peer reached ONLY over a relay. The per-neighbour `LIVENESS_*` above
/// only proves the RELAY neighbour, so a relay-reached peer that dies is never detected — it reads as
/// "online via relay" until its route TTL (the bug in docs/bugs/relay-presence-false-online-dead-peer.md).
/// We send a ROUTED `PeerPing` to such a peer when it's been end-to-end-quiet for `LIVENESS_PING_AFTER`
/// (it answers with a routed `PeerPong`), and prune its relay paths (+ `peer_down`) once it's been
/// silent past `RELAY_PEER_DEAD_AFTER` (a few missed probes; larger than `LIVENESS_DEAD_AFTER` to absorb
/// the relay round-trip). A peer with a Direct path is covered by neighbour liveness and never probed
/// here. `PEER_PING_HOP_LIMIT` bounds how far a probe/reply is forwarded (mesh relay is shallow).
const RELAY_PEER_DEAD_AFTER: Duration = Duration::from_secs(25);
const PEER_PING_HOP_LIMIT: u8 = 6;
/// How many probes to a relay-only peer are remembered (one per `LIVENESS_TICK`: ~20 s of answers
/// may still come back — inside `RELAY_PEER_DEAD_AFTER`).
const PEER_PING_REMEMBERED: usize = 5;

/// Bounded durable-frame store with TTL expiry + global/per-source caps. Keyed by
/// `(feed, seq)`. `now` is injected (not read from the clock) so TTL is unit-
/// testable. Holds others' frames for store-and-forward + our own for delivery +
/// dedup; bounds memory against flooding. См. mesh-durable-delivery.md §DoS.
#[derive(Default)]
struct DurableStore {
    frames: HashMap<(FeedId, u64), (DurableDataFrame, Instant)>,
}

impl DurableStore {
    fn new() -> Self {
        Self::default()
    }

    /// GC expired entries, then admit `frame` if new and within both caps.
    /// Returns true iff newly stored (false = duplicate / expired-on-arrival /
    /// over the global or per-source cap → dropped).
    ///
    /// The caps keep a source from flooding us with frames we only CARRY. A frame this node needs
    /// itself (`needed`: our own, addressed to us, or for a group we are in) is never refused for
    /// them: a full budget makes room by dropping that source's OLDEST frame instead, so the
    /// source stays bounded and its newest frame gets in. Refusing was a silent black hole —
    /// `send_durable` still said Ok — and it filled up for real: a device that once copied
    /// messages every 15 s to two unreachable own devices held ~7000 of its own 7-day frames, and
    /// the PC carrying them for it had its quota for that device full too; from then on every new
    /// group message and call invite of that device reached no one (live 2026-10-10,
    /// docs/bugs/full-durable-quota-swallowed-group-traffic.md).
    fn insert(
        &mut self,
        frame: DurableDataFrame,
        now: Instant,
        total_cap: usize,
        per_source_cap: usize,
        needed: bool,
    ) -> bool {
        self.gc(now);
        let key = (frame.feed_id, frame.seq);
        if self.frames.contains_key(&key) {
            return false; // dedup — already held
        }
        let expires_at = now
            .checked_add(Duration::from_secs(frame.ttl_secs as u64))
            .unwrap_or(now);
        if expires_at <= now {
            return false; // ttl 0 / expired on arrival — don't hold
        }
        let from = frame.from;
        if self.frames.len() >= total_cap && !(needed && self.evict_oldest_from(from)) {
            return false; // global budget full
        }
        let from_count = self.frames.values().filter(|(f, _)| f.from == from).count();
        if from_count >= per_source_cap && !(needed && self.evict_oldest_from(from)) {
            return false; // this source's quota is full
        }
        self.frames.insert(key, (frame, expires_at));
        true
    }

    /// Drop the frame of `from` that expires first — with one TTL for all, the oldest. False when
    /// `from` holds nothing here.
    fn evict_oldest_from(&mut self, from: NodeAddress) -> bool {
        let oldest = self
            .frames
            .iter()
            .filter(|(_, (f, _))| f.from == from)
            .min_by_key(|(_, (_, expires_at))| *expires_at)
            .map(|(key, _)| *key);
        oldest.is_some_and(|key| self.frames.remove(&key).is_some())
    }

    /// Drop entries whose TTL has elapsed at `now`.
    fn gc(&mut self, now: Instant) {
        self.frames.retain(|_, (_, expires_at)| *expires_at > now);
    }

    #[cfg(test)]
    fn contains(&self, key: &(FeedId, u64)) -> bool {
        self.frames.contains_key(key)
    }

    fn get(&self, key: &(FeedId, u64)) -> Option<&DurableDataFrame> {
        self.frames.get(key).map(|(f, _)| f)
    }

    fn frames(&self) -> impl Iterator<Item = &DurableDataFrame> {
        self.frames.values().map(|(f, _)| f)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.frames.len()
    }
}

/// What we remember about a peer after verifying its signed announce:
/// the pubkey (for reference / future use) and where to dial it.
#[derive(Debug, Clone)]
struct PeerRecord {
    // `Some` from a verified announce; `None` for an endpoint injected out-of-band (e.g.
    // serverless rendezvous, where we know the NodeAddress + endpoint but not the raw
    // pubkey — dial verifies identity by challenge against the NodeAddress anyway).
    #[allow(dead_code)] // retained for diagnostics + future trust checks
    pubkey: Option<VerifyingKey>,
    advertised_addresses: Vec<SocketAddr>,
    /// The peer's signed announce `capabilities` bitfield (e.g. `ANNOUNCE_CAP_DHT_PROXY`); 0 for an
    /// out-of-band-injected endpoint (rendezvous) where we have no verified announce. Drives role-based
    /// proxy selection (`peer_advertises_dht_proxy`). See docs/decisions/dht-over-peer.md.
    capabilities: u8,
    /// The port of the peer's TCP front door from its signed announce (0 = none or unknown).
    door_port: u16,
    /// The announce came straight from the peer (hop 0), not forwarded by a relay, so its advertised
    /// LAN addresses are the peer's own (`lan_door`).
    heard_direct: bool,
}

/// Role of an in-flight challenge we are awaiting a response to, keyed by
/// the neighbor (QUIC connection) it was sent over.
enum ChallengeRole {
    /// We initiated the connection via `dial`; on success hand the verified
    /// peer's inbox channels back to the waiting caller so it can build a Link.
    Dialer {
        waker: oneshot::Sender<Result<VerifiedLinkChannels, NetworkError>>,
    },
    /// We are the responder challenging the dialer back (mutual auth). On
    /// success we synthesize an incoming Link and hand it to `accept`.
    Responder,
}

/// Receiver halves of a freshly-verified Link's inbound channels: the
/// reliable frame plane (chat/data) and the unreliable datagram plane (media).
struct VerifiedLinkChannels {
    /// The identity the neighbour proved (for a dial at a bare address — `NodeAddress::ANY` — the one
    /// learned from its key).
    peer: NodeAddress,
    inbox_rx: mpsc::Receiver<Vec<u8>>,
    datagram_rx: mpsc::Receiver<Vec<u8>>,
}

/// An identity challenge we sent and are awaiting the matching response for.
struct OutboundChallenge {
    nonce: [u8; CHALLENGE_NONCE_BYTES],
    /// NodeAddress the responder must prove control of.
    expected: NodeAddress,
    role: ChallengeRole,
}

/// Per-neighbor buffer of pre-verification Data frames (origin `NodeAddress` + payload).
type PendingDataFrames = HashMap<NeighborId, Vec<(NodeAddress, Vec<u8>)>>;
/// In-flight DHT-over-peer requests: `req_id` → completion channel for the awaiting requester.
type DhtProxyPending = HashMap<u64, oneshot::Sender<(bool, Vec<u8>)>>;

/// Top-level networking orchestrator.
/// The carrier of OUR hops — from this node to the next one, a relay or the contact (owner,
/// 2026-10-11). `Tcp`: TLS-over-TCP only (the relay's TCP carrier, a peer's TCP door) — no raw UDP
/// dial, no punch, no climb. `Quic`: QUIC/UDP only — no door. `Auto`: both, QUIC preferred. What the
/// next node does onwards is its own setting. See docs/decisions/connection-quality-ladder.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CarrierPreference {
    Auto = 0,
    Tcp = 1,
    Quic = 2,
}

pub struct Network {
    identity: SigningKey,
    local_address: NodeAddress,
    interfaces: Vec<InterfaceHandle>,
    /// Soft-state reachability for **Phase β multi-hop routing** (next-hop, cost,
    /// TTL). Populated from announces; читается multi-hop форвардом
    /// (`decide_data_forward`).
    path_table: Arc<PathTable>,
    /// Phase β: пересылаем ли мы Data-кадры **не нам** дальше по next-hop
    /// (relay для mesh'а). Cheap-forwarding tier из product-vision — мелкие
    /// chat/control кадры; default ON. Bounded hop_limit'ом; per-source квоты —
    /// follow-up (durable-delivery DoS, см. ADR mesh-durable-delivery.md).
    relay_enabled: bool,
    /// Address book: verified-announce identities → where to dial them.
    peer_book: Arc<SyncMutex<HashMap<NodeAddress, PeerRecord>>>,
    /// Inbound reliable-Data queues for active Links, keyed by the **end-peer
    /// identity** (`data.from` for inbound, `destination` for outbound) — NOT
    /// the arrival neighbor. Multi-hop: a frame from origin A relayed by
    /// neighbor B is delivered to the Link for A. Per-hop auth (the relay is a
    /// verified neighbor) gates injection; origin authenticity is the E2E
    /// layer's job (см. ADR mesh-multihop-identity-delivery.md).
    link_inboxes: Arc<SyncMutex<HashMap<NodeAddress, mpsc::Sender<Vec<u8>>>>>,
    /// Per-IDENTITY inbound datagram queues (unreliable media plane). Keyed by the peer
    /// NodeAddress like `link_inboxes` (R2): the datagram dispatcher parses the routing envelope
    /// (`MediaDatagram`) and delivers a media payload destined for us to the inbox of its logical
    /// source (`from`) — so a RELAYED media datagram reaches the right Link (the relay is not the
    /// source). Created alongside `link_inboxes` at verification; persists across path churn.
    link_datagram_inboxes: Arc<SyncMutex<HashMap<NodeAddress, mpsc::Sender<Vec<u8>>>>>,
    /// Neighbors whose identity we have verified via challenge-response, and
    /// the NodeAddress they proved. Gate for accepting their Data.
    verified_neighbors: Arc<SyncMutex<HashMap<NeighborId, NodeAddress>>>,
    /// Inbound bulk streams (attachment transfer), tagged with the peer identity that opened them.
    /// One entry per TRANSFER, not per packet — the queue only has to absorb concurrent offers.
    bulk_tx: mpsc::Sender<(NodeAddress, BulkStream)>,
    bulk_rx: Arc<AsyncMutex<mpsc::Receiver<(NodeAddress, BulkStream)>>>,
    /// Last time we received ANY frame from a verified neighbour. The liveness
    /// pump pings the quiet ones and prunes the long-silent ones (mesh
    /// ping-liveness — prompt link-DOWN). Set at verification, refreshed on every
    /// inbound frame, removed on prune. See `LIVENESS_*`.
    last_seen: Arc<SyncMutex<HashMap<NeighborId, Instant>>>,
    /// Older direct paths of a peer that just re-verified over a new connection, with the instant
    /// they were pinged. Settled by `superseded_plan`. See `SUPERSEDED_PROBE_WINDOW`.
    superseded_probes: SyncMutex<HashMap<NeighborId, Instant>>,
    /// Per path-neighbour connection RX-byte count at the previous liveness tick. A connection whose
    /// RX grew since last tick is alive even if it sent no mesh FRAMES (e.g. a bulk attachment stream
    /// pouring in) — that refreshes `last_seen` so it isn't pruned as idle. See
    /// docs/bugs/chat-gated-on-stale-has-link.md.
    conn_rx_baseline: Arc<SyncMutex<HashMap<NeighborId, u64>>>,
    /// End-to-end last-seen keyed by the PEER's own identity (NOT the path neighbour) — refreshed by a
    /// routed `PeerPong` from that peer or `Data` it originated, and seeded when a relay path to it is
    /// first registered. Drives relay-only-peer liveness: a relay-reached peer is never our path
    /// neighbour, so per-neighbour `last_seen` can't tell when it dies. "No entry" = alive by default
    /// (mirrors `liveness_plan`). See `RELAY_PEER_DEAD_AFTER` + docs/bugs/relay-presence-false-online-dead-peer.md.
    peer_last_seen: Arc<SyncMutex<HashMap<NodeAddress, Instant>>>,
    /// The nonce of the most-recent routed `PeerPing` we sent each relay-only peer. A `PeerPong`
    /// refreshes `peer_last_seen` ONLY if its nonce matches this — a stale / duplicate / relay-buffered
    /// pong (old nonce) must not re-seed liveness, which was the post-death flap. Mirrors the AutoNAT
    /// dial-back / PunchSync nonce gate. See docs/bugs/relay-presence-false-online-dead-peer.md.
    peer_ping_nonce: Arc<SyncMutex<HashMap<NodeAddress, u64>>>,
    /// The probes still in flight to each relay-only peer: (nonce, when sent), newest last, at most
    /// [`PEER_PING_REMEMBERED`]. See `note_peer_pong`.
    peer_ping_sent: Arc<SyncMutex<HashMap<NodeAddress, std::collections::VecDeque<(u64, Instant)>>>>,
    /// Challenges we have sent and are awaiting responses to, keyed by the
    /// neighbor the challenge went over.
    outbound_challenges: Arc<SyncMutex<HashMap<NeighborId, OutboundChallenge>>>,
    /// Data frames received from a neighbor before its mutual handshake
    /// completed. Drained into the Link inbox once the neighbor is verified;
    /// dropped if verification fails. See `MAX_PENDING_DATA_FRAMES`.
    pending_data: Arc<SyncMutex<PendingDataFrames>>,
    /// Origins for which we created an incoming multi-hop Link on-demand (peers
    /// we never dialed, whose Data reached us relayed by a verified neighbor),
    /// mapped to the relay neighbor that introduced each. Bounded per-relay and
    /// globally (`MAX_MULTIHOP_ORIGINS_*`) to cap injection-spam — one bad relay
    /// can't exhaust the pool for everyone.
    multihop_origins: Arc<SyncMutex<HashMap<NodeAddress, NeighborId>>>,
    /// Recently-seen frame identities (origin + msg_id) for mesh-level dedup —
    /// drops replays and duplicate multi-hop fan-out before deliver/forward.
    seen_frames: Arc<SyncMutex<SeenFrames>>,
    /// Recently-seen path-discovery requests, keyed by `(origin, nonce)` — process+forward each
    /// PathRequest once (the flood loop/dup guard). Separate window from `seen_frames` so a
    /// request nonce can't collide with a Data msg_id from the same origin (R3).
    seen_path_requests: Arc<SyncMutex<SeenFrames>>,
    /// Monotone source of PathRequest nonces, seeded from the wall clock so post-restart nonces
    /// don't collide with a still-cached pre-restart `(origin, nonce)` on some node (R3).
    path_request_seq: std::sync::atomic::AtomicU64,
    /// Per-neighbour sliding-window counter of accepted path-control frames (R3 anti-eclipse/DoS):
    /// `NeighborId → (window_start, count)`. Bounded by the (small) verified-neighbour count.
    path_control_quota: Arc<SyncMutex<HashMap<NeighborId, (Instant, u32)>>>,
    /// Reflexive-address reports from verified neighbours (NeighborId → the addr
    /// that neighbour saw us connect from). Aggregated by `infer_reflexive` to
    /// learn our external endpoint + NAT class without STUN. P1 / review §4.1A.
    reflexive_observations: Arc<SyncMutex<HashMap<NeighborId, SocketAddr>>>,
    /// Per-peer transport path sets (PeerLink). Each peer's `Link` shares its
    /// `PathSet` so adding a better path silently upgrades the live Link's sends.
    /// P2 / docs/decisions/mesh-peer-link-multipath.md.
    peer_paths: Arc<SyncMutex<HashMap<NodeAddress, Arc<SyncMutex<PathSet>>>>>,
    /// Под каким ключом у пира сейчас стоит туннель в его дверь.
    ///
    /// Карта туннелей внутри `MuxRouter` ключуется АДРЕСОМ (quinn знает только адреса), а адрес пира
    /// меняется вместе с портом при каждом его перезапуске или ремапе NAT. Без этой записи прежний
    /// туннель некому вытеснить: он остаётся в карте под старым ключом и живёт до конца процесса.
    /// Единственное место во всём слое, где ресурс ПИРА ключевался адресом, — здесь и лечится.
    /// См. docs/bugs/dead-doors-retried-forever.md.
    peer_doors: Arc<SyncMutex<HashMap<NodeAddress, SocketAddr>>>,
    /// Peer → (no migrate-back attempt before, current pause). A failed migrate-back costs up to
    /// `MIGRATE_BACK_PROOF_TIMEOUT` of traffic sent into dead UDP; on a carrier that really blocks
    /// UDP that must not repeat every rendezvous sweep (~90 s) and freeze a call each time.
    migrate_back_backoff: Arc<SyncMutex<HashMap<NodeAddress, (Instant, Duration)>>>,
    /// AutoNAT reachability state (P2): are we directly dialable from outside?
    /// Confirmed when a verifier we asked dials our candidate address back and
    /// its `DialBackProbe` reaches us. Feeds the relay-vs-direct decision (no
    /// point advertising / preferring direct if nobody can dial us). Distinct
    /// from P1 reflexive (knowing the address) — this proves the address works.
    reachability: Arc<SyncMutex<ReachabilityTracker>>,
    /// Task 3 — TCP-direct front door confirmation. `pending` holds the in-flight
    /// `(nonce, candidate)` of a `TcpReachabilityRequest`; `confirmed` is the candidate a verifier's
    /// TCP-connect-back succeeded for (advertised in the beacon `tcp_direct` only once confirmed —
    /// validate-before-advertise, the TCP analog of `reachability`). See tcp-direct-peer-fallback.md.
    pending_tcp_confirm: Arc<SyncMutex<Option<(u64, SocketAddr)>>>,
    confirmed_tcp_address: Arc<SyncMutex<Option<SocketAddr>>>,
    /// Durable-delivery store (P4): held DurableData frames keyed by `(feed, seq)`
    /// — ours to deliver-on-recv + others' to store-and-forward until the
    /// destination is reachable. In-memory for now; `omni-storage`-backed (survives
    /// restart) is a later increment. See mesh-durable-delivery.md.
    durable_store: Arc<SyncMutex<DurableStore>>,
    /// Which `(feed, seq)` we hold — drives anti-entropy high-water/offers.
    durable_reconcile: Arc<SyncMutex<ReconcileEngine>>,
    /// Frames addressed to us that the app has finished handling (`note_durable_handled`). Only
    /// these are exported along with the frames held for others: re-imported, they restore our
    /// high-water so holders stop re-offering what we already have, while a frame that reached the
    /// network but not the app stays unmarked and is offered again after a restart.
    durable_handled: SyncMutex<HashSet<(FeedId, u64)>>,
    /// Group subscriptions (G3): synthetic group destination → the member node
    /// addresses to gossip-push to. A durable frame addressed to a subscribed
    /// destination is delivered to OUR inbox (subscription-based delivery — no
    /// node owns the group address) and re-pushed once to linked members, so a
    /// group message reaches online members sub-second while anti-entropy stays
    /// the catch-all. См. docs/decisions/group-envelope-and-transport.md §2.
    group_subscriptions: Arc<SyncMutex<HashMap<NodeAddress, Vec<NodeAddress>>>>,
    /// Each peer's last-advertised anti-entropy INTERESTS (the group destinations + own address it
    /// wants frames for). Populated from inbound HAVE frames; used to scope what we offer/advertise
    /// so we stop broadcasting our full feed inventory to route-candidates that aren't co-members and
    /// aren't forwarding for us. See docs/decisions/anti-entropy-have-scoping.md.
    peer_interests: Arc<SyncMutex<HashMap<NodeAddress, Vec<NodeAddress>>>>,
    /// Durable frames addressed to US, delivered for the app to drain (`recv_durable`).
    durable_inbox_tx: mpsc::Sender<DurableDataFrame>,
    durable_inbox_rx: AsyncMutex<mpsc::Receiver<DurableDataFrame>>,
    /// Peers that became fully unreachable (their last path was pruned), for the
    /// app to drain (`try_recv_peer_down`) and tear down stale session state — so a
    /// message to a just-offline peer falls through to durable instead of a dead link.
    peer_down_tx: mpsc::Sender<NodeAddress>,
    peer_down_rx: AsyncMutex<mpsc::Receiver<NodeAddress>>,
    /// DHT-proxy requests addressed to US (R7 Phase 2 mode b): delivered for the app to serve via its
    /// local `omni-dht-helper` (`try_recv_dht_proxy_request`) and answer with `send_dht_proxy_response`.
    /// omni-network only routes; the opt-in gate + the helper IPC live in omni-app (which holds the
    /// helper endpoint). See docs/decisions/dht-over-peer.md.
    dht_proxy_request_tx: mpsc::Sender<DhtProxyRequestFrame>,
    dht_proxy_request_rx: AsyncMutex<mpsc::Receiver<DhtProxyRequestFrame>>,
    /// Outstanding DHT-over-peer requests WE originated (R7 Phase 2 mode b), keyed by `req_id` → a
    /// oneshot the matching `DhtProxyResponse` completes. The requester half: `dht_get/put_over_peer`
    /// register here, `handle_dht_proxy_response` completes. See docs/decisions/dht-over-peer.md.
    dht_proxy_pending: Arc<SyncMutex<DhtProxyPending>>,
    /// DHT bootstrap seed requests addressed to US (step B): a direct neighbour asked for the
    /// in-segment DHT bootstrap addresses we know. Delivered for the app to answer with its configured
    /// list (`try_recv_dht_boot_request` → `send_dht_boot_response`), gated on DHT-rendezvous-enabled in
    /// omni-app. The tuple is `(requester identity, req_id)`. NEVER forwarded — direct-neighbour only,
    /// so there is no multi-hop harvest of DHT IPs. See docs/decisions/dht-bootstrap-mesh-propagation.md.
    dht_boot_request_tx: mpsc::Sender<(NodeAddress, u64)>,
    dht_boot_request_rx: AsyncMutex<mpsc::Receiver<(NodeAddress, u64)>>,
    /// Outstanding DHT bootstrap requests WE originated (step B), keyed by `req_id` → a oneshot the
    /// matching `DhtBootResp` completes. See docs/decisions/dht-bootstrap-mesh-propagation.md.
    dht_boot_pending: Arc<SyncMutex<HashMap<u64, oneshot::Sender<Vec<SocketAddr>>>>>,
    /// The card this node hands out as a relay (relay directory stage 2): `Some` — answer «who are
    /// you» challenges and `RelayCardReq`s; `None` — neither (an ordinary node does not tell a scanner
    /// who sits at its address).
    relay_card: SyncMutex<Option<Vec<u8>>>,
    relay_card_pending: Arc<SyncMutex<HashMap<u64, oneshot::Sender<Vec<u8>>>>>,
    /// Event-driven relay→direct upgrade signal (P2). A peer that just became reachable ONLY over a
    /// relay — whether a fresh relay link came up, or a DIRECT link dropped back to relay — is pushed
    /// here so the runtime can hole-punch it IMMEDIATELY (`punch_upgrade_peer`) instead of waiting up
    /// to a full reconcile interval. The periodic `upgrade_relay_links_via_punch` sweep stays as the
    /// backstop/retry; this only removes the up-to-one-interval latency before the FIRST punch after
    /// the relay link forms — the "почти сразу" the direct-upgrade needs. Unbounded so the sync
    /// `register_path` / `prune_path` emit sites never block; effectively bounded by peer count.
    /// См. docs/decisions/mesh-hole-punching.md.
    relay_upgrade_tx: mpsc::UnboundedSender<NodeAddress>,
    relay_upgrade_rx: AsyncMutex<mpsc::UnboundedReceiver<NodeAddress>>,
    /// Master gate for the relay→direct **upgrade** (the `ConnectionPrivacyMode::RelayOnly` privacy
    /// choice). `false` ⇒ keep every working relay path and NEVER climb to a direct P2P link, so a
    /// contact never learns our IP. The ONE source of truth, read live on every attempt: it gates the
    /// UDP hole-punch at the `punch_upgrade_peer` chokepoint (both the periodic sweep AND the
    /// event-driven signal funnel through it) and — via `allows_direct_punch`, which the runtime
    /// forwards to the rendezvous worker — the rendezvous direct-dial (UDP-direct + TCP-direct), so ONE
    /// setting gates both carriers uniformly. Default `true` (Auto/DirectOnly climb); the app flips it
    /// from the persisted privacy mode on mesh-start and on change (no restart). NB: an already-direct
    /// link is untouched — this only stops NEW upgrades. См. docs/decisions/connection-quality-ladder.md
    /// + connection-privacy-modes.md + tcp-direct-peer-fallback.md (follow-up d).
    allow_direct_punch: std::sync::atomic::AtomicBool,
    /// While `Some`: hold direct connections only with these identities (the relays) — the other half
    /// of "never direct". `allow_direct_punch` stops us climbing to direct; this also closes direct
    /// links already up and refuses a contact who dials us directly (it knows an old address of ours).
    /// Without it «Только через релей» / a pinned relay left a direct link standing (stand 2026-10-14).
    direct_only_with: SyncMutex<Option<std::collections::HashSet<NodeAddress>>>,
    /// The carrier of our own hops ([`CarrierPreference`]), as `CarrierPreference as u8`. Set at
    /// mesh-start from the profile's carrier policy.
    carrier_preference: std::sync::atomic::AtomicU8,
    /// Our own signed-announce `capabilities` bitfield (e.g. `ANNOUNCE_CAP_DHT_PROXY`) — what roles we
    /// advertise to the mesh. Set once at mesh-start from config (`set_capabilities`), read on every
    /// `announce`. Default 0 (a plain client advertises no roles). See docs/decisions/dht-over-peer.md.
    capabilities: std::sync::atomic::AtomicU8,
    /// The port of our own TCP front door, advertised in every announce (0 = no door). Set by the
    /// runtime once the door is listening (`set_door_port`).
    door_port: std::sync::atomic::AtomicU16,
    /// Our peers' public doors (from their beacons), handed over with the last carrier switch: in
    /// TCP mode `dial` reaches a peer over one when it has no LAN door (`door_for_dial`).
    door_hints: SyncMutex<HashMap<NodeAddress, SocketAddr>>,
    /// Our own private IPv4 address on the LAN, from the addresses we last announced. A peer's
    /// announced private address is its LAN door only inside this /24 (`lan_door`).
    own_lan_v4: SyncMutex<Option<std::net::Ipv4Addr>>,
    /// Per-friend consent for **friend-proxy** carriage (Axis 2): origins this node has explicitly
    /// opted in to relay/proxy for. Advertising `ANNOUNCE_CAP_FRIEND_RELAY` says "willing"; this set
    /// says "for whom". Off by default (empty) + revocable. Distinct from — and does NOT gate — the
    /// always-on mesh forwarding tier (`relay_enabled`). See docs/decisions/friend-proxy-by-consent.md.
    relay_consent: SyncMutex<std::collections::HashSet<NodeAddress>>,
    /// Friend-proxy per-origin serve log — carry timestamps per origin for the sliding-window cap
    /// ([`FRIEND_RELAY_MAX_CARRIES_PER_WINDOW`]). Node-global (not per-profile → a flooder can't reset
    /// it by switching profiles), self-bounded by the window + lazy GC. See friend-proxy-by-consent.md.
    friend_relay_serve_log: SyncMutex<HashMap<NodeAddress, Vec<u64>>>,
    /// Per-friend consent for **friend-exit** (Axis 2, Phase D): origins this node has explicitly
    /// opted in to EGRESS traffic for. Distinct from `relay_consent` — egress lends the uplink + IP,
    /// a heavier deal — so it is its own set, off by default + revocable, and advertised via the
    /// separate `ANNOUNCE_CAP_FRIEND_EXIT` bit. See docs/decisions/friend-proxy-by-consent.md.
    exit_consent: SyncMutex<std::collections::HashSet<NodeAddress>>,
    /// Friend-exit per-origin serve log — per window, the distinct destinations egressed toward and
    /// the total bytes egressed for each origin, for the distinct-destination + byte caps
    /// ([`FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW`], [`FRIEND_EXIT_MAX_BYTES_PER_WINDOW`]). Node-global
    /// (a flooder can't reset by switching profiles), self-bounded by the window + lazy GC. See
    /// friend-proxy-by-consent.md (Phase D).
    friend_exit_serve_log: SyncMutex<HashMap<NodeAddress, FriendExitWindow>>,
    /// Event-driven AutoNAT wake (Phase B). Fired when a neighbour is freshly VERIFIED — the moment a
    /// new WAN UDP verifier (the relay's UDP carrier) may have appeared — so the self-reachability loop
    /// can `request_dial_back` IMMEDIATELY instead of waiting up to a full `announce_interval`. The
    /// periodic loop stays the backstop (a UPnP mapping that maps later, a NAT remap, or a
    /// responder-role verifier whose path registers after verification). `Notify` coalesces a burst to
    /// one wake and never loses a signal (a `notify_one` with no waiter stores a permit). См.
    /// omni-mesh-runtime AutoNAT loop + docs/decisions/connectivity-upgrade-via-peer-relay.md.
    autonat_trigger: tokio::sync::Notify,
    /// Queue для новых incoming Link'ов которые мы ещё не accepted.
    incoming_links: Arc<AsyncMutex<mpsc::Receiver<Link>>>,
    incoming_links_tx: mpsc::Sender<Link>,
    /// Запущен ли уже dispatcher.
    started: SyncMutex<bool>,
    /// Background task handles чтобы можно было аккуратно shutdown'нуть.
    background_tasks: SyncMutex<Vec<JoinHandle<()>>>,
}

/// Forwarding decision for an inbound Data frame. Pure → unit-testable без
/// async Network. См. `Network::handle_data`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DataDisposition {
    /// Адресован нам — отдать в локальный Link inbox.
    DeliverLocal,
    /// Не нам, но есть next-hop — relay'им дальше (Phase β multi-hop) с
    /// декрементированным hop-бюджетом.
    Forward {
        interface: InterfaceId,
        neighbor: NeighborId,
        frame: DataFrame,
    },
    /// Не нам и не пересылаем (relay off / нет пути / hop-бюджет исчерпан /
    /// был бы возврат отправителю) — drop.
    Drop,
}

/// Решить, что делать с входящим Data-кадром. Чистая: зависит только от кадра,
/// нашего адреса, соседа-источника (и прошёл ли он challenge), path-таблицы и
/// relay-флага.
///
/// Anti-open-relay: релеим (`Forward`) **только** если сосед-источник прошёл
/// взаимный challenge (`from_neighbor_verified`). Иначе любой, кто завершил QUIC
/// (а он `AcceptAny`), мог бы гонять трафик третьим лицам через наш узел
/// (open-relay / laundering / инъекция). Доставка нам (`DeliverLocal`) не
/// гейтится тут — handshake-racing буфер в `handle_data` принимает кадры от
/// ещё-не-verified соседа отдельно.
///
/// Loop safety (multi-hop): форвардим только пока `hop_limit > 0` (декремент на
/// каждом хопе, как IP TTL) И никогда обратно тому соседу, от кого кадр пришёл.
fn decide_data_forward(
    local: &NodeAddress,
    data: &DataFrame,
    from_neighbor: &NeighborId,
    from_neighbor_verified: bool,
    path_table: &PathTable,
    relay_enabled: bool,
    now: Instant,
) -> DataDisposition {
    if data.to == *local {
        return DataDisposition::DeliverLocal;
    }
    if !relay_enabled || data.hop_limit == 0 || !from_neighbor_verified {
        return DataDisposition::Drop;
    }
    match path_table.lookup(&data.to, now) {
        Some(entry) if entry.next_hop_neighbor != *from_neighbor => DataDisposition::Forward {
            interface: entry.next_hop_interface,
            neighbor: entry.next_hop_neighbor,
            frame: DataFrame {
                from: data.from,
                to: data.to,
                hop_limit: data.hop_limit - 1,
                // Preserved across hops — dedup identity of the logical frame.
                msg_id: data.msg_id,
                payload: data.payload.clone(),
            },
        },
        _ => DataDisposition::Drop,
    }
}

/// Emit an `OMNI_NET_TRACE` diagnostic line (forwarding/relay debugging). If the env var holds a file
/// PATH, append there — reliable when stderr capture is awkward (e.g. a WMI-detached Windows daemon
/// whose `2>` redirect doesn't catch the child's stderr); if it is just set (e.g. `1`), write to
/// stderr (captured by journald for the systemd relay). Unset → no-op. Diagnostic only, never on a
/// hot path beyond the cheap env read.
fn net_trace(msg: &str) {
    let Some(val) = std::env::var_os("OMNI_NET_TRACE") else {
        return;
    };
    let val = val.to_string_lossy();
    // Epoch-ms prefix (tab-separated, like `tcp_trace`) so a FILE sink isn't blind on ordering/latency —
    // journald timestamps stderr for free, but a `2>`/file sink carries none.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if val.is_empty() || val == "1" {
        eprintln!("{ms}\t[net-trace] {msg}");
    } else {
        omni_net_primitives::trace::append_whole_line(
            std::path::Path::new(val.as_ref()),
            &format!("{ms}\t[net-trace] {msg}\n"),
        );
    }
}

/// Decide whether to admit an on-demand multi-hop origin — a peer we never dialed
/// whose Data reached us relayed by the verified neighbor `relay`. Pure → unit-
/// testable. Bounded **per-relay** (so one malicious verified relay is capped to
/// `per_relay_cap` and can't exhaust the pool for everyone) **and globally** (so
/// total on-demand state is memory-bounded). Already-known origins are re-admitted
/// idempotently. On admit, records `origin → relay`. См. external review §3.4.
fn admit_multihop_origin(
    origins: &mut HashMap<NodeAddress, NeighborId>,
    origin: NodeAddress,
    relay: &NeighborId,
    per_relay_cap: usize,
    total_cap: usize,
) -> bool {
    if origins.contains_key(&origin) {
        return true;
    }
    if origins.len() >= total_cap {
        return false;
    }
    let from_this_relay = origins.values().filter(|r| *r == relay).count();
    if from_this_relay >= per_relay_cap {
        return false;
    }
    origins.insert(origin, relay.clone());
    true
}

/// Bounded capacity-FIFO dedup window of recently-seen frame identities
/// (`origin` + `msg_id`). Drops replays and duplicate multi-hop fan-out (the same
/// logical frame arriving via two paths carries the same `msg_id`). Oldest
/// evicted when full — a short-window flood/replay guard, not the durable
/// layer's long-term reconciliation. См. external review §3.4.
struct SeenFrames {
    seen: HashSet<(NodeAddress, u64)>,
    order: VecDeque<(NodeAddress, u64)>,
    cap: usize,
}

impl SeenFrames {
    fn new(cap: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    /// Record a frame identity. Returns `true` if newly seen (caller should
    /// process it), `false` if it's a duplicate within the window (drop it).
    fn note(&mut self, key: (NodeAddress, u64)) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key);
        self.order.push_back(key);
        if self.order.len() > self.cap {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }
}

/// Inferred NAT mapping behaviour, from reflexive observations (review §4.1A).
#[derive(Debug, Clone, PartialEq, Eq)]
enum NatClass {
    /// Fewer than 2 independent observers — can't conclude yet.
    Unknown,
    /// ≥2 observers agree on our (ip, port): endpoint-independent mapping
    /// (full / restricted / port-restricted cone). External endpoint is stable
    /// → announceable + directly punchable.
    EndpointIndependent,
    /// Observers disagree on the port: address/port-dependent mapping (symmetric
    /// NAT). No single stable external endpoint → relay fallback (P2).
    AddressDependent,
}

/// Our inferred reflexive endpoint + NAT class.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReflexiveInference {
    class: NatClass,
    /// Stable external endpoint to announce, iff endpoint-independent.
    reflexive: Option<SocketAddr>,
}

/// Infer NAT mapping from neighbours' reports of the address each saw us connect
/// from. Pure → unit-testable. Needs ≥2 DISTINCT observers (different neighbours)
/// to conclude — one report can't distinguish cone from symmetric. All observers
/// agree ⇒ endpoint-independent (cone); they disagree ⇒ address/port-dependent
/// (symmetric). См. external review §4.1A.
fn infer_reflexive(observations: &HashMap<NeighborId, SocketAddr>) -> ReflexiveInference {
    if observations.len() < 2 {
        return ReflexiveInference {
            class: NatClass::Unknown,
            reflexive: None,
        };
    }
    let distinct: HashSet<SocketAddr> = observations.values().copied().collect();
    if distinct.len() == 1 {
        ReflexiveInference {
            class: NatClass::EndpointIndependent,
            reflexive: observations.values().next().copied(),
        }
    } else {
        ReflexiveInference {
            class: NatClass::AddressDependent,
            reflexive: None,
        }
    }
}

/// Двустековый сокет отдаёт адрес IPv4-пира в отображённом виде — `[::ffff:192.0.2.144]:63697`.
/// Это тот же самый адрес, но снаружи он неузнаваем: разборщик, ждущий IPv4, его не принимает, а
/// человеку он ничего не говорит. Живой экран 2026-08-31: поле «Сеть» из-за этого не опознало
/// публичный интернет-адрес, прошло мимо ветки «Интернет» и подписалось «Loopback» по нашему же
/// локальному адресу. Разворачиваем обратно в IPv4 у ИСТОЧНИКА — пир действительно там.
fn unmap_ipv4_mapped(addr: std::net::SocketAddr) -> std::net::SocketAddr {
    match addr {
        std::net::SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => std::net::SocketAddr::new(std::net::IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        std::net::SocketAddr::V4(_) => addr,
    }
}

/// Что транспорт знает о живом линке с пиром: адрес соединения и измеренный им RTT.
/// См. [`Network::peer_link_facts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLinkFacts {
    pub remote: Option<std::net::SocketAddr>,
    pub rtt: Option<Duration>,
}

impl Network {
    /// Construct a new Network. Interfaces должны быть bound уже — но
    /// dispatcher не запускается до `start()`.
    pub fn new(identity: SigningKey, interfaces: Vec<InterfaceHandle>) -> Self {
        let local_address = NodeAddress::from_pubkey(&identity.verifying_key());
        let (incoming_tx, incoming_rx) = mpsc::channel(64);
        let (durable_inbox_tx, durable_inbox_rx) = mpsc::channel(256);
        let (peer_down_tx, peer_down_rx) = mpsc::channel(64);
        let (dht_proxy_request_tx, dht_proxy_request_rx) = mpsc::channel(64);
        let (dht_boot_request_tx, dht_boot_request_rx) = mpsc::channel(64);
        let (relay_upgrade_tx, relay_upgrade_rx) = mpsc::unbounded_channel();
        let (bulk_stream_tx, bulk_stream_rx) = mpsc::channel(32);
        Self {
            identity,
            local_address,
            interfaces,
            path_table: Arc::new(PathTable::new()),
            relay_enabled: true,
            peer_book: Arc::new(SyncMutex::new(HashMap::new())),
            link_inboxes: Arc::new(SyncMutex::new(HashMap::new())),
            link_datagram_inboxes: Arc::new(SyncMutex::new(HashMap::new())),
            verified_neighbors: Arc::new(SyncMutex::new(HashMap::new())),
            bulk_tx: bulk_stream_tx,
            bulk_rx: Arc::new(AsyncMutex::new(bulk_stream_rx)),
            last_seen: Arc::new(SyncMutex::new(HashMap::new())),
            superseded_probes: SyncMutex::new(HashMap::new()),
            conn_rx_baseline: Arc::new(SyncMutex::new(HashMap::new())),
            peer_last_seen: Arc::new(SyncMutex::new(HashMap::new())),
            peer_ping_nonce: Arc::new(SyncMutex::new(HashMap::new())),
            peer_ping_sent: Arc::new(SyncMutex::new(HashMap::new())),
            multihop_origins: Arc::new(SyncMutex::new(HashMap::new())),
            seen_frames: Arc::new(SyncMutex::new(SeenFrames::new(SEEN_FRAMES_CAP))),
            seen_path_requests: Arc::new(SyncMutex::new(SeenFrames::new(SEEN_FRAMES_CAP))),
            path_request_seq: std::sync::atomic::AtomicU64::new(current_unix_ms() as u64),
            path_control_quota: Arc::new(SyncMutex::new(HashMap::new())),
            reflexive_observations: Arc::new(SyncMutex::new(HashMap::new())),
            peer_paths: Arc::new(SyncMutex::new(HashMap::new())),
            peer_doors: Arc::new(SyncMutex::new(HashMap::new())),
            migrate_back_backoff: Arc::new(SyncMutex::new(HashMap::new())),
            reachability: Arc::new(SyncMutex::new(ReachabilityTracker::default())),
            pending_tcp_confirm: Arc::new(SyncMutex::new(None)),
            confirmed_tcp_address: Arc::new(SyncMutex::new(None)),
            durable_store: Arc::new(SyncMutex::new(DurableStore::new())),
            durable_reconcile: Arc::new(SyncMutex::new(ReconcileEngine::new())),
            durable_handled: SyncMutex::new(HashSet::new()),
            group_subscriptions: Arc::new(SyncMutex::new(HashMap::new())),
            peer_interests: Arc::new(SyncMutex::new(HashMap::new())),
            durable_inbox_tx,
            durable_inbox_rx: AsyncMutex::new(durable_inbox_rx),
            peer_down_tx,
            peer_down_rx: AsyncMutex::new(peer_down_rx),
            dht_proxy_request_tx,
            dht_proxy_request_rx: AsyncMutex::new(dht_proxy_request_rx),
            dht_proxy_pending: Arc::new(SyncMutex::new(HashMap::new())),
            dht_boot_request_tx,
            dht_boot_request_rx: AsyncMutex::new(dht_boot_request_rx),
            dht_boot_pending: Arc::new(SyncMutex::new(HashMap::new())),
            relay_card: SyncMutex::new(None),
            relay_card_pending: Arc::new(SyncMutex::new(HashMap::new())),
            relay_upgrade_tx,
            relay_upgrade_rx: AsyncMutex::new(relay_upgrade_rx),
            allow_direct_punch: std::sync::atomic::AtomicBool::new(true),
            direct_only_with: SyncMutex::new(None),
            carrier_preference: std::sync::atomic::AtomicU8::new(CarrierPreference::Auto as u8),
            capabilities: std::sync::atomic::AtomicU8::new(0),
            door_port: std::sync::atomic::AtomicU16::new(0),
            door_hints: SyncMutex::new(HashMap::new()),
            own_lan_v4: SyncMutex::new(None),
            relay_consent: SyncMutex::new(std::collections::HashSet::new()),
            friend_relay_serve_log: SyncMutex::new(HashMap::new()),
            exit_consent: SyncMutex::new(std::collections::HashSet::new()),
            friend_exit_serve_log: SyncMutex::new(HashMap::new()),
            autonat_trigger: tokio::sync::Notify::new(),
            outbound_challenges: Arc::new(SyncMutex::new(HashMap::new())),
            pending_data: Arc::new(SyncMutex::new(HashMap::new())),
            incoming_links: Arc::new(AsyncMutex::new(incoming_rx)),
            incoming_links_tx: incoming_tx,
            started: SyncMutex::new(false),
            background_tasks: SyncMutex::new(Vec::new()),
        }
    }

    pub fn local_address(&self) -> NodeAddress {
        self.local_address
    }

    pub fn interface_count(&self) -> usize {
        self.interfaces.len()
    }

    pub fn path_table(&self) -> Arc<PathTable> {
        Arc::clone(&self.path_table)
    }

    /// Start dispatcher tasks для каждого interface'а. Idempotent — повторный
    /// вызов no-op.
    pub fn start(self: &Arc<Self>) {
        let mut started = self.started.lock();
        if *started {
            return;
        }
        *started = true;

        for iface in &self.interfaces {
            let net = Arc::clone(self);
            let iface_for_frames = Arc::clone(iface);
            let handle = tokio::spawn(async move {
                net.dispatcher_loop(iface_for_frames).await;
            });
            self.background_tasks.lock().push(handle);

            // Datagram-capable interfaces get a second pump for the media plane.
            if iface.supports_datagrams() {
                let net = Arc::clone(self);
                let iface_for_datagrams = Arc::clone(iface);
                let handle = tokio::spawn(async move {
                    net.datagram_dispatcher_loop(iface_for_datagrams).await;
                });
                self.background_tasks.lock().push(handle);
            }

            // Third pump: connection-close events → prune the dead path proactively
            // (P2 cleanup-on-close). Default recv_closed never resolves, so on
            // interfaces without close detection this task just parks.
            let net = Arc::clone(self);
            let iface_for_closes = Arc::clone(iface);
            let handle = tokio::spawn(async move {
                net.close_dispatcher_loop(iface_for_closes).await;
            });
            self.background_tasks.lock().push(handle);

            // Fourth pump: inbound bulk streams (attachment transfer). Default
            // `accept_bulk_stream` never resolves, so this parks on interfaces without a bulk
            // plane, exactly like the close pump.
            let net = Arc::clone(self);
            let iface_for_bulk = Arc::clone(iface);
            let handle = tokio::spawn(async move {
                net.bulk_dispatcher_loop(iface_for_bulk).await;
            });
            self.background_tasks.lock().push(handle);
        }

        // Liveness pump (one, not per-interface): ping quiet verified neighbours and
        // prune the long-silent ones, so link-DOWN is detected fast (mesh
        // ping-liveness). See `liveness_loop` + `LIVENESS_*`.
        let net = Arc::clone(self);
        let handle = tokio::spawn(async move {
            net.liveness_loop().await;
        });
        self.background_tasks.lock().push(handle);
    }

    /// Ping verified neighbours that have gone quiet and prune those silent past
    /// `LIVENESS_DEAD_AFTER` (→ `peer_down`). App-level liveness independent of the
    /// QUIC idle timeout — a live peer answers `Pong` (or any frame) so it never
    /// churns, while a vanished peer is declared down within ~`LIVENESS_DEAD_AFTER`
    /// and the next message falls through to durable without a process restart.
    async fn liveness_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(LIVENESS_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Connection-level liveness. A bulk attachment stream (and any non-frame traffic) rides a
            // peer's quinn connection but never reaches the mesh-frame path that refreshes `last_seen`,
            // so a peer quiet on FRAMES while a large download pours in over the SAME connection looks
            // idle → `liveness_plan` prunes its direct path → `has_link` goes false → chat is blocked
            // (`no_mesh_link`) on a demonstrably-live link. Refresh `last_seen` for any path-neighbour
            // whose connection RX grew since the last tick; a dead peer's RX doesn't grow, so it still
            // prunes at `LIVENESS_DEAD_AFTER`. See docs/bugs/chat-gated-on-stale-has-link.md.
            {
                let candidates: Vec<(NeighborId, InterfaceHandle)> = {
                    let paths = self.peer_paths.lock();
                    let mut m: HashMap<NeighborId, InterfaceHandle> = HashMap::new();
                    for set in paths.values() {
                        for path in &set.lock().paths {
                            m.entry(path.neighbor.clone())
                                .or_insert_with(|| path.iface.clone());
                        }
                    }
                    m.into_iter().collect()
                };
                let current: Vec<(NeighborId, u64)> = candidates
                    .iter()
                    .filter_map(|(neighbor, iface)| {
                        iface
                            .connection_rx_bytes(neighbor)
                            .map(|rx| (neighbor.clone(), rx))
                    })
                    .collect();
                let mut baseline = self.conn_rx_baseline.lock();
                let active = Self::rx_active_neighbours(&current, &baseline);
                baseline.clear();
                for (neighbor, rx) in &current {
                    baseline.insert(neighbor.clone(), *rx);
                }
                drop(baseline);
                if !active.is_empty() {
                    let now = Instant::now();
                    let mut seen = self.last_seen.lock();
                    for neighbor in &active {
                        seen.insert(neighbor.clone(), now);
                    }
                }
            }
            let now = Instant::now();
            let (to_ping, to_prune) = self.liveness_plan(now);
            // Say what this tick decided, in the SAME sink the app writes to. Until now the direct-
            // neighbour half of liveness was silent (only the relay half had `net_trace`, and that
            // needs its own env var and lands in a different file), so a link that kept dropping
            // could not be told apart from three different causes: pings never sent, pings sent and
            // unanswered, or answered and pruned anyway. On a flapping mobile path that ambiguity
            // cost a whole evening of guessing. Quiet when there is nothing to report.
            if !to_ping.is_empty() || !to_prune.is_empty() {
                let describe = |entries: &[NeighborId]| -> String {
                    let seen = self.last_seen.lock();
                    entries
                        .iter()
                        .take(4)
                        .map(|neighbor| {
                            let idle_ms = seen
                                .get(neighbor)
                                .map(|last| now.saturating_duration_since(*last).as_millis())
                                .unwrap_or(0);
                            format!("{neighbor:?}@{idle_ms}ms")
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let ping_ids: Vec<NeighborId> =
                    to_ping.iter().map(|(neighbor, _)| neighbor.clone()).collect();
                crate::tcp_transport::tcp_trace(
                    "mesh/liveness",
                    &format!(
                        "ping={} prune={} dead_after={}s pinging=[{}] pruning=[{}]",
                        to_ping.len(),
                        to_prune.len(),
                        LIVENESS_DEAD_AFTER.as_secs(),
                        describe(&ping_ids),
                        describe(&to_prune),
                    ),
                );
            }
            let ping = NetworkFrame::Ping.encode_to_wire();
            for (neighbor, iface) in to_ping {
                // Fire-and-forget: we judge liveness by the absence of an inbound
                // Pong, not by whether this send succeeds (a dead conn may buffer).
                let _ = iface.send_frame(&neighbor, &ping).await;
            }
            for neighbor in to_prune {
                self.prune_path(&neighbor);
            }
            let superseded = self.superseded_plan(now);
            if !superseded.is_empty() {
                crate::tcp_transport::tcp_trace(
                    "mesh/superseded-prune",
                    &format!("silent after the probe: {superseded:?}"),
                );
            }
            for neighbor in superseded {
                self.prune_path(&neighbor);
            }
            // End-to-end relay-peer liveness: a peer reached ONLY over a relay is never our path
            // neighbour, so the neighbour pass above can't see it die. Probe end-to-end-quiet relay
            // peers with a routed `PeerPing` (they answer `PeerPong`), and prune those silent past the
            // dead window so a dead relay-reachable peer stops reading as "online via relay". See
            // docs/bugs/relay-presence-false-online-dead-peer.md.
            let (relay_to_probe, relay_to_prune) = self.relay_peer_liveness_plan(Instant::now());
            for (peer, neighbor, out) in relay_to_probe {
                // Fresh unguessable nonce per probe, remembered so only THIS probe's pong refreshes
                // liveness — a stale/duplicate/relay-buffered pong (old nonce) is ignored (the flap fix).
                let nonce = OsRng.next_u64();
                self.peer_ping_nonce.lock().insert(peer, nonce);
                self.remember_peer_ping(peer, nonce, Instant::now());
                let probe = NetworkFrame::PeerPing(PeerPingFrame {
                    from: self.local_address,
                    to: peer,
                    nonce,
                    hop_limit: PEER_PING_HOP_LIMIT,
                })
                .encode_to_wire();
                let _ = out.send_frame(&neighbor, &probe).await;
            }
            for peer in relay_to_prune {
                self.prune_relay_only_peer(peer);
            }
        }
    }

    /// The peer's direct paths other than `fresh`, recorded as superseded at `now` (an entry already
    /// under probe keeps its original instant). Returns them so the caller can ping each.
    fn mark_superseded_direct_paths(
        &self,
        peer: NodeAddress,
        fresh: &NeighborId,
        now: Instant,
    ) -> Vec<(NeighborId, InterfaceHandle)> {
        let older: Vec<(NeighborId, InterfaceHandle)> = {
            let paths = self.peer_paths.lock();
            let Some(set) = paths.get(&peer) else {
                return Vec::new();
            };
            let guard = set.lock();
            guard
                .paths
                .iter()
                .filter(|p| matches!(p.quality, PathQuality::Direct) && &p.neighbor != fresh)
                .map(|p| (p.neighbor.clone(), p.iface.clone()))
                .collect()
        };
        let mut probes = self.superseded_probes.lock();
        for (neighbor, _) in &older {
            probes.entry(neighbor.clone()).or_insert(now);
        }
        older
    }

    /// A peer just verified over `fresh`: ping its older direct paths now, so the next liveness
    /// tick can tell a live spare from the dead leftover of a restart. See
    /// `SUPERSEDED_PROBE_WINDOW`.
    async fn probe_superseded_direct_paths(&self, peer: NodeAddress, fresh: &NeighborId) {
        let older = self.mark_superseded_direct_paths(peer, fresh, Instant::now());
        if older.is_empty() {
            return;
        }
        crate::tcp_transport::tcp_trace(
            "mesh/superseded-probe",
            &format!("peer={peer:?} fresh={fresh:?} probing={}", older.len()),
        );
        let ping = NetworkFrame::Ping.encode_to_wire();
        for (neighbor, iface) in older {
            let _ = iface.send_frame(&neighbor, &ping).await;
        }
    }

    /// Settle the superseded probes at `now`: a path heard from after its ping is alive and leaves
    /// the probe set; one still silent `SUPERSEDED_PROBE_WINDOW` after the ping is returned for
    /// pruning; one already gone is forgotten.
    fn superseded_plan(&self, now: Instant) -> Vec<NeighborId> {
        let seen = self.last_seen.lock();
        let mut probes = self.superseded_probes.lock();
        let mut dead = Vec::new();
        probes.retain(|neighbor, probed_at| {
            let Some(last) = seen.get(neighbor) else {
                return false; // already pruned
            };
            if *last > *probed_at {
                return false; // answered
            }
            if now.saturating_duration_since(*probed_at) >= SUPERSEDED_PROBE_WINDOW {
                dead.push(neighbor.clone());
                return false;
            }
            true
        });
        dead
    }

    /// Pure-ish liveness classification at instant `now`: which verified path-
    /// neighbours to ping (quiet for `LIVENESS_PING_AFTER`) and which to prune
    /// (silent past `LIVENESS_DEAD_AFTER`). Reads `peer_paths` + `last_seen` only;
    /// no async, no side effects — so the policy is unit-testable with an injected
    /// `now`. The two locks are taken in sequence (never nested), so this can't
    /// invert lock order against `prune_path`.
    fn liveness_plan(
        &self,
        now: Instant,
    ) -> (Vec<(NeighborId, InterfaceHandle)>, Vec<NeighborId>) {
        // 1. Distinct path-neighbours + an interface to reach each (a neighbour may
        //    appear in several peers' PathSets — e.g. a shared relay).
        let mut candidates: HashMap<NeighborId, InterfaceHandle> = HashMap::new();
        {
            let paths = self.peer_paths.lock();
            for set in paths.values() {
                for path in &set.lock().paths {
                    candidates
                        .entry(path.neighbor.clone())
                        .or_insert_with(|| path.iface.clone());
                }
            }
        }
        // 2. Classify by idle time (last_seen lock only).
        let mut to_ping: Vec<(NeighborId, InterfaceHandle)> = Vec::new();
        let mut to_prune: Vec<NeighborId> = Vec::new();
        {
            let seen = self.last_seen.lock();
            for (neighbor, iface) in candidates {
                let Some(last) = seen.get(&neighbor).copied() else {
                    continue; // not yet tracked (just added) — alive by default
                };
                let idle = now.saturating_duration_since(last);
                // A QuicOverTcp carrier (direct-TCP door / TCP-relay) has its OWN liveness — the
                // byte-pipe's break+reconnect detection, plus path removal on send-failure — so it must
                // NOT be pruned on inbound-idle. On an asymmetric mobile link the peer answers over UDP,
                // so the TCP carrier legitimately sees no inbound yet must persist; pruning it at
                // DEAD_AFTER caused the churn (prune@17s → re-dial → a FRESH quinn peer per bridge
                // reconnect → never a stable data-carrying connection, so laptop→PC never delivered).
                // We DO still ping it when quiet — that keeps the byte-pipe warm so the carrier's
                // idle-reset can't tear it down. See docs/decisions/tcp-direct-peer-fallback.md.
                let is_tcp_carrier = iface.kind() == InterfaceKind::QuicOverTcp;
                if idle >= LIVENESS_DEAD_AFTER && !is_tcp_carrier {
                    to_prune.push(neighbor);
                } else if idle >= LIVENESS_PING_AFTER {
                    to_ping.push((neighbor, iface));
                }
            }
        }
        (to_ping, to_prune)
    }

    /// Given each path-neighbour's CURRENT connection RX-byte count and the PREVIOUS tick's
    /// `baseline`, return the neighbours whose connection received new bytes since the last tick —
    /// i.e. the connection is alive (carrying a bulk stream or any traffic) and `last_seen` should be
    /// refreshed so mesh-frame idle alone doesn't prune it. Pure (baseline read-only); the caller
    /// stores the new counts. See docs/bugs/chat-gated-on-stale-has-link.md.
    fn rx_active_neighbours(
        current: &[(NeighborId, u64)],
        baseline: &HashMap<NeighborId, u64>,
    ) -> Vec<NeighborId> {
        current
            .iter()
            .filter(|(neighbor, rx)| baseline.get(neighbor).is_some_and(|&prev| *rx > prev))
            .map(|(neighbor, _)| neighbor.clone())
            .collect()
    }

    /// Plan end-to-end liveness for RELAY-ONLY peers — peers whose best path is `Relay` with no Direct
    /// link, so the per-neighbour `liveness_plan` above (which only sees the relay neighbour) can never
    /// tell when they die. Returns the peers to PROBE (end-to-end-quiet ≥ `LIVENESS_PING_AFTER`, with
    /// the best-path neighbour + iface to route a `PeerPing` over — i.e. toward the relay) and the
    /// peers to PRUNE (end-to-end-silent ≥ `RELAY_PEER_DEAD_AFTER`). Reads `peer_paths` +
    /// `peer_last_seen` only; `now` injected for unit-testability. A peer with no `peer_last_seen`
    /// entry is probed but never pruned (alive by default — mirrors `liveness_plan`). See
    /// docs/bugs/relay-presence-false-online-dead-peer.md.
    #[allow(clippy::type_complexity)]
    fn relay_peer_liveness_plan(
        &self,
        now: Instant,
    ) -> (Vec<(NodeAddress, NeighborId, InterfaceHandle)>, Vec<NodeAddress>) {
        // 1. relay-only peers + the best-path neighbour/iface (the relay) to route a probe over.
        let relay_only: Vec<(NodeAddress, NeighborId, InterfaceHandle)> = {
            let map = self.peer_paths.lock();
            map.iter()
                .filter(|(peer, _)| **peer != self.local_address)
                .filter_map(|(peer, set)| {
                    let guard = set.lock();
                    let best = guard.best()?;
                    // A Direct path anywhere ⇒ not relay-only ⇒ neighbour liveness already covers it.
                    if guard.paths.iter().any(|p| matches!(p.quality, PathQuality::Direct)) {
                        return None;
                    }
                    Some((*peer, best.neighbor.clone(), best.iface.clone()))
                })
                .collect()
        };
        // 2. classify by end-to-end idle time (peer_last_seen lock only).
        let mut to_probe: Vec<(NodeAddress, NeighborId, InterfaceHandle)> = Vec::new();
        let mut to_prune: Vec<NodeAddress> = Vec::new();
        {
            let seen = self.peer_last_seen.lock();
            // A probe is out and unanswered (a prune clears it, a pong answers it).
            let probed = self.peer_ping_nonce.lock().keys().copied().collect::<std::collections::HashSet<_>>();
            for (peer, neighbor, iface) in relay_only {
                match seen.get(&peer) {
                    None => {
                        net_trace(&format!("RELAY_LIVENESS peer={peer:?} idle=none -> probe"));
                        to_probe.push((peer, neighbor, iface)) // not tracked yet → probe, never prune
                    }
                    // Silent past the dead window but not asked since its path came back: ask first.
                    // Pruning at once made a peer that reaches us only through a relay — and never
                    // directly, because we keep no direct link with it (a pinned relay) — «down» again
                    // the moment its frames re-registered the path, every few seconds, for good: it was
                    // never probed, so it could never prove itself (stand 2026-10-15). A dead one stays
                    // dead: the probe goes unanswered and the next tick prunes it.
                    Some(last)
                        if now.saturating_duration_since(*last) >= RELAY_PEER_DEAD_AFTER && !probed.contains(&peer) =>
                    {
                        net_trace(&format!("RELAY_LIVENESS peer={peer:?} silent, not asked yet -> probe"));
                        to_probe.push((peer, neighbor, iface));
                    }
                    Some(last) => {
                        let idle = now.saturating_duration_since(*last);
                        if idle >= RELAY_PEER_DEAD_AFTER {
                            net_trace(&format!(
                                "RELAY_LIVENESS peer={peer:?} idle_ms={} -> PRUNE",
                                idle.as_millis()
                            ));
                            to_prune.push(peer);
                        } else if idle >= LIVENESS_PING_AFTER {
                            to_probe.push((peer, neighbor, iface));
                        }
                    }
                }
            }
        }
        (to_probe, to_prune)
    }

    /// Prune a relay-only peer that went end-to-end silent: drop its (relay) paths and surface
    /// `peer_down` so the app clears stale session state and presence drops to offline. The peer
    /// returns only when a fresh genuine link forms (it re-announces, we re-dial, and a nonce-matched
    /// `PeerPong` re-proves it). Mirrors `prune_path`'s peer_down emission, but keyed by the END-peer.
    ///
    /// We DELIBERATELY keep the (now stale) `peer_last_seen` entry: if it were removed, a re-registered
    /// relay path's `or_insert` grace would re-seed it FRESH → the peer reads online again until the
    /// next dead-window elapses → FLAP. Leaving the stale timestamp makes a re-register's `or_insert` a
    /// no-op, so the peer is immediately re-pruned and `peer_link_quality` stays None. The stale nonce
    /// IS cleared (a re-probe sets a new one). Tiny bounded leak in `peer_last_seen` (the peer rotates /
    /// GC later) — acceptable. See docs/bugs/relay-presence-false-online-dead-peer.md.
    fn prune_relay_only_peer(&self, peer: NodeAddress) {
        self.peer_paths.lock().remove(&peer);
        self.peer_ping_nonce.lock().remove(&peer);
        self.peer_ping_sent.lock().remove(&peer);
        let _ = self.peer_down_tx.try_send(peer);
    }

    /// Broadcast our announce через все interfaces. Используется как
    /// periodic heartbeat (caller вызывает в timer'е) и одноразово после
    /// `start()` чтобы соседи нас увидели сразу.
    pub async fn announce(&self, advertised_addresses: Vec<std::net::SocketAddr>) -> io::Result<()> {
        let now_ms = current_unix_ms();
        *self.own_lan_v4.lock() = advertised_addresses.iter().find_map(|addr| match addr.ip() {
            IpAddr::V4(v4) if !is_enumerable_wan(addr) && !v4.is_loopback() => Some(v4),
            _ => None,
        });
        // Augment with our learned reflexive endpoint (external NAT-mapped addr),
        // but only once AutoNAT has CONFIRMED it is actually dialable (P2 / review
        // §4.1A & §4.1F): P1 infers the address from neighbour reports, AutoNAT
        // proves a fresh peer can reach it. Advertising an unconfirmed/undialable
        // address just wastes peers' direct-dial attempts — relay + multicast reach
        // us meanwhile, and once a dial-back confirms we begin advertising direct.
        let reflexive = infer_reflexive(&self.reflexive_observations.lock()).reflexive;
        let addresses = select_advertised_addresses(
            advertised_addresses,
            reflexive,
            self.reachability_state() == ReachabilityState::Reachable,
        );
        let announce = Announce::sign_new_with_capabilities(
            &self.identity,
            addresses,
            now_ms,
            DEFAULT_ANNOUNCE_TTL_SECONDS,
            self.capabilities.load(std::sync::atomic::Ordering::Relaxed),
            self.door_port.load(std::sync::atomic::Ordering::Relaxed),
        );
        let frame = NetworkFrame::Announce(announce);
        let bytes = frame.encode_to_wire();
        for iface in &self.interfaces {
            // Best-effort: один сломавшийся interface не должен блокировать остальных.
            let _ = iface.broadcast_frame(&bytes).await;
        }
        Ok(())
    }

    /// Connection-coalescing helper (#61): if we already hold a live, verified DIRECT connection to
    /// `destination`, return a Link reusing it; else `None`. "Live" = a `Direct` path whose
    /// neighbour is still in `verified_neighbors` (liveness prunes the dead). The Link shares the
    /// existing `PathSet` (sends ride the live connection) and gets a DETACHED inbox — the canonical
    /// identity-keyed inbox stays with the app's existing pump (`register_routed_link_channels`
    /// reuses it via `or_insert`), so reception is never orphaned and no duplicate QUIC connection is
    /// opened. Locks are taken sequentially (peer_paths → PathSet → verified_neighbors, each released
    /// before the next) so this never inverts lock order against `prune_path`.
    fn reuse_live_direct_link(&self, destination: NodeAddress) -> Option<Link> {
        let set = self.peer_paths.lock().get(&destination)?.clone();
        let direct_neighbors: Vec<NeighborId> = {
            let guard = set.lock();
            guard
                .paths
                .iter()
                .filter(|p| matches!(p.quality, PathQuality::Direct))
                .map(|p| p.neighbor.clone())
                .collect()
        };
        if direct_neighbors.is_empty() {
            return None;
        }
        let live = {
            let verified = self.verified_neighbors.lock();
            direct_neighbors.iter().any(|n| verified.contains_key(n))
        };
        if !live {
            return None;
        }
        let channels = self.register_routed_link_channels(destination);
        Some(Link {
            local: self.local_address,
            destination,
            paths: set,
            inbox: AsyncMutex::new(channels.inbox_rx),
            datagram_inbox: AsyncMutex::new(channels.datagram_rx),
        })
    }

    /// Establish a verified Link to `destination`.
    ///
    /// 1. Look up the peer's advertised addresses (from a verified announce).
    /// 2. For each connection-capable interface × advertised address, try to
    ///    connect (QUIC handshake — encrypted but not yet authenticated).
    /// 3. Run the identity challenge as initiator: send a nonce, await a
    ///    signed response proving the peer controls `destination`'s key.
    ///    The peer challenges us back (mutual auth) — we answer that from the
    ///    dispatcher in parallel.
    /// 4. On success, return a Link whose data plane is this QUIC connection.
    ///
    /// Requires `start()` to have been called (the dispatcher delivers the
    /// challenge response that unblocks this call).
    pub async fn dial(&self, destination: NodeAddress) -> Result<Link, NetworkError> {
        if destination == self.local_address {
            return Err(NetworkError::DialSelf);
        }

        // Connection coalescing (#61): if we ALREADY have a live, verified DIRECT connection to
        // `destination`, reuse it instead of opening a duplicate. A re-dial (the autoconnect /
        // maintenance sweep, or a bootstrap relay re-dial) otherwise piles up parallel QUIC
        // connections; the old ones go stale and a relay can end up forwarding over one while the peer
        // reads another — the live "first message lands, later ones don't" durability churn. When the
        // existing connection has actually died, liveness has pruned it from `verified_neighbors`, so
        // this returns `None` and we re-dial a fresh one (correct on a mobile NAT remap). The reused
        // Link shares the existing `PathSet` (sends ride the live connection) and gets a detached
        // inbox — the canonical identity-keyed inbox stays with the app's existing pump (see
        // `register_routed_link_channels`), so reception is never orphaned.
        if let Some(link) = self.reuse_live_direct_link(destination) {
            return Ok(link);
        }

        // A peer we keep no direct link with (`keep_direct_only_with`) is not dialed directly at all —
        // straight to a route through a relay below. Dialing it only to refuse our own link after the
        // handshake spun a connection a few seconds apart (stand 2026-10-14).
        let mut addresses = if self.direct_refused(&destination) {
            Vec::new()
        } else {
            let book = self.peer_book.lock();
            book.get(&destination)
                .map(|record| record.advertised_addresses.clone())
                .unwrap_or_default()
        };
        // TCP for our hops: the raw-UDP addresses above are refused, so the peer is dialed at its
        // door — on our LAN the only one it has without UPnP.
        if self.carrier_preference() == CarrierPreference::Tcp && !self.direct_refused(&destination) {
            for door in self.doors_for_dial(&destination).into_iter().rev() {
                if !addresses.contains(&door) && self.register_door_route(destination, door).await {
                    addresses.insert(0, door);
                }
            }
        }
        // Direct path: if we know advertised addresses for `destination`, connect
        // and challenge it as a directly-reachable neighbor. If we don't (empty),
        // skip straight to the multi-hop fallback below.
        //
        // R1a dual-carrier: when a native UDP carrier is present, the TLS-over-TCP relay carrier
        // reaches ONLY its relay (dialed via the pinned `dial_over_carrier`), never a peer's
        // advertised endpoint — a direct dial over it would stall on a doomed QUIC-over-TCP handshake
        // before the UDP/multihop fallback. Skip it for direct dials. When the TCP relay carrier is
        // the SOLE interface it is NOT skipped — the relay is dialed through here.
        let skip_tcp_relay_for_direct = self
            .interfaces
            .iter()
            .any(|i| i.kind() == InterfaceKind::QuicOverUdp);
        let tcp_hops = self.carrier_preference() == CarrierPreference::Tcp;
        // Every (interface, endpoint) pair, in preference order: interface-major, the peer-book order
        // within (fresh beacon first, forwards before the STUN address). The order is a preference,
        // not a queue — `race_connect` overlaps the attempts, so a dead endpoint costs one
        // `DIAL_ATTEMPT_DELAY` rather than a whole handshake timeout.
        let mut candidates: VecDeque<(InterfaceHandle, String)> = self
            .interfaces
            .iter()
            .filter(|iface| {
                !(skip_tcp_relay_for_direct && iface.kind() == InterfaceKind::QuicOverTcp)
            })
            .flat_map(|iface| {
                // TCP for our hops: over the native socket only to an address its mux routes over a
                // TCP door (`dial_over_door` registers it first) — never raw UDP.
                let tcp_only = tcp_hops && iface.kind() == InterfaceKind::QuicOverUdp;
                addresses
                    .iter()
                    .filter(move |addr| !tcp_only || iface.peer_on_door(**addr))
                    .map(move |addr| (Arc::clone(iface), addr.to_string()))
            })
            .collect();
        let mut last_error: Option<NetworkError> = None;
        while let Some((iface, neighbor)) = race_connect(&mut candidates).await {
            // The inboxes + link entries were created at the verification point
            // (handle_challenge_response) so any Data that raced ahead is already buffered.
            match self
                .challenge_and_register_direct(&iface, neighbor, destination)
                .await
            {
                Ok(link) => return Ok(link),
                // Connected, but not to `destination` (a stale endpoint someone else holds now) or the
                // challenge got lost — remember the error and race the rest.
                Err(error) => last_error = Some(error),
            }
        }

        // Reactive path discovery (R3): no direct connection worked. Re-flood a PathRequest when we
        // have no known route OR the cached route's next-hop is no longer a verified neighbour — a
        // relay route that aged out (or whose relay link dropped) leaves a STALE path_table entry, and
        // the old `is_none()` gate meant `dial` would NOT re-discover it, so a relay route was never
        // re-established and chat/media stopped crossing the relay after the pairing burst. Flooding
        // when the route is unusable lets a re-dial (e.g. the autoconnect sweep) rebuild the relay
        // route so the link stays durable. Gated by `relay_enabled`. See omni-network-model.md (D6) +
        // docs/bugs/relay-pairing-approval-gap.md.
        let needs_path_discovery = match self.path_table.lookup(&destination, Instant::now()) {
            None => true,
            Some(entry) => !self
                .verified_neighbors
                .lock()
                .contains_key(&entry.next_hop_neighbor),
        };
        if self.relay_enabled && needs_path_discovery {
            self.discover_path(destination).await;
        }

        // Multi-hop fallback: no direct connection worked (or none was known),
        // but the routing table may now have a next-hop toward `destination` over an
        // already-verified neighbor (seeded by announce or by the discovery above). Route over it
        // without challenging `destination` — it isn't our neighbor; its identity is E2E-verified
        // above mesh. См. ADR mesh-multihop-identity-delivery.md.
        if let Some(link) = self.dial_multihop(destination) {
            return Ok(link);
        }

        Err(last_error.unwrap_or(NetworkError::NoPathToDestination(destination)))
    }

    /// Dial `destination` at caller-supplied `endpoints` learned out-of-band (e.g. via
    /// serverless rendezvous), not from a multicast announce. Merges the endpoints into
    /// the address book, then runs the normal [`dial`](Self::dial) — identity is still
    /// proven by the mutual challenge against `destination`, so a wrong endpoint just
    /// fails to verify. This is the cross-network entry point: announces are
    /// LAN-multicast-local, so a peer on another network is only reachable once its
    /// endpoint arrives this way. See docs/decisions/serverless-bootstrap-rendezvous.md.
    pub async fn dial_at_endpoints(
        &self,
        destination: NodeAddress,
        endpoints: Vec<SocketAddr>,
    ) -> Result<Link, NetworkError> {
        {
            let mut book = self.peer_book.lock();
            let record = book.entry(destination).or_insert_with(|| PeerRecord {
                pubkey: None,
                advertised_addresses: Vec::new(),
                capabilities: 0,
                door_port: 0,
                heard_direct: false,
            });
            merge_rendezvous_endpoints(&mut record.advertised_addresses, endpoints);
        }
        self.dial(destination).await
    }

    /// Run the identity challenge against `destination` over `iface`'s freshly-connected `neighbor`
    /// and, on success, register a `Direct` path + return the verified Link. The direct-dial core
    /// shared by [`dial`](Self::dial) (first-success across every interface) and
    /// [`dial_over_carrier`](Self::dial_over_carrier) (one pinned carrier), so the
    /// challenge → register → Link-build stay in lock-step. On failure the half-registered outbound
    /// challenge is cleaned up. Does NOT coalesce onto an existing connection — the carrier-pinned
    /// caller deliberately wants a fresh per-carrier link.
    async fn challenge_and_register_direct(
        &self,
        iface: &InterfaceHandle,
        neighbor: NeighborId,
        destination: NodeAddress,
    ) -> Result<Link, NetworkError> {
        match self.run_dialer_challenge(iface, &neighbor, destination).await {
            Ok(channels) => {
                let paths = self.register_path(
                    destination,
                    neighbor.clone(),
                    Arc::clone(iface),
                    PathQuality::Direct,
                    true, // we dialed this connection (outbound)
                );
                self.probe_superseded_direct_paths(destination, &neighbor).await;
                Ok(Link {
                    local: self.local_address,
                    destination,
                    paths,
                    inbox: AsyncMutex::new(channels.inbox_rx),
                    datagram_inbox: AsyncMutex::new(channels.datagram_rx),
                })
            }
            Err(error) => {
                // Clean up any half-registered challenge so a retry isn't blocked.
                self.outbound_challenges.lock().remove(&neighbor);
                Err(error)
            }
        }
    }

    /// Establish a verified DIRECT link to `destination` over ONE SPECIFIC carrier, dialing it at `endpoint`
    /// — an interface-specific string, as `connect_endpoint` takes it. Two users: the R1a
    /// connection-quality ladder (the carrier's OWN registry address: the UDP relay endpoint for
    /// `QuicOverUdp`, the TLS-over-TCP bridge for `QuicOverTcp`). Unlike [`dial`](Self::dial) this
    /// (a) PINS the carrier (selects the interface by `carrier` kind) rather than taking the first
    /// interface that connects, and (b) does NOT coalesce onto a link over the OTHER carrier — so a
    /// node can hold an A↔R-TCP (rung-0 base) AND an A↔R-UDP (rung-1) link to the SAME relay at once.
    ///
    /// The two carrier links MUST land DISTINCT `NeighborId`s — which they do precisely because each
    /// is dialed at its own endpoint (`:8443` TCP vs `:443` UDP). Were they to collide (e.g. both
    /// dialed at `:443`) the relay's `PathSet` (keyed by neighbour) would collapse them into one
    /// Direct path — the climber would have nothing to add — AND pruning one carrier on its death
    /// would tear the other down too (regressing the rung-0 self-heal). The climber
    /// (`upgrade_relay_carriers`) then registers each relayed peer a path over both A↔R carriers.
    ///
    /// Returns the verified Link. The dual-carrier autoconnect keeps the TCP base as the relay's
    /// canonical link and DISCARDS the UDP one — only the Direct PATH it registered matters; the
    /// identity-keyed inbox is reused, never orphaned (same pattern as `upgrade_relay_links_via_punch`).
    /// See docs/decisions/connection-quality-ladder.md.
    pub async fn dial_over_carrier(
        &self,
        destination: NodeAddress,
        endpoint: &str,
        carrier: InterfaceKind,
    ) -> Result<Link, NetworkError> {
        if destination == self.local_address {
            return Err(NetworkError::DialSelf);
        }
        let iface = self
            .interfaces
            .iter()
            .find(|i| i.kind() == carrier)
            .cloned()
            .ok_or(NetworkError::NoPathToDestination(destination))?;
        let neighbor = iface
            .connect_endpoint(endpoint)
            .await
            .map_err(|_| NetworkError::NoPathToDestination(destination))?;
        self.challenge_and_register_direct(&iface, neighbor, destination)
            .await
    }


    /// R1a — do we already hold a path to `peer` over an interface of `carrier` kind? Lets the
    /// dual-carrier autoconnect skip re-dialing (and re-challenging) a carrier link it already has.
    pub fn has_carrier_path(&self, peer: &NodeAddress, carrier: InterfaceKind) -> bool {
        self.peer_paths
            .lock()
            .get(peer)
            .map(|set| set.lock().paths.iter().any(|p| p.iface.kind() == carrier))
            .unwrap_or(false)
    }

    /// R1a connection-quality-ladder FAIL-BACK: flag `peer`'s direct-UDP path as degraded so `best()`
    /// prefers the reliable direct-TCP door for [`UDP_DEMOTE_DURATION`]. Called by the app when a
    /// message stayed undelivered over the (phantom) UDP link — the asymmetric-mobile case where UDP
    /// inbound keeps the path from ever pruning while outbound is dead. Returns whether the peer had a
    /// path set to flag. See docs/decisions/connection-quality-ladder.md + tcp-direct-peer-fallback.md.
    pub fn demote_udp_direct(&self, peer: &NodeAddress) -> bool {
        if let Some(set) = self.peer_paths.lock().get(peer) {
            set.lock().udp_demoted_until = Some(Instant::now() + UDP_DEMOTE_DURATION);
            true
        } else {
            false
        }
    }

    /// The Direct-`QuicOverUdp` path for `peer` (its native-UDP carrier + the neighbour/quic address
    /// quinn dialed it at) — the anchor for switching that peer's ONE connection between UDP and its
    /// door on the mux. `None` if the peer has no such path yet.
    /// Every direct path to `peer` over the native UDP carrier.
    fn direct_udp_paths(&self, peer: &NodeAddress) -> Vec<PeerPath> {
        self.peer_paths
            .lock()
            .get(peer)
            .map(|set| {
                set.lock()
                    .paths
                    .iter()
                    .filter(|p| {
                        matches!(p.quality, PathQuality::Direct) && p.iface.kind() == InterfaceKind::QuicOverUdp
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn direct_udp_path(&self, peer: &NodeAddress) -> Option<PeerPath> {
        self.peer_paths.lock().get(peer).and_then(|set| {
            set.lock()
                .paths
                .iter()
                .find(|p| {
                    matches!(p.quality, PathQuality::Direct)
                        && p.iface.kind() == InterfaceKind::QuicOverUdp
                })
                .cloned()
        })
    }

    /// Meta-B fail-down: route `peer`'s single quinn connection over its direct-TCP door at `door`.
    /// The connection MIGRATES onto the door (same connection id) — no separate QuicOverTcp
    /// neighbour, no re-dial. Returns `false` if the peer has no direct-UDP path to migrate (nothing
    /// is dialed yet) or its quic address is unknown. See [`crate::mux_socket`].
    /// Запомнить ключ, под которым у `peer` поставлен туннель в дверь, и СНЕСТИ ПРЕЖНИЙ.
    ///
    /// Возвращает вытесненный ключ (для теста и трассы). Ничего не вытесняет, если ключ тот же:
    /// повторная установка на тот же адрес — штатный no-op.
    fn install_peer_door_key(&self, peer: NodeAddress, key: SocketAddr) -> Option<SocketAddr> {
        let previous = self.peer_doors.lock().insert(peer, key).filter(|old| *old != key);
        if let Some(stale) = previous {
            // Туннель под прежним ключом больше не адресует этого пира: его супервизор иначе будет
            // дозваниваться в исчезнувшую дверь до конца процесса.
            for iface in &self.interfaces {
                iface.route_peer_over_udp(stale);
            }
            crate::tcp_transport::tcp_trace(
                "door/evict",
                &format!("peer={peer:?} stale_key={stale} replaced_by={key}"),
            );
        }
        previous
    }

    /// Our reflexive address from public STUN, asked from the socket of the native-UDP QUIC carrier —
    /// the socket peers dial. `None` if no interface can ask or fewer than two servers agree.
    pub async fn stun_reflexive(
        &self,
        servers: &[&str],
        timeout: Duration,
    ) -> Option<omni_net_primitives::stun::Reflexive> {
        for iface in &self.interfaces {
            if let Some(reflexive) = iface.stun_reflexive(servers, timeout).await {
                return Some(reflexive);
            }
        }
        None
    }

    pub async fn route_peer_over_door(&self, peer: &NodeAddress, door: SocketAddr) -> io::Result<bool> {
        // QUIC for our hops: no TCP door.
        if self.carrier_preference() == CarrierPreference::Quic {
            return Ok(false);
        }
        // Only a connection WE opened can move: in QUIC the client migrates, a server's packets from a
        // new address are dropped by its client. The mux routes by the peer's address, so a connection
        // the peer opened to the same address would ride the door too and go silent — it is closed;
        // the one we keep carries the link. With no connection of our own there is nothing to move.
        // Live this was the whole failure (two instances on one laptop, 2026-10-17): two connections
        // after a simultaneous dial, the link on the peer's one, 0 of 20 frames after the move.
        let udp_paths = self.direct_udp_paths(peer);
        let Some(own) = udp_paths.iter().find(|path| path.initiated_locally) else {
            return Ok(false);
        };
        let Some(peer_quic) = own.iface.remote_addr(&own.neighbor) else {
            return Ok(false);
        };
        own.iface.route_peer_over_door(peer_quic, door).await?;
        self.install_peer_door_key(*peer, peer_quic);
        let same = |addr: SocketAddr| crate::mux_socket::peer_key(addr) == crate::mux_socket::peer_key(peer_quic);
        for theirs in udp_paths.iter().filter(|path| !path.initiated_locally) {
            if theirs.iface.remote_addr(&theirs.neighbor).is_some_and(same) {
                theirs.iface.close_neighbor(&theirs.neighbor).await;
                self.prune_path(&theirs.neighbor);
            }
        }
        Ok(true)
    }

    /// FIRST CONTACT over a peer's TCP front door: dial `peer` when there is no connection yet and
    /// the door is the only address we can reach (a CGNAT peer on a UDP-hostile carrier).
    ///
    /// [`route_peer_over_door`](Self::route_peer_over_door) cannot do this — it MIGRATES an existing
    /// UDP path, so it needs one to exist. Here we know the address up front: quinn will address the
    /// peer AT the door, so we register that route in the mux BEFORE dialing and the handshake
    /// itself travels the TLS-over-TCP tunnel. The peer's bridge terminates that TLS and feeds the
    /// framed QUIC straight into its own quinn, so what comes up is an ordinary mesh link whose
    /// carrier happens to be TCP. See docs/bugs/door-first-contact.md.
    pub async fn dial_over_door(
        &self,
        peer: NodeAddress,
        door: SocketAddr,
    ) -> Result<Link, NetworkError> {
        // QUIC for our hops: no TCP door.
        if self.carrier_preference() == CarrierPreference::Quic {
            return Err(NetworkError::NoPathToDestination(peer));
        }
        if !self.register_door_route(peer, door).await {
            return Err(NetworkError::NoPathToDestination(peer));
        }
        self.dial_at_endpoints(peer, vec![door]).await
    }

    /// Route the address `door` through the TLS-over-TCP tunnel to that door, so a dial AT it rides
    /// TCP. Registered BEFORE the dial: the very first Initial packet must already ride the tunnel,
    /// otherwise quinn sends it as raw UDP to a TCP port and it disappears. An interface without a
    /// mux router (pure-TCP relay carrier, multicast) answers Err, so the first Ok is the native-UDP
    /// QUIC carrier that owns the mux. `false` when no interface can route it.
    async fn register_door_route(&self, peer: NodeAddress, door: SocketAddr) -> bool {
        let mut routed = false;
        for iface in &self.interfaces {
            if iface.route_peer_over_door(door, door).await.is_ok() {
                routed = true;
                break;
            }
        }
        if !routed {
            return false;
        }
        // Здесь ключ — сама дверь: quinn адресует пира ПО НЕЙ.
        self.install_peer_door_key(peer, door);
        crate::tcp_transport::tcp_trace(
            "door/first-contact",
            &format!("peer={peer:?} door={door} dialing over door tunnel"),
        );
        true
    }

    /// Carrier UPGRADE for a peer currently carried by its TCP door: try to stand up a direct-UDP
    /// path to `udp` alongside it. Returns whether the UDP path came up.
    ///
    /// Safe by construction, which is the whole point: this only ADDS a path, and only after the
    /// mutual identity challenge has succeeded over it. If UDP is blocked the connect or the
    /// challenge fails and nothing changes — the door keeps carrying the link. That is real proof
    /// the path works, unlike a bare carrier swap in the mux, which happens BELOW quinn and would
    /// silently black-hole traffic if the new path were dead.
    ///
    /// Nothing else has to switch: `path_rank` ranks direct-UDP above direct-TCP, so `send_failover`
    /// picks the new path on the next send and the door remains as the fallback beneath it.
    pub async fn upgrade_peer_to_udp(&self, peer: NodeAddress, udp: SocketAddr) -> bool {
        if self.carrier_preference() == CarrierPreference::Tcp {
            return false; // our hop stays on the door
        }
        if !self.peer_on_door(&peer) {
            return false; // not on a door — nothing to upgrade
        }
        // The door may carry the very connection `udp` names (a Meta-B migration onto the door).
        // Dialing `udp` then hands back THAT connection, the challenge rides the door and "passes",
        // and the link stays on TCP while the upgrade reports success. That case needs the real
        // thing: migrate the connection back to UDP, prove it there, and return to the door if not.
        if let Some(upgraded) = self.migrate_door_connection_back_to_udp(peer, udp).await {
            return upgraded;
        }
        for iface in &self.interfaces {
            if iface.kind() != InterfaceKind::QuicOverUdp {
                continue;
            }
            let Ok(neighbor) = iface.connect_endpoint(&udp.to_string()).await else {
                continue;
            };
            if self
                .challenge_and_register_direct(iface, neighbor, peer)
                .await
                .is_ok()
            {
                crate::tcp_transport::tcp_trace(
                    "door/upgrade-udp",
                    &format!("peer={peer:?} udp={udp} direct-UDP path is up; door stays as fallback"),
                );
                return true;
            }
        }
        crate::tcp_transport::tcp_trace(
            "door/upgrade-udp",
            &format!("peer={peer:?} udp={udp} FAILED — staying on the door"),
        );
        false
    }

    /// `None` — `udp` is not the address of a door-carried connection (nothing to migrate). Otherwise
    /// whether the connection now works over UDP; on failure it is back on its door.
    async fn migrate_door_connection_back_to_udp(
        &self,
        peer: NodeAddress,
        udp: SocketAddr,
    ) -> Option<bool> {
        let same = |a: SocketAddr| a.ip().to_canonical() == udp.ip().to_canonical() && a.port() == udp.port();
        let path = self.peer_paths.lock().get(&peer).and_then(|set| {
            set.lock()
                .paths
                .iter()
                // Ours only: a server's address change is dropped by its client (see
                // `route_peer_over_door`).
                .find(|p| {
                    p.initiated_locally
                        && path_rides_door(p)
                        && p.iface.remote_addr(&p.neighbor).is_some_and(same)
                })
                .cloned()
        })?;
        let quic = path.iface.remote_addr(&path.neighbor)?;
        let door = path.iface.peer_door_of(quic)?;
        let now = Instant::now();
        if self
            .migrate_back_backoff
            .lock()
            .get(&peer)
            .is_some_and(|(not_before, _)| now < *not_before)
        {
            return Some(false); // UDP failed recently — stay on the door, no traffic gamble
        }
        path.iface.route_peer_over_udp(quic);
        let proven = matches!(
            tokio::time::timeout(
                MIGRATE_BACK_PROOF_TIMEOUT,
                self.run_dialer_challenge(&path.iface, &path.neighbor, peer),
            )
            .await,
            Ok(Ok(_))
        );
        if proven {
            self.migrate_back_backoff.lock().remove(&peer);
            self.peer_doors.lock().remove(&peer);
            crate::tcp_transport::tcp_trace(
                "door/upgrade-udp",
                &format!("peer={peer:?} udp={udp} connection migrated back to UDP (proven)"),
            );
        } else {
            self.outbound_challenges.lock().remove(&path.neighbor);
            let _ = path.iface.route_peer_over_door(quic, door).await;
            let mut backoff = self.migrate_back_backoff.lock();
            let pause = backoff
                .get(&peer)
                .map(|(_, pause)| (*pause * 2).min(MIGRATE_BACK_MAX_PAUSE))
                .unwrap_or(MIGRATE_BACK_FIRST_PAUSE);
            backoff.insert(peer, (Instant::now() + pause, pause));
            crate::tcp_transport::tcp_trace(
                "door/upgrade-udp",
                &format!("peer={peer:?} udp={udp} migrate-back FAILED — back on door {door}"),
            );
        }
        Some(proven)
    }

    /// Meta-B optimistic upgrade: route `peer` back over the UDP base (migrates back, tears the door
    /// tunnel down). Returns whether a direct-UDP path with a known quic address was found to switch.
    pub fn route_peer_over_udp(&self, peer: &NodeAddress) -> bool {
        // Сносим по ЗАПОМНЕННОМУ ключу, а не по текущему адресу пира. Если адрес успел смениться,
        // текущий указывает не на тот туннель, и поставленный ранее остался бы жить — та же ошибка,
        // что и при установке.
        // Замок СНИМАЕТСЯ до захода в интерфейсы: в `if let` временный объект живёт до конца блока,
        // то есть замок `peer_doors` держался бы, пока мы берём замок карты дверей внутри mux.
        // Порядок захвата двух замков — не то, что стоит оставлять на волю формы записи.
        let installed = self.peer_doors.lock().remove(peer);
        if let Some(installed) = installed {
            for iface in &self.interfaces {
                iface.route_peer_over_udp(installed);
            }
        }
        let Some(path) = self.direct_udp_path(peer) else {
            return false;
        };
        let Some(peer_quic) = path.iface.remote_addr(&path.neighbor) else {
            return false;
        };
        path.iface.route_peer_over_udp(peer_quic);
        true
    }

    /// Establish an outbound Link to `destination` over a multi-hop path: resolve
    /// the next-hop from the routing table and reuse our verified connection to
    /// that neighbor. The destination is NOT challenged (it isn't our neighbor;
    /// its identity is established E2E above mesh). `Link::send` addresses
    /// `to = destination` over the next-hop neighbor, which relays it onward
    /// (см. `decide_data_forward`). Returns `None` if there is no known route or
    /// no live verified connection to the next-hop.
    fn dial_multihop(&self, destination: NodeAddress) -> Option<Link> {
        let entry = self.path_table.lookup(&destination, Instant::now())?;
        // Reuse the direct link to the next-hop — it must already be verified.
        let next_hop_identity = *self.verified_neighbors.lock().get(&entry.next_hop_neighbor)?;
        let iface = self
            .interfaces
            .iter()
            .find(|i| i.id() == entry.next_hop_interface)?;
        // Register the reliable inbox keyed by `destination` so replies route back.
        let channels = self.register_routed_link_channels(destination);
        // `cost == 0` IS NOT A RELAY, and calling it one was a live-observed lie.
        //
        // `PathEntry::cost` documents itself: "0 = direct neighbor". An announce arrives with
        // `hop_count = 0` only from the originator, so a cost-0 entry means the destination IS the
        // next hop — the verified neighbour we just checked for above is the destination itself.
        // Reaching here with cost 0 is normal and useful: the outbound dial failed while an INBOUND
        // verified connection to that same peer exists (live 2026-08-29: the peer had no IPv6 and
        // could not dial back, so reusing the connection the other side opened was its only path).
        // Reusing it is right. Labelling it `Relay { hops: 0 }` was not, and it cost three things
        // beyond the wrong word in the UI:
        //   - `register_path` seeds the relay dead clock for relay paths only ("Direct peers are
        //     covered by per-neighbour liveness, so they need no seed here") — a direct link got a
        //     clock it must not have;
        //   - `peer_link_quality` puts the relay arm behind `relay_peer_fresh_at`, a gate the same
        //     file says a direct path does not need;
        //   - the unreachable→relay transition fires a punch-upgrade ("Direct paths never signal
        //     (already the goal)") — we chased an upgrade for a link that was already direct.
        // See docs/bugs/direct-link-reported-as-relay-with-zero-hops.md.
        //
        // The deciding fact is WHO the next hop is, not the cost number: cost 0 was the first place
        // this surfaced, but the route to a neighbour's own identity was also written as cost 1 (the
        // per-frame liveness seed, PathRequest/PathReply), and that `Relay { hops: 1 }` then
        // overwrote the Direct path over the SAME neighbour in `PathSet::add` — live 2026-09-24, a
        // two-node pair stuck at `online_via_relay relay_hops=1` with no third node anywhere.
        // Same test as `arrival_is_the_origin` for inbound frames.
        // See docs/bugs/a-direct-neighbour-was-costed-as-one-relay-hop.md.
        let quality = if next_hop_identity == destination {
            PathQuality::Direct
        } else {
            // Someone else is the next hop, so there is at least one intermediary even if a table
            // entry claims cost 0 (e.g. the neighbour's identity changed under the same ip:port).
            PathQuality::Relay { hops: entry.cost.max(1) }
        };
        let paths = self.register_path(
            destination,
            entry.next_hop_neighbor,
            Arc::clone(iface),
            quality,
            // Not a connection we opened either way: at cost 0 it is the peer's inbound connection,
            // above it, someone else's hop.
            false,
        );
        Some(Link {
            local: self.local_address,
            destination,
            paths,
            inbox: AsyncMutex::new(channels.inbox_rx),
            datagram_inbox: AsyncMutex::new(channels.datagram_rx),
        })
    }

    // ===== Reactive path discovery (R3, AODV-style) =================================================
    // announce is single-hop, so the path_table never organically learns cost≥2 routes. When a dial
    // has no direct/known path, `discover_path` floods a PathRequest; relays reply if they are (or
    // know a route to) the target, else re-broadcast; the PathReply routes back to `origin` over the
    // reverse route the request set up, filling the forward route along the way. This is what makes
    // "one connected peer → reach the whole mesh" real. See docs/decisions/omni-network-model.md (D6).

    /// Unicast `wire` to every verified path-neighbour except `exclude` (the arrival hop). Mirrors
    /// `liveness_plan`'s neighbour enumeration (peer_paths → one iface per neighbour). Best-effort.
    async fn broadcast_to_verified_neighbours(&self, wire: &[u8], exclude: Option<&NeighborId>) {
        let targets: Vec<(NeighborId, InterfaceHandle)> = {
            let mut map: HashMap<NeighborId, InterfaceHandle> = HashMap::new();
            let paths = self.peer_paths.lock();
            for set in paths.values() {
                for path in &set.lock().paths {
                    map.entry(path.neighbor.clone())
                        .or_insert_with(|| path.iface.clone());
                }
            }
            map.into_iter()
                .filter(|(neighbor, _)| exclude != Some(neighbor))
                .collect()
        };
        for (neighbor, iface) in targets {
            let _ = iface.send_frame(&neighbor, wire).await;
        }
    }

    /// Flood a PathRequest for `target` and wait (bounded) for a PathReply to fill its route. Called
    /// by `dial` when there's no direct address and no known route. No-op-ish if we have no verified
    /// neighbours to flood to.
    async fn discover_path(&self, target: NodeAddress) {
        // Nothing to flood to → don't waste the wait (e.g. an isolated node, or a maintenance dial
        // with no verified neighbours yet). Keeps a failed/early dial fast.
        if self.verified_neighbors.lock().is_empty() {
            return;
        }
        let nonce = self
            .path_request_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Mark our own request seen so its echo can't loop back through us.
        self.seen_path_requests
            .lock()
            .note((self.local_address, nonce));
        let wire = NetworkFrame::PathRequest(PathRequestFrame {
            origin: self.local_address,
            target,
            nonce,
            hop_limit: PATH_REQUEST_DEFAULT_HOP_LIMIT,
        })
        .encode_to_wire();
        self.broadcast_to_verified_neighbours(&wire, None).await;
        let started = Instant::now();
        net_trace(&format!("PATH_REQ target={target:?} nonce={nonce} neighbours={}", self.verified_neighbors.lock().len()));

        let deadline = started + PATH_REQUEST_TIMEOUT;
        while Instant::now() < deadline {
            if self.path_table.lookup(&target, Instant::now()).is_some() {
                net_trace(&format!("PATH_REQ target={target:?} learned in {}ms", started.elapsed().as_millis()));
                return; // route learned → dial_multihop can proceed
            }
            tokio::time::sleep(PATH_REQUEST_POLL).await;
        }
        net_trace(&format!("PATH_REQ target={target:?} TIMEOUT"));
    }

    /// Anti-eclipse/DoS admission for a path-control frame from `neighbor`: false (drop) once the
    /// neighbour exceeds `PATH_CONTROL_RATE_MAX` accepted frames in the current sliding window. A
    /// verified neighbour is only a route candidate — this caps a malicious one's request-flood
    /// amplification + reply pollution without hurting honest discovery (a few frames per dial).
    fn path_control_admit(&self, neighbor: &NeighborId) -> bool {
        let now = Instant::now();
        let mut quota = self.path_control_quota.lock();
        let entry = quota.entry(neighbor.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= PATH_CONTROL_RATE_WINDOW {
            *entry = (now, 0); // window elapsed → reset
        }
        entry.1 += 1;
        entry.1 <= PATH_CONTROL_RATE_MAX
    }

    /// Handle an inbound PathRequest: record the reverse route to `origin`, then answer (we are /
    /// know a route to `target`) or re-broadcast. Verified-neighbour-gated (anti-open-relay),
    /// per-neighbour rate-limited (anti-eclipse/DoS), and deduped by `(origin, nonce)` (loop/flood).
    async fn handle_path_request(
        &self,
        req: PathRequestFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return; // only relay/answer requests from a verified neighbour
        }
        if !self.path_control_admit(neighbor) {
            return; // neighbour over its path-control rate budget — drop
        }
        if !self.seen_path_requests.lock().note((req.origin, req.nonce)) {
            return; // already processed this request — drop (loop/dup)
        }
        if req.origin == self.local_address {
            return; // our own request echoed back
        }
        let now = Instant::now();
        let ttl = Duration::from_secs(DEFAULT_ANNOUNCE_TTL_SECONDS as u64);
        // Reverse route to the requester (so the reply + future data can route back). Cost in the
        // table's own unit — intermediaries, "0 = direct neighbor": a request straight from its
        // origin still carries the full hop limit and gives 0; each relay on the way took one off.
        // (It was `+ 1`, which filed the origin — our own neighbour — as one relay hop away.)
        let reverse_cost = PATH_REQUEST_DEFAULT_HOP_LIMIT.saturating_sub(req.hop_limit);
        self.path_table
            .update(req.origin, iface.id(), neighbor.clone(), reverse_cost, ttl, now);

        // Can we answer? We ARE the target (cost 0), or we know a route to it that doesn't point
        // back toward the requester (that would be a bogus loop).
        let reply_cost = if req.target == self.local_address {
            Some(0u8)
        } else {
            self.path_table
                .lookup(&req.target, now)
                .filter(|entry| entry.next_hop_neighbor != *neighbor)
                .map(|entry| entry.cost)
        };
        if let Some(cost) = reply_cost {
            let reply = NetworkFrame::PathReply(PathReplyFrame {
                origin: req.origin,
                target: req.target,
                nonce: req.nonce,
                cost,
            })
            .encode_to_wire();
            // Back toward origin = the neighbour this request arrived from.
            let _ = iface.send_frame(neighbor, &reply).await;
            return;
        }
        // No answer — re-broadcast onward (TTL-bounded), never back to the arrival neighbour.
        if req.hop_limit > 0 {
            let fwd = NetworkFrame::PathRequest(PathRequestFrame {
                origin: req.origin,
                target: req.target,
                nonce: req.nonce,
                hop_limit: req.hop_limit - 1,
            })
            .encode_to_wire();
            self.broadcast_to_verified_neighbours(&fwd, Some(neighbor)).await;
        }
    }

    /// Handle an inbound PathReply: record the forward route to `target` via the replying neighbour,
    /// then — unless we're the origin — forward it toward `origin` over the reverse route the request
    /// established. Verified-gated; cost-bounded (a reply beyond the flood radius is bogus).
    async fn handle_path_reply(
        &self,
        rep: PathReplyFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if !self.path_control_admit(neighbor) {
            net_trace(&format!("PATH_REPLY target={:?} DROP over budget", rep.target));
            return; // neighbour over its path-control rate budget — drop (anti-eclipse/DoS)
        }
        net_trace(&format!("PATH_REPLY target={:?} origin={:?} cost={}", rep.target, rep.origin, rep.cost));
        // `rep.cost` is the replier's own cost to the target (0 when the replier IS the target).
        // From us, the replier is one more intermediary — unless it is the target itself, in which
        // case the target is our direct neighbour and the cost is 0, not 1.
        let replier_is_target = self.verified_neighbors.lock().get(neighbor) == Some(&rep.target);
        let our_cost = if replier_is_target { 0 } else { rep.cost.saturating_add(1) };
        if our_cost > PATH_REQUEST_DEFAULT_HOP_LIMIT {
            return; // beyond the flood radius — loop/abuse guard
        }
        let now = Instant::now();
        let ttl = Duration::from_secs(DEFAULT_ANNOUNCE_TTL_SECONDS as u64);
        self.path_table
            .update(rep.target, iface.id(), neighbor.clone(), our_cost, ttl, now);
        if rep.origin == self.local_address {
            return; // we initiated — the route is in the table; `discover_path` polling picks it up
        }
        // Forward toward origin over the reverse route, never back to the arrival neighbour.
        if let Some(entry) = self.path_table.lookup(&rep.origin, now) {
            if entry.next_hop_neighbor != *neighbor {
                if let Some(out) = self
                    .interfaces
                    .iter()
                    .find(|i| i.id() == entry.next_hop_interface)
                {
                    let fwd = NetworkFrame::PathReply(PathReplyFrame {
                        origin: rep.origin,
                        target: rep.target,
                        nonce: rep.nonce,
                        cost: our_cost,
                    })
                    .encode_to_wire();
                    let _ = out.send_frame(&entry.next_hop_neighbor, &fwd).await;
                }
            }
        }
    }

    /// Refresh the end-to-end last-seen for a peer's OWN identity (a routed `PeerPong` it answered, or
    /// `Data` it originated). Drives relay-only-peer liveness — see `peer_last_seen` +
    /// `RELAY_PEER_DEAD_AFTER` + docs/bugs/relay-presence-false-online-dead-peer.md.
    fn note_peer_seen(&self, peer: NodeAddress, reason: &str) {
        if peer == self.local_address {
            return;
        }
        net_trace(&format!("PEER_SEEN {peer:?} reason={reason}"));
        self.peer_last_seen.lock().insert(peer, Instant::now());
    }

    /// Refresh end-to-end last-seen from a routed `PeerPong`, but ONLY if its nonce matches the probe
    /// we most recently sent that peer. A stale / duplicate / relay-buffered pong (old nonce) must not
    /// re-seed liveness — that post-death reseed was the relay-presence FLAP (a dead peer kept being
    /// re-marked alive by a forwarded old pong). Mirrors the AutoNAT dial-back nonce gate. See
    /// docs/bugs/relay-presence-false-online-dead-peer.md.
    ///
    /// Not only the latest probe: one goes out every `LIVENESS_TICK` (4 s), and over a slow network a
    /// pong through a relay takes longer than that — each answer then found a newer nonce, was dropped
    /// as stale, and a live peer was pruned every 25 s (stand 2026-10-15, a bad public Wi-Fi: the call
    /// invite went out and the path was gone 2 s later). An answer to an earlier probe still in flight
    /// counts — as proof the peer was alive when THAT probe left, not now, so a pong a relay held back
    /// cannot carry a dead peer past its dead window.
    fn note_peer_pong(&self, peer: NodeAddress, nonce: u64) {
        let expected = self.peer_ping_nonce.lock().get(&peer).copied();
        let sent_at = self
            .peer_ping_sent
            .lock()
            .get(&peer)
            .and_then(|sent| sent.iter().find(|(n, _)| *n == nonce).map(|(_, at)| *at));
        if expected == Some(nonce) && sent_at.is_none() {
            self.note_peer_seen(peer, "peerpong");
        } else if let Some(at) = sent_at {
            net_trace(&format!("PEER_SEEN {peer:?} reason=peerpong sent_ago_ms={}", at.elapsed().as_millis()));
            let mut seen = self.peer_last_seen.lock();
            let last = seen.entry(peer).or_insert(at);
            if *last < at {
                *last = at;
            }
        } else {
            net_trace(&format!(
                "PEER_SEEN {peer:?} reason=peerpong DROP stale-nonce got={nonce} want={expected:?}"
            ));
        }
    }

    fn remember_peer_ping(&self, peer: NodeAddress, nonce: u64, at: Instant) {
        let mut sent = self.peer_ping_sent.lock();
        let probes = sent.entry(peer).or_default();
        probes.push_back((nonce, at));
        while probes.len() > PEER_PING_REMEMBERED {
            probes.pop_front();
        }
    }

    /// Handle a routed end-to-end liveness probe. Verified-gated + rate-limited (anti-amplification,
    /// like path-control). If we ARE the probed peer, answer with a `PeerPong` over the reverse first
    /// hop (the arrival neighbour); otherwise (a relay carrying it on) forward toward `to` via the
    /// path_table, hop-limited and never back to the arrival neighbour. See
    /// docs/bugs/relay-presence-false-online-dead-peer.md.
    async fn handle_peer_ping(
        &self,
        ping: PeerPingFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if !self.path_control_admit(neighbor) {
            return;
        }
        if ping.to == self.local_address {
            let pong = NetworkFrame::PeerPong(PeerPongFrame {
                from: self.local_address,
                to: ping.from,
                nonce: ping.nonce,
                hop_limit: PEER_PING_HOP_LIMIT,
            })
            .encode_to_wire();
            let _ = iface.send_frame(neighbor, &pong).await;
            return;
        }
        if ping.hop_limit == 0 || ping.from == self.local_address {
            return;
        }
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&ping.to, now) {
            if entry.next_hop_neighbor != *neighbor {
                if let Some(out) =
                    self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface)
                {
                    let fwd = NetworkFrame::PeerPing(PeerPingFrame {
                        from: ping.from,
                        to: ping.to,
                        nonce: ping.nonce,
                        hop_limit: ping.hop_limit - 1,
                    })
                    .encode_to_wire();
                    let _ = out.send_frame(&entry.next_hop_neighbor, &fwd).await;
                }
            }
        }
    }

    /// Handle a routed `PeerPong`. If addressed to us, the probed peer is alive end-to-end → refresh
    /// its per-peer last-seen (so its relay path is not pruned). Otherwise forward toward the prober
    /// (`to`) via the path_table, hop-limited. See docs/bugs/relay-presence-false-online-dead-peer.md.
    async fn handle_peer_pong(&self, pong: PeerPongFrame, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if !self.path_control_admit(neighbor) {
            return;
        }
        if pong.to == self.local_address {
            self.note_peer_pong(pong.from, pong.nonce);
            return;
        }
        if pong.hop_limit == 0 {
            return;
        }
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&pong.to, now) {
            if entry.next_hop_neighbor != *neighbor {
                if let Some(out) =
                    self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface)
                {
                    let fwd = NetworkFrame::PeerPong(PeerPongFrame {
                        from: pong.from,
                        to: pong.to,
                        nonce: pong.nonce,
                        hop_limit: pong.hop_limit - 1,
                    })
                    .encode_to_wire();
                    let _ = out.send_frame(&entry.next_hop_neighbor, &fwd).await;
                }
            }
        }
    }

    /// Route a DHT-proxy request (R7 Phase 2 mode b, `dht-over-peer.md`). Verified-gated +
    /// path-control-rate-limited (anti-amplification, like `PeerPing`). If addressed to us, deliver it to
    /// the app (`dht_proxy_request_tx` → `try_recv_dht_proxy_request`) — the opt-in gate + the
    /// `omni-dht-helper` IPC live in omni-app, which answers via `send_dht_proxy_response`. Otherwise
    /// (we're a relay carrying it on) forward toward `to` via the path_table, hop-limited and never back
    /// to the arrival neighbour (mirrors `handle_peer_ping`).
    async fn handle_dht_proxy_request(&self, req: DhtProxyRequestFrame, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if !self.path_control_admit(neighbor) {
            return;
        }
        if req.to == self.local_address {
            // Deliver to the app to serve via its local omni-dht-helper (the opt-in gate + the helper
            // IPC live in omni-app, which holds the endpoint); it answers with `send_dht_proxy_response`.
            // Best-effort: a full channel (no app draining / not a proxy) just drops the request — the
            // requester times out and falls back (N-of-many). See docs/decisions/dht-over-peer.md.
            net_trace(&format!(
                "DHT_PROXY req from={:?} req_id={} op={:?} -> app",
                req.from, req.req_id, req.op
            ));
            let _ = self.dht_proxy_request_tx.try_send(req);
            return;
        }
        if req.hop_limit == 0 || req.from == self.local_address {
            return;
        }
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&req.to, now) {
            if entry.next_hop_neighbor != *neighbor {
                if let Some(out) =
                    self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface)
                {
                    let fwd = NetworkFrame::DhtProxyRequest(DhtProxyRequestFrame {
                        hop_limit: req.hop_limit - 1,
                        ..req
                    })
                    .encode_to_wire();
                    let _ = out.send_frame(&entry.next_hop_neighbor, &fwd).await;
                }
            }
        }
    }

    /// Route a DHT-proxy response back toward the requester (`to`). If addressed to us, complete the
    /// awaiting requester by matching `req_id` in `dht_proxy_pending` (an unknown/late id is dropped).
    /// Otherwise forward toward `to`, hop-limited.
    async fn handle_dht_proxy_response(&self, resp: DhtProxyResponseFrame, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if !self.path_control_admit(neighbor) {
            return;
        }
        if resp.to == self.local_address {
            // Complete the awaiting requester by req_id. An unknown / duplicate / late req_id (already
            // completed or timed out) finds no pending entry and is dropped. See dht-over-peer.md.
            net_trace(&format!(
                "DHT_PROXY resp from={:?} req_id={} ok={} -> requester",
                resp.from, resp.req_id, resp.ok
            ));
            if let Some(tx) = self.dht_proxy_pending.lock().remove(&resp.req_id) {
                let _ = tx.send((resp.ok, resp.value));
            }
            return;
        }
        if resp.hop_limit == 0 {
            return;
        }
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&resp.to, now) {
            if entry.next_hop_neighbor != *neighbor {
                if let Some(out) =
                    self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface)
                {
                    let fwd = NetworkFrame::DhtProxyResponse(DhtProxyResponseFrame {
                        hop_limit: resp.hop_limit - 1,
                        ..resp
                    })
                    .encode_to_wire();
                    let _ = out.send_frame(&entry.next_hop_neighbor, &fwd).await;
                }
            }
        }
    }

    /// A DIRECT neighbour asked us for the in-segment DHT bootstrap addresses we know (step B). We
    /// surface it to the app (which holds the configured bootstrap list + the DHT-rendezvous gate); it
    /// answers with `send_dht_boot_response`. NEVER forwarded — this is a single-hop exchange between
    /// linked peers, so there is no multi-hop harvest of DHT IPs from strangers. Only a verified
    /// neighbour is served (anti-spoof), under the shared path-control quota (anti-abuse). Best-effort:
    /// a full channel / no app draining just drops it (the requester retries another peer). See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    async fn handle_dht_boot_req(&self, req: DhtBootReqFrame, neighbor: &NeighborId) {
        let Some(requester) = self.verified_neighbors.lock().get(neighbor).copied() else {
            return;
        };
        if !self.path_control_admit(neighbor) {
            return;
        }
        let _ = self.dht_boot_request_tx.try_send((requester, req.req_id));
    }

    /// A neighbour answered our DHT bootstrap seed request (step B): complete the awaiting requester by
    /// matching `req_id` in `dht_boot_pending` (an unknown / late id is dropped). Not forwarded.
    async fn handle_dht_boot_resp(&self, resp: DhtBootRespFrame, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        if let Some(tx) = self.dht_boot_pending.lock().remove(&resp.req_id) {
            let _ = tx.send(resp.nodes);
        }
    }

    /// A direct neighbour asked for our relay card: answer on the same connection — the card when we
    /// are a relay, empty otherwise. Single-hop, rate-limited like the other path-control frames.
    async fn handle_relay_card_req(&self, req: RelayCardReqFrame, iface: &InterfaceHandle, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) || !self.path_control_admit(neighbor) {
            return;
        }
        let card = self.relay_card.lock().clone().unwrap_or_default();
        let wire = NetworkFrame::RelayCardResp(RelayCardRespFrame { req_id: req.req_id, card }).encode_to_wire();
        let _ = iface.send_frame(neighbor, &wire).await;
    }

    /// Hand out `card` as a relay (`Some`) or stop (`None`): while set, this node answers «whoever you
    /// are» challenges at its address and `RelayCardReq`s with it. Relay directory stage 2.
    pub fn set_relay_card(&self, card: Option<Vec<u8>>) {
        *self.relay_card.lock() = card;
    }

    /// Dial a bare address whose identity we do not know (a Mainline relay shelf gives only `ip:port`):
    /// challenge «whoever you are», take the identity from the key that answers — only a node holding a
    /// relay card answers — and register a direct link to it. The caller then asks for the card and
    /// checks it is signed by that same key.
    pub async fn dial_unknown(&self, endpoint: SocketAddr) -> Result<(NodeAddress, Link), NetworkError> {
        let iface = self
            .interfaces
            .iter()
            .find(|iface| iface.kind() == InterfaceKind::QuicOverUdp)
            .cloned()
            .ok_or(NetworkError::NoPathToDestination(NodeAddress::ANY))?;
        let neighbor = iface
            .connect_endpoint(&endpoint.to_string())
            .await
            .map_err(|_| NetworkError::NoPathToDestination(NodeAddress::ANY))?;
        let channels = match self.run_dialer_challenge(&iface, &neighbor, NodeAddress::ANY).await {
            Ok(channels) => channels,
            Err(error) => {
                self.outbound_challenges.lock().remove(&neighbor);
                return Err(error);
            }
        };
        let peer = channels.peer;
        if peer == self.local_address {
            iface.close_neighbor(&neighbor).await;
            self.prune_path(&neighbor);
            return Err(NetworkError::DialSelf);
        }
        let paths = self.register_path(peer, neighbor, Arc::clone(&iface), PathQuality::Direct, true);
        Ok((
            peer,
            Link {
                local: self.local_address,
                destination: peer,
                paths,
                inbox: AsyncMutex::new(channels.inbox_rx),
                datagram_inbox: AsyncMutex::new(channels.datagram_rx),
            },
        ))
    }

    /// Ask a directly linked `peer` for its relay card; `None` — no direct link, no answer in time, or
    /// not a relay (empty card).
    pub async fn request_relay_card(&self, peer: NodeAddress, deadline: Duration) -> Option<Vec<u8>> {
        let target = self
            .verified_neighbors
            .lock()
            .iter()
            .find(|(_, identity)| **identity == peer)
            .map(|(neighbor, _)| neighbor.clone())?;
        let req_id = OsRng.next_u64();
        let (tx, rx) = oneshot::channel();
        self.relay_card_pending.lock().insert(req_id, tx);
        let wire = NetworkFrame::RelayCardReq(RelayCardReqFrame { req_id }).encode_to_wire();
        let mut sent = false;
        for iface in &self.interfaces {
            if iface.send_frame(&target, &wire).await.is_ok() {
                sent = true;
                break;
            }
        }
        let card = if sent { timeout(deadline, rx).await.ok().and_then(Result::ok) } else { None };
        self.relay_card_pending.lock().remove(&req_id);
        card.filter(|card| !card.is_empty())
    }

    /// Initiator half of the mutual challenge: register a pending challenge,
    /// send the request, await the dispatcher signalling the verified result.
    /// On success returns the inbox channels for the freshly-verified Link.
    async fn run_dialer_challenge(
        &self,
        iface: &InterfaceHandle,
        neighbor: &NeighborId,
        destination: NodeAddress,
    ) -> Result<VerifiedLinkChannels, NetworkError> {
        let nonce = fresh_challenge_nonce();
        let (waker_tx, waker_rx) = oneshot::channel();
        self.outbound_challenges.lock().insert(
            neighbor.clone(),
            OutboundChallenge {
                nonce,
                expected: destination,
                role: ChallengeRole::Dialer { waker: waker_tx },
            },
        );

        let wire = NetworkFrame::ChallengeRequest(ChallengeRequest {
            nonce,
            from: self.local_address,
            to: destination,
        })
        .encode_to_wire();

        // Re-send the SAME challenge (same nonce) up to `CHALLENGE_SEND_ATTEMPTS` times over the SAME
        // connection. On a flaky link (mobile CGNAT) a single `ChallengeRequest` or its response is
        // easily lost, and a one-shot 5 s wait then failed the whole dial — which made the app open a
        // churny NEW connection. Re-sending rides out transient loss without churn; a late or resent
        // response still matches (same nonce), and success on any attempt resolves immediately.
        let mut waker_rx = waker_rx;
        let mut send_err: Option<String> = None;
        for _ in 0..CHALLENGE_SEND_ATTEMPTS {
            if let Err(e) = iface.send_frame(neighbor, &wire).await {
                // Momentarily unwritable — remember it, but still wait for a possibly in-flight
                // response before re-sending.
                send_err = Some(e.to_string());
            }
            match timeout(CHALLENGE_TIMEOUT, &mut waker_rx).await {
                Ok(Ok(result)) => return result,
                // Waker sender dropped (shutdown) — terminal.
                Ok(Err(_)) => {
                    self.outbound_challenges.lock().remove(neighbor);
                    return Err(NetworkError::ChallengeFailed);
                }
                // This attempt timed out → re-send (unless out of attempts).
                Err(_) => {}
            }
        }
        self.outbound_challenges.lock().remove(neighbor);
        Err(send_err
            .map(NetworkError::ChallengeSend)
            .unwrap_or(NetworkError::ChallengeTimeout))
    }

    /// Wait for next incoming Link. Returns when remote peer sent us a
    /// Data frame и we had no existing inbox для that peer.
    pub async fn accept(&self) -> Option<Link> {
        let mut incoming = self.incoming_links.lock().await;
        incoming.recv().await
    }

    /// Stop dispatcher tasks. Existing Link'и могут продолжать send'ить
    /// напрямую через interface (until next interface failure), но recv
    /// больше не будет получать frame'ы.
    /// Close the connections of every interface (see [`Interface::close`]), so peers drop their
    /// paths to us at once instead of sending into a dead connection until their liveness prune.
    pub async fn close_interfaces(&self) {
        for iface in &self.interfaces {
            iface.close().await;
        }
    }

    pub fn shutdown(&self) {
        for task in self.background_tasks.lock().drain(..) {
            task.abort();
        }
        // Drop any in-flight challenges so a `dial` awaiting a response fails
        // fast (waker sender dropped) instead of waiting out CHALLENGE_TIMEOUT.
        self.outbound_challenges.lock().clear();
        self.pending_data.lock().clear();
        *self.started.lock() = false;
    }

    /// Per-interface dispatcher: in a loop, read frames, route them.
    async fn dispatcher_loop(self: Arc<Self>, iface: InterfaceHandle) {
        loop {
            let (neighbor, bytes) = match iface.recv_frame().await {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("[omni-network] interface {:?} recv error: {e}", iface.id());
                    // Brief pause to avoid hot-loop on persistent failure.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            let frame = match NetworkFrame::decode_from_wire(&bytes) {
                Ok(f) => f,
                Err(NetworkFrameError::UnknownKind(_)) => {
                    // Forward compat: unknown frame type — silent drop.
                    continue;
                }
                Err(e) => {
                    eprintln!("[omni-network] frame decode error: {e}");
                    continue;
                }
            };

            // Liveness: any frame from a verified neighbour proves it is alive, so
            // refresh its last-seen (the liveness pump only acts on verified
            // neighbours, which bounds this map). See `LIVENESS_*`.
            //
            // AND refresh a cost-0 `path_table` route to that neighbour's IDENTITY, so
            // `decide_data_forward` can always relay TO a directly-linked verified neighbour. A relay
            // forwards purely by `path_table.lookup(to)`, and otherwise only ever had the *transient*
            // PathRequest reverse-route to a neighbour — TTL-bounded, and its next-hop churned by the
            // connection-coalescing re-dial — so the relay stopped forwarding to its own directly-
            // linked peers ~after the pairing burst (the live "chat stops crossing the relay" decay,
            // not the carrier: the dropped frame rides the SAME verified link as the liveness/pairing frames
            // that keep flowing). The seed is liveness-bound: a silent (pruned) neighbour's route
            // ages out by TTL. Cost 0 because that is what it is — `PathEntry::cost` says "0 = direct
            // neighbor", and the destination here IS the neighbour. It used to be 1 ("the minimum,
            // so it never shadows a better route"), but nothing is better than the neighbour itself,
            // and the 1 read back as one relay hop (docs/bugs/a-direct-neighbour-was-costed-as-one-relay-hop.md).
            let now = Instant::now();
            let neighbor_addr = self.verified_neighbors.lock().get(&neighbor).copied();
            if let Some(neighbor_addr) = neighbor_addr {
                self.last_seen.lock().insert(neighbor.clone(), now);
                // `refresh_live` (not `update`): the connection this frame arrived on IS the
                // live path to that identity, so an equal-cost route from a reconnected
                // next-hop (NAT remap → new NeighborId = new ip:port) REPLACES the stale one.
                // `update` would keep the dead incumbent until TTL, black-holing relayed
                // traffic after a remap. See docs/bugs/relay-pairing-approval-gap.md.
                self.path_table.refresh_live(
                    neighbor_addr,
                    iface.id(),
                    neighbor.clone(),
                    0,
                    Duration::from_secs(DEFAULT_ANNOUNCE_TTL_SECONDS as u64),
                    now,
                );
            }

            match frame {
                NetworkFrame::Announce(announce) => {
                    if let Err(e) = self.handle_announce(announce, &iface, &neighbor).await {
                        eprintln!("[omni-network] announce error: {e}");
                    }
                }
                NetworkFrame::Data(data) => {
                    self.handle_data(data, &neighbor, &iface).await;
                }
                NetworkFrame::ChallengeRequest(request) => {
                    self.handle_challenge_request(request, &iface, &neighbor).await;
                }
                NetworkFrame::ChallengeResponse(response) => {
                    self.handle_challenge_response(response, &iface, &neighbor)
                        .await;
                }
                NetworkFrame::ObservedAddress(observed) => {
                    self.handle_observed_address(observed, &neighbor);
                }
                NetworkFrame::DialBackRequest(req) => {
                    self.handle_dial_back_request(req, &iface, &neighbor);
                }
                NetworkFrame::DialBackProbe(probe) => {
                    self.reachability.lock().on_probe(probe.nonce);
                }
                // Task 3 — TCP-direct front door confirmation (validate-before-advertise).
                NetworkFrame::TcpReachabilityRequest(req) => {
                    self.handle_tcp_reachability_request(req, &iface, &neighbor);
                }
                NetworkFrame::TcpReachabilityResult(res) => {
                    self.handle_tcp_reachability_result(res);
                }
                // Durable delivery (P4): store + deliver-if-ours / hold for S&F.
                // A subscribed group frame is additionally gossip-pushed once to
                // linked co-members (G3) — dedup at each hop bounds the flood.
                NetworkFrame::DurableData(durable) => {
                    let push_targets = self.handle_durable_data(durable.clone());
                    self.push_durable_to(&durable, &push_targets).await;
                }
                // Anti-entropy reconciliation (P4-3b): have → offer → request → send.
                NetworkFrame::AntiEntropyHave(have) => {
                    self.handle_anti_entropy_have(have, &neighbor, &iface).await;
                }
                NetworkFrame::AntiEntropyOffer(offer) => {
                    self.handle_anti_entropy_offer(offer, &neighbor, &iface).await;
                }
                NetworkFrame::AntiEntropyRequest(request) => {
                    self.handle_anti_entropy_request(request, &neighbor, &iface).await;
                }
                // Hole-punch coordination (P2). Both handlers are fast (only
                // send_frame) — no dial here, so no dispatcher hang; the direct
                // dial that completes the punch is the caller's (app/test), through
                // the pinholes these open. См. docs/decisions/mesh-hole-punching.md.
                NetworkFrame::PunchRequest(req) => {
                    self.handle_punch_request(req, &neighbor, &iface).await;
                }
                NetworkFrame::PunchSync(sync) => {
                    self.handle_punch_sync(sync, &iface).await;
                }
                // Liveness probe from a verified neighbour → answer so it keeps us
                // marked alive. Don't answer unverified neighbours (no free echo).
                NetworkFrame::Ping => {
                    if self.verified_neighbors.lock().contains_key(&neighbor) {
                        let pong = NetworkFrame::Pong.encode_to_wire();
                        let _ = iface.send_frame(&neighbor, &pong).await;
                    }
                }
                // Liveness reply — last-seen was already refreshed above.
                NetworkFrame::Pong => {}
                // Reactive path discovery (R3, AODV-style): find/relay a route to a target so a
                // dial with no direct/known path can still reach it ("one peer → reach all").
                NetworkFrame::PathRequest(req) => {
                    self.handle_path_request(req, &neighbor, &iface).await;
                }
                NetworkFrame::PathReply(rep) => {
                    self.handle_path_reply(rep, &neighbor, &iface).await;
                }
                // Routed end-to-end liveness (relay-only-peer presence honesty).
                NetworkFrame::PeerPing(ping) => {
                    self.handle_peer_ping(ping, &neighbor, &iface).await;
                }
                NetworkFrame::PeerPong(pong) => {
                    self.handle_peer_pong(pong, &neighbor).await;
                }
                // DHT-as-a-service over the mesh (R7 Phase 2 mode b). Routed like PeerPing; addressed
                // to us → delivered to the app (request) / completes the awaiting requester (response).
                // See docs/decisions/dht-over-peer.md.
                NetworkFrame::DhtProxyRequest(req) => {
                    self.handle_dht_proxy_request(req, &neighbor).await;
                }
                NetworkFrame::DhtProxyResponse(resp) => {
                    self.handle_dht_proxy_response(resp, &neighbor).await;
                }
                // DHT bootstrap seed exchange (step B): a DIRECT neighbour asks for the in-segment DHT
                // bootstrap addresses we know (request), or answers ours (response). Never forwarded —
                // single-hop by design. See docs/decisions/dht-bootstrap-mesh-propagation.md.
                NetworkFrame::DhtBootReq(req) => {
                    self.handle_dht_boot_req(req, &neighbor).await;
                }
                NetworkFrame::DhtBootResp(resp) => {
                    self.handle_dht_boot_resp(resp, &neighbor).await;
                }
                // Relay card (relay directory stage 2): single-hop, never forwarded.
                NetworkFrame::RelayCardReq(req) => {
                    self.handle_relay_card_req(req, &iface, &neighbor).await;
                }
                NetworkFrame::RelayCardResp(resp) => {
                    if self.verified_neighbors.lock().contains_key(&neighbor) {
                        if let Some(tx) = self.relay_card_pending.lock().remove(&resp.req_id) {
                            let _ = tx.send(resp.card);
                        }
                    }
                }
                // Friend-proxy carriage (Axis 2): a consenting friend carries this toward `to` on the
                // origin's behalf (gated on consent + a per-origin cap). See friend-proxy-by-consent.md.
                NetworkFrame::FriendRelayCarry(carry) => {
                    self.handle_friend_relay_carry(carry, &neighbor).await;
                }
            }
        }
    }

    /// Per-interface media pump: route inbound datagrams to the verified
    /// Link's datagram inbox. Datagrams are loss-tolerant — anything from an
    /// unverified neighbor (or with no Link inbox yet) is simply dropped, no
    /// buffering. Runs only on `supports_datagrams()` interfaces.
    async fn datagram_dispatcher_loop(self: Arc<Self>, iface: InterfaceHandle) {
        loop {
            let (neighbor, bytes) = match iface.recv_datagram().await {
                Ok(pair) => pair,
                // Channel closed (interface gone) — terminal, stop the pump.
                Err(_) => break,
            };
            // Per-hop auth: only a verified neighbour may inject media (anti-spoof / anti-open-relay).
            if !self.verified_neighbors.lock().contains_key(&neighbor) {
                continue;
            }
            // Parse the routing envelope; an unparseable / wrong-version datagram is dropped (media
            // is loss-tolerant — there is no raw fallback once framing is uniform).
            let Some(datagram) = MediaDatagram::decode(&bytes) else {
                continue;
            };
            if datagram.to == self.local_address {
                // Destined for us → deliver the payload to the Link of its logical SOURCE (`from`),
                // so a RELAYED datagram (arrival neighbour = relay, not the source) still reaches the
                // right Link's inbox (keyed by identity, like the reliable plane).
                if let Some(tx) = self.link_datagram_inboxes.lock().get(&datagram.from) {
                    // Best-effort: drop if the receiver is behind (media is realtime).
                    let _ = tx.try_send(datagram.payload);
                }
            } else if self.relay_enabled {
                // Not for us → relay it toward `datagram.to`, mirroring decide_data_forward's gates:
                // only from a verified neighbour (checked above = anti-open-relay), TTL-bounded
                // (`forwarded()` drops at hop_limit 0), and never back to the arrival neighbour (loop
                // guard). The payload stays opaque E2E media — we forward, never read it.
                if let Some(entry) = self.path_table.lookup(&datagram.to, Instant::now()) {
                    if entry.next_hop_neighbor != neighbor {
                        if let Some(forwarded) = datagram.forwarded() {
                            if let Some(out_iface) = self
                                .interfaces
                                .iter()
                                .find(|i| i.id() == entry.next_hop_interface)
                            {
                                let _ = out_iface
                                    .send_datagram(&entry.next_hop_neighbor, &forwarded.encode())
                                    .await;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Per-interface close pump (P2 cleanup-on-close): when a connection ends,
    /// prune that path so `send` stops choosing it and a peer that lost its only
    /// path is correctly unreachable — proactively, not just on the next failed
    /// send. Default `recv_closed` never resolves, so this parks on interfaces
    /// without close detection.
    async fn close_dispatcher_loop(self: Arc<Self>, iface: InterfaceHandle) {
        // `recv_closed` erroring = channel closed (interface gone) → terminal, stop the pump.
        while let Ok(neighbor) = iface.recv_closed().await {
            // A dial still waiting for its challenge answer over this connection gets its answer
            // now: the far end closed it (a relay-only node refusing us). Left alone, the dialer
            // re-sent into the dead connection for 4 × 2.5 s before trying a route through a relay —
            // longer than its callers wait (stand 2026-10-14: the contact stayed «searching»).
            let pending = self.outbound_challenges.lock().remove(&neighbor);
            if let Some(OutboundChallenge { role: ChallengeRole::Dialer { waker }, .. }) = pending {
                let _ = waker.send(Err(NetworkError::ChallengeRejected(
                    "the far end closed the connection".to_string(),
                )));
            }
            self.prune_path(&neighbor);
        }
    }

    /// Per-interface bulk pump: hand inbound bulk streams (attachment transfer) to the owner,
    /// tagged with the peer IDENTITY that opened them.
    ///
    /// Same per-hop auth as the media pump: only a verified neighbour may open a bulk stream, so an
    /// unverified or spoofed source cannot push a multi-gigabyte body at us. The bulk plane is
    /// direct-only (no relay leg), so the opener's identity IS the verified neighbour's — no
    /// routing envelope to parse.
    async fn bulk_dispatcher_loop(self: Arc<Self>, iface: InterfaceHandle) {
        loop {
            let (neighbor, stream) = match iface.accept_bulk_stream().await {
                Ok(pair) => pair,
                // Channel closed (interface gone) — terminal, stop the pump.
                Err(_) => break,
            };
            let Some(peer) = self.verified_neighbors.lock().get(&neighbor).copied() else {
                // Unverified neighbour: drop the stream (closes it) rather than deliver it.
                continue;
            };
            if self.bulk_tx.send((peer, stream)).await.is_err() {
                break; // owner dropped the receiver
            }
        }
    }

    /// Receive the next inbound bulk stream from any peer, with the identity that opened it.
    /// Blocks until one arrives; the owner (mesh runtime) pumps this in a dedicated task.
    pub async fn recv_bulk_stream(&self) -> io::Result<(NodeAddress, BulkStream)> {
        let mut rx = self.bulk_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::other("bulk stream channel closed"))
    }

    /// Drop all routing/path state for a dead `neighbor`: it is no longer a verified inbound source
    /// or a send path. The neighbor may be the next-hop in MANY peers' PathSets — its own identity's
    /// DIRECT path AND every RELAYED path that used it as the relay next-hop (a relay neighbour fans
    /// out to lots of destinations). Remove it from **all** of them so `Link::send_failover` / `best()`
    /// never pick a path over a dead connection. Other paths to those peers survive. The per-identity
    /// reliable + datagram inboxes are NOT dropped here — keyed by peer, they persist across path
    /// churn (reused on re-verify). Any peer left with no path is surfaced as `peer_down`.
    ///
    /// Before this swept all PathSets, a relay connection that churned (e.g. the relay restarted, or a
    /// re-dial opened a new connection and the old one died) left a STALE relayed path behind in every
    /// destination's PathSet; `best()` could then keep sending into the dead connection and the chat /
    /// media silently stopped crossing the relay. См. docs/bugs/relay-pairing-approval-gap.md.
    fn prune_path(&self, neighbor: &crate::interface::NeighborId) {
        let own_identity = self.verified_neighbors.lock().remove(neighbor);
        self.last_seen.lock().remove(neighbor);
        // Clear every routing-table entry that forwards over this dead neighbour, too — otherwise
        // `decide_data_forward` (which routes purely by `path_table.lookup`) keeps choosing the dead
        // next-hop and forwarding into a black hole. The liveness seed re-points to the peer's live
        // connection on its next inbound frame.
        self.path_table.remove_via_neighbor(neighbor);
        // Sweep the dead neighbour out of EVERY peer's PathSet; collect peers left with no path, and
        // (separately) peers that LOST a path but still have a relay fallback — those just dropped to
        // relay-only and should be re-punched toward direct fast, not on the next reconcile tick.
        let mut fell_to_relay: Vec<NodeAddress> = Vec::new();
        let mut newly_unreachable: Vec<NodeAddress> = {
            let map = self.peer_paths.lock();
            map.iter()
                .filter_map(|(peer, set)| {
                    let mut guard = set.lock();
                    if !guard.remove(neighbor) {
                        return None;
                    }
                    match guard.best() {
                        None => Some(*peer),
                        Some(p) => {
                            if matches!(p.quality, PathQuality::Relay { .. }) {
                                fell_to_relay.push(*peer);
                            }
                            None
                        }
                    }
                })
                .collect()
        };
        // A dropped DIRECT link that still has a relay fallback → signal a fast re-punch so direct is
        // restored without waiting for the reconcile timer (the direct→relay→direct cycle).
        for peer in fell_to_relay {
            if peer != self.local_address {
                let _ = self.relay_upgrade_tx.send(peer);
            }
        }
        // The neighbour's own proven identity may have had no PathSet entry at all (verified but no
        // path registered) — still surface it as down so the app tears down stale session state.
        if let Some(peer) = own_identity {
            if !self.peer_paths.lock().contains_key(&peer) && !newly_unreachable.contains(&peer) {
                newly_unreachable.push(peer);
            }
        }
        // A peer still routable through a live neighbour is not down: losing a direct connection to it
        // (one a relay-only node refused, a stale one) leaves the route through the relay standing.
        // Reporting it down made the app drop the session to a contact whose frames were arriving
        // through the relay that very second, re-send its hello and lose it again, every few seconds
        // (stand 2026-10-14, a pinned relay: «will deliver later» with traffic flowing both ways).
        let now = Instant::now();
        newly_unreachable.retain(|peer| {
            !self
                .path_table
                .lookup(peer, now)
                .is_some_and(|route| self.verified_neighbors.lock().contains_key(&route.next_hop_neighbor))
        });
        // Surface fully-unreachable peers so the app clears stale session state and the next
        // message falls through to durable instead of a dead link.
        for peer in newly_unreachable {
            let _ = self.peer_down_tx.try_send(peer);
        }
    }

    /// Drain the next peer that became fully unreachable (last path pruned). The app
    /// uses this to clear stale connection state proactively. See
    /// docs/decisions/headless-omni-node.md (proactive teardown follow-up).
    pub fn try_recv_peer_down(&self) -> Option<NodeAddress> {
        let mut rx = self.peer_down_rx.try_lock().ok()?;
        rx.try_recv().ok()
    }

    /// Drain a DHT-proxy request addressed to us (R7 Phase 2 mode b). The app serves it via its local
    /// `omni-dht-helper` (after an opt-in gate) and answers with [`Network::send_dht_proxy_response`].
    /// `None` when nothing is queued. See docs/decisions/dht-over-peer.md.
    pub fn try_recv_dht_proxy_request(&self) -> Option<DhtProxyRequestFrame> {
        let mut rx = self.dht_proxy_request_rx.try_lock().ok()?;
        rx.try_recv().ok()
    }

    /// Originate a DHT-proxy response toward the requester `to` (we are the proxy that ran the op on
    /// our local DHT). Fills `from` = our identity + a fresh hop budget, then routes via the path_table
    /// next hop, best-effort (dropped if we have no route back — the requester times out + falls back).
    /// See docs/decisions/dht-over-peer.md.
    pub async fn send_dht_proxy_response(&self, to: NodeAddress, req_id: u64, ok: bool, value: Vec<u8>) {
        let resp = DhtProxyResponseFrame {
            from: self.local_address,
            to,
            req_id,
            ok,
            value,
            hop_limit: PEER_PING_HOP_LIMIT,
        };
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&to, now) {
            if let Some(out) = self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface) {
                let wire = NetworkFrame::DhtProxyResponse(resp).encode_to_wire();
                let _ = out.send_frame(&entry.next_hop_neighbor, &wire).await;
            }
        }
    }

    /// Drain a DHT bootstrap seed request addressed to us (step B): `(requester identity, req_id)`. The
    /// app answers with its configured bootstrap list via `send_dht_boot_response` (gated on
    /// DHT-rendezvous-enabled). `None` when nothing is queued. See dht-bootstrap-mesh-propagation.md.
    pub fn try_recv_dht_boot_request(&self) -> Option<(NodeAddress, u64)> {
        let mut rx = self.dht_boot_request_rx.try_lock().ok()?;
        rx.try_recv().ok()
    }

    /// Answer a DHT bootstrap seed request (step B): send our configured candidates back toward the
    /// requester `to` (a direct neighbour). Routed via the path_table next hop (1 hop for a direct
    /// neighbour); best-effort (dropped if no route back — the requester times out + retries). The
    /// candidate list is capped to `DHT_BOOT_MAX_NODES` by the encoder. See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub async fn send_dht_boot_response(&self, to: NodeAddress, req_id: u64, nodes: Vec<SocketAddr>) {
        let resp = DhtBootRespFrame { req_id, nodes };
        let now = Instant::now();
        if let Some(entry) = self.path_table.lookup(&to, now) {
            if let Some(out) = self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface) {
                let wire = NetworkFrame::DhtBootResp(resp).encode_to_wire();
                let _ = out.send_frame(&entry.next_hop_neighbor, &wire).await;
            }
        }
    }

    /// Ask a linked `peer` for the in-segment DHT bootstrap addresses it knows (step B, requester side).
    /// Sends a `DhtBootReq` toward the peer's direct link and awaits the matching `DhtBootResp` (by a
    /// fresh `req_id`) bounded by `deadline`. `None` = no route / no answer in time (peer not sharing).
    /// The returned addresses are candidates to merge into our own bootstrap list — a lying peer can
    /// only return junk that never dials (fail-closed). Intended for DIRECT neighbours; a non-forwarded
    /// request to a multi-hop peer is simply answered by the first hop (still a useful seed source). See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub async fn request_dht_bootstrap(
        &self,
        peer: NodeAddress,
        deadline: Duration,
    ) -> Option<Vec<SocketAddr>> {
        let req_id = OsRng.next_u64();
        let (tx, rx) = oneshot::channel();
        self.dht_boot_pending.lock().insert(req_id, tx);

        let req = DhtBootReqFrame { req_id };
        let now = Instant::now();
        let routed = match self.path_table.lookup(&peer, now) {
            Some(entry) => match self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface) {
                Some(out) => {
                    let wire = NetworkFrame::DhtBootReq(req).encode_to_wire();
                    out.send_frame(&entry.next_hop_neighbor, &wire).await.is_ok()
                }
                None => false,
            },
            None => false,
        };
        if !routed {
            self.dht_boot_pending.lock().remove(&req_id);
            return None;
        }
        let result = timeout(deadline, rx).await;
        self.dht_boot_pending.lock().remove(&req_id);
        match result {
            Ok(Ok(nodes)) => Some(nodes),
            _ => None,
        }
    }

    /// Run a BEP44 op on a PROXY peer's DHT over the mesh (R7 Phase 2 mode b) — for when our own DHT is
    /// unreachable. Routes a `DhtProxyRequest` toward `proxy` and awaits the matching response (by a
    /// fresh `req_id`) bounded by `deadline`. `None` = no route / no answer in time (proxy down or not
    /// serving) → the caller falls back (N-of-many). See docs/decisions/dht-over-peer.md.
    async fn dht_proxy_op(
        &self,
        proxy: NodeAddress,
        op: DhtProxyOp,
        key: [u8; 32],
        salt: Vec<u8>,
        value: Vec<u8>,
        deadline: Duration,
    ) -> Option<(bool, Vec<u8>)> {
        let req_id = OsRng.next_u64();
        let (tx, rx) = oneshot::channel();
        self.dht_proxy_pending.lock().insert(req_id, tx);

        // Route the request toward the proxy via the path_table next hop.
        let req = DhtProxyRequestFrame {
            from: self.local_address,
            to: proxy,
            req_id,
            op,
            key,
            salt,
            value,
            hop_limit: PEER_PING_HOP_LIMIT,
        };
        let now = Instant::now();
        let routed = match self.path_table.lookup(&proxy, now) {
            Some(entry) => match self.interfaces.iter().find(|i| i.id() == entry.next_hop_interface) {
                Some(out) => {
                    let wire = NetworkFrame::DhtProxyRequest(req).encode_to_wire();
                    out.send_frame(&entry.next_hop_neighbor, &wire).await.is_ok()
                }
                None => false,
            },
            None => false,
        };
        if !routed {
            self.dht_proxy_pending.lock().remove(&req_id);
            return None;
        }

        let result = timeout(deadline, rx).await;
        self.dht_proxy_pending.lock().remove(&req_id);
        match result {
            Ok(Ok(answer)) => Some(answer),
            _ => None, // timeout, or the oneshot was dropped
        }
    }

    /// Read a BEP44 slot via a proxy peer's DHT (mode b). `Some(value)` on a successful response (the
    /// value may be empty = empty slot); `None` on timeout / no route / proxy error.
    /// BEP 5 `get_peers` under `info_hash` via `proxy`'s DHT over the mesh: the relay directory's
    /// Mainline shelves when our own Mainline is dead. `None` — no route / no answer in time.
    pub async fn dht_peers_over_peer(
        &self,
        proxy: NodeAddress,
        info_hash: [u8; 20],
        deadline: Duration,
    ) -> Option<Vec<std::net::SocketAddrV4>> {
        let mut key = [0u8; 32];
        key[..20].copy_from_slice(&info_hash);
        match self.dht_proxy_op(proxy, DhtProxyOp::Peers, key, Vec::new(), Vec::new(), deadline).await {
            Some((true, packed)) => Some(crate::protocol::unpack_ipv4_peers(&packed)),
            _ => None,
        }
    }

    pub async fn dht_get_over_peer(
        &self,
        proxy: NodeAddress,
        key: [u8; 32],
        salt: Vec<u8>,
        deadline: Duration,
    ) -> Option<Vec<u8>> {
        match self.dht_proxy_op(proxy, DhtProxyOp::Get, key, salt, Vec::new(), deadline).await {
            Some((true, value)) => Some(value),
            _ => None,
        }
    }

    /// Write a BEP44 slot via a proxy peer's DHT (mode b). `true` if the proxy reported success.
    pub async fn dht_put_over_peer(
        &self,
        proxy: NodeAddress,
        key: [u8; 32],
        salt: Vec<u8>,
        value: Vec<u8>,
        deadline: Duration,
    ) -> bool {
        matches!(
            self.dht_proxy_op(proxy, DhtProxyOp::Put, key, salt, value, deadline).await,
            Some((true, _))
        )
    }

    /// AutoNAT (P2): ask a verified `peer` to dial us back at `candidate` and
    /// prove it by returning a probe. Records the pending nonce and sends a
    /// `DialBackRequest` over the peer's best path. A later `DialBackProbe` with
    /// this nonce flips us to `Reachable`. Returns the request nonce.
    pub async fn request_dial_back(
        &self,
        peer: &NodeAddress,
        candidate: SocketAddr,
    ) -> io::Result<u64> {
        // Send over a DIRECT QuicOverUdp path (not `best()`, which may be the TCP-relay base): the
        // verifier must observe our REAL public source IP for its dial-back anti-amplification guard
        // to fire, and the TCP bridge would present its loopback instead. No such path ⇒ AutoNAT
        // can't validate this cycle. См. `verified_wan_peer` + connectivity-upgrade-via-peer-relay.md.
        let path = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| {
                set.lock()
                    .paths
                    .iter()
                    .find(|p| {
                        matches!(p.quality, PathQuality::Direct)
                            && p.iface.kind() == InterfaceKind::QuicOverUdp
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "no direct UDP path to the dial-back verifier (the TCP bridge presents loopback)",
                )
            })?;
        let nonce = OsRng.next_u64();
        self.reachability.lock().issue(nonce, candidate);
        let frame = NetworkFrame::DialBackRequest(DialBackRequestFrame { nonce, candidate });
        path.iface
            .send_frame(&path.neighbor, &frame.encode_to_wire())
            .await?;
        // Schedule a timeout: if no probe confirms this nonce, count it as a
        // verifier failure (≥ threshold ⇒ Unreachable). on_timeout is a no-op if
        // the nonce was already resolved by a probe.
        let reachability = Arc::clone(&self.reachability);
        tokio::spawn(async move {
            tokio::time::sleep(DIAL_BACK_TIMEOUT).await;
            reachability.lock().on_timeout(nonce);
        });
        Ok(nonce)
    }

    /// Handle an inbound `DialBackRequest`: a peer wants us to verify it is
    /// reachable at `candidate`. Make a FRESH dial to that address and send back a
    /// `DialBackProbe`. **Anti-amplification:** only probe when the candidate IP
    /// matches the source IP the request arrived from — so we can never be tricked
    /// into flooding a third party (the requester must be asking about itself).
    fn handle_dial_back_request(
        &self,
        req: DialBackRequestFrame,
        iface: &InterfaceHandle,
        neighbor: &NeighborId,
    ) {
        match iface.remote_addr(neighbor) {
            Some(src) if src.ip() == req.candidate.ip() => {}
            // Mismatched or unknown source — refuse (would be an amplifier to a victim).
            _ => return,
        }
        let iface = Arc::clone(iface);
        let candidate = req.candidate;
        let nonce = req.nonce;
        tokio::spawn(async move {
            if let Ok(target) = iface.connect_endpoint(&candidate.to_string()).await {
                let probe = NetworkFrame::DialBackProbe(DialBackProbeFrame { nonce });
                let _ = iface.send_frame(&target, &probe.encode_to_wire()).await;
            }
        });
    }

    /// Current AutoNAT reachability verdict (Unknown / Reachable / Unreachable).
    pub fn reachability_state(&self) -> ReachabilityState {
        self.reachability.lock().state
    }

    /// The candidate address a verifier confirmed reachable, if any.
    pub fn reachable_address(&self) -> Option<SocketAddr> {
        self.reachability.lock().confirmed
    }

    /// Task 3 — ask `peer` (a verified WAN verifier) to TCP-connect to our `candidate` front door
    /// and report whether it's reachable. The TCP analog of [`request_dial_back`](Self::request_dial_back):
    /// sent over a DIRECT QuicOverUdp path so the verifier observes our REAL WAN source IP (its
    /// anti-abuse guard only TCP-probes a candidate whose IP matches the requester). A
    /// `TcpReachabilityResult{reachable:true}` confirms our front door. Returns the request nonce.
    pub async fn request_tcp_reachability(
        &self,
        peer: &NodeAddress,
        candidate: SocketAddr,
    ) -> io::Result<u64> {
        let path = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| {
                set.lock()
                    .paths
                    .iter()
                    .find(|p| {
                        matches!(p.quality, PathQuality::Direct)
                            && p.iface.kind() == InterfaceKind::QuicOverUdp
                    })
                    .cloned()
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "no direct UDP path to the TCP-reachability verifier",
                )
            })?;
        let nonce = OsRng.next_u64();
        *self.pending_tcp_confirm.lock() = Some((nonce, candidate));
        let frame =
            NetworkFrame::TcpReachabilityRequest(TcpReachabilityRequestFrame { nonce, candidate });
        path.iface
            .send_frame(&path.neighbor, &frame.encode_to_wire())
            .await?;
        Ok(nonce)
    }

    /// Handle an inbound `TcpReachabilityRequest`: a peer wants us to confirm its TCP front door is
    /// reachable. **Anti-abuse:** only probe when `candidate`'s IP matches the source IP the request
    /// arrived from — so we can't be tricked into TCP-scanning a third party (mirrors the UDP
    /// dial-back guard). TCP-connect (timeout-bounded) and report the result back over the SAME path.
    fn handle_tcp_reachability_request(
        &self,
        req: TcpReachabilityRequestFrame,
        iface: &InterfaceHandle,
        neighbor: &NeighborId,
    ) {
        match iface.remote_addr(neighbor) {
            Some(src) if src.ip() == req.candidate.ip() => {}
            // Mismatched / unknown source — refuse (would be a third-party TCP scanner).
            _ => return,
        }
        let iface = Arc::clone(iface);
        let neighbor = neighbor.clone();
        let candidate = req.candidate;
        let nonce = req.nonce;
        tokio::spawn(async move {
            // A bare TCP handshake (no TLS) proves the port forwards + the front door accepts; the
            // dropped conn is harmless (the bridge's per-conn TLS accept just fails and ends).
            let reachable =
                tokio::time::timeout(TCP_CONFIRM_TIMEOUT, tokio::net::TcpStream::connect(candidate))
                    .await
                    .map(|r| r.is_ok())
                    .unwrap_or(false);
            let result = NetworkFrame::TcpReachabilityResult(TcpReachabilityResultFrame {
                nonce,
                reachable,
            });
            let _ = iface.send_frame(&neighbor, &result.encode_to_wire()).await;
        });
    }

    /// Handle a `TcpReachabilityResult`: if it matches our in-flight request and the verifier could
    /// connect, our TCP front door is confirmed externally reachable → safe to advertise.
    fn handle_tcp_reachability_result(&self, res: TcpReachabilityResultFrame) {
        let mut pending = self.pending_tcp_confirm.lock();
        if let Some((nonce, candidate)) = *pending {
            if nonce == res.nonce {
                *pending = None;
                if res.reachable {
                    *self.confirmed_tcp_address.lock() = Some(candidate);
                }
            }
        }
    }

    /// The TCP front door a verifier confirmed externally reachable, if any — advertised in the
    /// beacon `tcp_direct` only once confirmed (validate-before-advertise). See tcp-direct-peer-fallback.md.
    pub fn confirmed_tcp_address(&self) -> Option<SocketAddr> {
        *self.confirmed_tcp_address.lock()
    }

    /// Pick a verified neighbour we reach over a **direct QuicOverUdp path to a routable-WAN
    /// address** — the right AutoNAT verifier for confirming our OWN WAN candidate (UPnP / reflexive).
    /// The verifier dials our candidate back (`handle_dial_back_request`), whose anti-amplification
    /// guard fires only when the candidate IP equals the source IP it observes for us. That holds
    /// **only** when the verifier reaches us over the WAN AND over **UDP**:
    /// - DIRECT, not multi-hop via a relay (a relayed path presents the relay's IP), and
    /// - a WAN address, not a LAN neighbour (which sees our private IP), and
    /// - **QuicOverUdp, not the TCP bridge** — the bridge tunnels QUIC to the verifier's loopback, so
    ///   the verifier observes `127.0.0.1` as our source and the guard would reject the dial-back
    ///   (proven live: AutoNAT over the TCP-bridge carrier falsely concluded "behind NAT" because the
    ///   relay saw loopback). См. docs/decisions/connectivity-upgrade-via-peer-relay.md.
    ///
    /// In practice this is the bootstrap relay reached over its UDP carrier. `None` ⇒ AutoNAT can't
    /// run this cycle (we hold no direct UDP WAN peer — e.g. only the TCP relay base), so reachability
    /// stays honestly Unknown rather than a false Unreachable. P2 AutoNAT.
    pub fn verified_wan_peer(&self) -> Option<NodeAddress> {
        let verified = self.verified_neighbors.lock();
        let map = self.peer_paths.lock();
        for (peer, set) in map.iter() {
            if *peer == self.local_address {
                continue;
            }
            let set = set.lock();
            let eligible = set.paths.iter().any(|p| {
                matches!(p.quality, PathQuality::Direct)
                    && p.iface.kind() == InterfaceKind::QuicOverUdp
                    && verified.get(&p.neighbor) == Some(peer)
                    && p.iface
                        .remote_addr(&p.neighbor)
                        .is_some_and(|addr| is_enumerable_wan(&addr))
            });
            if eligible {
                return Some(*peer);
            }
        }
        None
    }

    /// Quality of the current best path to `peer` (None if we have no path to it).
    /// `has_link`-style queries are binary; this distinguishes a `Direct` link from a
    /// `Relay { hops }` one so the UI can honestly say "online — direct" vs "online —
    /// via N hops". Read-only snapshot of the path selector; maps the crate-private
    /// `PathQuality` to the public `LinkQuality`. See docs/decisions/omni-network-model.md.
    pub fn peer_link_quality(&self, peer: &NodeAddress) -> Option<LinkQuality> {
        self.peer_link_quality_at(peer, Instant::now())
    }

    /// `peer_link_quality` with an injected `now` (unit-testable). A `Relay` best-path is reported as
    /// reachable ONLY with proven-fresh end-to-end liveness — `peer_last_seen` within
    /// `RELAY_PEER_DEAD_AFTER` (None = grace, just-registered/not-yet-probed). Without this gate a relay
    /// path re-added by autoconnect re-dial / `upgrade_relay_carriers` between prune ticks would read
    /// "online via relay" for a dead peer (the relay-presence bug). A `Direct` best-path needs no gate —
    /// per-neighbour `liveness_plan` prunes it promptly. See docs/bugs/relay-presence-false-online-dead-peer.md.
    fn peer_link_quality_at(&self, peer: &NodeAddress, now: Instant) -> Option<LinkQuality> {
        let best = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| set.lock().best())?;
        match best.quality {
            PathQuality::Direct => Some(LinkQuality::Direct),
            PathQuality::Relay { hops } => {
                self.relay_peer_fresh_at(peer, now).then_some(LinkQuality::Relay { hops })
            }
        }
    }

    /// Whether a relay-only peer's end-to-end liveness is proven-fresh at `now`: `peer_last_seen` newer
    /// than `RELAY_PEER_DEAD_AFTER`, or no entry yet (grace — a just-registered peer not yet probed,
    /// mirrors `relay_peer_liveness_plan`'s `None => alive`). The single freshness predicate behind both
    /// `peer_link_quality` (the Relay arm) and the relay-only prune, so the readout and the prune agree.
    fn relay_peer_fresh_at(&self, peer: &NodeAddress, now: Instant) -> bool {
        match self.peer_last_seen.lock().get(peer) {
            None => true,
            Some(last) => now.saturating_duration_since(*last) < RELAY_PEER_DEAD_AFTER,
        }
    }

    /// The carrier of the current best path to `peer` (R1a connection-quality ladder) — the rung
    /// distinguisher that `LinkQuality` alone cannot give: a TCP-relay (rung 0) and a UDP-relay
    /// (rung 1) are both `Relay { hops: 1 }`, but ride `QuicOverTcp` vs `QuicOverUdp`. None if no
    /// path. Read-only; used by the climb-verification readout.
    pub fn peer_best_carrier(&self, peer: &NodeAddress) -> Option<InterfaceKind> {
        let best = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| set.lock().best())?;
        Some(best.iface.kind())
    }

    /// Факты ЖИВОГО линка с пиром: адрес, по которому мы с ним соединены, и измеренный
    /// транспортом проход туда-обратно. Оба берутся с лучшего пути — одна выборка, один замок,
    /// поэтому они заведомо про одно и то же соединение.
    ///
    /// Существует затем, чтобы наверх шли ИЗМЕРЕНИЯ, а не пересказ: панель связи печатала
    /// «Пинг», посчитанный по presence-обмену (322 мс медиана при 49 мс на транспорте), и
    /// «Endpoint», в котором лежал адрес УЗЛА, а не сокета. См.
    /// docs/bugs/ping-was-our-own-pipeline.md.
    ///
    /// `None` — линка нет. Внутри пары любое поле может быть `None`: не всякий интерфейс
    /// адресный и не всякий меряет RTT (multicast, будущее радио).
    /// Применить смену контроллера перегрузки на всех интерфейсах. Возвращает, сколько соединений
    /// пришлось закрыть — ноль значит «применять было не к чему», и вызывающий не должен
    /// показывать это как «применено к живому линку».
    ///
    /// Дешёвая замена перезапуску меша: тот сносит дискавери, анонсы и проброс порта и стоит
    /// больше минуты без связи, тогда как здесь переживает всё, кроме самих QUIC-соединений.
    pub fn refresh_congestion_control(&self) -> std::io::Result<usize> {
        let mut closed = 0usize;
        for iface in &self.interfaces {
            closed += iface.refresh_congestion_control()?;
        }
        Ok(closed)
    }

    pub fn peer_link_facts(&self, peer: &NodeAddress) -> Option<PeerLinkFacts> {
        let best = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| set.lock().best())?;
        Some(PeerLinkFacts {
            remote: best.iface.remote_addr(&best.neighbor).map(unmap_ipv4_mapped),
            rtt: best.iface.connection_rtt(&best.neighbor),
        })
    }

    /// Whether `peer`'s direct UDP-carrier connection is currently routed over its TCP door (Meta-B
    /// mux migration) rather than native UDP. `peer_best_carrier` reports the carrier INTERFACE kind
    /// (`QuicOverUdp` even when the mux under it rides the door), so it alone reads `direct:udp` on the
    /// door — a lie the honest readout needs to correct. False if the peer has no direct UDP path or
    /// isn't on a door. See [`crate::mux_socket`].
    pub fn peer_on_door(&self, peer: &NodeAddress) -> bool {
        // The BEST path decides — what traffic actually rides. With a door path and an upgraded
        // UDP path side by side, "the first direct path" kept answering "on the door".
        let best = self
            .peer_paths
            .lock()
            .get(peer)
            .and_then(|set| set.lock().best());
        best.as_ref().is_some_and(path_rides_door)
    }

    /// Snapshot the DIRECT endpoints per peer — the source for the warm-rejoin cache (G3). Each
    /// Direct path contributes the socket it was reached on, which a restart re-dials straight away instead of
    /// re-running LAN/UPnP/rendezvous cold. Every Direct path, not just the first: a peer reached
    /// over two carriers keeps both. Mesh-relay-only peers are skipped — the relay's address is not
    /// the peer's. Read-only snapshot. See docs/decisions/omni-network-model.md (D7).
    pub fn snapshot_peer_book(&self) -> Vec<(NodeAddress, String)> {
        let paths = self.peer_paths.lock();
        let mut out = Vec::new();
        for (peer, set) in paths.iter() {
            let set = set.lock();
            for path in set.paths.iter().filter(|p| p.quality == PathQuality::Direct) {
                if let Some(endpoint) = path.iface.remote_addr(&path.neighbor).map(|addr| addr.to_string()) {
                    if !out.contains(&(*peer, endpoint.clone())) {
                        out.push((*peer, endpoint));
                    }
                }
            }
        }
        out
    }

    /// Subscribe to a synthetic group destination (G3): frames addressed to it
    /// are delivered to OUR inbox (we are a member) and gossip-pushed once to
    /// `members` (the co-member node addresses) when they have live paths.
    /// Re-subscribing replaces the member push set (membership changed).
    pub fn subscribe_group_destination(&self, dest: NodeAddress, members: Vec<NodeAddress>) {
        let newly = self.group_subscriptions.lock().insert(dest, members).is_none();
        if !newly {
            return;
        }
        // Frames of this group that arrived BEFORE we subscribed were only held (e.g. the first
        // message after our Welcome, sent while we were still joining). Our reconcile marks say we
        // have them, so no peer will ever offer them again — hand them to the inbox now, like
        // `import_durable` does after a restart. Oldest first per feed; the app dedups.
        let mut held: Vec<DurableDataFrame> = self
            .durable_store
            .lock()
            .frames
            .values()
            .filter(|(frame, _)| frame.to == dest && frame.from != self.local_address)
            .map(|(frame, _)| frame.clone())
            .collect();
        held.sort_by(|a, b| a.feed_id.cmp(&b.feed_id).then(a.seq.cmp(&b.seq)));
        for frame in held {
            let _ = self.durable_inbox_tx.try_send(frame);
        }
    }

    /// Withdraw a group subscription (left/removed from the group). Frames for
    /// this destination degrade to ordinary store-and-forward holding.
    pub fn unsubscribe_group_destination(&self, dest: &NodeAddress) {
        self.group_subscriptions.lock().remove(dest);
    }

    /// Handle an inbound DurableData (P4): dedup by `(feed, seq)`; store it (so we
    /// can store-and-forward + answer anti-entropy); if it's addressed to us —
    /// or to a group destination we subscribe to (G3) — deliver it to the durable
    /// inbox. Holding a frame for someone else is the store-and-forward that
    /// bridges non-overlapping online windows (A→B→C). Returns the co-member
    /// nodes the (async) caller should gossip-push this frame to: only for a
    /// NEWLY stored frame of a subscribed group, only members with a live path,
    /// never back toward the original sender — dedup at every hop bounds the
    /// flood to ~one copy per link.
    fn handle_durable_data(&self, frame: DurableDataFrame) -> Vec<NodeAddress> {
        // Admit into the bounded store (dedup + TTL + global/per-source caps). If
        // it's a duplicate / expired / over a cap, stop — don't re-reconcile or
        // re-deliver.
        let needed = self.needs_durable_frame(&frame);
        let newly_stored = self.durable_store.lock().insert(
            frame.clone(),
            Instant::now(),
            MAX_DURABLE_FRAMES,
            MAX_DURABLE_FRAMES_PER_SOURCE,
            needed,
        );
        if !newly_stored {
            return Vec::new();
        }
        self.durable_reconcile
            .lock()
            .insert(frame.feed_id, frame.seq);
        if frame.to == self.local_address {
            let _ = self.durable_inbox_tx.try_send(frame);
            return Vec::new();
        }
        let members = self.group_subscriptions.lock().get(&frame.to).cloned();
        if let Some(members) = members {
            let push_targets: Vec<NodeAddress> = {
                let paths = self.peer_paths.lock();
                members
                    .into_iter()
                    .filter(|member| {
                        *member != frame.from
                            && *member != self.local_address
                            && paths.contains_key(member)
                    })
                    .collect()
            };
            // Our own outbound frame is already applied locally — deliver only
            // co-members' frames to the inbox.
            if frame.from != self.local_address {
                let _ = self.durable_inbox_tx.try_send(frame);
            }
            return push_targets;
        }
        Vec::new()
        // else: held for store-and-forward; anti-entropy hands it on when the
        // destination becomes reachable.
    }

    /// A durable frame this node needs itself rather than only carries: ours, addressed to us, or
    /// for a group we are in. See `DurableStore::insert`.
    fn needs_durable_frame(&self, frame: &DurableDataFrame) -> bool {
        frame.from == self.local_address
            || frame.to == self.local_address
            || self.group_subscriptions.lock().contains_key(&frame.to)
    }

    /// Push an already-stored durable frame to specific peers over their best
    /// current paths (group gossip). Best-effort — anti-entropy is the catch-all.
    async fn push_durable_to(&self, frame: &DurableDataFrame, targets: &[NodeAddress]) {
        if targets.is_empty() {
            return;
        }
        let wire = NetworkFrame::DurableData(frame.clone()).encode_to_wire();
        for target in targets {
            let path = self
                .peer_paths
                .lock()
                .get(target)
                .and_then(|set| set.lock().best());
            if let Some(path) = path {
                let _ = path.iface.send_frame(&path.neighbor, &wire).await;
            }
        }
    }

    /// Send a DurableData frame toward `frame.to` over its best current path. It is
    /// stored locally first, so even with no path yet (or a failed send) we HOLD it
    /// and anti-entropy delivers it when a path appears — send is best-effort.
    /// For a subscribed group destination there is no single owner to route to —
    /// the frame is gossip-pushed to every linked co-member instead.
    pub async fn send_durable(&self, frame: DurableDataFrame) -> io::Result<()> {
        let push_targets = self.handle_durable_data(frame.clone());
        if push_targets.is_empty() {
            let group_members = self.group_subscriptions.lock().get(&frame.to).cloned();
            if let Some(members) = group_members {
                // A group frame pushed to nobody is otherwise silent: the sender's thread shows
                // the message, the members never get it until anti-entropy (live 2026-10-10).
                crate::tcp_transport::tcp_trace(
                    "group/push-none",
                    &format!("seq={} members={}", frame.seq, members.len()),
                );
            }
        }
        if !push_targets.is_empty() {
            self.push_durable_to(&frame, &push_targets).await;
            return Ok(());
        }
        let path = self
            .peer_paths
            .lock()
            .get(&frame.to)
            .and_then(|set| set.lock().best());
        match path {
            Some(path) => {
                let wire = NetworkFrame::DurableData(frame).encode_to_wire();
                path.iface.send_frame(&path.neighbor, &wire).await
            }
            None => Ok(()), // no path yet — held; anti-entropy will deliver.
        }
    }

    /// Drain the next durable frame addressed to us, if one has been delivered.
    pub fn try_recv_durable(&self) -> Option<DurableDataFrame> {
        let mut rx = self.durable_inbox_rx.try_lock().ok()?;
        rx.try_recv().ok()
    }

    /// The app has finished handling a durable frame addressed to us (the pump calls this after
    /// `on_wire` returns). From now on the frame is exported, so our high-water survives a restart.
    pub fn note_durable_handled(&self, feed: FeedId, seq: u64) {
        self.durable_handled.lock().insert((feed, seq));
    }

    /// Snapshot, for the app to persist (omni-storage) so it survives a restart: the store-and-
    /// forward frames we hold **for others** (`to != us`), plus the frames addressed to us that the
    /// app has already HANDLED. The latter restore our high-water on import, so holders stop
    /// re-offering a week of conversation after every restart — re-offers the app can recognise as
    /// duplicates only within its replay window, and otherwise reads as a broken session
    /// (docs/bugs/receipt-after-restart-fails-to-decrypt.md). A frame for us that the app had not
    /// handled is left out on purpose: the holder offers it again and it is delivered then.
    /// omni-network stays storage-agnostic; the app owns where these bytes live (Android-portable).
    /// См. mesh-durable-delivery.md.
    pub fn export_durable(&self) -> Vec<DurableDataFrame> {
        let me = self.local_address;
        let store = self.durable_store.lock();
        let mut handled = self.durable_handled.lock();
        // Forget marks whose frame has left the store (TTL / caps) — bounds the set.
        handled.retain(|key| store.get(key).is_some());
        store
            .frames()
            .filter(|f| f.to != me || handled.contains(&(f.feed_id, f.seq)))
            .cloned()
            .collect()
    }

    /// Restore previously-persisted store-and-forward frames at startup. Re-admits
    /// each through the bounded store (dedup + TTL + caps) and marks reconcile, so
    /// limits/dedup hold across restart and anti-entropy can offer them again.
    /// Store-only for frames held for others; a frame of a SUBSCRIBED group
    /// destination (register subscriptions BEFORE importing) is re-delivered to
    /// the inbox — our reconcile marks make peers never re-offer it, so this is
    /// the only path that gets an arrived-but-undrained group message to the app
    /// after a restart. The app dedups by message_id, so re-delivery is idempotent.
    pub fn import_durable(&self, frames: Vec<DurableDataFrame>) {
        let now = Instant::now();
        let stored: Vec<DurableDataFrame> = {
            let mut store = self.durable_store.lock();
            frames
                .into_iter()
                .filter(|frame| {
                    store.insert(
                        frame.clone(),
                        now,
                        MAX_DURABLE_FRAMES,
                        MAX_DURABLE_FRAMES_PER_SOURCE,
                        // Restoring old frames never evicts: within the caps, as they were.
                        false,
                    )
                })
                .collect()
        };
        let mut reconcile = self.durable_reconcile.lock();
        let subscriptions = self.group_subscriptions.lock();
        let mut handled = self.durable_handled.lock();
        for frame in stored {
            reconcile.insert(frame.feed_id, frame.seq);
            if frame.to == self.local_address {
                // Exported only once the app had handled it (`export_durable`): mark, don't deliver.
                handled.insert((frame.feed_id, frame.seq));
                continue;
            }
            if frame.from != self.local_address && subscriptions.contains_key(&frame.to) {
                let _ = self.durable_inbox_tx.try_send(frame);
            }
        }
    }

    // --- Anti-entropy reconciliation (P4 increment 3b) ---------------------
    // Two nodes converge by exchanging high-water vectors and sending each other
    // only what the other lacks: `have` (my high-water) → `offer` (what I hold
    // past your high-water, with msg_ids) → `request` (the msg_ids you still
    // lack) → DurableData. Kicked off on link-up; this is what actually moves a
    // store-and-forward-held frame to its destination. См. mesh-durable-delivery.md.

    /// The destinations we want anti-entropy frames for: the group destinations we're subscribed to,
    /// plus our own address (1:1 durable frames are addressed to us). We ADVERTISE this as our
    /// `interests` so a holder can offer us the right frames without us — or it — listing the full
    /// feed inventory. See docs/decisions/anti-entropy-have-scoping.md.
    fn own_interests(&self) -> Vec<NodeAddress> {
        let mut interests: Vec<NodeAddress> =
            self.group_subscriptions.lock().keys().copied().collect();
        interests.push(self.local_address);
        interests
    }

    /// Whether `node` currently FORWARDS traffic for us — it is the next hop for some OTHER
    /// destination in the routing table. A forwarder must still be offered our held frames even when
    /// it isn't a co-member, because store-and-forward rides through it. A bare neighbour that routes
    /// nothing of ours is NOT a forwarder — declining to advertise our inventory to it is the leak we
    /// close (the project invariant: a neighbour is a route candidate, not a trusted contact).
    fn is_forwarder(&self, node: &NodeAddress) -> bool {
        let neighbor_ids: Vec<NeighborId> = {
            let verified = self.verified_neighbors.lock();
            verified
                .iter()
                .filter(|(_, n)| **n == *node)
                .map(|(id, _)| id.clone())
                .collect()
        };
        if neighbor_ids.is_empty() {
            return false;
        }
        self.path_table
            .snapshot()
            .into_iter()
            .any(|(dest, entry)| dest != *node && neighbor_ids.contains(&entry.next_hop_neighbor))
    }

    /// Whether a held frame for `dest` is worth offering/advertising to a peer.
    ///
    /// Only frames for a GROUP WE ARE A MEMBER OF (`dest` ∈ our subscriptions) are scoped — those are
    /// the ones whose inventory reveals our membership graph. For them the peer must be a co-member
    /// (it advertised interest in `dest`, or it's in our member list), or forward for us (S&F rides
    /// through it). Everything else — 1:1 frames and frames we merely CARRY for others (dest not one
    /// of our groups) — FAILS OPEN, flooding to neighbours exactly as before, so store-and-forward to
    /// an unreachable destination still walks A→B→C. See docs/decisions/anti-entropy-have-scoping.md.
    fn feed_relevant_to_peer(
        &self,
        dest: &NodeAddress,
        peer_node: Option<&NodeAddress>,
        peer_interests: &[NodeAddress],
        peer_is_forwarder: bool,
    ) -> bool {
        match self.group_subscriptions.lock().get(dest) {
            None => true, // not one of our groups → 1:1 / carried S&F: flood as before
            Some(members) => {
                peer_is_forwarder
                    || peer_interests.contains(dest)
                    || peer_node.is_some_and(|p| members.contains(p))
            }
        }
    }

    /// Advertise to `peer_node` our INTERESTS (always — so it can offer us what we lack) and the
    /// high-water of the held feeds RELEVANT to it (its subscribed dests, or all if it forwards for
    /// us). Marks are scoped rather than the full inventory: a route-candidate that is neither a
    /// co-member nor a forwarder learns nothing about the feeds we carry. See
    /// docs/decisions/anti-entropy-have-scoping.md.
    async fn send_anti_entropy_have(
        &self,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
        peer_node: &NodeAddress,
    ) {
        let peer_interests = self
            .peer_interests
            .lock()
            .get(peer_node)
            .cloned()
            .unwrap_or_default();
        let peer_is_forwarder = self.is_forwarder(peer_node);
        // Feed → destination for what we currently HOLD; a GC'd feed drops out (nothing to serve).
        let held_feed_dests: HashMap<FeedId, NodeAddress> = {
            let store = self.durable_store.lock();
            let mut map = HashMap::new();
            for frame in store.frames() {
                map.entry(frame.feed_id).or_insert(frame.to);
            }
            map
        };
        // Filter by relevance FIRST (locks group_subscriptions), then read high-water (locks
        // reconcile) — never nested, so the lock order stays store → group_subscriptions → reconcile.
        let relevant_feeds: Vec<FeedId> = held_feed_dests
            .into_iter()
            .filter(|(_, dest)| {
                self.feed_relevant_to_peer(dest, Some(peer_node), &peer_interests, peer_is_forwarder)
            })
            .map(|(feed, _)| feed)
            .collect();
        let marks: Vec<(FeedId, u64)> = {
            let reconcile = self.durable_reconcile.lock();
            relevant_feeds
                .into_iter()
                .map(|feed| (feed, reconcile.state().high_water(&feed)))
                .filter(|(_, hw)| *hw > 0)
                .collect()
        };
        let frame = NetworkFrame::AntiEntropyHave(AntiEntropyHaveFrame {
            marks,
            interests: self.own_interests(),
        });
        let _ = iface.send_frame(neighbor, &frame.encode_to_wire()).await;
    }

    /// Periodically re-reconcile with every peer we have a path to (the runtime
    /// drives this on a timer). Link-up reconciliation only covers frames held at
    /// connect time; this propagates frames added later and heals gaps — so a
    /// store-and-forward frame walks A→B→C across links over time, not only when
    /// a fresh link comes up. See mesh-durable-delivery.md §anti-entropy.
    pub async fn reconcile_all_peers(&self) {
        let targets: Vec<(NodeAddress, NeighborId, InterfaceHandle)> = {
            let map = self.peer_paths.lock();
            map.iter()
                .filter_map(|(node, set)| {
                    set.lock().best().map(|p| (*node, p.neighbor, p.iface))
                })
                .collect()
        };
        for (node, neighbor, iface) in targets {
            self.send_anti_entropy_have(&neighbor, &iface, &node).await;
        }
    }

    /// Try to upgrade relay-only peers to a DIRECT link via hole-punch (P2 — the vision's
    /// "direct without a public IP"). For every peer we currently reach ONLY over a relay
    /// (best path is `Relay` — i.e. we are actively routing to it via a next-hop but have no
    /// direct link), coordinate a punch through the common relay (`request_punch`), let the
    /// pinholes open, then dial it directly. On success `dial` registers a `Direct` path that
    /// supersedes the relay in `best()`. Best-effort + idempotent: a peer that gains a direct
    /// path leaves the relay-only set; one that can't be punched (e.g. symmetric × symmetric
    /// NAT) keeps its working relay path and is retried next tick. The runtime calls this on
    /// the reconcile timer — without it the hole-punch lived only in tests and the real path
    /// stayed relay-only. См. docs/decisions/mesh-hole-punching.md.
    /// Set the master relay→direct **upgrade** gate (the `ConnectionPrivacyMode::RelayOnly` privacy
    /// choice). `false` ⇒ never climb a relay link to direct and never direct-dial in rendezvous — the
    /// data path stays relay-mediated so a contact can't learn our IP. `true` (default) ⇒ the full
    /// ladder. Live: the app calls this on mesh-start and whenever the user changes the privacy mode,
    /// and both the punch loop and the rendezvous worker read it on every attempt (no restart). An
    /// already-direct link is untouched — this only stops NEW upgrades. См. connection-quality-ladder.md.
    pub fn set_allow_direct_punch(&self, allow: bool) {
        self.allow_direct_punch
            .store(allow, std::sync::atomic::Ordering::Relaxed);
    }

    /// Hold direct connections only with `allowed` (`None` — with anyone). Direct links to anyone
    /// else are closed now, and a new one is refused when it verifies: the peer goes on through a
    /// relay (the `direct_only_with` field).
    pub async fn keep_direct_only_with(&self, allowed: Option<std::collections::HashSet<NodeAddress>>) {
        *self.direct_only_with.lock() = allowed.clone();
        let Some(allowed) = allowed else {
            return;
        };
        let refused: Vec<(NeighborId, NodeAddress)> = self
            .verified_neighbors
            .lock()
            .iter()
            .filter(|(_, peer)| !allowed.contains(peer))
            .map(|(neighbor, peer)| (neighbor.clone(), *peer))
            .collect();
        for (neighbor, peer) in refused {
            crate::tcp_transport::tcp_trace("net/direct-off", &format!("closing neighbor={neighbor:?} peer={peer:?}"));
            for iface in &self.interfaces {
                iface.close_neighbor(&neighbor).await;
            }
            self.prune_path(&neighbor);
        }
    }

    /// A direct link to `peer` is not kept (see `keep_direct_only_with`).
    fn direct_refused(&self, peer: &NodeAddress) -> bool {
        self.direct_only_with.lock().as_ref().is_some_and(|allowed| !allowed.contains(peer))
    }

    /// A connection at `neighbor` rides a carrier our hops do not use (see [`CarrierPreference`]).
    /// TCP: the relay's TCP carrier, a door we dialed (the mux routes it), or our own door's bridge.
    /// Discovery (`Mdns`) carries no link and is never refused.
    fn carrier_refused(&self, iface: &InterfaceHandle, neighbor: &NeighborId) -> bool {
        let rides_tcp = match iface.kind() {
            InterfaceKind::QuicOverTcp => true,
            InterfaceKind::QuicOverUdp => iface.remote_addr(neighbor).is_some_and(|addr| {
                iface.peer_on_door(addr) || crate::tcp_transport::is_door_bridge_source(addr)
            }),
            InterfaceKind::Mdns => return false,
        };
        match self.carrier_preference() {
            CarrierPreference::Auto => false,
            CarrierPreference::Tcp => !rides_tcp,
            CarrierPreference::Quic => rides_tcp,
        }
    }

    /// Carry our own hops over this carrier (see [`CarrierPreference`]).
    pub fn set_carrier_preference(&self, preference: CarrierPreference) {
        self.carrier_preference
            .store(preference as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// Change the carrier of our hops on a running network, at once. New dials and inbound
    /// connections follow the new preference from here; a direct connection it refuses is closed and
    /// the autoconnect redials it on the allowed carrier — over the peer's door for TCP
    /// (`door_for_dial`: its LAN door, else its public door from `doors`), over UDP for QUIC. Returns
    /// how many connections were closed.
    ///
    /// Nothing is migrated in place, in either direction: a closed link came back over the allowed
    /// carrier in seconds on the stand, and a call survived it. Both migrations had failed live —
    /// the mux moved the peer's connection too, where we are the server, which its client drops
    /// (fixed since in `route_peer_over_door` / `migrate_door_connection_back_to_udp`, not yet
    /// proven live, so the switch keeps the path that is):
    /// - back from a door to UDP (`migrate_door_connection_back_to_udp`, laptop Wi-Fi <-> PC,
    ///   2026-10-17): 0 of 4, a 6 s blackout per door path; closed, the UDP link was back in 0.7 s;
    /// - from UDP onto a LAN door (`route_peer_over_door`, two instances on one laptop, 2026-10-17):
    ///   the peer's quinn flipped between the bridge and the old address, never validated the new
    ///   path and reset the connection 40 s later; closed, the dial over the door linked in ~2.5 s.
    pub async fn switch_carrier_preference(
        &self,
        preference: CarrierPreference,
        doors: &HashMap<NodeAddress, SocketAddr>,
    ) -> usize {
        self.set_carrier_preference(preference);
        *self.door_hints.lock() = doors.clone();
        let direct: Vec<(NodeAddress, PeerPath)> = {
            let map = self.peer_paths.lock();
            map.iter()
                .flat_map(|(peer, set)| {
                    set.lock()
                        .paths
                        .iter()
                        .filter(|path| matches!(path.quality, PathQuality::Direct))
                        .map(|path| (*peer, path.clone()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let mut closed = 0;
        for (peer, path) in direct {
            if !self.carrier_refused(&path.iface, &path.neighbor) {
                continue;
            }
            crate::tcp_transport::tcp_trace(
                "net/carrier-switch",
                &format!("preference={preference:?} peer={peer:?} neighbor={:?} closing", path.neighbor),
            );
            for iface in &self.interfaces {
                iface.close_neighbor(&path.neighbor).await;
            }
            self.prune_path(&path.neighbor);
            closed += 1;
        }
        closed
    }

    /// The doors to dial `peer` at in TCP mode, in preference order: its LAN door (a contact on the
    /// same Wi-Fi is reached without the router; its public door needs a hairpin the router may not
    /// do), then the public door its beacon advertised. The dial races them, so a LAN door that is
    /// not ours costs one attempt delay, not the link.
    fn doors_for_dial(&self, peer: &NodeAddress) -> Vec<SocketAddr> {
        let mut doors: Vec<SocketAddr> = self.lan_door(peer).into_iter().collect();
        if let Some(public) = self.door_hints.lock().get(peer).copied() {
            if !doors.contains(&public) {
                doors.push(public);
            }
        }
        doors
    }

    /// Whether one of our interfaces is of `kind` — e.g. the relay's TCP carrier, bound only at start.
    pub fn has_interface(&self, kind: InterfaceKind) -> bool {
        self.interfaces.iter().any(|iface| iface.kind() == kind)
    }

    pub fn carrier_preference(&self) -> CarrierPreference {
        match self.carrier_preference.load(std::sync::atomic::Ordering::Relaxed) {
            x if x == CarrierPreference::Tcp as u8 => CarrierPreference::Tcp,
            x if x == CarrierPreference::Quic as u8 => CarrierPreference::Quic,
            _ => CarrierPreference::Auto,
        }
    }

    /// Set the roles we advertise in our announce (`capabilities` bitfield, e.g.
    /// `ANNOUNCE_CAP_DHT_PROXY`). Called once at mesh-start from config; read on every `announce`.
    /// See docs/decisions/dht-over-peer.md (R7 2e-4 advertised role).
    pub fn set_capabilities(&self, capabilities: u8) {
        self.capabilities
            .store(capabilities, std::sync::atomic::Ordering::Relaxed);
    }

    /// Advertise our TCP front door's port in our announces (0 = no door).
    pub fn set_door_port(&self, port: u16) {
        self.door_port.store(port, std::sync::atomic::Ordering::Relaxed);
    }

    /// `peer`'s TCP front door on our LAN: its announced door port at the address we know it by on
    /// this segment. Without UPnP the door is never in the beacon, so this is the only door a
    /// contact on the same Wi-Fi has: what TCP mode needs to reach it at all, and to move a live
    /// connection onto when the carrier switches to TCP.
    ///
    /// The door listens on IPv4 only, so the address is the peer's private IPv4 one, from either:
    /// - its current direct UDP path, when that is a LAN address (`::ffff:`-mapped included);
    /// - else its own announce, heard straight from it (hop 0, over multicast or a direct link),
    ///   when the address is in our /24.
    ///   On the stand two instances on one machine linked over its global IPv6, and the door was
    ///   only reachable at the LAN IPv4 the announce carries.
    /// A public IPv4 path is behind a NAT, where the door port means nothing, and an announce a relay
    /// forwarded may carry another network's LAN addresses; neither gives a door. A door that does
    /// not answer costs one refused connect, never the link. `None` when the peer announced no door.
    pub fn lan_door(&self, peer: &NodeAddress) -> Option<SocketAddr> {
        fn private_v4(addr: &SocketAddr) -> Option<std::net::Ipv4Addr> {
            let v4 = match addr.ip() {
                IpAddr::V4(v4) => v4,
                IpAddr::V6(v6) => v6.to_ipv4_mapped()?,
            };
            (!is_enumerable_wan(&SocketAddr::new(IpAddr::V4(v4), addr.port()))).then_some(v4)
        }
        let (door_port, heard_ip) = {
            let book = self.peer_book.lock();
            let record = book.get(peer)?;
            // Its announced private address is OUR LAN only inside our /24: a peer linked straight
            // over the internet announces its own home address too (stand 2026-10-17: the laptop
            // took the PC's 192.168.77.x for a door, and a switch to TCP waited 12.9 s on it).
            let own = *self.own_lan_v4.lock();
            let heard_ip = record
                .heard_direct
                .then(|| {
                    record
                        .advertised_addresses
                        .iter()
                        .filter_map(private_v4)
                        .find(|ip| ip.is_loopback() || own.is_some_and(|own| same_slash24(own, *ip)))
                })
                .flatten();
            (record.door_port, heard_ip)
        };
        if door_port == 0 {
            return None;
        }
        let linked_ip = self
            .direct_udp_path(peer)
            .and_then(|path| path.iface.remote_addr(&path.neighbor))
            .filter(|addr| !crate::tcp_transport::is_door_bridge_source(*addr))
            .as_ref()
            .and_then(private_v4);
        linked_ip.or(heard_ip).map(|ip| SocketAddr::new(IpAddr::V4(ip), door_port))
    }

    /// Whether `peer` advertised the DHT-proxy role in its (verified) announce — the role-based
    /// selector for DHT-over-peer (mode b), so we delegate only to a peer that opted in. `false` if we
    /// have no announce-backed record for it. See docs/decisions/dht-over-peer.md.
    pub fn peer_advertises_dht_proxy(&self, peer: &NodeAddress) -> bool {
        self.peer_book
            .lock()
            .get(peer)
            .is_some_and(|r| r.capabilities & crate::announce::ANNOUNCE_CAP_DHT_PROXY != 0)
    }

    /// Friend-proxy (Axis 2): opt in to carry `peer`'s traffic through us. Per-friend, off by default,
    /// revocable ([`revoke_relay_consent`](Self::revoke_relay_consent)). Advertising
    /// `ANNOUNCE_CAP_FRIEND_RELAY` (`set_capabilities`) says we're *willing*; this records *for whom*.
    /// Gating carriage on it is Phase B — the always-on mesh forwarding tier (`relay_enabled`) is
    /// unaffected. See docs/decisions/friend-proxy-by-consent.md.
    pub fn consent_to_relay_for(&self, peer: NodeAddress) {
        self.relay_consent.lock().insert(peer);
    }

    /// Withdraw a prior [`consent_to_relay_for`](Self::consent_to_relay_for) — effective on the next
    /// carriage admission.
    pub fn revoke_relay_consent(&self, peer: &NodeAddress) {
        self.relay_consent.lock().remove(peer);
    }

    /// Whether we have consented to friend-proxy for `peer` (the Phase B carriage gate reads this).
    pub fn relay_consent_for(&self, peer: &NodeAddress) -> bool {
        self.relay_consent.lock().contains(peer)
    }

    /// Whether `peer` advertised the friend-relay role in its (verified) announce — the consumer-side
    /// selector, mirroring [`peer_advertises_dht_proxy`](Self::peer_advertises_dht_proxy). See
    /// docs/decisions/friend-proxy-by-consent.md.
    pub fn peer_advertises_friend_relay(&self, peer: &NodeAddress) -> bool {
        self.peer_book
            .lock()
            .get(peer)
            .is_some_and(|r| r.capabilities & crate::announce::ANNOUNCE_CAP_FRIEND_RELAY != 0)
    }

    /// A known peer that advertises the friend-relay capability (any advertiser — the consumer checks
    /// reachability + our own consent when it actually routes, Phase B). `None` if none is known.
    /// Mirrors the DHT-proxy candidate selector. See docs/decisions/friend-proxy-by-consent.md.
    pub fn friend_relay_candidate(&self) -> Option<NodeAddress> {
        self.peer_book
            .lock()
            .iter()
            .find(|(peer, r)| {
                **peer != self.local_address
                    && r.capabilities & crate::announce::ANNOUNCE_CAP_FRIEND_RELAY != 0
            })
            .map(|(peer, _)| *peer)
    }

    /// Admit (or reject) carrying ONE frame for `origin` as a friend-proxy (Axis 2): requires prior
    /// per-friend consent ([`relay_consent_for`](Self::relay_consent_for)) AND a per-origin
    /// sliding-window cap ([`FRIEND_RELAY_MAX_CARRIES_PER_WINDOW`]). Records the serve on admit.
    /// `now_ms` injected for testability. See docs/decisions/friend-proxy-by-consent.md.
    fn admit_friend_relay(&self, origin: &NodeAddress, now_ms: u64) -> bool {
        if !self.relay_consent_for(origin) {
            return false;
        }
        let window_start = now_ms.saturating_sub(FRIEND_RELAY_WINDOW_MS);
        let mut log = self.friend_relay_serve_log.lock();
        // Lazy global GC when the map grows large — keeps the common path O(1) on one origin's window.
        if log.len() > 4096 {
            log.retain(|_, hits| {
                hits.retain(|&t| t >= window_start);
                !hits.is_empty()
            });
        }
        let hits = log.entry(*origin).or_default();
        hits.retain(|&t| t >= window_start);
        if hits.len() >= FRIEND_RELAY_MAX_CARRIES_PER_WINDOW {
            return false;
        }
        hits.push(now_ms);
        true
    }

    /// Friend-exit (Axis 2, Phase D): opt in to EGRESS `peer`'s traffic through us. Per-friend, off by
    /// default, revocable ([`revoke_exit_consent`](Self::revoke_exit_consent)). Distinct from
    /// friend-relay consent — egress lends our uplink + IP, a heavier deal. Advertising
    /// `ANNOUNCE_CAP_FRIEND_EXIT` (`set_capabilities`) says we're *willing*; this records *for whom*.
    /// See docs/decisions/friend-proxy-by-consent.md (Phase D).
    pub fn consent_to_exit_for(&self, peer: NodeAddress) {
        self.exit_consent.lock().insert(peer);
    }

    /// Withdraw a prior [`consent_to_exit_for`](Self::consent_to_exit_for) — effective on the next
    /// egress admission.
    pub fn revoke_exit_consent(&self, peer: &NodeAddress) {
        self.exit_consent.lock().remove(peer);
    }

    /// Whether we have consented to friend-exit for `peer` (the Phase D egress gate reads this).
    pub fn exit_consent_for(&self, peer: &NodeAddress) -> bool {
        self.exit_consent.lock().contains(peer)
    }

    /// Whether `peer` advertised the friend-exit role in its (verified) announce — the consumer-side
    /// selector, mirroring [`peer_advertises_friend_relay`](Self::peer_advertises_friend_relay).
    pub fn peer_advertises_friend_exit(&self, peer: &NodeAddress) -> bool {
        self.peer_book
            .lock()
            .get(peer)
            .is_some_and(|r| r.capabilities & crate::announce::ANNOUNCE_CAP_FRIEND_EXIT != 0)
    }

    /// A known peer that advertises the friend-exit capability (any advertiser). `None` if none is
    /// known. Mirrors [`friend_relay_candidate`](Self::friend_relay_candidate).
    pub fn friend_exit_candidate(&self) -> Option<NodeAddress> {
        self.peer_book
            .lock()
            .iter()
            .find(|(peer, r)| {
                **peer != self.local_address
                    && r.capabilities & crate::announce::ANNOUNCE_CAP_FRIEND_EXIT != 0
            })
            .map(|(peer, _)| *peer)
    }

    /// Admit (or reject) egressing ONE datagram of `bytes` toward `destination` for `origin` as a
    /// friend-exit (Axis 2, Phase D): requires prior per-friend consent
    /// ([`exit_consent_for`](Self::exit_consent_for)) AND, within the sliding window, both the
    /// distinct-destination cap ([`FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW`]) and the total-byte cap
    /// ([`FRIEND_EXIT_MAX_BYTES_PER_WINDOW`]). Records the egress on admit. `destination` is an opaque
    /// key; `now_ms` injected for testability. Public because it is the admission primitive the
    /// (pending) Phase D2 egress leg calls before it forwards a datagram off-mesh — landed now with
    /// the consent set so the gate is testable + stable ahead of the leg. See
    /// docs/decisions/friend-proxy-by-consent.md (Phase D).
    pub fn admit_friend_exit(
        &self,
        origin: &NodeAddress,
        destination: &[u8],
        bytes: u64,
        now_ms: u64,
    ) -> bool {
        if !self.exit_consent_for(origin) {
            return false;
        }
        let window_start = now_ms.saturating_sub(FRIEND_EXIT_WINDOW_MS);
        let mut log = self.friend_exit_serve_log.lock();
        // Lazy global GC when the map grows large — bound memory across many origins.
        if log.len() > 4096 {
            log.retain(|_, window| {
                window.destinations.retain(|_, &mut t| t >= window_start);
                window.bytes.retain(|&(_, t)| t >= window_start);
                !window.destinations.is_empty() || !window.bytes.is_empty()
            });
        }
        let window = log.entry(*origin).or_default();
        window.destinations.retain(|_, &mut t| t >= window_start);
        window.bytes.retain(|&(_, t)| t >= window_start);

        // A brand-new destination would push us over the distinct-destination cap → reject (an
        // already-seen destination within the window is free to keep using).
        let is_new_destination = !window.destinations.contains_key(destination);
        if is_new_destination && window.destinations.len() >= FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW {
            return false;
        }
        let used: u64 = window.bytes.iter().map(|&(b, _)| b).sum();
        if used.saturating_add(bytes) > FRIEND_EXIT_MAX_BYTES_PER_WINDOW {
            return false;
        }
        window.destinations.insert(destination.to_vec(), now_ms);
        window.bytes.push((bytes, now_ms));
        true
    }

    /// Friend-proxy responder (Axis 2): a consenting friend carries `carry.payload` toward `carry.to`
    /// on the origin's behalf. The origin is the VERIFIED neighbour the frame arrived on (anti-spoof —
    /// the frame carries no `from`; carriage is one hop from the origin's friend). Gated on
    /// [`admit_friend_relay`](Self::admit_friend_relay) (per-friend consent + per-origin cap). On
    /// admit, re-emit as routed `Data` FROM the origin over our best path to `to` — content-blind: the
    /// payload is opaque (already E2E-sealed for `to`). See docs/decisions/friend-proxy-by-consent.md.
    async fn handle_friend_relay_carry(
        &self,
        carry: FriendRelayCarryFrame,
        from_neighbor: &NeighborId,
    ) {
        let Some(origin) = self.verified_neighbors.lock().get(from_neighbor).copied() else {
            return; // unverified neighbour — never carry
        };
        if !self.admit_friend_relay(&origin, current_unix_ms().max(0) as u64) {
            return; // no consent, or over the per-origin cap
        }
        let frame =
            NetworkFrame::Data(DataFrame::origin(origin, carry.to, carry.payload)).encode_to_wire();
        let path = self.peer_paths.lock().get(&carry.to).and_then(|s| s.lock().best());
        if let Some(path) = path {
            let _ = path.iface.send_frame(&path.neighbor, &frame).await;
        }
    }

    /// Friend-proxy consumer (Axis 2): ask our consenting friend `friend` to carry `payload` (opaque,
    /// already E2E-sealed for `to`) toward `to`. Sent one hop over our best path to `friend`; the
    /// friend gates on its own consent + cap. `Err` if we have no path to the friend right now. See
    /// docs/decisions/friend-proxy-by-consent.md.
    pub async fn carry_via_friend(
        &self,
        friend: NodeAddress,
        to: NodeAddress,
        payload: Vec<u8>,
    ) -> io::Result<()> {
        let frame =
            NetworkFrame::FriendRelayCarry(FriendRelayCarryFrame { to, payload }).encode_to_wire();
        let path = self.peer_paths.lock().get(&friend).and_then(|s| s.lock().best());
        match path {
            Some(path) => path.iface.send_frame(&path.neighbor, &frame).await,
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no path to the friend-proxy",
            )),
        }
    }

    /// Whether the relay→direct upgrade is currently permitted (see `set_allow_direct_punch`). The
    /// runtime forwards this so the rendezvous worker skips the direct-dial leg for a `RelayOnly` profile.
    pub fn allows_direct_punch(&self) -> bool {
        self.allow_direct_punch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn upgrade_relay_links_via_punch(&self) {
        let relay_only: Vec<NodeAddress> = {
            let map = self.peer_paths.lock();
            map.iter()
                .filter(|(peer, _)| **peer != self.local_address)
                .filter_map(|(peer, set)| {
                    let best = set.lock().best()?;
                    (!matches!(best.quality, PathQuality::Direct)).then_some(*peer)
                })
                .collect()
        };
        for peer in relay_only {
            self.punch_upgrade_peer(peer).await;
        }
    }

    /// Hole-punch a single relay-only `peer` toward a DIRECT link: coordinate a punch through the
    /// common relay (`request_punch`), let the pinholes open, then dial it directly. Idempotent +
    /// self-guarding — if the peer is already reachable directly (a prior attempt won, or another
    /// path raced in) this is a no-op. That makes it safe to call from BOTH the periodic
    /// `upgrade_relay_links_via_punch` sweep AND the event-driven `next_relay_upgrade` signal, so a
    /// relay link is upgraded within ~`PUNCH_PROBE_GRACE` of forming instead of within a reconcile
    /// interval. См. docs/decisions/mesh-hole-punching.md.
    pub async fn punch_upgrade_peer(&self, peer: NodeAddress) {
        if peer == self.local_address {
            return;
        }
        // Master privacy gate (ConnectionPrivacyMode::RelayOnly): never climb to a direct link — keep
        // the working relay path so a contact can't learn our IP. Both the periodic sweep and the
        // event-driven signal funnel through here, so this one check disables the UDP hole-punch
        // upgrade entirely. См. docs/decisions/connection-quality-ladder.md (privacy axis).
        if !self.allows_direct_punch() {
            return;
        }
        // TCP for our hops: a punched path is UDP.
        if self.carrier_preference() == CarrierPreference::Tcp {
            return;
        }
        // Re-check at call time: the signal may be stale (the peer went direct between the emit and
        // now), in which case there is nothing to punch.
        let still_relay_only = {
            let map = self.peer_paths.lock();
            map.get(&peer)
                .and_then(|set| set.lock().best())
                .map(|p| !matches!(p.quality, PathQuality::Direct))
                .unwrap_or(false)
        };
        if !still_relay_only {
            return;
        }
        // Ask the shared relay to coordinate: its `PunchSync` drives BOTH sides — each opens its
        // pinhole and the role-elected dialer (`decide_punch_role`) dials through them
        // (`handle_punch_sync`). No dial here: that would ignore the role and double-dial when both
        // peers auto-upgrade the same pair (QUIC has no symmetric open). The relay coordinates both
        // even if only one side initiated (it `PunchSync`s both).
        self.request_punch(peer).await;
    }

    /// Await the next peer signalled for a fast (event-driven) relay→direct punch-upgrade. The
    /// runtime drains this in a dedicated task and calls `punch_upgrade_peer` immediately, so the
    /// upgrade fires on the relay-link-up / direct-link-loss EVENT rather than on the reconcile
    /// timer. Returns `None` only once the Network is dropped (channel closed).
    pub async fn next_relay_upgrade(&self) -> Option<NodeAddress> {
        self.relay_upgrade_rx.lock().await.recv().await
    }

    /// Resolve the moment a neighbour is freshly verified — the event-driven wake for the AutoNAT
    /// self-reachability loop. A fresh verification may mean a new WAN UDP verifier (the relay's UDP
    /// carrier) just came up, so the loop can confirm our endpoint NOW rather than on the next announce
    /// tick. The periodic interval stays the backstop. Mirrors `next_relay_upgrade`; `Notify` coalesces
    /// and never drops a wake. Phase B. См. docs/decisions/connectivity-upgrade-via-peer-relay.md.
    pub async fn autonat_triggered(&self) {
        self.autonat_trigger.notified().await;
    }

    /// R1a connection-quality ladder — carrier climb. For every peer we reach over a RELAY, register
    /// an additional relay path over EVERY carrier link we hold to that same relay node, so
    /// `path_rank` can climb to the faster carrier (UDP relay, rung 1) and fail back to the reliable
    /// one (TCP relay, rung 0). The relay is identified by IDENTITY (`verified_neighbors`), since the
    /// same relay reached over TCP vs UDP is a DIFFERENT per-carrier neighbour. Best-effort +
    /// idempotent: a carrier the peer already has is skipped, so this is a no-op until we actually
    /// hold two carrier links to a relay (the dual-carrier autoconnect — STEP 2b wiring). The runtime
    /// calls it on the reconcile timer. See docs/decisions/connection-quality-ladder.md.
    pub fn upgrade_relay_carriers(&self) {
        // TCP for our hops: the relay stays on its TCP carrier (there is no UDP one to climb to in
        // QUIC mode either way).
        if self.carrier_preference() == CarrierPreference::Tcp {
            return;
        }
        // Snapshot the relayed peers + their relay node (by identity) + the route cost.
        let relayed: Vec<(NodeAddress, NodeAddress, u8)> = {
            let verified = self.verified_neighbors.lock();
            let map = self.peer_paths.lock();
            map.iter()
                .filter(|(peer, _)| **peer != self.local_address)
                .filter_map(|(peer, set)| {
                    let best = set.lock().best()?;
                    match best.quality {
                        PathQuality::Direct => None, // already direct — nothing to climb here
                        PathQuality::Relay { hops } => {
                            let relay_node = *verified.get(&best.neighbor)?;
                            Some((*peer, relay_node, hops))
                        }
                    }
                })
                .collect()
        };

        for (peer, relay_node, hops) in relayed {
            // Every carrier link we hold to this relay = its DIRECT paths in our peer_paths.
            let relay_links: Vec<(NeighborId, InterfaceHandle)> = {
                let map = self.peer_paths.lock();
                match map.get(&relay_node) {
                    Some(set) => set
                        .lock()
                        .paths
                        .iter()
                        .filter(|p| matches!(p.quality, PathQuality::Direct))
                        .map(|p| (p.neighbor.clone(), Arc::clone(&p.iface)))
                        .collect(),
                    None => Vec::new(),
                }
            };
            // Carriers the peer already has a path over — don't double-register.
            let have_ifaces: Vec<InterfaceId> = {
                let map = self.peer_paths.lock();
                match map.get(&peer) {
                    Some(set) => set.lock().paths.iter().map(|p| p.iface.id()).collect(),
                    None => Vec::new(),
                }
            };
            for (relay_neighbor, iface) in relay_links {
                if have_ifaces.contains(&iface.id()) {
                    continue;
                }
                // A 1-hop-to-the-relay carrier path to `peer`: the relay forwards regardless of which
                // carrier we ride to it, so this is a valid equal-cost alternative `path_rank` ranks.
                self.register_path(
                    peer,
                    relay_neighbor,
                    iface,
                    PathQuality::Relay { hops },
                    false,
                );
            }
        }
    }

    /// Ask common neighbours to coordinate a direct hole-punch to `target` (P2):
    /// send a `PunchRequest` to every peer we have a path to — whichever one also
    /// has a path to `target` relays a `PunchSync` to both sides, so both open
    /// pinholes. Best-effort. The actual direct dial is the CALLER's (it owns the
    /// resulting Link); this only opens the pinholes so that dial can pass the
    /// peer's NAT filter. См. docs/decisions/mesh-hole-punching.md.
    pub async fn request_punch(&self, target: NodeAddress) {
        let nonce = OsRng.next_u64();
        let wire =
            NetworkFrame::PunchRequest(PunchRequestFrame { target, nonce }).encode_to_wire();
        let relays: Vec<(NeighborId, InterfaceHandle)> = {
            let map = self.peer_paths.lock();
            map.iter()
                .filter(|(peer, _)| **peer != target)
                .filter_map(|(_, set)| set.lock().best().map(|p| (p.neighbor, p.iface)))
                .collect()
        };
        for (neighbor, iface) in relays {
            let _ = iface.send_frame(&neighbor, &wire).await;
        }
    }

    /// Relay role: a peer asked us to coordinate a hole-punch to `req.target`. If
    /// we have a verified path to the target, tell each side the OTHER's reflexive
    /// endpoint (the source address we observe for its connection to us) + the
    /// shared nonce, so both probe each other's endpoint. P2.
    async fn handle_punch_request(
        &self,
        req: PunchRequestFrame,
        from_neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        let requester = match self.verified_neighbors.lock().get(from_neighbor).copied() {
            Some(id) => id,
            None => return, // unverified neighbour — ignore
        };
        if requester == req.target {
            return;
        }
        // Our best path to the target — we must be able to reach it to relay.
        let target_path = self
            .peer_paths
            .lock()
            .get(&req.target)
            .and_then(|set| set.lock().best());
        let Some(target_path) = target_path else {
            return; // not the relay for this punch (no path to target)
        };
        // Reflexive endpoints as WE observe each side (its mapped source address).
        let (Some(requester_reflexive), Some(target_reflexive)) = (
            iface.remote_addr(from_neighbor),
            target_path.iface.remote_addr(&target_path.neighbor),
        ) else {
            return;
        };
        // Tell the target about the requester …
        let to_target = NetworkFrame::PunchSync(PunchSyncFrame {
            peer: requester,
            peer_endpoint: requester_reflexive,
            nonce: req.nonce,
        })
        .encode_to_wire();
        let _ = target_path
            .iface
            .send_frame(&target_path.neighbor, &to_target)
            .await;
        // … and the requester about the target.
        let to_requester = NetworkFrame::PunchSync(PunchSyncFrame {
            peer: req.target,
            peer_endpoint: target_reflexive,
            nonce: req.nonce,
        })
        .encode_to_wire();
        let _ = iface.send_frame(from_neighbor, &to_requester).await;
    }

    /// `PunchSync` told us the peer's reflexive endpoint. Exactly ONE side dials (QUIC has no
    /// symmetric simultaneous-open); [`decide_punch_role`] elects it by address order:
    /// - **Listener**: fire one outbound QUIC Initial at the peer's endpoint (over the same
    ///   internet-facing interface the `PunchSync` arrived on) to open OUR pinhole toward it. It
    ///   won't complete — the dialer's Initial forms the link — so it's spawned: the handshake
    ///   timeout can't stall the dispatcher, and the Initial packet opens the pinhole regardless.
    /// - **Dialer**: after a grace for the listener's probe to land, dial the peer AT that endpoint
    ///   → mutual challenge → verified DIRECT Link (joins the PathSet; `send` leaves the relay).
    ///
    /// Both arms spawn, so the dispatcher never blocks (a real dial would). D1 fix: the probe is a
    /// real `connect_endpoint`, not `send_frame` (which is a prod no-op for an unlinked neighbour).
    /// P2 / docs/decisions/mesh-hole-punching.md.
    async fn handle_punch_sync(self: &Arc<Self>, sync: PunchSyncFrame, iface: &InterfaceHandle) {
        // The peer may climb to UDP; our hop stays TCP.
        if self.carrier_preference() == CarrierPreference::Tcp {
            return;
        }
        match decide_punch_role(&self.local_address, &sync.peer) {
            PunchRole::Listener => {
                let iface = iface.clone();
                let endpoint = sync.peer_endpoint.to_string();
                tokio::spawn(async move {
                    let _ = iface.connect_endpoint(&endpoint).await;
                });
            }
            PunchRole::Dialer => {
                let this = Arc::clone(self);
                let peer = sync.peer;
                let endpoint = sync.peer_endpoint;
                tokio::spawn(async move {
                    tokio::time::sleep(PUNCH_PROBE_GRACE).await;
                    let _ = this.dial_at_endpoints(peer, vec![endpoint]).await;
                });
            }
        }
    }

    /// Peer told us its high-water → offer it every `(feed, seq, msg_id)` we hold
    /// past it (bounded). Re-offering something it has above a gap is harmless
    /// (the receiver dedups by `(feed, seq)`).
    async fn handle_anti_entropy_have(
        &self,
        have: AntiEntropyHaveFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        let mut peer = ReconcileState::new();
        for (feed, seq) in have.marks {
            peer.set(feed, seq);
        }
        // Record the peer's advertised interests (its subscribed dests) + whether it forwards for us,
        // so we can scope the offer: we hand a held frame to this peer only if the frame's dest is one
        // the peer wants, or the peer carries our traffic. Everything else stays with us — closing the
        // "offer the whole inventory to any route-candidate" leak.
        let peer_node = self.verified_neighbors.lock().get(neighbor).copied();
        let peer_interests = have.interests;
        if let Some(node) = peer_node {
            self.peer_interests
                .lock()
                .insert(node, peer_interests.clone());
        }
        let peer_is_forwarder = peer_node.map(|n| self.is_forwarder(&n)).unwrap_or(false);

        let offers = self.durable_reconcile.lock().offers_for(&peer);
        if offers.is_empty() {
            return;
        }
        // Snapshot (feed, seq, dest, msg_id) under the store lock, THEN filter by relevance (which
        // locks group_subscriptions) — never nested, keeping the store → group_subscriptions order.
        let candidates: Vec<(FeedId, u64, NodeAddress, u64)> = {
            let store = self.durable_store.lock();
            offers
                .into_iter()
                .filter_map(|(feed, seq)| {
                    store.get(&(feed, seq)).map(|f| (feed, seq, f.to, f.msg_id))
                })
                .collect()
        };
        let entries: Vec<(FeedId, u64, u64)> = candidates
            .into_iter()
            .filter(|(_, _, to, _)| {
                self.feed_relevant_to_peer(to, peer_node.as_ref(), &peer_interests, peer_is_forwarder)
            })
            .map(|(feed, seq, _, msg_id)| (feed, seq, msg_id))
            .take(MAX_ANTI_ENTROPY_ENTRIES)
            .collect();
        if entries.is_empty() {
            return;
        }
        let frame = NetworkFrame::AntiEntropyOffer(AntiEntropyOfferFrame { entries });
        let _ = iface.send_frame(neighbor, &frame.encode_to_wire()).await;
    }

    /// Peer offered `(feed, seq, msg_id)` entries → request the msg_ids we lack.
    async fn handle_anti_entropy_offer(
        &self,
        offer: AntiEntropyOfferFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        let want: Vec<u64> = {
            let reconcile = self.durable_reconcile.lock();
            offer
                .entries
                .iter()
                .filter(|(feed, seq, _)| !reconcile.contains(feed, *seq))
                .map(|(_, _, msg_id)| *msg_id)
                .take(MAX_ANTI_ENTROPY_ENTRIES)
                .collect()
        };
        if want.is_empty() {
            return;
        }
        let frame = NetworkFrame::AntiEntropyRequest(AntiEntropyRequestFrame { msg_ids: want });
        let _ = iface.send_frame(neighbor, &frame.encode_to_wire()).await;
    }

    /// Peer requested msg_ids → send the matching durable frames it lacks.
    async fn handle_anti_entropy_request(
        &self,
        request: AntiEntropyRequestFrame,
        neighbor: &NeighborId,
        iface: &InterfaceHandle,
    ) {
        let wanted: HashSet<u64> = request.msg_ids.into_iter().collect();
        let frames: Vec<DurableDataFrame> = {
            let store = self.durable_store.lock();
            store
                .frames()
                .filter(|f| wanted.contains(&f.msg_id))
                .cloned()
                .collect()
        };
        for frame in frames {
            let wire = NetworkFrame::DurableData(frame).encode_to_wire();
            let _ = iface.send_frame(neighbor, &wire).await;
        }
    }

    async fn handle_announce(
        &self,
        announce: Announce,
        iface: &InterfaceHandle,
        neighbor: &crate::interface::NeighborId,
    ) -> Result<(), AnnounceHandlerError> {
        // 1. Игнорируем свои own announce'ы (loopback при multicast).
        if announce.destination == self.local_address {
            return Ok(());
        }
        // 2. Verify signature (anti-impersonation gate).
        announce
            .verify(current_unix_ms())
            .map_err(AnnounceHandlerError::Verify)?;
        // 3. Remember who this is and where to dial them (address book).
        let dest = announce.destination;
        // NB: deliberately do NOT refresh `peer_last_seen` from an announce. A relay CACHES a peer's
        // announce and keeps re-forwarding it for up to its TTL (~300s) AFTER the peer dies, so an
        // announce is NOT proof of CURRENT liveness — treating it as such re-seeds the dead peer and
        // defeats the relay-only-peer prune (observed live: a killed relay peer lingered ~2.5 min,
        // re-seeded 7× by relay-forwarded announces). Relay-peer liveness is driven SOLELY by the
        // SOLICITED routed `PeerPong` — Data is excluded for the SAME reason (a relay drains a dead
        // peer's buffered Data; see the data handler), so neither cacheable announce nor buffered Data
        // re-seeds it. See docs/bugs/relay-presence-false-online-dead-peer.md.
        self.peer_book.lock().insert(
            dest,
            PeerRecord {
                pubkey: Some(announce.pubkey),
                advertised_addresses: announce.advertised_addresses.clone(),
                capabilities: announce.capabilities,
                door_port: announce.door_port,
                heard_direct: announce.hop_count == 0,
            },
        );
        // 4. Update path table soft-state (feeds Phase β multi-hop routing).
        let ttl = Duration::from_secs(announce.ttl_seconds as u64);
        self.path_table.update(
            dest,
            iface.id(),
            neighbor.clone(),
            announce.hop_count,
            ttl,
            Instant::now(),
        );
        Ok(())
    }

    /// Responder side of identity verification. Someone challenged us to prove
    /// we own `request.to`. If that's really us, sign the nonce and reply; then
    /// challenge *them* back (with resends — see below) so the link is mutually
    /// authenticated.
    async fn handle_challenge_request(
        &self,
        request: ChallengeRequest,
        iface: &InterfaceHandle,
        neighbor: &NeighborId,
    ) {
        // Misdirected — the challenger expects a different identity at this
        // endpoint. Don't sign (we couldn't prove their expected address anyway).
        // «Whoever you are» is answered only by a relay — its address is public in the directory
        // anyway; an ordinary node does not tell a scanner who sits at its address.
        let anonymous = request.to == NodeAddress::ANY && self.relay_card.lock().is_some();
        if request.to == NodeAddress::ANY && !anonymous {
            // Not a relay: close at once, so a searcher moves on instead of waiting out its resends.
            iface.close_neighbor(neighbor).await;
            return;
        }
        if request.to != self.local_address && !anonymous {
            return;
        }
        // Direct links are kept only with the relays (`keep_direct_only_with`). Refuse a dialer we
        // will not keep BEFORE proving who we are: answering first let the dialer verify us, count
        // the link as up, lose it a moment later when our own check refused it — and dial again
        // every few seconds, holding on to that dying direct path instead of the relay one (stand
        // 2026-10-14: 170 refusals in 10 min, the contact never moved onto the relay). The claim is
        // unverified, but a refusal grants nothing.
        if self.direct_refused(&request.from) {
            crate::tcp_transport::tcp_trace(
                "net/direct-off",
                &format!("refusing dialer neighbor={neighbor:?} claims={:?}", request.from),
            );
            iface.close_neighbor(neighbor).await;
            return;
        }
        // Our hops' carrier binds a dialer too, not only our own dials: TCP takes a dialer only
        // through a door, QUIC only over raw UDP. So «tcp — auto» rides TCP, «quic — auto» QUIC,
        // and «quic — tcp» has no link at all (owner, 2026-10-16). Before: a TCP profile dialed
        // nothing over UDP but took a contact's UDP dial, and its hop rode UDP all the same.
        if self.carrier_refused(iface, neighbor) {
            crate::tcp_transport::tcp_trace(
                "net/carrier-off",
                &format!(
                    "refusing dialer neighbor={neighbor:?} claims={:?} ours={:?}",
                    request.from,
                    self.carrier_preference()
                ),
            );
            iface.close_neighbor(neighbor).await;
            return;
        }

        let response = respond_to_challenge(&self.identity, &request.nonce);
        let response_frame = NetworkFrame::ChallengeResponse(ChallengeResponseFrame {
            nonce: request.nonce,
            from: self.local_address,
            to: request.from,
            response,
        });
        if let Err(e) = iface
            .send_frame(neighbor, &response_frame.encode_to_wire())
            .await
        {
            eprintln!("[omni-network] challenge response send failed: {e}");
            return;
        }

        // Challenge back for mutual auth — but only if we didn't already
        // initiate a challenge to this neighbor (we're the dialer) and haven't
        // already verified them. This breaks the otherwise-infinite ping-pong.
        let should_challenge_back = {
            let pending = self.outbound_challenges.lock();
            let verified = self.verified_neighbors.lock();
            !pending.contains_key(neighbor) && !verified.contains_key(neighbor)
        };
        if !should_challenge_back {
            return;
        }

        let nonce = fresh_challenge_nonce();
        self.outbound_challenges.lock().insert(
            neighbor.clone(),
            OutboundChallenge {
                nonce,
                expected: request.from,
                role: ChallengeRole::Responder,
            },
        );
        let back = NetworkFrame::ChallengeRequest(ChallengeRequest {
            nonce,
            from: self.local_address,
            to: request.from,
        });
        let wire = back.encode_to_wire();
        crate::tcp_transport::tcp_trace(
            "challenge/back",
            &format!("neighbor={neighbor:?} expect={:?}", request.from),
        );
        if let Err(e) = iface.send_frame(neighbor, &wire).await {
            eprintln!("[omni-network] challenge-back send failed: {e}");
            self.outbound_challenges.lock().remove(neighbor);
            return;
        }
        // Re-send the SAME challenge-back (same nonce) like `run_dialer_challenge` does for the
        // dialer half — and for the same reason. On an asymmetric-UDP carrier (mobile flow-throttle:
        // peer→us dies, us→peer lives) the peer's single ChallengeResponse is easily lost; without
        // resends we then stay "handshaking" FOREVER: never a verified neighbour, so every live
        // Data frame the peer sends us is buffered/dropped in `handle_data`, node-ctx never forms,
        // and only durable frames (which bypass the verify gate) cross — the observed
        // "laptop→PC delivers only via the ~40 s durable backstop" failure. A resend re-elicits the
        // response, and by the later attempts the peer's carrier has typically migrated onto its
        // TCP door, which the response then rides. A LATE response still verifies — the pending
        // entry is only removed by `handle_challenge_response` / teardown, never by this loop.
        // See docs/bugs/relay-less-mobile-transport-findings.md.
        let challenges = Arc::clone(&self.outbound_challenges);
        let iface = Arc::clone(iface);
        let neighbor = neighbor.clone();
        tokio::spawn(async move {
            for _ in 1..CHALLENGE_SEND_ATTEMPTS {
                tokio::time::sleep(CHALLENGE_TIMEOUT).await;
                let still_pending = challenges
                    .lock()
                    .get(&neighbor)
                    .is_some_and(|c| c.nonce == nonce);
                if !still_pending {
                    return; // answered (verified) or torn down — stop resending
                }
                crate::tcp_transport::tcp_trace(
                    "challenge/back-resend",
                    &format!("neighbor={neighbor:?}"),
                );
                let _ = iface.send_frame(&neighbor, &wire).await;
            }
        });
    }

    /// We sent a challenge and this is the answer. Verify it proves the
    /// expected NodeAddress; on success mark the neighbor verified and either
    /// wake `dial` (we were the dialer) or hand an incoming Link to `accept`
    /// (we were the responder challenging back).
    async fn handle_challenge_response(
        &self,
        response: ChallengeResponseFrame,
        iface: &InterfaceHandle,
        neighbor: &NeighborId,
    ) {
        let challenge = match self.outbound_challenges.lock().remove(neighbor) {
            Some(c) => c,
            // Unsolicited / already-handled response — ignore.
            None => return,
        };

        // A dial at a bare address asked «whoever you are»: the identity is the key that answered.
        let expected = if challenge.expected == NodeAddress::ANY {
            NodeAddress::from_pubkey(&response.response.pubkey)
        } else {
            challenge.expected
        };
        let verify = verify_challenge_response(&response.response, &challenge.nonce, &expected);
        let challenge = OutboundChallenge { expected, ..challenge };
        // Direct links are kept only with the relays: the peer goes on through one of them.
        if verify.is_ok() && self.direct_refused(&challenge.expected) {
            crate::tcp_transport::tcp_trace(
                "net/direct-off",
                &format!("refusing neighbor={neighbor:?} peer={:?}", challenge.expected),
            );
            self.pending_data.lock().remove(neighbor);
            iface.close_neighbor(neighbor).await;
            if let ChallengeRole::Dialer { waker } = challenge.role {
                let _ = waker.send(Err(NetworkError::ChallengeRejected(
                    "direct links are off: through a relay only".to_string(),
                )));
            }
            return;
        }

        crate::tcp_transport::tcp_trace(
            "challenge/result",
            &format!(
                "neighbor={neighbor:?} peer={:?} ok={}",
                challenge.expected,
                verify.is_ok()
            ),
        );
        match (verify, challenge.role) {
            (Ok(_pubkey), ChallengeRole::Dialer { waker }) => {
                let channels = self.register_verified_link(neighbor, challenge.expected);
                // If the dialer gave up (timeout) the receiver is gone; the
                // verified-neighbor state still stands so its Link is reusable.
                let _ = waker.send(Ok(channels));
                // Link up → kick off durable anti-entropy with this neighbour.
                self.send_anti_entropy_have(neighbor, iface, &challenge.expected).await;
            }
            (Ok(_pubkey), ChallengeRole::Responder) => {
                let channels = self.register_verified_link(neighbor, challenge.expected);
                // Reflexive-address hint: we are the QUIC acceptor, so the
                // neighbour's remote address here is the source it connected from
                // (its external NAT mapping). Report it back so the peer can learn
                // its reflexive endpoint by aggregating ≥2 such reports. P1 §4.1A.
                if let Some(observed) = iface.remote_addr(neighbor) {
                    let hint = NetworkFrame::ObservedAddress(ObservedAddressFrame { observed });
                    let _ = iface.send_frame(neighbor, &hint.encode_to_wire()).await;
                }
                // Hand the inbound Link to whoever is awaiting `accept`.
                let paths = self.register_path(
                    challenge.expected,
                    neighbor.clone(),
                    Arc::clone(iface),
                    PathQuality::Direct,
                    false, // the peer dialed us (we accepted)
                );
                self.probe_superseded_direct_paths(challenge.expected, neighbor).await;
                let link = Link {
                    local: self.local_address,
                    destination: challenge.expected,
                    paths,
                    inbox: AsyncMutex::new(channels.inbox_rx),
                    datagram_inbox: AsyncMutex::new(channels.datagram_rx),
                };
                let _ = self.incoming_links_tx.try_send(link);
                // Link up → kick off durable anti-entropy with this neighbour.
                self.send_anti_entropy_have(neighbor, iface, &challenge.expected).await;
            }
            (Err(error), ChallengeRole::Dialer { waker }) => {
                self.pending_data.lock().remove(neighbor);
                let _ = waker.send(Err(NetworkError::ChallengeRejected(error.to_string())));
            }
            (Err(error), ChallengeRole::Responder) => {
                self.pending_data.lock().remove(neighbor);
                eprintln!("[omni-network] dialer failed our challenge: {error}");
            }
        }
    }

    /// Mark a neighbor verified and create its Link channels (reliable frame
    /// inbox + unreliable datagram inbox), draining any Data that arrived (and
    /// was buffered) before the handshake completed. Returns the receiver
    /// halves for the Link.
    fn register_verified_link(
        &self,
        neighbor: &NeighborId,
        peer: NodeAddress,
    ) -> VerifiedLinkChannels {
        // P2 PeerLink uncorking — was a latest-wins coalescing STUB that evicted any
        // other connection verified for the same identity. Now ADDITIVE: a
        // re-verified identity (reconnect / NAT remap / simultaneous dial / a second
        // relay vs direct) registers ANOTHER path; it no longer supersedes the
        // existing one. Every verified neighbour keeps mapping to `peer`, so each
        // connection stays both a live INBOUND source (its frames route to this
        // peer's canonical inbox below) and a SEND / failover path in the peer's
        // PathSet (Link::send_failover picks the best one per send). This removes the
        // stub's "frame lost on the old path during the switch" residual. A
        // connection that actually dies is pruned on the first failed send, and will
        // be pruned proactively by cleanup-on-close (next P2 increment). The reliable
        // inbox is still keyed by identity (overwritten below), so the latest
        // verified Link is the canonical receive handle — a single stable Link object
        // across reconnects is the remaining PeerLink step.
        self.verified_neighbors.lock().insert(neighbor.clone(), peer);
        // A freshly verified neighbour may be a new WAN UDP AutoNAT verifier (e.g. the relay's UDP
        // carrier just came up) — wake the self-reachability loop so it confirms our WAN endpoint NOW
        // instead of on the next announce tick. Over-signalling is harmless: the loop re-checks
        // `verified_wan_peer()` and no-ops without a real direct-UDP-WAN verifier. Phase B.
        self.autonat_trigger.notify_one();
        // Start the liveness clock now — a freshly verified neighbour is alive.
        self.last_seen.lock().insert(neighbor.clone(), Instant::now());
        let (inbox_tx, inbox_rx) = mpsc::channel(64);
        // Keyed by the proven peer identity (for a direct link the neighbor maps 1:1 to this peer);
        // multi-hop inbound frames route here by `data.from`. REUSE an existing canonical inbox if
        // the peer already has a live one — a reconnect or the relay→direct upgrade must not orphan
        // the app's inbound pump (same hazard as register_routed_link_channels). Only the first link
        // wires the channel; the handshake-buffered frames + future inbound flush into whichever
        // sender is canonical, and a re-dial's returned rx detaches with its throwaway Link.
        let reused = Self::install_canonical_inbox(&self.link_inboxes, peer, inbox_tx);
        crate::tcp_transport::tcp_trace(
            "net/inbox",
            &format!("verified peer={peer:?} neighbor={neighbor:?} reused={reused}"),
        );
        let canonical_inbox = self
            .link_inboxes
            .lock()
            .get(&peer)
            .cloned()
            .expect("canonical inbox was just installed");
        // Flush anything buffered during the handshake, keeping only frames
        // whose `from` matches the identity the neighbor just proved.
        if let Some(buffered) = self.pending_data.lock().remove(neighbor) {
            for (from, payload) in buffered {
                if from == peer {
                    let _ = canonical_inbox.try_send(payload);
                }
            }
        }

        // Datagram inbox (media). No pre-verification buffering — media is
        // loss-tolerant, so anything that arrived early was already dropped. Keyed by the proven
        // peer IDENTITY (like the reliable inbox above), so the dispatcher can deliver a media
        // datagram to it by the envelope's `from` — including a relayed one whose arrival neighbour
        // is the relay, not the source. Reused across re-dials for the same reason.
        let (datagram_tx, datagram_rx) = mpsc::channel(256);
        Self::install_canonical_inbox(&self.link_datagram_inboxes, peer, datagram_tx);

        VerifiedLinkChannels {
            peer,
            inbox_rx,
            datagram_rx,
        }
    }

    /// Install `candidate` as the peer's canonical inbox sender, KEEPING the existing one while it
    /// is still live. Returns `true` when the existing entry was kept — i.e. the caller's paired
    /// receiver is detached and must not have a pump spawned on it.
    ///
    /// `or_insert` on its own was the bug, and it cost reception entirely. A sender whose RECEIVER
    /// has been dropped (the previous Link's pump exited) stayed in the map forever: every inbound
    /// frame for that peer was `try_send`-ed into a dead channel and discarded, and no re-dial could
    /// ever attach a working pump because `or_insert` never replaces. Live trace on a mobile path
    /// (2026-08-12): a re-dial logs `net/inbox … reused=true`, the pump spawned on its detached
    /// receiver logs `pump/exit` in the SAME millisecond, and nothing is delivered for minutes —
    /// while the shell still shows the contact online. See docs/bugs/mesh-link-flaps-on-mobile.md.
    ///
    /// `is_closed()` separates the two cases exactly: the receiver is gone iff the pump that owned
    /// it is gone. So a LIVE pump is still never orphaned (the hazard `or_insert` was protecting
    /// against — see `register_routed_link_channels`), and a DEAD one no longer black-holes the peer.
    fn install_canonical_inbox(
        map: &SyncMutex<HashMap<NodeAddress, mpsc::Sender<Vec<u8>>>>,
        peer: NodeAddress,
        candidate: mpsc::Sender<Vec<u8>>,
    ) -> bool {
        let mut guard = map.lock();
        match guard.get(&peer) {
            Some(existing) if !existing.is_closed() => true,
            _ => {
                guard.insert(peer, candidate);
                false
            }
        }
    }

    /// Create Link channels for a peer reached over a multi-hop path — used both for an outbound
    /// multi-hop `dial` and for an inbound on-demand incoming Link. Unlike `register_verified_link`
    /// this does NOT mark a verified neighbor (the peer isn't our neighbor; its identity is
    /// E2E-verified above mesh). Both planes are keyed by the peer identity: reliable frames route
    /// here by `data.from`, and (R2) media datagrams destined for us route here by the envelope's
    /// `from` too — so a relayed Link's `recv_datagram` works (the relay forwards the datagram, the
    /// destination delivers it here by identity).
    fn register_routed_link_channels(&self, peer: NodeAddress) -> VerifiedLinkChannels {
        let (inbox_tx, inbox_rx) = mpsc::channel(64);
        let (datagram_tx, datagram_rx) = mpsc::channel(256);
        // Keep the canonical inbox keyed by peer IDENTITY while a pump is still reading it: a
        // re-dial (e.g. the periodic relay→direct punch-upgrade in `upgrade_relay_links_via_punch`,
        // which DISCARDS the Link it produces) must NOT overwrite the inbox the app's inbound pump
        // is already reading — that silently black-holes every inbound frame for this peer after the
        // first maintenance tick (the live relay-chat decay: pairing works, then chat sent ~20 s
        // later never lands, docs/bugs/relay-pairing-approval-gap.md). When the previous pump is
        // GONE the entry is replaced instead of kept — see `install_canonical_inbox` for why
        // "keep unconditionally" black-holed the peer just as thoroughly, from the other side.
        let reused = Self::install_canonical_inbox(&self.link_inboxes, peer, inbox_tx);
        crate::tcp_transport::tcp_trace(
            "net/inbox",
            &format!("routed peer={peer:?} reused={reused}"),
        );
        Self::install_canonical_inbox(&self.link_datagram_inboxes, peer, datagram_tx);
        VerifiedLinkChannels {
            peer,
            inbox_rx,
            datagram_rx,
        }
    }

    /// Record a transport path to `peer` and return the shared `PathSet` (created
    /// on first use). The peer's `Link` holds the same `Arc`, so a later, better
    /// path added here silently upgrades the Link's sends. P2 / PeerLink.
    fn register_path(
        &self,
        peer: NodeAddress,
        neighbor: NeighborId,
        iface: InterfaceHandle,
        quality: PathQuality,
        initiated_locally: bool,
    ) -> Arc<SyncMutex<PathSet>> {
        let set = {
            let mut map = self.peer_paths.lock();
            map.entry(peer)
                .or_insert_with(|| Arc::new(SyncMutex::new(PathSet::new())))
                .clone()
        };
        // Snapshot reachability BEFORE adding the path so we can detect the unreachable→relay
        // transition (a relay link just came up for a peer we couldn't reach) and fire the
        // event-driven punch-upgrade. Re-learns of an already-reachable peer don't re-signal — the
        // reconcile sweep handles ongoing retries. Direct paths never signal (already the goal).
        let was_reachable = { set.lock().best().is_some() };
        // Seed end-to-end last-seen when a RELAY path is first registered, so the relay-peer dead
        // clock starts at link time (a relay-reached peer that never answers a probe still ages out).
        // `or_insert` only — a routed PeerPong / Data refreshes it; re-registration (e.g. the carrier
        // climber) must NOT reset it, else a dead peer would never be pruned. Direct peers are covered
        // by per-neighbour liveness, so they need no seed here.
        if matches!(quality, PathQuality::Relay { .. }) && peer != self.local_address {
            self.peer_last_seen.lock().entry(peer).or_insert_with(Instant::now);
        }
        set.lock().add(PeerPath {
            neighbor,
            iface,
            quality,
            initiated_locally,
        });
        if !was_reachable && peer != self.local_address {
            let now_relay_only = {
                set.lock()
                    .best()
                    .map(|p| matches!(p.quality, PathQuality::Relay { .. }))
                    .unwrap_or(false)
            };
            if now_relay_only {
                let _ = self.relay_upgrade_tx.send(peer);
            }
        }
        set
    }

    /// Record a neighbour's report of the address it saw us connect from. Only
    /// verified neighbours count (a route candidate vouching for what it saw;
    /// aggregated across ≥2 by `infer_reflexive`). P1 / external review §4.1A.
    fn handle_observed_address(&self, frame: ObservedAddressFrame, neighbor: &NeighborId) {
        if !self.verified_neighbors.lock().contains_key(neighbor) {
            return;
        }
        self.reflexive_observations
            .lock()
            .insert(neighbor.clone(), frame.observed);
    }

    /// Deliver inbound Data to the matching Link inbox, relay it to a next-hop
    /// (multi-hop), or drop it.
    ///
    /// Delivery (`to == self`): accepted only if it arrived over a **verified
    /// neighbor link** (the relay is authenticated — no injection from
    /// strangers), then routed to the Link inbox for `data.from` (the END-peer
    /// identity), which on a multi-hop path differs from the arrival neighbor.
    /// Origin authenticity + confidentiality are the E2E layer's job (the mesh
    /// only attests the frame traversed authenticated hops). An unsolicited
    /// origin with no Link yet gets an on-demand incoming Link, bounded by
    /// `MAX_MULTIHOP_ORIGINS`. См. ADR mesh-multihop-identity-delivery.md.
    async fn handle_data(
        &self,
        data: DataFrame,
        neighbor: &NeighborId,
        arrival_iface: &InterfaceHandle,
    ) {
        // Mesh-level dedup: drop replays and duplicate multi-hop fan-out — the
        // same logical frame arriving via two paths carries the same
        // (origin, msg_id). Bounded FIFO window; orthogonal to the verified-relay
        // gate below. См. external review §3.4.
        if !self.seen_frames.lock().note((data.from, data.msg_id)) {
            return;
        }
        let arrival_verified = self.verified_neighbors.lock().contains_key(neighbor);
        let disposition = decide_data_forward(
            &self.local_address,
            &data,
            neighbor,
            arrival_verified,
            &self.path_table,
            self.relay_enabled,
            Instant::now(),
        );
        // Env-gated forwarding trace (relay diagnosis): a logged line for `to=<peer>` proves the
        // frame REACHED this node over the wire — so a non-arriving relayed chat is a routing DROP
        // here, NOT an ISP wire drop (the frame rides the same verified link as the liveness /
        // pairing frames that keep flowing). `-> FORWARD` vs `-> DROP` shows the forwarding decision.
        {
            let what = match &disposition {
                DataDisposition::DeliverLocal => "deliver-local",
                DataDisposition::Forward { .. } => "FORWARD",
                DataDisposition::Drop => "DROP",
            };
            net_trace(&format!(
                "DATA to={:?} from={:?} via={neighbor:?} verified={arrival_verified} -> {what}",
                data.to, data.from
            ));
        }
        match disposition {
            DataDisposition::DeliverLocal => {
                if !arrival_verified {
                    // Neighbor handshake not yet complete. If one is in flight,
                    // buffer briefly so Data that raced ahead of the final
                    // handshake frame isn't lost; otherwise drop (unsolicited).
                    let handshaking = self.outbound_challenges.lock().contains_key(neighbor);
                    if handshaking {
                        let mut pending = self.pending_data.lock();
                        let queue = pending.entry(neighbor.clone()).or_default();
                        if queue.len() < MAX_PENDING_DATA_FRAMES {
                            queue.push((data.from, data.payload));
                        }
                    }
                    return;
                }

                // NB: deliberately do NOT refresh `peer_last_seen` from delivered Data. A relay
                // store-and-forwards a dead peer's BUFFERED frames and re-drains them for a while AFTER
                // it dies, so Data is NOT proof of CURRENT liveness — counting it re-seeds the dead peer
                // and defeats the relay-only-peer prune (live trace: a killed relay peer was re-seeded
                // 33× by drained Data — pruned at ~26 s then refreshed every ~3 s, forever). Relay-peer
                // liveness is driven ONLY by the SOLICITED routed `PeerPong` (a live peer answers our
                // `PeerPing`; the relay forwards both). См. docs/bugs/relay-presence-false-online-dead-peer.md.
                // Route by origin identity. Existing Link inbox → deliver — while the origin still
                // has a path. A pruned one (a relay-only peer silent for a dead window) keeps its inbox,
                // and its frames kept arriving there while it stayed «unreachable» for good: nothing
                // re-registered the path, so it was never probed again (stand 2026-10-15: the laptop's
                // Alice got the caller's frames and could not answer the call — «no live path»).
                // Such a frame goes on below and re-registers the path like a first one; a dead peer
                // whose relay drains old frames is probed and pruned again (`relay_peer_liveness_plan`).
                let has_path = self
                    .peer_paths
                    .lock()
                    .get(&data.from)
                    .is_some_and(|set| set.lock().best().is_some());
                let inbox = if has_path { self.link_inboxes.lock().get(&data.from).cloned() } else { None };
                if let Some(tx) = inbox {
                    // Best-effort: drop if the receiver is too slow.
                    let result = tx.try_send(data.payload);
                    // Diagnostic: a `full`/`closed` here means the canonical inbox has NO live
                    // consumer — frames reach the mesh but never the app (the exact failure the
                    // relay-less door workstream chased). Low-volume: chat/hello/receipt frames.
                    crate::tcp_transport::tcp_trace(
                        "net/deliver-local",
                        &format!(
                            "from={:?} result={}",
                            data.from,
                            match &result {
                                Ok(()) => "ok",
                                Err(mpsc::error::TrySendError::Full(_)) => "full",
                                Err(mpsc::error::TrySendError::Closed(_)) => "closed",
                            }
                        ),
                    );
                    return;
                }

                // No Link yet for this origin — it reached us over a multi-hop
                // path without us dialing it (an inbound multi-hop conversation).
                // Admit a bounded number of distinct unsolicited origins, capped
                // per-relay so one bad verified neighbor can't exhaust the pool.
                let admit = admit_multihop_origin(
                    &mut self.multihop_origins.lock(),
                    data.from,
                    neighbor,
                    MAX_MULTIHOP_ORIGINS_PER_RELAY,
                    MAX_MULTIHOP_ORIGINS_TOTAL,
                );
                if !admit {
                    return;
                }
                // Create the incoming Link over the arrival neighbor (the reply
                // path back toward the origin) and deliver this first frame.
                let channels = self.register_routed_link_channels(data.from);
                if let Some(tx) = self.link_inboxes.lock().get(&data.from).cloned() {
                    let _ = tx.try_send(data.payload);
                }
                // «МЫ ЭТОГО НЕ НАБИРАЛИ» — НЕ ТО ЖЕ САМОЕ, ЧТО «ПРИШЛО ЧЕРЕЗ ПОСРЕДНИКА».
                //
                // Здесь стоял безусловный `Relay { hops: 1 }` с признанием прямо в комментарии:
                // «exact hop count unknown here». Единица была не измерением, а заглушкой. В сети
                // из двух узлов пир набирает НАС, его кадры приходят прямо по его входящему
                // соединению, своего пути у нас ещё нет — и связь получала ярлык «через релей,
                // 1 прыжок» при том, что релея в топологии нет вовсе.
                //
                // Живой прогон 2026-08-31: `link=online_via_relay relay_hops=1`, при этом
                // `rendezvous/dial outcome: Dialed { …, relay_bundle: None }`, один-единственный
                // удалённый адрес за весь звонок и ни одного третьего узла. Пользователь оспорил
                // ярлык раньше, чем это заметил я.
                //
                // Спрашиваем проверенную личность соседа, по которому кадр приехал: если она И
                // ЕСТЬ отправитель — посредника между нами нет, путь прямой. Это та же правка,
                // что и для `cost == 0` в `dial_multihop`, и та же ошибка вывода: путь, который
                // не набирали мы, объявлялся релейным.
                let arrival_is_the_origin = self
                    .verified_neighbors
                    .lock()
                    .get(neighbor)
                    .is_some_and(|verified| *verified == data.from);
                let quality = if arrival_is_the_origin {
                    PathQuality::Direct
                } else {
                    // Отправитель — не тот, с кем мы соединены: между нами действительно кто-то
                    // есть. Точное число прыжков отсюда не видно, единица — нижняя граница.
                    PathQuality::Relay { hops: 1 }
                };
                let paths = self.register_path(
                    data.from,
                    neighbor.clone(),
                    Arc::clone(arrival_iface),
                    quality,
                    false, // не наш дозвон в обоих случаях
                );
                let link = Link {
                    local: self.local_address,
                    destination: data.from,
                    paths,
                    inbox: AsyncMutex::new(channels.inbox_rx),
                    datagram_inbox: AsyncMutex::new(channels.datagram_rx),
                };
                let _ = self.incoming_links_tx.try_send(link);
            }
            // Phase β multi-hop: relay to the resolved next-hop interface/neighbor.
            DataDisposition::Forward {
                interface,
                neighbor: next_hop,
                frame,
            } => {
                if let Some(iface) = self.interfaces.iter().find(|i| i.id() == interface) {
                    let send_result = iface
                        .send_frame(&next_hop, &NetworkFrame::Data(frame).encode_to_wire())
                        .await;
                    // The forwarding DECISION was logged FORWARD above, but the actual wire send can
                    // still fail when the next-hop connection is dead (peer churn: a re-dial opened a
                    // new connection and the old one — which a stale route still points at — closed,
                    // or the NAT pinhole lapsed). Best-effort delivery, BUT prune the dead next-hop so
                    // we don't keep forwarding into a black hole: clearing the stale route lets the
                    // liveness seed re-point to the peer's LIVE connection on its next inbound frame
                    // (its liveness Pong arrives within a tick). Without this the relay logged FORWARD
                    // yet the relayed chat/media silently never arrived. См. relay-pairing-approval-gap.md.
                    if let Err(error) = send_result {
                        net_trace(&format!(
                            "forward send_frame to {next_hop:?} FAILED: {error} — pruning dead next-hop"
                        ));
                        self.prune_path(&next_hop);
                    } else {
                        net_trace(&format!("forward send_frame to {next_hop:?} ok"));
                    }
                }
            }
            DataDisposition::Drop => {}
        }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        for task in self.background_tasks.lock().drain(..) {
            task.abort();
        }
    }
}

/// Quality of a transport path to a peer (its hop topology). The path selector minimises
/// [`effective_path_rank`], which combines this with the path's CARRIER (its `iface.kind()`): `Direct` always
/// beats relay; among relays a UDP carrier beats the TCP-relay base (rung 1 > rung 0), then fewer
/// hops. Carrier is NOT stored here — the interface is the single source of truth. See the PeerLink
/// model in docs/decisions/mesh-peer-link-multipath.md + docs/decisions/connection-quality-ladder.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathQuality {
    /// A direct link to the peer (it is our verified neighbour).
    Direct,
    /// A relayed path: the peer is reached via a next-hop neighbour, `hops` away.
    Relay { hops: u8 },
}

/// Public, FFI-safe view of how a peer is currently reached — the crate's public
/// surface for the internal (private) `PathQuality`. `Direct` = a verified-neighbour
/// single hop; `Relay { hops }` = reached via a next-hop neighbour `hops` away. Lets the
/// shell render an honest per-contact link state (direct vs via-relay). See
/// docs/decisions/omni-network-model.md (honest connection-state surface).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkQuality {
    Direct,
    Relay { hops: u8 },
}

/// The CARRIER axis, independent of topology: QUIC-over-UDP is always preferred; TCP is only worth
/// riding where UDP is throttled or cut (it pays TLS + TCP head-of-line blocking). A relay is a ROLE
/// a node volunteers for and implies nothing about a carrier — every hop picks its carrier from what
/// the network actually allows, on a peer hop and a relay hop alike.
fn carrier_rank(kind: InterfaceKind) -> u8 {
    match kind {
        InterfaceKind::QuicOverTcp => 1,
        // QuicOverUdp / Mdns / future kinds are UDP-class.
        _ => 0,
    }
}

/// Path rank for the connection-quality ladder (lower = better) — two INDEPENDENT axes compared
/// lexicographically: **topology** (Direct beats Relay) then **carrier** (QUIC beats TCP) then hop
/// count. Carrier is the path's interface `kind()` (single source of truth; no carrier state
/// duplicated into `PathQuality`).
///
/// The tuple is what lets carrier apply to DIRECT paths too — the fix for R1a #3. It used to be a
/// flat `u16` with `Direct => 0`, which ignored the carrier: two direct links to one peer (e.g. a
/// relay's A↔R over TCP :8443 and over UDP :443) tied, so our own traffic could ride TCP with UDP
/// healthy, against "QUIC on every hop unless it is blocked". A flat encoding could not fix it —
/// making Direct-TCP nonzero would collide it with Relay-UDP{hops:1}=1 — but a lexicographic tuple
/// keeps every order intact: topology 0 (Direct) sorts below topology 1 (Relay) whatever the carrier,
/// so Direct-TCP still beats Relay-UDP, and within each topology QUIC beats TCP then fewer hops.
///
/// This composes with the simultaneous-open tie-break in `best_preferring`: carrier is a higher key,
/// so two direct paths of DIFFERENT carriers no longer tie (both ends pick the globally-preferred
/// QUIC one → same physical connection); two of the SAME carrier still tie and fall to
/// `initiated_locally`, exactly as before. See docs/decisions/connection-quality-ladder.md.
/// [`path_rank_parts`] with the connection-quality-ladder FAIL-BACK applied: when `udp_demoted` is
/// active for the peer, a direct-UDP path is ranked BELOW direct-TCP (carrier axis forced to 2, worse
/// than TCP's 1) so `best()` sends over the reliable TCP door. Only direct-UDP is affected;
/// direct-TCP, relay, and every other carrier keep their normal rank. `udp_demoted = false` →
/// identical to the plain ranking (no behaviour change on healthy links).
/// See docs/decisions/connection-quality-ladder.md.
fn effective_path_rank(path: &PeerPath, udp_demoted: bool) -> (u8, u8, u8) {
    // A `QuicOverUdp` path whose packets the mux carries over the peer's TCP door IS a TCP path.
    // Ranked by interface kind alone it tied with a genuine UDP path, so an upgrade that stood up a
    // real UDP path next to it changed nothing: `best()` kept the older (door) one.
    let carrier = if path_rides_door(path) {
        InterfaceKind::QuicOverTcp
    } else {
        path.iface.kind()
    };
    effective_path_rank_parts(path.quality, carrier, udp_demoted)
}

fn path_rides_door(path: &PeerPath) -> bool {
    matches!(path.quality, PathQuality::Direct)
        && path.iface.kind() == InterfaceKind::QuicOverUdp
        && path
            .iface
            .remote_addr(&path.neighbor)
            .is_some_and(|addr| path.iface.peer_on_door(addr))
}

/// Pure two-axis ranking with the UDP fail-back demotion — no `PeerPath`/`InterfaceHandle`, so it is
/// unit-testable without standing up interfaces. A demoted direct-UDP path gets carrier axis 2 (below
/// TCP's 1); everything else is `path_rank_parts` unchanged.
fn effective_path_rank_parts(
    quality: PathQuality,
    carrier: InterfaceKind,
    udp_demoted: bool,
) -> (u8, u8, u8) {
    let rank = path_rank_parts(quality, carrier);
    if udp_demoted
        && matches!(quality, PathQuality::Direct)
        && carrier == InterfaceKind::QuicOverUdp
    {
        (rank.0, 2, rank.2)
    } else {
        rank
    }
}

/// The ranking over the two axes alone — no `PeerPath`/`InterfaceHandle`, so the order is
/// unit-testable without standing up interfaces.
fn path_rank_parts(quality: PathQuality, carrier: InterfaceKind) -> (u8, u8, u8) {
    let carrier = carrier_rank(carrier);
    match quality {
        PathQuality::Direct => (0, carrier, 0),
        PathQuality::Relay { hops } => (1, carrier, hops),
    }
}

/// One way to reach a peer: send over `neighbor` on `iface`, with `quality`.
/// `initiated_locally` records whether WE dialed this connection (vs accepted the
/// peer's dial) — the deterministic tie-break for simultaneous-open (P2): among
/// equal-quality paths both endpoints converge on the connection dialed by the
/// peer with the smaller NodeAddress, so the redundant connection goes idle.
#[derive(Clone, Debug)]
struct PeerPath {
    neighbor: NeighborId,
    iface: InterfaceHandle,
    quality: PathQuality,
    initiated_locally: bool,
}

/// The set of currently-known transport paths to a single peer. Shared (`Arc`)
/// between the owning `Network` (which adds/updates/removes paths) and the
/// peer's `Link` (which selects the best one to send over). This is the heart of
/// PeerLink multi-path: relay-first + silent upgrade-to-direct + failover.
#[derive(Debug)]
struct PathSet {
    paths: Vec<PeerPath>,
    /// Connection-quality ladder FAIL-BACK (R1a): when set-and-fresh, a direct-UDP path to this peer
    /// is ranked BELOW direct-TCP, so `best()` sends over the reliable TCP door instead. Flagged when
    /// delivery over the UDP path fails (the app's phantom-drop signal): an asymmetric mobile link
    /// keeps UDP *inbound* alive (so per-neighbour `last_seen` never prunes it) while UDP *outbound* is
    /// dead — the ladder must fall back to TCP without waiting for a liveness prune that never comes.
    /// Time-boxed so a UDP path that recovers (network change) climbs back to preferred. `None` =
    /// normal (UDP preferred, "QUIC on every hop unless blocked"). See connection-quality-ladder.md.
    udp_demoted_until: Option<Instant>,
}

impl PathSet {
    fn new() -> Self {
        Self {
            paths: Vec::new(),
            udp_demoted_until: None,
        }
    }

    /// Whether direct-UDP is currently demoted below direct-TCP for this peer (fail-back active).
    fn udp_demoted(&self, now: Instant) -> bool {
        self.udp_demoted_until.is_some_and(|until| now < until)
    }

    /// Add a path, or refresh it if one over the same neighbour already exists.
    fn add(&mut self, path: PeerPath) {
        if let Some(existing) = self.paths.iter_mut().find(|p| p.neighbor == path.neighbor) {
            *existing = path;
        } else {
            self.paths.push(path);
        }
    }

    /// The best current path (lowest effective rank), if any. Applies the UDP fail-back demotion.
    fn best(&self) -> Option<PeerPath> {
        let udp_demoted = self.udp_demoted(Instant::now());
        self.paths
            .iter()
            .min_by_key(|p| effective_path_rank(p, udp_demoted))
            .cloned()
    }

    /// Best path with a deterministic simultaneous-open tie-break: lowest quality
    /// rank first, then — among equal-quality paths — the one whose initiation
    /// side matches `prefer_locally_initiated`. The caller sets that to
    /// `local_addr < peer_addr`, so both endpoints pick the SAME physical
    /// connection (the one dialed by the smaller-addressed peer) and the
    /// redundant simultaneous-open connection is left to idle out. P2.
    fn best_preferring(&self, prefer_locally_initiated: bool) -> Option<PeerPath> {
        let udp_demoted = self.udp_demoted(Instant::now());
        self.paths
            .iter()
            .min_by(|a, b| {
                effective_path_rank(a, udp_demoted)
                    .cmp(&effective_path_rank(b, udp_demoted))
                    .then_with(|| {
                        let a_match = a.initiated_locally == prefer_locally_initiated;
                        let b_match = b.initiated_locally == prefer_locally_initiated;
                        // A matching path sorts first (becomes the min/best).
                        b_match.cmp(&a_match)
                    })
            })
            .cloned()
    }

    /// All paths in the same order `best_preferring` would pick them, best first.
    ///
    /// For callers that want to TRY candidates without touching routing state — notably the bulk
    /// plane, where a path being unable to open a stream says nothing about its health for frames
    /// (it may simply have no bulk plane), so removing it would sabotage chat.
    /// The paths a bulk stream may open on: DIRECT ones only, best first. A relay path's neighbour
    /// is the relay — a stream opened there lands at the relay, not the destination: live
    /// 2026-10-16, a group member reaching the author through the PC's relay asked the PC instead,
    /// and every ask ended «closed before its note».
    fn bulk_candidates(&self, prefer_locally_initiated: bool) -> Vec<PeerPath> {
        self.ranked_preferring(prefer_locally_initiated)
            .into_iter()
            .filter(|path| path.quality == PathQuality::Direct)
            .collect()
    }

    fn ranked_preferring(&self, prefer_locally_initiated: bool) -> Vec<PeerPath> {
        let udp_demoted = self.udp_demoted(Instant::now());
        let mut ranked: Vec<PeerPath> = self.paths.clone();
        ranked.sort_by(|a, b| {
            effective_path_rank(a, udp_demoted)
                .cmp(&effective_path_rank(b, udp_demoted))
                .then_with(|| {
                    let a_match = a.initiated_locally == prefer_locally_initiated;
                    let b_match = b.initiated_locally == prefer_locally_initiated;
                    b_match.cmp(&a_match)
                })
        });
        ranked
    }

    /// Remove the path over `neighbor` — its connection died or a send failed.
    /// The failover primitive: drop the dead path so `best()` returns the next.
    /// Returns whether a path was actually removed.
    fn remove(&mut self, neighbor: &NeighborId) -> bool {
        let before = self.paths.len();
        self.paths.retain(|p| &p.neighbor != neighbor);
        self.paths.len() != before
    }
}

/// Whether we are directly dialable from outside (AutoNAT, P2). Feeds the
/// relay-vs-direct decision: `Reachable` ⇒ advertise + try direct; `Unreachable`
/// ⇒ rely on relay / hole-punch; `Unknown` ⇒ keep probing. Distinct from P1
/// reflexive (knowing the address) — this proves a fresh peer can use it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReachabilityState {
    #[default]
    Unknown,
    Reachable,
    Unreachable,
}

/// At least this many independent verifier failures before concluding we are not
/// directly reachable (one verifier failing could be that verifier's own fault).
const REACHABILITY_FAILURE_THRESHOLD: u32 = 2;

/// AutoNAT dial-back bookkeeping: which candidate addresses we asked verifiers to
/// reach us at, and the resulting verdict. One successful probe proves dialability
/// (`Reachable`); enough independent failures conclude `Unreachable`. Pure state —
/// the Network drives it from frame handlers + (future) request timeouts.
#[derive(Debug, Default)]
struct ReachabilityTracker {
    /// Outstanding dial-back requests: nonce → candidate we asked about.
    pending: HashMap<u64, SocketAddr>,
    /// Count of verifiers that could not reach us (timed-out requests).
    failures: u32,
    state: ReachabilityState,
    /// The candidate a verifier successfully dialed back, if confirmed.
    confirmed: Option<SocketAddr>,
}

impl ReachabilityTracker {
    /// Record a dial-back request we just sent for `candidate`.
    fn issue(&mut self, nonce: u64, candidate: SocketAddr) {
        self.pending.insert(nonce, candidate);
    }

    /// A `DialBackProbe` arrived. If it matches a pending request, we are proven
    /// directly reachable. Returns whether the nonce matched.
    fn on_probe(&mut self, nonce: u64) -> bool {
        if let Some(candidate) = self.pending.remove(&nonce) {
            self.state = ReachabilityState::Reachable;
            self.confirmed = Some(candidate);
            true
        } else {
            false
        }
    }

    /// A dial-back request timed out (no probe). Counts toward `Unreachable`,
    /// unless we are already proven reachable.
    fn on_timeout(&mut self, nonce: u64) {
        if self.pending.remove(&nonce).is_none() {
            return;
        }
        if self.state == ReachabilityState::Reachable {
            return;
        }
        self.failures += 1;
        if self.failures >= REACHABILITY_FAILURE_THRESHOLD {
            self.state = ReachabilityState::Unreachable;
        }
    }
}

/// True for a globally-routable ("enumerable") address — one a stranger could collect from the open gossip plane.
/// False for addresses that are useless to an enumerator: loopback, RFC1918 private, link-local,
/// CGNAT (100.64.0.0/10), unspecified, and the IPv6 equivalents (loopback / unspecified / unique-
/// local fc00::/7 / link-local fe80::/10). LAN/private addresses stay advertisable (same-LAN
/// multicast discovery); only routable WAN endpoints are withheld from the open gossip plane.
/// Two IPv4 addresses in one /24 — the home-LAN test for an announced private address.
fn same_slash24(a: std::net::Ipv4Addr, b: std::net::Ipv4Addr) -> bool {
    a.octets()[..3] == b.octets()[..3]
}

fn is_enumerable_wan(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let is_cgnat = o[0] == 100 && (o[1] & 0xC0) == 0x40; // 100.64.0.0/10
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || is_cgnat)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            let is_unique_local = (s[0] & 0xfe00) == 0xfc00; // fc00::/7
            let is_link_local = (s[0] & 0xffc0) == 0xfe80; // fe80::/10
            !(v6.is_loopback() || v6.is_unspecified() || is_unique_local || is_link_local)
        }
    }
}

/// Pick the addresses to advertise in an open (signed, hop-forwarded) announce. Explicit LAN /
/// configured addresses are kept; the reflexive (NAT-mapped) address is added only when AutoNAT has
/// confirmed it is directly dialable. **Address privacy:** routable WAN endpoints are
/// then STRIPPED — a relay's or peer's globally-blockable IP must NOT ride the open gossip plane
/// (a hostile mesh member would harvest it via forwarded announces / FoF). WAN reachability is
/// distributed only on the pinned registry + invite bundle + sealed per-pair rendezvous beacon, not
/// here; relay/multicast still reach us regardless (see `is_enumerable_wan`,
/// the relay-address privacy rule). The CI invariant
/// Сколько адресов пира держим в книге. `dial` перебирает их ПОСЛЕДОВАТЕЛЬНО, ожидая каждый
/// QUIC-connect, поэтому длина списка — это прямое время до соединения, а не просто память.
const MAX_PEER_ADVERTISED_ADDRESSES: usize = 8;

/// Happy Eyeballs (RFC 8305) over a dial's `candidates`: start the first connect, start the next
/// one every `DIAL_ATTEMPT_DELAY` (or at once when an attempt fails), and return the FIRST endpoint
/// that connects. The same race as libp2p's concurrent dial, iroh's probing of every path and
/// Syncthing's `dialParallel`.
///
/// All attempts are polled by this one task, so at most one completes per poll: the losers are
/// still mid-handshake when dropped, and dropping a quinn `Connecting` abandons it (implicit
/// close) — nothing is left adopted behind the winner. The losers go back to the FRONT of
/// `candidates` in their order, so if the winner then fails the identity challenge the caller races
/// them again. `None` once every candidate failed to connect.
async fn race_connect(
    candidates: &mut VecDeque<(InterfaceHandle, String)>,
) -> Option<(InterfaceHandle, NeighborId)> {
    async fn attempt(
        id: usize,
        iface: InterfaceHandle,
        endpoint: String,
    ) -> (usize, io::Result<NeighborId>) { (id, iface.connect_endpoint(&endpoint).await) }

    let race_started = Instant::now();
    let total = candidates.len();
    let mut in_flight = FuturesUnordered::new();
    // The candidates behind `in_flight`, in start order — the futures own their copies.
    let mut started: Vec<(usize, (InterfaceHandle, String))> = Vec::new();
    let mut next_id = 0usize;
    let mut start_next = |in_flight: &mut FuturesUnordered<_>,
                          started: &mut Vec<(usize, (InterfaceHandle, String))>,
                          candidates: &mut VecDeque<(InterfaceHandle, String)>| {
        if let Some((iface, endpoint)) = candidates.pop_front() {
            in_flight.push(attempt(next_id, Arc::clone(&iface), endpoint.clone()));
            started.push((next_id, (iface, endpoint)));
            next_id += 1;
        }
    };

    loop {
        if in_flight.is_empty() {
            if candidates.is_empty() {
                return None;
            }
            start_next(&mut in_flight, &mut started, candidates);
        }
        tokio::select! {
            // A finished attempt is looked at before the timer: an endpoint that connects at once
            // (an already-live connection, the net-sim) wins before anything else is started, which
            // keeps the old in-order behaviour wherever connecting is instant.
            biased;
            Some((id, result)) = in_flight.next() => {
                let index = started
                    .iter()
                    .position(|(started_id, _)| *started_id == id)
                    .expect("every attempt in flight was recorded when started");
                let (_, (iface, endpoint)) = started.remove(index);
                match result {
                    Ok(neighbor) => {
                        // Which endpoint won and how many had been started by then: the live
                        // evidence that a dead one ahead no longer holds the dial.
                        crate::tcp_transport::tcp_trace(
                            "dial/race",
                            &format!(
                                "won={endpoint} started={} of {total} abandoned={} elapsed_ms={}",
                                total - candidates.len(),
                                started.len(),
                                race_started.elapsed().as_millis()
                            ),
                        );
                        for (_, candidate) in started.drain(..).rev() {
                            candidates.push_front(candidate);
                        }
                        return Some((iface, neighbor));
                    }
                    // This interface can't reach this endpoint (connectionless, or the connect
                    // failed) — don't wait out the delay for the next one.
                    Err(_) => {
                        if in_flight.len() < DIAL_CONCURRENCY {
                            start_next(&mut in_flight, &mut started, candidates);
                        }
                    }
                }
            }
            _ = tokio::time::sleep(DIAL_ATTEMPT_DELAY),
                if !candidates.is_empty() && in_flight.len() < DIAL_CONCURRENCY =>
            {
                start_next(&mut in_flight, &mut started, candidates);
            }
            else => return None,
        }
    }
}

/// Влить адреса из СВЕЖЕГО маячка рандеву в книгу пира: новые — в начало, список — ограничен.
///
/// Раньше здесь был `push` без вытеснения, и это стоило минут до соединения. У пира за NAT порт
/// меняется при каждом перезапуске: книга копила КАЖДОЕ отображение, которое он когда-либо
/// объявлял, а `dial` честно перебирал их по очереди, ожидая таймаут на каждом мёртвом, прежде чем
/// дойти до живого — который лежал в конце, потому что был добавлен последним. Живой замер
/// 2026-08-29: одна попытка дозвона горела 130 с, а восстановление пары после перезапуска гуляло
/// от 25 с до одиннадцати минут — тем хуже, чем больше перезапусков накопилось за сессию.
///
/// С 2026-10-03 `dial` перекрывает попытки (`race_connect`, RFC 8305): мёртвый адрес стоит
/// `DIAL_ATTEMPT_DELAY`, а не таймаута рукопожатия. Порядок и лимит от этого не стали лишними:
/// порядок решает, кто стартует первым и выигрывает среди живых, лимит — сколько попыток вообще.
///
/// Свежий маячок авторитетен о том, где пир СЕЙЧАС: его адреса идут первыми. Прежние не
/// выбрасываются совсем (LAN-адрес из объявления остаётся полезным), но уходят вниз и вытесняются
/// по лимиту. Путь объявлений так себя и вёл всегда — `handle_announce` заменяет список целиком.
fn merge_rendezvous_endpoints(known: &mut Vec<SocketAddr>, fresh: Vec<SocketAddr>) {
    for addr in fresh.into_iter().rev() {
        known.retain(|existing| existing != &addr);
        known.insert(0, addr);
    }
    known.truncate(MAX_PEER_ADVERTISED_ADDRESSES);
}

/// `announce_never_gossips_a_wan_endpoint` enforces it. Pure → unit-testable. P2 / review §4.1F.
fn select_advertised_addresses(
    mut explicit: Vec<SocketAddr>,
    reflexive: Option<SocketAddr>,
    directly_reachable: bool,
) -> Vec<SocketAddr> {
    if directly_reachable {
        if let Some(reflexive) = reflexive {
            if !explicit.contains(&reflexive) {
                explicit.push(reflexive);
            }
        }
    }
    // Address privacy: never let a routable WAN endpoint onto the open gossip plane.
    explicit.retain(|addr| !is_enumerable_wan(addr));
    explicit
}

#[derive(Debug)]
pub struct Link {
    local: NodeAddress,
    destination: NodeAddress,
    /// Candidate transport paths to `destination` (direct / relay), shared with
    /// the owning `Network`. `send` picks the best current one, so adding a
    /// better path (e.g. direct after relay) silently upgrades and removing the
    /// live one fails over. PeerLink model (mesh-peer-link-multipath.md).
    paths: Arc<SyncMutex<PathSet>>,
    /// Reliable, ordered frame plane (chat / control / file transfer).
    inbox: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
    /// Unreliable datagram plane (realtime media — opus / RTP).
    datagram_inbox: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
}

impl Link {
    pub fn local(&self) -> NodeAddress {
        self.local
    }

    pub fn destination(&self) -> NodeAddress {
        self.destination
    }

    /// Send byte payload to destination. Frame wrapped как Data variant and
    /// transmitted over the best current path, failing over to the next-best.
    pub async fn send(&self, payload: &[u8]) -> io::Result<()> {
        let frame = NetworkFrame::Data(DataFrame::origin(
            self.local,
            self.destination,
            payload.to_vec(),
        ));
        self.send_failover(&frame.encode_to_wire(), false).await
    }

    /// Send `bytes` over the best current path, failing over to the next-best
    /// (and dropping the dead path) when a send errors. `datagram` selects the
    /// unreliable plane. Errors only when no path succeeds — the silent-failover
    /// half of relay-first (PeerLink, mesh-peer-link-multipath.md). Because
    /// `PathSet::add` dedups by neighbour, each failed `remove` strictly shrinks
    /// the set, so the loop terminates.
    async fn send_failover(&self, bytes: &[u8], datagram: bool) -> io::Result<()> {
        // Simultaneous-open tie-break: prefer the connection dialed by the smaller-
        // addressed peer, so both endpoints converge on one (P2). Cheap byte compare.
        let prefer_locally_initiated = self.local.as_bytes() < self.destination.as_bytes();
        let mut last_err: Option<io::Error> = None;
        loop {
            let path = match self.paths.lock().best_preferring(prefer_locally_initiated) {
                Some(path) => path,
                None => {
                    return Err(last_err.unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::NotConnected, "no path to destination")
                    }));
                }
            };
            let result = if datagram {
                path.iface.send_datagram(&path.neighbor, bytes).await
            } else {
                path.iface.send_frame(&path.neighbor, bytes).await
            };
            match result {
                Ok(()) => return Ok(()),
                Err(err) => {
                    self.paths.lock().remove(&path.neighbor);
                    last_err = Some(err);
                }
            }
        }
    }

    /// Receive next byte payload from destination. Block'ает пока frame
    /// не придёт.
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
        let mut inbox = self.inbox.lock().await;
        inbox
            .recv()
            .await
            .ok_or_else(|| io::Error::other("link inbox closed"))
    }

    /// Send an unreliable datagram (realtime media). No retransmit / ordering;
    /// dropped under loss. Rides the QUIC DATAGRAM extension over the verified
    /// connection — the carrier for opus/RTP.
    pub async fn send_datagram(&self, payload: &[u8]) -> io::Result<()> {
        // Wrap media in the routing envelope so a relay can forward it toward `destination` and the
        // destination delivers it by source identity (R2). On a direct path the destination IS the
        // neighbour and consumes it immediately (`to == self`); the hop limit only bounds relay.
        let wire = MediaDatagram {
            from: self.local,
            to: self.destination,
            hop_limit: MEDIA_DATAGRAM_DEFAULT_HOP_LIMIT,
            payload: payload.to_vec(),
        }
        .encode();
        self.send_failover(&wire, true).await
    }

    /// Receive the next inbound datagram from destination. Blocks until one
    /// arrives.
    pub async fn recv_datagram(&self) -> io::Result<Vec<u8>> {
        let mut inbox = self.datagram_inbox.lock().await;
        inbox
            .recv()
            .await
            .ok_or_else(|| io::Error::other("link datagram inbox closed"))
    }

    /// Open a [`BulkStream`] to the destination for a bulk payload (attachment transfer).
    ///
    /// Unlike `send`/`send_datagram` this is a DIRECT-path facility: the stream lives on the
    /// neighbour's own connection, so it is offered only while a direct bulk-capable path exists
    /// (no store-and-forward through a relay hop). Path selection matches `send_failover`, and a
    /// path that cannot open a stream is dropped and the next-best tried — the same silent-failover
    /// behaviour. Callers treat `Err` as "no bulk lane right now" and fall back.
    pub async fn open_bulk_stream(&self) -> io::Result<BulkStream> {
        let prefer_locally_initiated = self.local.as_bytes() < self.destination.as_bytes();
        // Snapshot the candidates instead of consuming the live PathSet: a path that cannot open a
        // bulk stream is NOT thereby a dead path (it may just have no bulk plane), and pruning it
        // here would take chat's route down with it.
        let candidates = self.paths.lock().bulk_candidates(prefer_locally_initiated);
        let mut last_err: Option<io::Error> = None;
        for path in candidates {
            if !path.iface.supports_bulk_streams() {
                continue;
            }
            match path.iface.open_bulk_stream(&path.neighbor).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "no bulk path to destination")
        }))
    }
}

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("cannot dial own address (would loop)")]
    DialSelf,
    #[error("no reachable address for destination {0:?} (no verified announce?)")]
    NoPathToDestination(NodeAddress),
    #[error("interface {0} no longer in network's interface list")]
    InterfaceMissing(String),
    #[error("failed to send identity challenge: {0}")]
    ChallengeSend(String),
    #[error("identity challenge timed out")]
    ChallengeTimeout,
    #[error("identity challenge failed before a response arrived")]
    ChallengeFailed,
    #[error("peer failed identity verification: {0}")]
    ChallengeRejected(String),
}

#[derive(Debug, Error)]
pub enum AnnounceHandlerError {
    #[error("announce verify failed: {0}")]
    Verify(#[from] crate::announce::AnnounceError),
}

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DATA_DEFAULT_HOP_LIMIT;

    // --- Phase β: pure multi-hop Data forwarding decision ---
    fn node() -> NodeAddress {
        NodeAddress::from_pubkey(&fresh_signing().verifying_key())
    }

    #[test]
    fn forward_decision_delivers_when_addressed_to_self() {
        let me = node();
        let data = DataFrame::origin(node(), me, b"hi".to_vec());
        let d = decide_data_forward(
            &me, &data, &NeighborId("n".into()), true, &PathTable::new(), true, Instant::now(),
        );
        assert_eq!(d, DataDisposition::DeliverLocal);
    }

    #[test]
    fn forward_decision_drops_when_relay_disabled() {
        let (me, dest) = (node(), node());
        let pt = PathTable::new();
        pt.update(dest, InterfaceId("if".into()), NeighborId("nh".into()), 1, Duration::from_secs(300), Instant::now());
        let data = DataFrame::origin(node(), dest, b"x".to_vec());
        let d = decide_data_forward(&me, &data, &NeighborId("src".into()), true, &pt, false, Instant::now());
        assert_eq!(d, DataDisposition::Drop);
    }

    #[test]
    fn forward_decision_drops_when_hop_budget_spent() {
        let (me, dest) = (node(), node());
        let pt = PathTable::new();
        pt.update(dest, InterfaceId("if".into()), NeighborId("nh".into()), 1, Duration::from_secs(300), Instant::now());
        let mut data = DataFrame::origin(node(), dest, b"x".to_vec());
        data.hop_limit = 0;
        let d = decide_data_forward(&me, &data, &NeighborId("src".into()), true, &pt, true, Instant::now());
        assert_eq!(d, DataDisposition::Drop);
    }

    #[test]
    fn forward_decision_drops_when_no_path() {
        let (me, dest) = (node(), node());
        let data = DataFrame::origin(node(), dest, b"x".to_vec());
        let d = decide_data_forward(&me, &data, &NeighborId("src".into()), true, &PathTable::new(), true, Instant::now());
        assert_eq!(d, DataDisposition::Drop);
    }

    #[test]
    fn forward_decision_drops_when_source_neighbor_unverified() {
        // Anti-open-relay: a frame to relay arriving from a neighbor that has NOT
        // passed the mutual challenge is dropped, never forwarded — otherwise
        // anyone who completed the AcceptAny QUIC handshake could relay arbitrary
        // traffic to third parties through us. Same inputs as the successful-
        // forward case, only the source neighbor is unverified.
        let (me, dest) = (node(), node());
        let now = Instant::now();
        let pt = PathTable::new();
        pt.update(dest, InterfaceId("eth".into()), NeighborId("nh".into()), 1, Duration::from_secs(300), now);
        let data = DataFrame::origin(node(), dest, b"x".to_vec());
        let d = decide_data_forward(&me, &data, &NeighborId("src".into()), false, &pt, true, now);
        assert_eq!(d, DataDisposition::Drop);
    }

    #[test]
    fn forward_decision_forwards_with_decremented_hop_limit() {
        let (me, dest) = (node(), node());
        let now = Instant::now();
        let pt = PathTable::new();
        pt.update(dest, InterfaceId("eth".into()), NeighborId("nh".into()), 1, Duration::from_secs(300), now);
        let data = DataFrame::origin(node(), dest, b"payload".to_vec());
        match decide_data_forward(&me, &data, &NeighborId("src".into()), true, &pt, true, now) {
            DataDisposition::Forward { interface, neighbor, frame } => {
                assert_eq!(interface, InterfaceId("eth".into()));
                assert_eq!(neighbor, NeighborId("nh".into()));
                assert_eq!(frame.hop_limit, DATA_DEFAULT_HOP_LIMIT - 1);
                assert_eq!(frame.to, dest);
                assert_eq!(frame.payload, b"payload".to_vec());
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn forward_decision_drops_when_next_hop_is_source_neighbor() {
        // Loop guard: не возвращаем кадр тому соседу, от кого он пришёл.
        let (me, dest) = (node(), node());
        let now = Instant::now();
        let pt = PathTable::new();
        pt.update(dest, InterfaceId("eth".into()), NeighborId("src".into()), 1, Duration::from_secs(300), now);
        let data = DataFrame::origin(node(), dest, b"x".to_vec());
        let d = decide_data_forward(&me, &data, &NeighborId("src".into()), true, &pt, true, now);
        assert_eq!(d, DataDisposition::Drop);
    }

    // --- on-demand multi-hop origin admission (per-relay + global cap) ---
    #[test]
    fn admit_origin_caps_per_relay_then_rejects() {
        let mut origins = HashMap::new();
        let relay = NeighborId("relayA".into());
        for _ in 0..8 {
            assert!(admit_multihop_origin(&mut origins, node(), &relay, 8, 256));
        }
        // 9th from the same relay rejected — one relay can't exceed its cap.
        assert!(!admit_multihop_origin(&mut origins, node(), &relay, 8, 256));
        assert_eq!(origins.len(), 8);
    }

    #[test]
    fn admit_origin_per_relay_cap_is_independent_across_relays() {
        let mut origins = HashMap::new();
        let (a, b) = (NeighborId("A".into()), NeighborId("B".into()));
        for _ in 0..8 {
            assert!(admit_multihop_origin(&mut origins, node(), &a, 8, 256));
        }
        // A is full, but B still has its own budget — one bad relay doesn't
        // starve others.
        assert!(!admit_multihop_origin(&mut origins, node(), &a, 8, 256));
        assert!(admit_multihop_origin(&mut origins, node(), &b, 8, 256));
    }

    #[test]
    fn admit_origin_global_cap_rejects_even_with_relay_budget() {
        let mut origins = HashMap::new();
        for i in 0..3 {
            assert!(admit_multihop_origin(&mut origins, node(), &NeighborId(format!("r{i}")), 100, 3));
        }
        assert!(!admit_multihop_origin(&mut origins, node(), &NeighborId("r-new".into()), 100, 3));
    }

    #[test]
    fn admit_origin_known_origin_is_idempotent() {
        let mut origins = HashMap::new();
        let relay = NeighborId("r".into());
        let o = node();
        assert!(admit_multihop_origin(&mut origins, o, &relay, 1, 1));
        // Same origin again: re-admitted without consuming more budget.
        assert!(admit_multihop_origin(&mut origins, o, &relay, 1, 1));
        assert_eq!(origins.len(), 1);
    }

    // --- durable delivery: store-and-forward + deliver-if-ours (P4) ---
    #[test]
    fn durable_data_delivered_if_ours_stored_if_relayed() {
        let net = Network::new(fresh_signing(), vec![]);
        let me = net.local_address();
        let other = node();
        let mk = |to: NodeAddress, seq: u64| DurableDataFrame {
            to,
            from: node(),
            feed_id: [7u8; 16],
            seq,
            msg_id: seq,
            ttl_secs: 3600,
            ciphertext: vec![1, 2, 3],
            signature: vec![],
        };

        // Addressed to us → delivered to the durable inbox.
        net.handle_durable_data(mk(me, 1));
        let got = net
            .try_recv_durable()
            .expect("a durable frame addressed to us is delivered");
        assert_eq!((got.seq, got.to), (1, me));
        assert!(net.try_recv_durable().is_none());

        // Dedup by (feed, seq): the same frame again is ignored (not re-delivered).
        net.handle_durable_data(mk(me, 1));
        assert!(net.try_recv_durable().is_none());

        // Addressed to someone else → HELD for store-and-forward, not delivered.
        net.handle_durable_data(mk(other, 2));
        assert!(net.try_recv_durable().is_none());
        assert!(
            net.durable_store.lock().contains(&([7u8; 16], 2)),
            "relayed durable frame must be stored so anti-entropy can hand it on"
        );
    }

    // --- group destination subscription + gossip push (G3) ---
    #[test]
    fn subscribed_group_destination_delivers_and_names_push_targets() {
        let net = Network::new(fresh_signing(), vec![]);
        let group_dest = crate::durable::group_destination_address("gid-test");
        let member_b = node();
        let member_c = node();
        let sender = node();
        let mk = |seq: u64, from: NodeAddress| DurableDataFrame {
            to: group_dest,
            from,
            feed_id: [9u8; 16],
            seq,
            msg_id: seq,
            ttl_secs: 3600,
            ciphertext: vec![4, 5, 6],
            signature: vec![],
        };

        // Not subscribed → held for others, NOT delivered, no push.
        let targets = net.handle_durable_data(mk(1, sender));
        assert!(targets.is_empty());
        assert!(net.try_recv_durable().is_none());
        assert!(net.durable_store.lock().contains(&([9u8; 16], 1)));

        // Subscribing hands over the frame held before the subscription (it is already in
        // our store, so no peer would ever offer it again).
        net.subscribe_group_destination(group_dest, vec![member_b, member_c, sender]);
        let held = net.try_recv_durable().expect("held frame delivered on subscribe");
        assert_eq!(held.seq, 1);
        assert!(net.try_recv_durable().is_none());
        // Re-subscribing (membership change) does not deliver it a second time.
        net.subscribe_group_destination(group_dest, vec![member_b, member_c, sender]);
        assert!(net.try_recv_durable().is_none());

        // Subscribed → delivered to the inbox; push targets = members with live
        // paths, excluding the original sender.
        // Give member_b a live path so it becomes a push target; member_c none.
        let iface = Arc::new(FailingInterface {
            id: crate::interface::InterfaceId("group-if".into()),
        }) as InterfaceHandle;
        net.register_path(
            member_b,
            NeighborId("group-conn".into()),
            iface,
            PathQuality::Direct,
            false,
        );
        let targets = net.handle_durable_data(mk(2, sender));
        assert_eq!(targets, vec![member_b]);
        let got = net.try_recv_durable().expect("subscribed frame delivered");
        assert_eq!((got.seq, got.to), (2, group_dest));

        // Our OWN outbound frame is not self-delivered (already applied locally).
        let own = net.handle_durable_data(mk(3, net.local_address()));
        assert_eq!(own, vec![member_b]);
        assert!(net.try_recv_durable().is_none());

        // Dedup still holds for subscribed frames.
        assert!(net.handle_durable_data(mk(2, sender)).is_empty());
        assert!(net.try_recv_durable().is_none());

        // Unsubscribe → back to plain store-and-forward holding.
        net.unsubscribe_group_destination(&group_dest);
        assert!(net.handle_durable_data(mk(4, sender)).is_empty());
        assert!(net.try_recv_durable().is_none());
    }

    #[test]
    fn import_durable_redelivers_only_subscribed_foreign_frames() {
        let net = Network::new(fresh_signing(), vec![]);
        let group_dest = crate::durable::group_destination_address("gid-import");
        let sender = node();
        net.subscribe_group_destination(group_dest, vec![]);
        let mk = |seq: u64, to: NodeAddress, from: NodeAddress| DurableDataFrame {
            to,
            from,
            feed_id: [8u8; 16],
            seq,
            msg_id: seq,
            ttl_secs: 3600,
            ciphertext: vec![1],
            signature: vec![],
        };
        net.import_durable(vec![
            mk(1, group_dest, sender),              // subscribed + foreign → redelivered
            mk(2, group_dest, net.local_address()), // our own → not redelivered
            mk(3, node(), sender),                  // unsubscribed → store-only
        ]);
        let got = net.try_recv_durable().expect("subscribed import redelivered");
        assert_eq!(got.seq, 1);
        assert!(net.try_recv_durable().is_none());
    }

    /// A frame for us that the app has handled survives a restart as a high-water mark, so holders
    /// stop re-offering it; it is not delivered to the app a second time. Negative half: a frame
    /// that reached the network but was never handed to the app is not kept — after the restart our
    /// high-water does not cover it, so the holder offers it again.
    #[test]
    fn handled_frames_for_us_keep_our_high_water_across_a_restart() {
        let signing = fresh_signing();
        let net = Network::new(signing.clone(), vec![]);
        let me = net.local_address();
        let sender = node();
        let feed = [9u8; 16];
        let mk = |seq: u64| DurableDataFrame {
            to: me,
            from: sender,
            feed_id: feed,
            seq,
            msg_id: seq,
            ttl_secs: 3600,
            ciphertext: vec![seq as u8],
            signature: vec![],
        };
        net.handle_durable_data(mk(1));
        net.handle_durable_data(mk(2));
        let first = net.try_recv_durable().expect("seq 1 reaches the app");
        net.note_durable_handled(first.feed_id, first.seq);
        // seq 2 is still waiting in the inbox when the node stops.

        let exported = net.export_durable();
        assert_eq!(
            exported.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![1],
            "only the handled frame for us is exported"
        );

        let restarted = Network::new(signing, vec![]);
        restarted.import_durable(exported);
        assert!(restarted.try_recv_durable().is_none(), "a handled frame is not delivered again");
        let state = restarted.durable_reconcile.lock().state();
        assert_eq!(state.high_water(&feed), 1, "our high-water covers seq 1");

        // The holder still has both; it offers exactly the one we never handled.
        let mut holder = ReconcileEngine::new();
        holder.insert(feed, 1);
        holder.insert(feed, 2);
        assert_eq!(holder.offers_for(&state), vec![(feed, 2)]);
        assert_eq!(
            restarted.export_durable().len(),
            1,
            "the mark is carried into the next export"
        );
    }

    #[test]
    fn redial_reuses_canonical_inbox_and_never_orphans_the_app_pump() {
        // Regression for the live relay-chat decay: the periodic relay→direct upgrade
        // (`upgrade_relay_links_via_punch`) re-dials a peer and DISCARDS the resulting Link. Before
        // the fix, the re-dial's `register_routed_link_channels` overwrote `link_inboxes[peer]`,
        // orphaning the inbox the app's inbound pump was already reading → every inbound frame for
        // that peer black-holed after the first maintenance tick (chat worked at pairing, died
        // ~20 s later). The inbox must stay the SAME channel, canonical by peer identity, across
        // re-dials.
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();

        // First link: the app holds this rx (its inbound pump reads it).
        let mut app_link = net.register_routed_link_channels(peer);
        // A re-dial (maintenance upgrade) — its Link/rx is thrown away.
        let mut throwaway = net.register_routed_link_channels(peer);

        // The dispatcher delivers inbound frames to the canonical inbox keyed by identity.
        let canonical = net
            .link_inboxes
            .lock()
            .get(&peer)
            .cloned()
            .expect("canonical inbox registered");
        canonical
            .try_send(b"hello".to_vec())
            .expect("send to canonical inbox");

        // The APP's original rx receives it — not orphaned by the re-dial.
        assert_eq!(
            app_link.inbox_rx.try_recv().ok(),
            Some(b"hello".to_vec()),
            "the app's original inbox must keep receiving after a re-dial"
        );
        // The discarded re-dial Link's rx is detached (its tx was dropped) — it must NOT have
        // captured the peer's inbound frames.
        assert!(
            throwaway.inbox_rx.try_recv().is_err(),
            "a discarded re-dial Link must not steal the peer's inbound frames"
        );
    }

    #[test]
    fn durable_store_enforces_ttl_dedup_and_caps() {
        let t0 = Instant::now();
        let mk = |from: NodeAddress, seq: u64, ttl: u32| DurableDataFrame {
            to: node(),
            from,
            feed_id: [1u8; 16],
            seq,
            msg_id: seq,
            ttl_secs: ttl,
            ciphertext: vec![],
            signature: vec![],
        };

        // TTL: a frame is GC'd once its ttl elapses (now is injected).
        let mut store = DurableStore::new();
        let src = node();
        assert!(store.insert(mk(src, 1, 10), t0, 100, 100, false));
        assert!(store.contains(&([1u8; 16], 1)));
        store.gc(t0 + Duration::from_secs(11));
        assert!(!store.contains(&([1u8; 16], 1)), "expired frame must be GC'd");
        // ttl 0 is never stored.
        assert!(!store.insert(mk(src, 2, 0), t0, 100, 100, false));

        // Dedup by (feed, seq).
        let mut store = DurableStore::new();
        assert!(store.insert(mk(src, 5, 10), t0, 100, 100, false));
        assert!(!store.insert(mk(src, 5, 10), t0, 100, 100, false));
        assert_eq!(store.len(), 1);

        // Global cap rejects past total_cap.
        let mut store = DurableStore::new();
        for s in 1..=3 {
            assert!(store.insert(mk(node(), s, 10), t0, 3, 100, false));
        }
        assert!(
            !store.insert(mk(node(), 99, 10), t0, 3, 100, false),
            "over global cap"
        );

        // Per-source cap: one `from` is bounded even with global headroom; a
        // different source still has budget.
        let mut store = DurableStore::new();
        let heavy = node();
        for s in 1..=2 {
            assert!(store.insert(mk(heavy, s, 10), t0, 100, 2, false));
        }
        assert!(
            !store.insert(mk(heavy, 3, 10), t0, 100, 2, false),
            "over per-source cap"
        );
        assert!(store.insert(mk(node(), 7, 10), t0, 100, 2, false));
    }

    /// A frame this node needs is never refused for a full budget: it goes in, the source's oldest
    /// frame goes out. Refused, it vanished while `send_durable` said Ok — a node whose quota had
    /// filled with week-long holds for unreachable devices reached its groups not at all, and a
    /// node carrying those holds for it refused that node's group frames too (live 2026-10-10).
    #[test]
    fn a_needed_frame_drops_the_sources_oldest_instead_of_itself() {
        let t0 = Instant::now();
        let source = node();
        let mk = |from: NodeAddress, seq: u64| DurableDataFrame {
            to: node(),
            from,
            feed_id: [2u8; 16],
            seq,
            msg_id: seq,
            ttl_secs: 600,
            ciphertext: vec![],
            signature: vec![],
        };

        // The source's quota is full of frames we only carry; its next frame is one we need.
        let mut store = DurableStore::new();
        for s in 1..=2 {
            assert!(store.insert(mk(source, s), t0 + Duration::from_secs(s), 100, 2, false));
        }
        assert!(!store.insert(mk(source, 3), t0, 100, 2, false), "a carried frame stays refused");
        assert!(
            store.insert(mk(source, 4), t0 + Duration::from_secs(4), 100, 2, true),
            "a needed frame must be stored"
        );
        assert!(!store.contains(&([2u8; 16], 1)), "the source's oldest frame makes room");
        assert!(store.contains(&([2u8; 16], 2)) && store.contains(&([2u8; 16], 4)));

        // Global budget full: the needed frame rotates out its own source's oldest, never another's.
        let mut store = DurableStore::new();
        let other = node();
        assert!(store.insert(mk(other, 10), t0, 3, 100, false));
        assert!(store.insert(mk(other, 11), t0, 3, 100, false));
        assert!(store.insert(mk(source, 12), t0, 3, 100, false));
        assert!(store.insert(mk(source, 13), t0, 3, 100, true));
        assert!(store.contains(&([2u8; 16], 10)) && store.contains(&([2u8; 16], 11)));
        assert!(!store.contains(&([2u8; 16], 12)));
        // With nothing of that source to drop, a full budget still refuses.
        let mut store = DurableStore::new();
        for s in 20..=22 {
            assert!(store.insert(mk(other, s), t0, 3, 100, false));
        }
        assert!(!store.insert(mk(source, 23), t0, 3, 100, true));
    }

    #[test]
    fn durable_export_import_round_trips_held_frames() {
        let net = Network::new(fresh_signing(), vec![]);
        let me = net.local_address();
        let other = node();
        let mk = |to: NodeAddress, feed: u8, msg_id: u64| DurableDataFrame {
            to,
            from: node(),
            feed_id: [feed; 16],
            seq: 1,
            msg_id,
            ttl_secs: 3600,
            ciphertext: vec![feed],
            signature: vec![],
        };
        // Hold one frame for someone else (S&F) and one for ourselves.
        net.handle_durable_data(mk(other, 3, 1));
        net.handle_durable_data(mk(me, 4, 2));
        let _ = net.try_recv_durable(); // drain ours

        // Export persists only the S&F frame (not the one addressed to us).
        let exported = net.export_durable();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].to, other);

        // A fresh node restores it store-only; reconcile is marked so it can offer.
        let restored = Network::new(fresh_signing(), vec![]);
        restored.import_durable(exported);
        assert!(restored.durable_store.lock().contains(&([3u8; 16], 1)));
        assert!(restored.durable_reconcile.lock().contains(&[3u8; 16], 1));
        // Nothing is delivered as "ours" on the fresh node.
        assert!(restored.try_recv_durable().is_none());
    }

    /// P4 convergence: A holds a durable frame for B but has no path to B yet.
    /// On link-up, anti-entropy (have → offer → request → DurableData) reconciles
    /// the held frame to its destination. This is the store-and-forward core that
    /// bridges non-overlapping online windows. См. mesh-durable-delivery.md.
    #[tokio::test]
    async fn durable_frame_reconciled_to_destination_on_link_up() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let a_pub = a_key.verifying_key();
        let b_pub = b_key.verifying_key();

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let b_node = b.local_address();

        // A stores a durable frame for B; no path yet, so it is just held.
        a.send_durable(DurableDataFrame {
            to: b_node,
            from: a.local_address(),
            feed_id: [9u8; 16],
            seq: 1,
            msg_id: 777,
            ttl_secs: 3600,
            ciphertext: vec![1, 2, 3],
            signature: vec![],
        })
        .await
        .unwrap();

        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        a.start();
        b.start();
        a.dial(b.local_address()).await.expect("dial + challenge");

        let got = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(f) = b.try_recv_durable() {
                    break f;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("B receives the durable frame via reconciliation within 5s");
        assert_eq!((got.msg_id, got.to), (777, b_node));

        a.shutdown();
        b.shutdown();
    }

    /// Mutual-challenge robustness on an asymmetric-lossy carrier (the live mobile flow-throttle
    /// failure): the handshake's THIRD message — the dialer's answer to the responder's
    /// challenge-back — is a single frame, and when the carrier eats it the responder used to stay
    /// "handshaking" forever: never a verified neighbour, every live Data frame from the (already
    /// verified-on-its-side) dialer buffered/dropped, node-ctx never formed, only durable frames
    /// crossing. The responder must RESEND the challenge-back (same nonce, like the dialer half
    /// resends its request) to re-elicit the answer. See
    /// docs/bugs/relay-less-mobile-transport-findings.md.
    #[tokio::test(start_paused = true)]
    async fn responder_resends_challenge_back_until_answered() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let a_pub = a_key.verifying_key();
        let b_pub = b_key.verifying_key();

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        a.start();
        b.start();

        // The carrier eats A's FIRST ChallengeResponse toward B — B's challenge-back
        // goes unanswered once (the throttle clamping right after the dial's packets).
        fabric.drop_next_challenge_responses(a_addr, b_addr, 1);

        // A's own dial still verifies (B's request/response toward A are intact).
        let link = a
            .dial(b.local_address())
            .await
            .expect("dial must verify on A's side");
        // A immediately sends live Data — exactly what the app does after pairing.
        link.send(b"hello-after-throttle").await.expect("send");

        // B must eventually verify A (via a challenge-back RESEND re-eliciting the
        // response) and deliver the buffered live frame. Without the resend this
        // never resolves.
        let b_link = timeout(Duration::from_secs(30), b.accept())
            .await
            .expect("B must verify A via challenge-back resend and hand out the incoming Link")
            .expect("accept yields a link");
        assert_eq!(b_link.destination(), a.local_address());
        let payload = timeout(Duration::from_secs(10), b_link.recv())
            .await
            .expect("the live Data frame buffered during the handshake must be delivered")
            .expect("recv ok");
        assert_eq!(payload, b"hello-after-throttle");

        a.shutdown();
        b.shutdown();
    }

    /// P4 multi-hop store-and-forward: A holds a frame for C; A↔B and B↔C are
    /// Scope decision (pure): a GROUP feed we're a member of is offered only to co-members,
    /// interested peers, or forwarders — never a bare route-candidate (the membership-graph leak we
    /// close). A dest that is NOT one of our groups (1:1 / carried S&F) fails open, so store-and-
    /// forward is untouched. See docs/decisions/anti-entropy-have-scoping.md.
    #[test]
    fn anti_entropy_scopes_group_feeds_and_floods_everything_else() {
        let net = Network::new(fresh_signing(), vec![]);
        let g = crate::durable::group_destination_address("scoped-gid");
        let co_member = node();
        let bare = node();
        let one_to_one = node(); // a dest that is not one of our groups

        // We're a member of G; `co_member` is a co-member. (Members list omits `bare`.)
        net.subscribe_group_destination(g, vec![co_member]);

        // Not one of our groups → flood to anyone, so 1:1 + carried S&F walk as before.
        assert!(net.feed_relevant_to_peer(&one_to_one, Some(&bare), &[], false));

        // A group frame reaches a co-member (in our member list)…
        assert!(net.feed_relevant_to_peer(&g, Some(&co_member), &[], false));
        // …a peer that advertised interest in the group…
        assert!(net.feed_relevant_to_peer(&g, Some(&bare), std::slice::from_ref(&g), false));
        // …and a forwarder that carries our traffic…
        assert!(net.feed_relevant_to_peer(&g, Some(&bare), &[], true));
        // …but NOT a bare neighbour that is none of those.
        assert!(!net.feed_relevant_to_peer(&g, Some(&bare), &[], false));
    }

    /// End-to-end: a member holding a group frame reconciles it to a peer that SUBSCRIBES to the
    /// group (advertises interest), but withholds it from a bare neighbour — the frame's inventory
    /// never reaches a node with no stake in the group.
    #[tokio::test]
    async fn anti_entropy_delivers_group_frame_to_subscriber_not_to_bare_neighbour() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let mk_node = |addr: &str| {
            let key = fresh_signing();
            let pubk = key.verifying_key();
            let net = Arc::new(Network::new(
                key,
                vec![fabric.interface(addr.parse().unwrap(), NatModel::Public)
                    as Arc<dyn Interface>],
            ));
            (net, pubk)
        };
        let g = crate::durable::group_destination_address("live-scope-gid");

        // Case 1: A ↔ SUBSCRIBER. A holds a group frame; the peer subscribes to G → gets it.
        let (a, a_pub) = mk_node("10.1.0.1:1");
        let (sub, sub_pub) = mk_node("10.1.0.2:1");
        seed_peer(&a, sub_pub, vec!["10.1.0.2:1".parse().unwrap()]);
        seed_peer(&sub, a_pub, vec!["10.1.0.1:1".parse().unwrap()]);
        a.start();
        sub.start();
        a.dial(sub.local_address()).await.expect("A dials subscriber");
        a.subscribe_group_destination(g, vec![]); // A is a member (holds the frame), no co-members listed
        sub.subscribe_group_destination(g, vec![]); // the peer subscribes → advertises interest in G
        a.send_durable(DurableDataFrame {
            to: g,
            from: a.local_address(),
            feed_id: [7u8; 16],
            seq: 1,
            msg_id: 777,
            ttl_secs: 3600,
            ciphertext: vec![1],
            signature: vec![],
        })
        .await
        .unwrap();
        let got = timeout(Duration::from_secs(5), async {
            loop {
                sub.reconcile_all_peers().await; // subscriber's HAVE carries its interest in G
                a.reconcile_all_peers().await; // A offers, scoped to the interested subscriber
                if let Some(f) = sub.try_recv_durable() {
                    break f;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("subscriber receives the group frame");
        assert_eq!(got.msg_id, 777);

        // Case 2: A2 ↔ BARE neighbour (never subscribes). The frame must NOT reach it.
        let (a2, a2_pub) = mk_node("10.2.0.1:1");
        let (bare, bare_pub) = mk_node("10.2.0.2:1");
        seed_peer(&a2, bare_pub, vec!["10.2.0.2:1".parse().unwrap()]);
        seed_peer(&bare, a2_pub, vec!["10.2.0.1:1".parse().unwrap()]);
        a2.start();
        bare.start();
        a2.dial(bare.local_address()).await.expect("A2 dials bare");
        a2.subscribe_group_destination(g, vec![]); // A2 member; bare is NOT listed and never subscribes
        a2.send_durable(DurableDataFrame {
            to: g,
            from: a2.local_address(),
            feed_id: [8u8; 16],
            seq: 1,
            msg_id: 888,
            ttl_secs: 3600,
            ciphertext: vec![1],
            signature: vec![],
        })
        .await
        .unwrap();
        // Drive many reconcile rounds; the bare neighbour must never receive/hold the group frame.
        for _ in 0..25 {
            bare.reconcile_all_peers().await;
            a2.reconcile_all_peers().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !bare.durable_store.lock().contains(&([8u8; 16], 1)),
            "a bare neighbour must not receive a group frame it has no stake in"
        );
        assert!(bare.try_recv_durable().is_none());
    }

    /// linked but A and C never link directly. Periodic reconcile walks the held
    /// frame A→B→C (pull at each link), bridging two nodes that are never online
    /// together — the headline durable promise. См. mesh-durable-delivery.md.
    #[tokio::test]
    async fn durable_frame_walks_a_to_c_via_periodic_reconcile() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let c_addr: SocketAddr = "10.0.0.3:1".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (a_pub, b_pub, c_pub) = (
            a_key.verifying_key(),
            b_key.verifying_key(),
            c_key.verifying_key(),
        );

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c = Arc::new(Network::new(
            c_key,
            vec![fabric.interface(c_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c_node = c.local_address();

        // Link A↔B and B↔C only (A and C never link directly).
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        seed_peer(&b, c_pub, vec![c_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);
        a.start();
        b.start();
        c.start();
        a.dial(b.local_address()).await.expect("A dials B");
        b.dial(c.local_address()).await.expect("B dials C");

        // A holds a frame for C — no path to C, so it is just held.
        a.send_durable(DurableDataFrame {
            to: c_node,
            from: a.local_address(),
            feed_id: [5u8; 16],
            seq: 1,
            msg_id: 555,
            ttl_secs: 3600,
            ciphertext: vec![1, 2, 3],
            signature: vec![],
        })
        .await
        .unwrap();

        // Periodic reconcile pulls A→B then B→C, walking the frame to C.
        let got = timeout(Duration::from_secs(5), async {
            loop {
                b.reconcile_all_peers().await;
                c.reconcile_all_peers().await;
                if let Some(f) = c.try_recv_durable() {
                    break f;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("C receives the held frame via periodic reconcile within 5s");
        assert_eq!((got.msg_id, got.to), (555, c_node));

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    // --- mesh-level frame dedup window ---
    #[test]
    fn seen_frames_dedups_repeats_and_evicts_oldest() {
        let mut s = SeenFrames::new(2);
        let (a, b, c) = (node(), node(), node());
        assert!(s.note((a, 1))); // first sight
        assert!(!s.note((a, 1))); // exact replay → duplicate
        assert!(s.note((a, 2))); // same origin, different msg_id → new identity
        // window now holds {(a,1),(a,2)} at cap 2; noting (b,1) evicts (a,1).
        assert!(s.note((b, 1)));
        assert!(s.note((a, 1))); // (a,1) was evicted → seen as new again
        assert!(s.note((c, 99))); // distinct origin+id → new
    }

    // --- P2 PeerLink: re-verified identity adds a path (additive, not latest-wins) ---
    #[test]
    fn register_verified_link_retains_alternate_path_additively() {
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();
        let n1 = NeighborId("conn-1".into());
        let n2 = NeighborId("conn-2".into());

        let ch1 = net.register_verified_link(&n1, peer);
        // A second connection verifying the SAME identity ADDS a path — it no longer
        // evicts the first (relay-first / reconnect needs ≥2 live paths to a peer).
        let ch2 = net.register_verified_link(&n2, peer);

        // BOTH connections stay verified for the peer: each is a live inbound source
        // and a send/failover path. (Was: the first got dropped.)
        {
            let verified = net.verified_neighbors.lock();
            assert_eq!(verified.get(&n1), Some(&peer), "alternate path retained");
            assert_eq!(verified.get(&n2), Some(&peer));
        }
        // The reliable inbox is keyed by identity and STABLE across re-verifications: the FIRST
        // link's inbox stays canonical, so the app's original inbound pump is never orphaned. A
        // re-verify (reconnect, or the discarded relay→direct upgrade Link) reuses it — its own
        // returned rx detaches. Inbound from EITHER connection routes to the one canonical inbox
        // (frames carry the peer identity). This is the "single stable Link" the overwrite stopgap
        // was missing — overwriting silently black-holed inbound after a maintenance re-dial (live
        // relay-chat decay). См. docs/bugs/relay-pairing-approval-gap.md.
        let mut rx1 = ch1.inbox_rx;
        assert!(
            matches!(
                rx1.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the first link's inbox stays canonical (live), not orphaned by the re-verify"
        );
        let mut rx2 = ch2.inbox_rx;
        assert!(
            matches!(
                rx2.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
            ),
            "the re-verify's returned rx detaches — its sender was never wired in"
        );
    }

    // --- reflexive endpoint / NAT-class inference (P1) ---
    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn infer_reflexive_unknown_below_two_observers() {
        let mut obs = HashMap::new();
        assert_eq!(infer_reflexive(&obs).class, NatClass::Unknown);
        obs.insert(NeighborId("m1".into()), sa("203.0.113.7:5000"));
        // One observer can't distinguish cone from symmetric.
        assert_eq!(infer_reflexive(&obs).class, NatClass::Unknown);
    }

    #[test]
    fn infer_reflexive_endpoint_independent_when_observers_agree() {
        let mut obs = HashMap::new();
        obs.insert(NeighborId("m1".into()), sa("203.0.113.7:5000"));
        obs.insert(NeighborId("m2".into()), sa("203.0.113.7:5000"));
        let r = infer_reflexive(&obs);
        assert_eq!(r.class, NatClass::EndpointIndependent);
        assert_eq!(r.reflexive, Some(sa("203.0.113.7:5000")));
    }

    #[test]
    fn infer_reflexive_symmetric_when_ports_differ() {
        let mut obs = HashMap::new();
        obs.insert(NeighborId("m1".into()), sa("203.0.113.7:5000"));
        obs.insert(NeighborId("m2".into()), sa("203.0.113.7:6001"));
        let r = infer_reflexive(&obs);
        assert_eq!(r.class, NatClass::AddressDependent);
        assert_eq!(r.reflexive, None);
    }

    // --- PeerLink path selection (P2) ---
    #[test]
    fn path_set_selects_best_quality_and_dedups_by_neighbor() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let mk = |addr: &str, quality: PathQuality| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface(addr.parse().unwrap(), NatModel::Public) as InterfaceHandle,
            quality,
            initiated_locally: false,
        };
        let mut set = PathSet::new();
        set.add(mk("10.0.0.1:1", PathQuality::Relay { hops: 2 }));
        set.add(mk("10.0.0.2:1", PathQuality::Direct));
        set.add(mk("10.0.0.3:1", PathQuality::Relay { hops: 1 }));
        // Direct beats every relay.
        assert_eq!(set.best().unwrap().quality, PathQuality::Direct);

        // Re-adding the same neighbour updates its quality (no duplicate entry).
        set.add(mk("10.0.0.2:1", PathQuality::Relay { hops: 5 }));
        assert_eq!(set.paths.len(), 3);
        // With no Direct left, the 1-hop relay wins over the 2- and 5-hop ones.
        assert_eq!(set.best().unwrap().quality, PathQuality::Relay { hops: 1 });
    }

    #[test]
    fn a_bulk_stream_opens_on_a_direct_path_only() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let mk = |addr: &str, quality: PathQuality| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface(addr.parse().unwrap(), NatModel::Public) as InterfaceHandle,
            quality,
            initiated_locally: false,
        };
        let mut set = PathSet::new();
        set.add(mk("10.0.0.1:1", PathQuality::Relay { hops: 1 }));
        // Reached through a relay only: no bulk lane (the stream would land at the relay).
        assert!(set.bulk_candidates(false).is_empty());
        set.add(mk("10.0.0.2:1", PathQuality::Direct));
        let candidates = set.bulk_candidates(false);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].neighbor, NeighborId("10.0.0.2:1".into()));
    }

    #[test]
    fn path_set_prefers_udp_relay_over_tcp_relay_then_direct() {
        // Connection-quality ladder: carrier ranks within relays — a UDP relay (rung 1) beats the
        // TCP-relay base (rung 0); direct (rung 2) beats both. Carrier comes from the iface kind.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let path = |addr: &str, quality: PathQuality, kind: InterfaceKind| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface_with_kind(addr.parse().unwrap(), NatModel::Public, kind)
                as InterfaceHandle,
            quality,
            initiated_locally: false,
        };
        let mut set = PathSet::new();
        // Rung 0 base: a 1-hop relay over the TCP carrier — the only path so far.
        set.add(path("10.0.0.1:1", PathQuality::Relay { hops: 1 }, InterfaceKind::QuicOverTcp));
        assert_eq!(set.best().unwrap().neighbor.0, "10.0.0.1:1");
        // Rung 1: an equal-hop relay over UDP must win (UDP carrier beats the TCP-relay base).
        set.add(path("10.0.0.2:1", PathQuality::Relay { hops: 1 }, InterfaceKind::QuicOverUdp));
        assert_eq!(
            set.best().unwrap().neighbor.0,
            "10.0.0.2:1",
            "UDP relay (rung 1) beats the equal-hop TCP relay (rung 0)"
        );
        // Carrier dominates within relays: even a 5-hop UDP relay outranks the 1-hop TCP one — but
        // among UDP relays fewer hops still wins, so the 1-hop UDP stays best.
        set.add(path("10.0.0.3:1", PathQuality::Relay { hops: 5 }, InterfaceKind::QuicOverUdp));
        assert_eq!(set.best().unwrap().neighbor.0, "10.0.0.2:1");
        // Rung 2: direct beats every relay regardless of carrier.
        set.add(path("10.0.0.4:1", PathQuality::Direct, InterfaceKind::QuicOverUdp));
        assert_eq!(set.best().unwrap().quality, PathQuality::Direct);
    }

    /// R1a #3: carrier is an axis independent of topology, and QUIC wins on EVERY hop — including a
    /// DIRECT one, which the old flat `Direct => 0` rank ignored. Pure order check, no interfaces.
    #[test]
    fn path_rank_prefers_quic_on_every_hop_including_direct() {
        use InterfaceKind::{QuicOverTcp, QuicOverUdp};
        let direct_udp = path_rank_parts(PathQuality::Direct, QuicOverUdp);
        let direct_tcp = path_rank_parts(PathQuality::Direct, QuicOverTcp);
        let relay_udp_1 = path_rank_parts(PathQuality::Relay { hops: 1 }, QuicOverUdp);
        let relay_udp_5 = path_rank_parts(PathQuality::Relay { hops: 5 }, QuicOverUdp);
        let relay_tcp_1 = path_rank_parts(PathQuality::Relay { hops: 1 }, QuicOverTcp);

        // The fix: QUIC beats TCP on a direct hop (used to tie at 0).
        assert!(direct_udp < direct_tcp);
        // Preserved: a direct hop beats borrowing a relay, whatever the carrier — so Direct-TCP must
        // stay strictly below Relay-UDP{1hop}, the exact collision a flat u16 could not express.
        assert!(direct_tcp < relay_udp_1);
        // Preserved: among relays the carrier dominates the hop count, and fewer hops break the tie.
        assert!(relay_udp_5 < relay_tcp_1);
        assert!(relay_udp_1 < relay_udp_5);
    }

    #[test]
    fn udp_demotion_fails_back_to_direct_tcp() {
        use InterfaceKind::{QuicOverTcp, QuicOverUdp};
        // Healthy (not demoted): direct-UDP is preferred over direct-TCP (unchanged behaviour).
        let udp_ok = effective_path_rank_parts(PathQuality::Direct, QuicOverUdp, false);
        let tcp = effective_path_rank_parts(PathQuality::Direct, QuicOverTcp, false);
        assert!(udp_ok < tcp, "healthy: QUIC/UDP still wins on a direct hop");
        assert_eq!(udp_ok, path_rank_parts(PathQuality::Direct, QuicOverUdp), "no demotion = identical");

        // Demoted: direct-UDP falls BELOW direct-TCP so best() sends over the reliable door.
        let udp_demoted = effective_path_rank_parts(PathQuality::Direct, QuicOverUdp, true);
        assert!(tcp < udp_demoted, "demoted: direct-TCP door now beats the phantom UDP path");
        // Demotion touches ONLY direct-UDP — direct-TCP and relay paths keep their normal rank.
        assert_eq!(tcp, effective_path_rank_parts(PathQuality::Direct, QuicOverTcp, true));
        assert_eq!(
            effective_path_rank_parts(PathQuality::Relay { hops: 1 }, QuicOverUdp, true),
            path_rank_parts(PathQuality::Relay { hops: 1 }, QuicOverUdp),
            "relay-UDP is not a direct hop → unaffected by the direct-UDP demotion"
        );
    }

    /// Carrier-awareness must compose with the simultaneous-open convergence tie-break, not replace
    /// it. Two direct paths of DIFFERENT carriers no longer tie, so both ends pick the globally
    /// preferred QUIC one (same physical connection); two of the SAME carrier still tie and fall to
    /// `initiated_locally`, exactly as before.
    #[test]
    fn carrier_preference_and_simultaneous_open_tiebreak_both_converge() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let path = |addr: &str, kind: InterfaceKind, initiated_locally: bool| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface_with_kind(addr.parse().unwrap(), NatModel::Public, kind)
                as InterfaceHandle,
            quality: PathQuality::Direct,
            initiated_locally,
        };

        // Different carriers: one end dialed TCP-direct, the peer's link is UDP.
        let mut mixed = PathSet::new();
        mixed.add(path("10.0.0.2:1", InterfaceKind::QuicOverUdp, false));
        mixed.add(path("10.0.0.1:1", InterfaceKind::QuicOverTcp, true));
        // BOTH ends (prefer-true and prefer-false) pick UDP → the same physical connection, so the
        // carrier preference converges them even though their `initiated_locally` sides differ.
        assert_eq!(
            mixed.best_preferring(true).unwrap().iface.kind(),
            InterfaceKind::QuicOverUdp
        );
        assert_eq!(
            mixed.best_preferring(false).unwrap().iface.kind(),
            InterfaceKind::QuicOverUdp
        );

        // Same carrier: the carrier no longer discriminates, so convergence falls back to the
        // initiation-side tie-break (unchanged from before the fix).
        let mut same = PathSet::new();
        same.add(path("10.0.0.3:1", InterfaceKind::QuicOverUdp, true));
        same.add(path("10.0.0.4:1", InterfaceKind::QuicOverUdp, false));
        assert!(same.best_preferring(true).unwrap().initiated_locally);
        assert!(!same.best_preferring(false).unwrap().initiated_locally);
    }

    #[test]
    fn path_set_best_preferring_picks_initiation_side_for_convergence() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let mk = |addr: &str, initiated_locally: bool| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface(addr.parse().unwrap(), NatModel::Public) as InterfaceHandle,
            quality: PathQuality::Direct,
            initiated_locally,
        };
        let mut set = PathSet::new();
        set.add(mk("10.0.0.1:1", true)); // the connection we dialed
        set.add(mk("10.0.0.2:1", false)); // the connection the peer dialed (we accepted)

        // Smaller-addressed endpoint prefers its own dial; the larger prefers the
        // peer's dial → both converge on the SAME physical connection.
        assert!(set.best_preferring(true).unwrap().initiated_locally);
        assert!(!set.best_preferring(false).unwrap().initiated_locally);

        // Quality still dominates: a direct path beats a relay regardless of side.
        set.add(PeerPath {
            neighbor: NeighborId("10.0.0.3:1".into()),
            iface: fabric.interface("10.0.0.3:1".parse().unwrap(), NatModel::Public) as InterfaceHandle,
            quality: PathQuality::Relay { hops: 1 },
            initiated_locally: false,
        });
        assert_eq!(
            set.best_preferring(false).unwrap().quality,
            PathQuality::Direct
        );
    }

    #[test]
    fn path_set_remove_drops_path_and_reveals_next_best() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let mk = |addr: &str, quality: PathQuality| PeerPath {
            neighbor: NeighborId(addr.into()),
            iface: fabric.interface(addr.parse().unwrap(), NatModel::Public) as InterfaceHandle,
            quality,
            initiated_locally: false,
        };
        let mut set = PathSet::new();
        set.add(mk("10.0.0.1:1", PathQuality::Direct));
        set.add(mk("10.0.0.2:1", PathQuality::Relay { hops: 1 }));
        // Drop the Direct path → the relay becomes best.
        assert!(set.remove(&NeighborId("10.0.0.1:1".into())));
        assert_eq!(set.best().unwrap().quality, PathQuality::Relay { hops: 1 });
        // Removing an unknown neighbour is a no-op.
        assert!(!set.remove(&NeighborId("nope:0".into())));
        assert_eq!(set.paths.len(), 1);
    }

    /// Интерфейс, который умеет отвечать на два необязательных вопроса: адрес соединения и
    /// измеренный RTT. Отдельный от `FailingInterface` намеренно — тот отвечает `None` на оба
    /// (значения по умолчанию из трейта), и на нём нельзя отличить «проводка есть» от «её нет».
    #[derive(Debug)]
    struct FactsInterface {
        id: crate::interface::InterfaceId,
        neighbor: NeighborId,
        remote: SocketAddr,
        rtt: Duration,
    }

    #[async_trait::async_trait]
    impl Interface for FactsInterface {
        fn id(&self) -> crate::interface::InterfaceId {
            self.id.clone()
        }
        fn kind(&self) -> crate::interface::InterfaceKind {
            crate::interface::InterfaceKind::QuicOverUdp
        }
        fn mtu(&self) -> usize {
            1200
        }
        fn bandwidth_estimate(&self) -> u64 {
            1
        }
        fn latency_estimate(&self) -> Duration {
            Duration::from_millis(1)
        }
        fn reliable(&self) -> bool {
            true
        }
        fn broadcast_capable(&self) -> bool {
            false
        }
        async fn send_frame(&self, _n: &NeighborId, _f: &[u8]) -> io::Result<()> {
            Ok(())
        }
        async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
            std::future::pending().await
        }
        async fn broadcast_frame(&self, _f: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn remote_addr(&self, neighbor: &NeighborId) -> Option<SocketAddr> {
            (neighbor == &self.neighbor).then_some(self.remote)
        }
        fn connection_rtt(&self, neighbor: &NeighborId) -> Option<Duration> {
            (neighbor == &self.neighbor).then_some(self.rtt)
        }
    }

    /// ФАКТЫ ЖИВОГО ЛИНКА БЕРУТСЯ С ЛУЧШЕГО ПУТИ, А НЕ ОТКУДА ПРИДЁТСЯ.
    ///
    /// Ради этого метод и появился: панель связи печатала «Пинг», посчитанный по
    /// presence-обмену (медиана 322 мс при 49 мс на транспорте), и «Endpoint», в котором лежал
    /// адрес УЗЛА. Теперь наверх идёт то, что измерил транспорт.
    #[test]
    fn peer_link_facts_come_from_the_best_paths_connection() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let neighbor = NeighborId("203.0.113.108:27406".into());
        // Двустековый сокет отдаёт IPv4-пира отображённым — ровно так это и приходит живьём.
        let remote: SocketAddr = "[::ffff:203.0.113.108]:27406".parse().unwrap();
        let iface = Arc::new(FactsInterface {
            id: iface_id.clone(),
            neighbor: neighbor.clone(),
            remote,
            rtt: Duration::from_millis(49),
        }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![Arc::clone(&iface)]);
        let peer = node();

        assert!(
            net.peer_link_facts(&peer).is_none(),
            "пути нет — фактов тоже нет, а не нули"
        );

        net.register_path(peer, neighbor, Arc::clone(&iface), PathQuality::Direct, true);

        let facts = net.peer_link_facts(&peer).expect("путь есть — факты обязаны быть");
        assert_eq!(
            facts.remote,
            Some("203.0.113.108:27406".parse::<SocketAddr>().unwrap()),
            "адрес соединения, развёрнутый из отображённого вида: снаружи `[::ffff:…]`              неузнаваем — разборщик IPv4 его не берёт, и поле «Сеть» уезжало в Loopback"
        );
        assert_eq!(facts.rtt, Some(Duration::from_millis(49)));

        // Отрицательная половина: интерфейс, который этих вопросов не понимает (значения по
        // умолчанию из трейта), обязан отдавать `None`, а не выдумывать ноль. Ноль миллисекунд и
        // «не мерили» — разные утверждения, и подмена одного другим уже стоила этому проекту
        // разбора.
        let silent_peer = node();
        let silent_neighbor = NeighborId("silent".into());
        let silent = Arc::new(FailingInterface { id: iface_id }) as InterfaceHandle;
        net.register_path(
            silent_peer,
            silent_neighbor,
            silent,
            PathQuality::Direct,
            true,
        );
        let silent_facts = net
            .peer_link_facts(&silent_peer)
            .expect("путь есть, просто интерфейс молчит");
        assert_eq!(silent_facts.remote, None);
        assert_eq!(silent_facts.rtt, None);
    }

    /// A path whose sends always fail — models a dead connection for failover.
    #[derive(Debug)]
    struct FailingInterface {
        id: crate::interface::InterfaceId,
    }

    #[async_trait::async_trait]
    impl Interface for FailingInterface {
        fn id(&self) -> crate::interface::InterfaceId {
            self.id.clone()
        }
        fn kind(&self) -> crate::interface::InterfaceKind {
            crate::interface::InterfaceKind::QuicOverUdp
        }
        fn mtu(&self) -> usize {
            1200
        }
        fn bandwidth_estimate(&self) -> u64 {
            1
        }
        fn latency_estimate(&self) -> std::time::Duration {
            std::time::Duration::from_millis(1)
        }
        fn reliable(&self) -> bool {
            true
        }
        fn broadcast_capable(&self) -> bool {
            false
        }
        async fn send_frame(&self, _n: &NeighborId, _f: &[u8]) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "path down"))
        }
        async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
            std::future::pending().await
        }
        async fn broadcast_frame(&self, _f: &[u8]) -> io::Result<()> {
            Ok(())
        }
        async fn send_datagram(&self, _n: &NeighborId, _p: &[u8]) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::ConnectionReset, "path down"))
        }
    }

    #[tokio::test]
    async fn link_send_fails_over_from_dead_path_to_working_path() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        // Best path (Direct) is dead; the lower-ranked Relay path works. send must
        // fail over to it and drop the dead one — relay-first silent failover.
        let dead = PeerPath {
            neighbor: NeighborId("dead:1".into()),
            iface: Arc::new(FailingInterface {
                id: crate::interface::InterfaceId("dead".into()),
            }) as InterfaceHandle,
            quality: PathQuality::Direct,
            initiated_locally: false,
        };
        let live_addr: SocketAddr = "10.0.0.9:1".parse().unwrap();
        let working = PeerPath {
            neighbor: NeighborId(live_addr.to_string()),
            iface: fabric.interface(live_addr, NatModel::Public) as InterfaceHandle,
            quality: PathQuality::Relay { hops: 1 },
            initiated_locally: false,
        };
        let mut set = PathSet::new();
        set.add(dead);
        set.add(working);
        let paths = Arc::new(SyncMutex::new(set));

        let (_tx, rx) = mpsc::channel::<Vec<u8>>(1);
        let (_dtx, drx) = mpsc::channel::<Vec<u8>>(1);
        let link = Link {
            local: NodeAddress::from_pubkey(&fresh_signing().verifying_key()),
            destination: NodeAddress::from_pubkey(&fresh_signing().verifying_key()),
            paths: Arc::clone(&paths),
            inbox: AsyncMutex::new(rx),
            datagram_inbox: AsyncMutex::new(drx),
        };

        link.send(b"hello").await.expect("failover to working path");
        let guard = paths.lock();
        assert_eq!(guard.paths.len(), 1, "dead path dropped");
        assert_eq!(guard.best().unwrap().quality, PathQuality::Relay { hops: 1 });
    }

    /// Losing a DIRECT connection to a peer still routable through a live relay neighbour is not
    /// «peer down»: the app would drop the session while the peer's frames arrive through the relay
    /// (stand 2026-10-14, a pinned relay refusing the contact's direct dials every few seconds).
    #[test]
    fn a_peer_still_routed_through_a_live_neighbour_is_not_reported_down() {
        let net = Network::new(fresh_signing(), vec![]);
        let (relay, peer) = (node(), node());
        let (relay_neighbor, direct_neighbor) = (NeighborId("relay-conn".into()), NeighborId("direct-conn".into()));
        let iface = Arc::new(FailingInterface { id: crate::interface::InterfaceId("if".into()) }) as InterfaceHandle;
        net.verified_neighbors.lock().insert(relay_neighbor.clone(), relay);
        net.register_path(peer, direct_neighbor.clone(), iface.clone(), PathQuality::Direct, false);
        net.path_table.update(
            peer,
            crate::interface::InterfaceId("if".into()),
            relay_neighbor,
            1,
            Duration::from_secs(300),
            Instant::now(),
        );

        net.prune_path(&direct_neighbor);
        assert_eq!(net.try_recv_peer_down(), None, "routed through the relay — not down");

        // With no route left it is down, as before.
        let lonely = node();
        let lonely_neighbor = NeighborId("lonely-conn".into());
        net.register_path(lonely, lonely_neighbor.clone(), iface, PathQuality::Direct, false);
        net.prune_path(&lonely_neighbor);
        assert_eq!(net.try_recv_peer_down(), Some(lonely));
    }

    #[test]
    fn prune_path_sweeps_a_dead_relay_neighbour_out_of_every_relayed_pathset() {
        // Regression: a relay neighbour is the next-hop for MANY destinations. When it dies,
        // prune_path must remove it from EVERY peer's PathSet — not only its own identity's —
        // otherwise a stale relayed path lingers and best()/send_failover keep sending into the dead
        // connection (the post-churn "chat silently stops crossing the relay" bug). See
        // docs/bugs/relay-pairing-approval-gap.md.
        let net = Network::new(fresh_signing(), vec![]);
        let relay_addr = node();
        let b = node();
        let relay_neighbor = NeighborId("relay-conn".into());
        let iface = Arc::new(FailingInterface {
            id: crate::interface::InterfaceId("relay-if".into()),
        }) as InterfaceHandle;
        // The relay is a verified neighbour: a DIRECT path to its own identity AND a RELAY path that
        // routes B over it (the fan-out a real relay has).
        net.verified_neighbors
            .lock()
            .insert(relay_neighbor.clone(), relay_addr);
        net.register_path(
            relay_addr,
            relay_neighbor.clone(),
            iface.clone(),
            PathQuality::Direct,
            false,
        );
        net.register_path(
            b,
            relay_neighbor.clone(),
            iface.clone(),
            PathQuality::Relay { hops: 1 },
            false,
        );
        // The relay also holds a cost-1 routing-table entry for B via this neighbour (one intermediary) — the
        // thing decide_data_forward actually routes by. It too must be cleared on prune.
        net.path_table.update(
            b,
            crate::interface::InterfaceId("relay-if".into()),
            relay_neighbor.clone(),
            1,
            Duration::from_secs(300),
            Instant::now(),
        );

        net.prune_path(&relay_neighbor);

        // The routing-table route to B over the dead neighbour is gone (else decide_data_forward
        // keeps forwarding into the dead connection).
        assert!(
            net.path_table.lookup(&b, Instant::now()).is_none(),
            "stale path_table route via the dead neighbour must be cleared"
        );

        // B's only path was over the dead relay neighbour → it must be swept (pre-fix it lingered,
        // because prune only touched the neighbour's OWN identity's PathSet).
        let b_reachable = net
            .peer_paths
            .lock()
            .get(&b)
            .map(|s| s.lock().best().is_some())
            .unwrap_or(false);
        assert!(
            !b_reachable,
            "stale relayed path via the dead relay neighbour must be swept from B's PathSet"
        );
        // And B is surfaced as fully unreachable so the app tears down stale session state.
        let mut downed = std::collections::HashSet::new();
        while let Some(p) = net.try_recv_peer_down() {
            downed.insert(p);
        }
        assert!(
            downed.contains(&b),
            "B lost its last path → must be surfaced as peer_down"
        );
    }

    #[tokio::test]
    async fn relay_path_up_signals_event_driven_upgrade() {
        // A peer that becomes reachable ONLY over a relay must be signalled for an IMMEDIATE
        // punch-upgrade (event-driven), so direct comes up without waiting for the reconcile timer.
        // A DIRECT link is already the goal and must NOT signal. См. mesh-hole-punching.md.
        let net = Network::new(fresh_signing(), vec![]);
        let relay_peer = node();
        let direct_peer = node();
        let iface = Arc::new(FailingInterface {
            id: crate::interface::InterfaceId("if".into()),
        }) as InterfaceHandle;

        net.register_path(
            relay_peer,
            NeighborId("relay-conn".into()),
            iface.clone(),
            PathQuality::Relay { hops: 1 },
            false,
        );
        net.register_path(
            direct_peer,
            NeighborId("direct-conn".into()),
            iface.clone(),
            PathQuality::Direct,
            true,
        );

        assert_eq!(
            net.next_relay_upgrade().await,
            Some(relay_peer),
            "a relay-only peer must be signalled for a fast punch-upgrade"
        );
        // The direct peer never enqueues — the next recv must block (time out), not yield it.
        let next = tokio::time::timeout(Duration::from_millis(50), net.next_relay_upgrade()).await;
        assert!(next.is_err(), "a direct link must not enqueue an upgrade signal");
    }

    #[test]
    fn a_peer_whose_address_changed_loses_its_old_door_tunnel() {
        // ЕДИНСТВЕННОЕ место во всём сетевом слое, где ресурс ПИРА ключевался адресом. Карта
        // туннелей внутри MuxRouter ключуется адресом по построению (quinn знает только адреса), а
        // адрес пира меняется вместе с портом при каждом его перезапуске. Пока сеть не помнила,
        // ПОД КАКИМ ключом она поставила дверь, прежний туннель некому было вытеснить: он оставался
        // в карте и его супервизор дозванивался в исчезнувшую дверь до конца процесса —
        // 6189 попыток в два мёртвых адреса за одну сессию (живой замер 2026-08-29).
        //
        // Без правки падает: прежний ключ не возвращается, потому что его никто не помнил.
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();
        let first: SocketAddr = "203.0.113.7:41001".parse().unwrap();
        let second: SocketAddr = "203.0.113.7:52002".parse().unwrap();

        assert_eq!(
            net.install_peer_door_key(peer, first),
            None,
            "первая дверь ничего не вытесняет"
        );
        assert_eq!(
            net.install_peer_door_key(peer, first),
            None,
            "та же дверь повторно — штатный no-op, вытеснять нечего"
        );
        assert_eq!(
            net.install_peer_door_key(peer, second),
            Some(first),
            "пир сменил адрес — прежний туннель обязан быть снесён"
        );
        assert_eq!(
            net.peer_doors.lock().get(&peer).copied(),
            Some(second),
            "у пира ровно одна дверь, и это текущая"
        );

        // Возврат на UDP забывает ключ, чтобы следующая установка не вытесняла уже снесённое.
        net.route_peer_over_udp(&peer);
        assert!(
            net.peer_doors.lock().get(&peer).is_none(),
            "после возврата на UDP двери у пира не числится"
        );
        assert_eq!(
            net.install_peer_door_key(peer, first),
            None,
            "после возврата на UDP установка снова первая"
        );
    }

    #[test]
    fn a_fresh_beacon_is_dialled_before_the_ports_the_peer_has_already_lost() {
        // `dial` стартует адреса ПО ПОРЯДКУ. Пока сюда писали `push`, свежий адрес пира оказывался
        // ПОСЛЕДНИМ — за очередью отображений, которые пир потерял при прошлых перезапусках, и тогда
        // каждое из них надо было дождаться по таймауту. Живой замер 2026-08-29: 130 с на попытку,
        // восстановление пары от 25 с до 11 минут. Теперь попытки перекрываются (`race_connect`), и
        // мёртвый адрес стоит 250 мс, но старт первым по-прежнему решает, кто выиграет.
        //
        // Без правки падает: свежий адрес стоит в хвосте.
        let a: SocketAddr = "203.0.113.10:1000".parse().unwrap();
        let b: SocketAddr = "203.0.113.10:2000".parse().unwrap();
        let fresh: SocketAddr = "203.0.113.10:3000".parse().unwrap();

        let mut known = vec![a, b];
        merge_rendezvous_endpoints(&mut known, vec![fresh]);
        assert_eq!(
            known.first(),
            Some(&fresh),
            "адрес из свежего маячка обязан пробоваться первым, а не после мёртвых"
        );
        assert!(known.contains(&a) && known.contains(&b), "прежние адреса не выбрасываются совсем");

        // Порядок внутри свежего набора сохраняется.
        let x: SocketAddr = "203.0.113.10:4000".parse().unwrap();
        let y: SocketAddr = "203.0.113.10:5000".parse().unwrap();
        merge_rendezvous_endpoints(&mut known, vec![x, y]);
        assert_eq!(&known[..2], &[x, y][..], "набор ложится в начало в своём порядке");

        // Уже известный адрес не задваивается, а поднимается наверх.
        merge_rendezvous_endpoints(&mut known, vec![a]);
        assert_eq!(known.first(), Some(&a));
        assert_eq!(known.iter().filter(|addr| **addr == a).count(), 1, "без дублей");

        // И список не растёт без предела: у пира за NAT порт меняется КАЖДЫЙ перезапуск.
        for port in 6000..6100u16 {
            let addr: SocketAddr = format!("203.0.113.10:{port}").parse().unwrap();
            merge_rendezvous_endpoints(&mut known, vec![addr]);
        }
        assert_eq!(
            known.len(),
            MAX_PEER_ADVERTISED_ADDRESSES,
            "длина списка — это время до соединения, и она обязана быть ограничена"
        );
    }

    /// ВХОДЯЩЕЕ СОЕДИНЕНИЕ ОТ САМОГО ПИРА — НЕ РЕЛЕЙ.
    ///
    /// Живой прогон 2026-08-31 показывал `link=online_via_relay relay_hops=1` в сети, где релея
    /// нет вовсе: `rendezvous/dial outcome: Dialed { …, relay_bundle: None }`, один удалённый
    /// адрес на весь звонок, третьего узла не существует. Единица приезжала из заглушки —
    /// `PathQuality::Relay { hops: 1 }` ставился всякий раз, когда кадр приходил от отправителя,
    /// для которого у нас ещё нет своего пути. То есть «мы этого не набирали» читалось как
    /// «пришло через посредника».
    ///
    /// Проверка падает без правки: получается `Relay { hops: 1 }`.
    #[tokio::test]
    async fn an_inbound_connection_from_the_peer_itself_is_direct_not_a_one_hop_relay() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface.clone()]);
        let peer = node();
        let neighbor = NeighborId("peer-inbound-conn".into());

        // Сосед проверен, и его личность — ЭТО и есть отправитель.
        net.verified_neighbors.lock().insert(neighbor.clone(), peer);

        net.handle_data(
            DataFrame::origin(peer, net.local_address, b"hello".to_vec()),
            &neighbor,
            &iface,
        )
        .await;

        assert_eq!(
            net.peer_link_quality(&peer),
            Some(LinkQuality::Direct),
            "кадр пришёл прямо от пира по его же соединению — посредника между нами нет"
        );
        // Вторая половина цены ярлыка: релейный путь заводит сторож живости, прямому не нужный.
        assert!(
            net.peer_last_seen.lock().get(&peer).is_none(),
            "прямому пути не положен релейный отсчёт смерти"
        );
    }

    /// Отрицательная половина: если отправитель — НЕ тот, с кем мы соединены, между нами
    /// действительно кто-то есть, и ветка обязана продолжать называть это релеем.
    #[tokio::test]
    async fn an_origin_behind_the_neighbour_is_still_a_relay() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface.clone()]);
        let relay = node();
        let origin = node();
        let neighbor = NeighborId("relay-conn".into());

        // Сосед проверен, но его личность — релей, а кадр пришёл ОТ ДРУГОГО узла.
        net.verified_neighbors.lock().insert(neighbor.clone(), relay);

        net.handle_data(
            DataFrame::origin(origin, net.local_address, b"hello".to_vec()),
            &neighbor,
            &iface,
        )
        .await;

        assert_eq!(
            net.peer_link_quality(&origin),
            Some(LinkQuality::Relay { hops: 1 }),
            "отправитель за соседом — это и есть релейный путь"
        );
    }

    #[tokio::test]
    async fn cost_zero_route_is_direct_not_a_zero_hop_relay() {
        // Живой прогон 2026-08-29 показывал `link=online_via_relay relay_hops=0` — вердикт,
        // противоречащий себе: связь через релей, в которой нет ни одного релейного прыжка.
        //
        // Источник — запись таблицы путей с `cost = 0`. По её собственному определению это ПРЯМОЙ
        // сосед (объявление приходит с hop_count = 0 только от самого отправителя), и
        // `dial_multihop` попадает на неё штатно: исходящий дозвон не прошёл, а ВХОДЯЩЕЕ
        // проверенное соединение с тем же узлом есть. Переиспользовать его правильно — назвать
        // релеем нет.
        //
        // Проверка падает без правки: получается `Relay { hops: 0 }`.
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface]);
        let peer = node();
        let neighbor = NeighborId("peer-inbound-conn".into());

        // Объявление, услышанное НАПРЯМУЮ от назначения.
        net.path_table.update(
            peer,
            iface_id,
            neighbor.clone(),
            0,
            Duration::from_secs(300),
            Instant::now(),
        );
        net.verified_neighbors.lock().insert(neighbor, peer);

        let link = net.dial_multihop(peer).expect("маршрут по проверенному соседу должен строиться");
        drop(link);

        assert_eq!(
            net.peer_link_quality(&peer),
            Some(LinkQuality::Direct),
            "cost = 0 — прямой сосед; Relay {{ hops: 0 }} обещает посредника, которого нет"
        );
        // И вторая половина цены ярлыка: релейный путь заводит сторож живости, который прямому
        // не нужен (register_path сеет peer_last_seen только для релея).
        assert!(
            net.peer_last_seen.lock().get(&peer).is_none(),
            "прямому пути не положен релейный отсчёт смерти"
        );
    }

    /// Live 2026-09-24: a two-node pair (no third node anywhere) sat at `online_via_relay
    /// relay_hops=1`. The route to the neighbour's own identity was in the table at cost 1 (the
    /// per-frame liveness seed), `dial_multihop` turned that into `Relay { hops: 1 }`, and
    /// `PathSet::add` let it overwrite the Direct path over the SAME neighbour. The next hop being
    /// the destination is what makes a path direct — whatever number the table carries.
    #[tokio::test]
    async fn the_route_to_a_neighbours_own_identity_stays_direct_whatever_its_cost() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface.clone()]);
        let peer = node();
        let neighbor = NeighborId("peer-conn".into());
        net.verified_neighbors.lock().insert(neighbor.clone(), peer);
        net.register_path(peer, neighbor.clone(), iface, PathQuality::Direct, true);
        net.path_table.update(
            peer,
            iface_id,
            neighbor,
            1,
            Duration::from_secs(300),
            Instant::now(),
        );

        drop(net.dial_multihop(peer).expect("route over a verified neighbour"));

        assert_eq!(
            net.peer_link_quality(&peer),
            Some(LinkQuality::Direct),
            "the next hop IS the peer: there is no relay to count"
        );
    }

    /// Negative half: when the next hop is someone else, the same call must still say relay —
    /// otherwise the test above could pass by calling everything direct.
    #[tokio::test]
    async fn a_destination_behind_another_neighbour_is_still_a_relay() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface]);
        let (peer, relay) = (node(), node());
        let relay_neighbor = NeighborId("relay-conn".into());
        net.verified_neighbors.lock().insert(relay_neighbor.clone(), relay);
        // Even a cost-0 claim cannot make a path through someone else direct.
        net.path_table.update(
            peer,
            iface_id,
            relay_neighbor,
            0,
            Duration::from_secs(300),
            Instant::now(),
        );

        drop(net.dial_multihop(peer).expect("route over a verified neighbour"));

        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Relay { hops: 1 }));
    }

    /// Path discovery files routes in the table's own unit, "0 = direct neighbor": a reply from the
    /// target itself is cost 0; a reply from an intermediary adds that intermediary. It used to add
    /// one unconditionally, filing every neighbour-target as one relay hop away and every
    /// discovered route one hop longer than it is.
    #[tokio::test]
    async fn a_path_reply_from_the_target_itself_is_a_direct_route() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface.clone()]);
        let (target, relay) = (node(), node());
        let target_conn = NeighborId("target-conn".into());
        let relay_conn = NeighborId("relay-conn".into());
        net.verified_neighbors.lock().insert(target_conn.clone(), target);
        net.verified_neighbors.lock().insert(relay_conn.clone(), relay);
        let origin = net.local_address;

        net.handle_path_reply(
            PathReplyFrame { origin, target, nonce: 1, cost: 0 },
            &target_conn,
            &iface,
        )
        .await;
        assert_eq!(net.path_table.lookup(&target, Instant::now()).unwrap().cost, 0);

        // Negative half: the same cost-0 reply from an intermediary is one hop further.
        let far = node();
        net.handle_path_reply(
            PathReplyFrame { origin, target: far, nonce: 2, cost: 0 },
            &relay_conn,
            &iface,
        )
        .await;
        assert_eq!(net.path_table.lookup(&far, Instant::now()).unwrap().cost, 1);
    }

    /// The reverse route a PathRequest leaves behind, same unit: straight from its origin → 0,
    /// relayed once on the way (one hop_limit spent) → 1.
    #[tokio::test]
    async fn a_path_request_straight_from_its_origin_leaves_a_direct_reverse_route() {
        let iface_id = crate::interface::InterfaceId("if".into());
        let iface = Arc::new(FailingInterface { id: iface_id.clone() }) as InterfaceHandle;
        let net = Network::new(fresh_signing(), vec![iface.clone()]);
        let (near, far) = (node(), node());
        let conn = NeighborId("near-conn".into());
        net.verified_neighbors.lock().insert(conn.clone(), near);
        let target = node(); // unknown to us — no reply, just the reverse route

        net.handle_path_request(
            PathRequestFrame {
                origin: near,
                target,
                nonce: 1,
                hop_limit: PATH_REQUEST_DEFAULT_HOP_LIMIT,
            },
            &conn,
            &iface,
        )
        .await;
        assert_eq!(net.path_table.lookup(&near, Instant::now()).unwrap().cost, 0);

        net.handle_path_request(
            PathRequestFrame {
                origin: far,
                target,
                nonce: 2,
                hop_limit: PATH_REQUEST_DEFAULT_HOP_LIMIT - 1,
            },
            &conn,
            &iface,
        )
        .await;
        assert_eq!(net.path_table.lookup(&far, Instant::now()).unwrap().cost, 1);
    }

    #[tokio::test]
    async fn losing_direct_with_relay_fallback_resignals_upgrade() {
        // When a DIRECT link drops but the peer still has a relay path, prune_path must signal a fast
        // re-punch (the direct→relay→direct cycle) — not leave the re-upgrade to the reconcile timer.
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();
        let relay_neighbor = NeighborId("relay-conn".into());
        let direct_neighbor = NeighborId("direct-conn".into());
        let iface = Arc::new(FailingInterface {
            id: crate::interface::InterfaceId("if".into()),
        }) as InterfaceHandle;

        // Reachable both over a relay and directly.
        net.register_path(
            peer,
            relay_neighbor,
            iface.clone(),
            PathQuality::Relay { hops: 1 },
            false,
        );
        // Drain the unreachable→relay signal from that first registration …
        assert_eq!(net.next_relay_upgrade().await, Some(peer));
        net.register_path(peer, direct_neighbor.clone(), iface.clone(), PathQuality::Direct, true);
        net.verified_neighbors.lock().insert(direct_neighbor.clone(), peer);

        // The direct connection dies → still relay-reachable, and re-signalled for a fast re-punch.
        net.prune_path(&direct_neighbor);
        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Relay { hops: 1 }));
        let resignalled =
            tokio::time::timeout(Duration::from_millis(50), net.next_relay_upgrade()).await;
        assert_eq!(
            resignalled.ok().flatten(),
            Some(peer),
            "dropping direct while a relay remains must re-signal a punch-upgrade"
        );
    }

    #[tokio::test]
    async fn verifying_a_neighbour_wakes_the_autonat_loop() {
        // Event-driven AutoNAT (Phase B): a freshly verified neighbour wakes the self-reachability
        // loop so it confirms our WAN endpoint immediately, instead of waiting up to a full announce
        // interval (the ~36 s first-confirm latency the live run showed). The periodic loop stays the
        // backstop. Mirrors the event-driven relay→direct punch signal.
        let net = Network::new(fresh_signing(), vec![]);
        // Nothing verified yet → no wake pending.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), net.autonat_triggered())
                .await
                .is_err(),
            "no wake before any neighbour is verified"
        );
        // A neighbour gets verified → the loop is woken.
        let _ = net.register_verified_link(&NeighborId("verifier-conn".into()), node());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), net.autonat_triggered())
                .await
                .is_ok(),
            "verifying a neighbour must wake the AutoNAT loop"
        );
    }

    #[test]
    fn dial_coalesces_onto_a_live_verified_direct_connection() {
        // Connection coalescing (#61): a re-dial to a peer we already hold a LIVE verified DIRECT
        // connection to reuses it (no duplicate connection). Once that connection dies (pruned from
        // verified_neighbors) or only a relay path remains, reuse declines so a fresh dial proceeds.
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();
        let neighbor = NeighborId("direct-conn".into());
        let iface = Arc::new(FailingInterface {
            id: crate::interface::InterfaceId("if".into()),
        }) as InterfaceHandle;

        // No path at all → nothing to reuse.
        assert!(net.reuse_live_direct_link(peer).is_none());

        // A DIRECT path whose neighbour is verified → reuse it.
        net.register_path(peer, neighbor.clone(), iface.clone(), PathQuality::Direct, true);
        net.verified_neighbors.lock().insert(neighbor.clone(), peer);
        let reused = net.reuse_live_direct_link(peer);
        assert!(
            reused.is_some(),
            "a live verified direct connection must be reused (coalesced)"
        );
        assert_eq!(reused.unwrap().destination, peer);

        // Neighbour pruned from verified (connection died) → don't reuse; a fresh dial proceeds.
        net.verified_neighbors.lock().remove(&neighbor);
        assert!(
            net.reuse_live_direct_link(peer).is_none(),
            "a dead (unverified) connection must not be reused"
        );

        // Only a RELAY path (no direct) → don't reuse (relay-only peers dial via multi-hop).
        net.verified_neighbors.lock().insert(neighbor.clone(), peer);
        net.peer_paths.lock().get(&peer).unwrap().lock().paths.clear();
        net.register_path(
            peer,
            neighbor.clone(),
            iface.clone(),
            PathQuality::Relay { hops: 1 },
            false,
        );
        assert!(
            net.reuse_live_direct_link(peer).is_none(),
            "a relay-only path is not a direct connection to coalesce onto"
        );
    }

    #[test]
    fn peer_link_quality_reports_none_then_relay_then_direct() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let sim_iface = fabric.interface("10.0.0.1:1".parse().unwrap(), NatModel::Public);
        let net = Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]);
        let peer = node();
        // No path yet → None (honest "cannot reach" vs binary has_link).
        assert_eq!(net.peer_link_quality(&peer), None);
        // A relay path → Relay { hops }.
        let n1 = NeighborId("10.0.0.2:1".into());
        net.verified_neighbors.lock().insert(n1.clone(), peer);
        let _ = net.register_path(
            peer,
            n1,
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Relay { hops: 2 },
            false,
        );
        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Relay { hops: 2 }));
        // A direct path is better → Direct wins (mirrors PathSet::best ranking).
        let n2 = NeighborId("10.0.0.3:1".into());
        net.verified_neighbors.lock().insert(n2.clone(), peer);
        let _ = net.register_path(
            peer,
            n2,
            sim_iface as InterfaceHandle,
            PathQuality::Direct,
            false,
        );
        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Direct));
    }

    #[test]
    fn peer_best_carrier_climbs_tcp_to_udp_at_equal_quality() {
        // R1a connection-quality ladder: the carrier climb's target state. A peer reached over a
        // 1-hop TCP relay (rung 0) and an equal-hop UDP relay (rung 1) keeps the SAME LinkQuality
        // (`Relay { hops: 1 }`) — so only `peer_best_carrier` can see the climb (TCP→UDP). This pins
        // the behaviour the climber must produce. See docs/decisions/connection-quality-ladder.md.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let tcp_iface = fabric.interface_with_kind(
            "10.0.0.2:1".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverTcp,
        );
        let udp_iface = fabric.interface_with_kind(
            "10.0.0.3:1".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverUdp,
        );
        let net = Network::new(
            fresh_signing(),
            vec![
                tcp_iface.clone() as InterfaceHandle,
                udp_iface.clone() as InterfaceHandle,
            ],
        );
        let peer = node();
        assert_eq!(net.peer_best_carrier(&peer), None, "no path → no carrier");

        // Rung 0 base: the reliable TLS-over-TCP relay.
        let n_tcp = NeighborId("10.0.0.2:1".into());
        net.verified_neighbors.lock().insert(n_tcp.clone(), peer);
        let _ = net.register_path(
            peer,
            n_tcp,
            tcp_iface as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        assert_eq!(net.peer_best_carrier(&peer), Some(InterfaceKind::QuicOverTcp));
        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Relay { hops: 1 }));

        // Rung 1: the equal-hop UDP relay the climber adds — `best()` flips to UDP…
        let n_udp = NeighborId("10.0.0.3:1".into());
        net.verified_neighbors.lock().insert(n_udp.clone(), peer);
        let _ = net.register_path(
            peer,
            n_udp,
            udp_iface as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        assert_eq!(
            net.peer_best_carrier(&peer),
            Some(InterfaceKind::QuicOverUdp),
            "UDP relay (rung 1) wins the carrier climb"
        );
        // …while LinkQuality is unchanged — exactly why the carrier readout was needed.
        assert_eq!(net.peer_link_quality(&peer), Some(LinkQuality::Relay { hops: 1 }));
    }

    #[test]
    fn upgrade_relay_carriers_climbs_a_relayed_peer_to_the_udp_carrier() {
        // R1a carrier climb: once we hold BOTH a TCP and a UDP link to relay R, a peer B reached
        // only over the TCP relay gains an equal-cost UDP relay path and best() climbs to UDP. This
        // is exactly what the dual-carrier autoconnect (STEP 2b wiring) activates; until then the
        // method is a no-op (one carrier link → nothing to add). See connection-quality-ladder.md.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let tcp = fabric.interface_with_kind(
            "10.0.0.9:1".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverTcp,
        );
        let udp = fabric.interface_with_kind(
            "10.0.0.9:2".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverUdp,
        );
        let net = Network::new(
            fresh_signing(),
            vec![tcp.clone() as InterfaceHandle, udp.clone() as InterfaceHandle],
        );
        let relay = node();
        let peer = node();

        // Two carrier links to relay R (its Direct paths) over distinct per-carrier neighbours.
        let r_tcp = NeighborId("10.0.0.5:1".into());
        let r_udp = NeighborId("10.0.0.5:2".into());
        net.verified_neighbors.lock().insert(r_tcp.clone(), relay);
        net.verified_neighbors.lock().insert(r_udp.clone(), relay);
        let _ = net.register_path(
            relay,
            r_tcp.clone(),
            tcp.clone() as InterfaceHandle,
            PathQuality::Direct,
            true,
        );
        let _ = net.register_path(relay, r_udp, udp as InterfaceHandle, PathQuality::Direct, true);

        // B is reached ONLY over the TCP relay so far (rung 0).
        let _ = net.register_path(
            peer,
            r_tcp,
            tcp as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        assert_eq!(net.peer_best_carrier(&peer), Some(InterfaceKind::QuicOverTcp));

        // The carrier climb registers the equal-cost UDP relay path → best() flips to UDP.
        net.upgrade_relay_carriers();
        assert_eq!(
            net.peer_best_carrier(&peer),
            Some(InterfaceKind::QuicOverUdp),
            "climbed to the UDP relay carrier"
        );
        assert_eq!(
            net.peer_link_quality(&peer),
            Some(LinkQuality::Relay { hops: 1 }),
            "still a 1-hop relay — only the carrier improved"
        );
    }

    #[tokio::test]
    async fn dual_carrier_autoconnect_holds_two_distinct_relay_links_and_prune_keeps_rung0() {
        // STEP 2b-2 ACTIVATION regression guard (the indivisible piece). The dual-carrier autoconnect
        // dials the relay over EACH carrier at its OWN endpoint via `dial_over_carrier`, so the
        // two A↔R links land DISTINCT NeighborIds. Guards BOTH ways it could regress:
        //  (a) the links must NOT collapse into one Direct path (the NeighborId-collision footgun that
        //      would silently leave the climber nothing to add — both carriers dialed at `:443`); and
        //  (b) pruning the UDP carrier on its death must leave the TCP rung-0 base intact (the
        //      "never regress rung-0 self-heal" invariant). Builds on
        //      `upgrade_relay_carriers_climbs_a_relayed_peer_to_the_udp_carrier`, but drives the REAL
        //      pinned-carrier dial end-to-end. See docs/decisions/connection-quality-ladder.md.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        // Relay R answers on BOTH a TLS-over-TCP bridge endpoint (`:8443`) and a native UDP endpoint
        // (`:443`) — the very split that makes the two carrier links distinguishable.
        let r_tcp_addr: SocketAddr = "10.0.0.5:8443".parse().unwrap();
        let r_udp_addr: SocketAddr = "10.0.0.5:443".parse().unwrap();
        let relay = Arc::new(Network::new(
            fresh_signing(),
            vec![
                fabric.interface_with_kind(r_tcp_addr, NatModel::Public, InterfaceKind::QuicOverTcp)
                    as InterfaceHandle,
                fabric.interface_with_kind(r_udp_addr, NatModel::Public, InterfaceKind::QuicOverUdp)
                    as InterfaceHandle,
            ],
        ));
        // Client A holds both carriers (dual-carrier Auto), [TCP, UDP].
        let a_tcp = fabric.interface_with_kind(
            "10.0.0.1:8443".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverTcp,
        );
        let a_udp = fabric.interface_with_kind(
            "10.0.0.1:443".parse().unwrap(),
            NatModel::Public,
            InterfaceKind::QuicOverUdp,
        );
        let a = Arc::new(Network::new(
            fresh_signing(),
            vec![a_tcp.clone() as InterfaceHandle, a_udp.clone() as InterfaceHandle],
        ));
        relay.start();
        a.start();
        let relay_id = relay.local_address();

        // Establish BOTH A↔R links via the pinned primitive — each dialed at its OWN endpoint.
        a.dial_over_carrier(relay_id, &r_tcp_addr.to_string(), InterfaceKind::QuicOverTcp)
            .await
            .expect("TCP rung-0 base link");
        a.dial_over_carrier(relay_id, &r_udp_addr.to_string(), InterfaceKind::QuicOverUdp)
            .await
            .expect("UDP rung-1 additive link");

        // (a) Two DISTINCT Direct paths to the relay — NOT collapsed by a NeighborId collision.
        let relay_links: Vec<(NeighborId, InterfaceKind)> = {
            let map = a.peer_paths.lock();
            let set = map.get(&relay_id).expect("relay has paths");
            let g = set.lock();
            g.paths
                .iter()
                .map(|p| (p.neighbor.clone(), p.iface.kind()))
                .collect()
        };
        assert_eq!(
            relay_links.len(),
            2,
            "two carrier links to the relay must NOT collapse (distinct NeighborIds)"
        );
        let n_tcp = relay_links
            .iter()
            .find(|(_, k)| *k == InterfaceKind::QuicOverTcp)
            .map(|(n, _)| n.clone())
            .expect("a TCP A↔R link");
        let n_udp = relay_links
            .iter()
            .find(|(_, k)| *k == InterfaceKind::QuicOverUdp)
            .map(|(n, _)| n.clone())
            .expect("a UDP A↔R link");
        assert_ne!(n_tcp, n_udp, "the two A↔R links carry distinct neighbours");

        // Peer B is reached over the TCP relay (rung 0); the climber adds the equal-cost UDP path.
        let peer = node();
        let _ = a.register_path(
            peer,
            n_tcp.clone(),
            a_tcp as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        assert_eq!(a.peer_best_carrier(&peer), Some(InterfaceKind::QuicOverTcp));
        a.upgrade_relay_carriers();
        assert_eq!(
            a.peer_best_carrier(&peer),
            Some(InterfaceKind::QuicOverUdp),
            "climbed onto the UDP rung-1 carrier"
        );

        // (b) The UDP carrier dies → fail back to the TCP rung-0 base, which SURVIVES the prune.
        a.prune_path(&n_udp);
        assert_eq!(
            a.peer_best_carrier(&peer),
            Some(InterfaceKind::QuicOverTcp),
            "fell back to the TCP rung-0 carrier"
        );
        assert_eq!(
            a.peer_link_quality(&relay_id),
            Some(LinkQuality::Direct),
            "the relay keeps its TCP rung-0 Direct base after the UDP prune (no self-heal regression)"
        );
        assert!(
            a.verified_neighbors.lock().contains_key(&n_tcp),
            "the TCP rung-0 neighbour survives the UDP carrier prune"
        );
    }

    #[test]
    fn snapshot_peer_book_keeps_direct_endpoints_skips_relay_only() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let sim_iface = fabric.interface("10.0.0.1:1".parse().unwrap(), NatModel::Public);
        let net = Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]);

        let direct_peer = NodeAddress::from_pubkey(&fresh_signing().verifying_key());
        let relay_peer = NodeAddress::from_pubkey(&fresh_signing().verifying_key());
        // A Direct path → its neighbour's remote_addr is the dialable endpoint (SimInterface parses
        // the NeighborId as a SocketAddr).
        let dn = NeighborId("10.0.0.2:7".into());
        net.verified_neighbors.lock().insert(dn.clone(), direct_peer);
        let _ = net.register_path(
            direct_peer,
            dn,
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Direct,
            false,
        );
        // A relay-only peer → skipped (a relay's remote_addr isn't the peer's dialable endpoint).
        let rn = NeighborId("10.0.0.3:9".into());
        net.verified_neighbors.lock().insert(rn.clone(), relay_peer);
        let _ = net.register_path(
            relay_peer,
            rn,
            sim_iface as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );

        // Only the direct peer's endpoint is captured for the warm-rejoin cache.
        assert_eq!(
            net.snapshot_peer_book(),
            vec![(direct_peer, "10.0.0.2:7".to_string())]
        );
    }

    #[test]
    fn path_control_rate_limit_caps_a_neighbour_within_the_window() {
        let net = Network::new(fresh_signing(), vec![]);
        let spammer = NeighborId("10.0.0.2:1".into());
        // The first MAX path-control frames are admitted; the next is dropped (same window).
        for _ in 0..PATH_CONTROL_RATE_MAX {
            assert!(net.path_control_admit(&spammer));
        }
        assert!(!net.path_control_admit(&spammer), "over budget ⇒ dropped");
        // A different neighbour has its own independent budget.
        assert!(net.path_control_admit(&NeighborId("10.0.0.3:1".into())));
    }

    #[tokio::test]
    async fn close_event_prunes_dead_path_keeps_alternate() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(
            fresh_signing(),
            vec![sim_iface.clone() as InterfaceHandle],
        ));
        let peer = node();
        let n1 = NeighborId("10.0.0.2:1".into());
        let n2 = NeighborId("10.0.0.3:1".into());
        // Two verified paths to the same peer (P2 multi-path).
        net.verified_neighbors.lock().insert(n1.clone(), peer);
        net.verified_neighbors.lock().insert(n2.clone(), peer);
        let _ = net.register_path(peer, n1.clone(), sim_iface.clone() as InterfaceHandle, PathQuality::Relay { hops: 1 }, false);
        let _ = net.register_path(peer, n2.clone(), sim_iface.clone() as InterfaceHandle, PathQuality::Relay { hops: 2 }, false);
        net.start();

        // n1's connection closes → the close pump must prune that path.
        sim_iface.inject_close(n1.clone());

        let mut pruned = false;
        for _ in 0..100 {
            if !net.verified_neighbors.lock().contains_key(&n1) {
                pruned = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(pruned, "dead path pruned on close");
        assert_eq!(
            net.verified_neighbors.lock().get(&n2),
            Some(&peer),
            "the alternate path survives"
        );
        let set = net.peer_paths.lock().get(&peer).cloned().unwrap();
        assert_eq!(set.lock().paths.len(), 1, "only the live path remains in the PathSet");
    }

    #[test]
    fn rx_growth_marks_a_neighbour_active_for_liveness() {
        // Guards docs/bugs/chat-gated-on-stale-has-link.md: a connection whose RX grew since the last
        // tick (e.g. carrying a bulk attachment stream) counts as alive, so `last_seen` is refreshed
        // and the path is not pruned for mesh-frame idle.
        let n = |s: &str| NeighborId(s.to_string());
        let mut baseline: std::collections::HashMap<NeighborId, u64> = std::collections::HashMap::new();
        baseline.insert(n("a"), 1_000);
        baseline.insert(n("b"), 5_000);
        // "a": RX grew 1000->1500 => active. "b": flat => not. "c": no baseline yet => not this tick.
        let current = vec![(n("a"), 1_500u64), (n("b"), 5_000u64), (n("c"), 42u64)];
        assert_eq!(
            Network::rx_active_neighbours(&current, &baseline),
            vec![n("a")]
        );
    }

    #[test]
    fn canonical_inbox_is_kept_while_read_and_replaced_once_its_pump_is_gone() {
        // Guards docs/bugs/mesh-link-flaps-on-mobile.md. Two failure modes, one map:
        //   * replacing a LIVE inbox orphans the pump that is reading it (the relay-chat decay);
        //   * keeping a DEAD one black-holes the peer forever — `or_insert` can never replace it,
        //     so every inbound frame is dropped and each re-dial hands its pump a detached
        //     receiver that exits immediately (`reused=true` + `pump/exit` in the same ms).
        let map: SyncMutex<HashMap<NodeAddress, mpsc::Sender<Vec<u8>>>> =
            SyncMutex::new(HashMap::new());
        let peer = node();

        let (first_tx, first_rx) = mpsc::channel::<Vec<u8>>(4);
        assert!(
            !Network::install_canonical_inbox(&map, peer, first_tx),
            "the first link wires the channel"
        );

        // A re-dial while the first pump is still reading: keep the live inbox, detach the new rx.
        let (second_tx, _second_rx) = mpsc::channel::<Vec<u8>>(4);
        assert!(
            Network::install_canonical_inbox(&map, peer, second_tx),
            "a live inbox must not be orphaned by a re-dial"
        );
        assert!(
            !map.lock().get(&peer).unwrap().is_closed(),
            "the kept sender still reaches the first pump"
        );

        // The first pump exits (its receiver drops) — now the stored sender is dead.
        drop(first_rx);
        let (third_tx, third_rx) = mpsc::channel::<Vec<u8>>(4);
        assert!(
            !Network::install_canonical_inbox(&map, peer, third_tx),
            "a dead inbox must be replaced, not kept"
        );
        map.lock()
            .get(&peer)
            .unwrap()
            .try_send(b"frame".to_vec())
            .expect("inbound frames reach the new pump");
        drop(third_rx);
    }

    // --- mesh ping-liveness (prompt link-DOWN) ---
    #[tokio::test]
    async fn liveness_pings_quiet_neighbour_then_prunes_when_silent() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(
            fresh_signing(),
            vec![sim_iface.clone() as InterfaceHandle],
        ));
        let peer = node();
        let n = NeighborId("10.0.0.2:1".into());
        net.verified_neighbors.lock().insert(n.clone(), peer);
        let _ = net.register_path(
            peer,
            n.clone(),
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Direct,
            true,
        );
        // Anchor last-seen to a base instant; drive `now` forward synthetically so
        // the policy is deterministic (no real 4-17s waits).
        let t0 = Instant::now();
        net.last_seen.lock().insert(n.clone(), t0);

        // Fresh: leave it alone.
        let (ping, prune) = net.liveness_plan(t0 + Duration::from_secs(1));
        assert!(ping.is_empty() && prune.is_empty(), "fresh neighbour untouched");

        // Quiet past PING_AFTER (but not DEAD): ping it.
        let (ping, prune) = net.liveness_plan(t0 + LIVENESS_PING_AFTER + Duration::from_secs(1));
        assert_eq!(ping.len(), 1, "quiet neighbour pinged");
        assert_eq!(ping[0].0, n, "the right neighbour");
        assert!(prune.is_empty(), "not dead yet");

        // Silent past DEAD_AFTER: prune (and don't bother pinging).
        let (ping, prune) = net.liveness_plan(t0 + LIVENESS_DEAD_AFTER + Duration::from_secs(1));
        assert!(ping.is_empty(), "dead neighbour not pinged");
        assert_eq!(prune, vec![n.clone()], "silent neighbour scheduled for prune");

        // Acting on the plan prunes the last path → surfaces peer_down for the app,
        // so the next message falls through to durable without a process restart.
        net.prune_path(&n);
        assert_eq!(net.try_recv_peer_down(), Some(peer), "peer_down emitted");
        assert!(
            !net.verified_neighbors.lock().contains_key(&n),
            "neighbour dropped"
        );
        assert!(net.last_seen.lock().get(&n).is_none(), "last_seen cleared");
    }

    /// A peer that re-verifies over a new direct connection (it restarted) leaves its old one dead
    /// and silent: that one is pruned shortly after the probe instead of at `LIVENESS_DEAD_AFTER`,
    /// while the new path stays. Negative half: an older path that answers the probe (a live spare
    /// from a simultaneous open) is kept.
    #[tokio::test]
    async fn a_reconnect_prunes_the_silent_old_path_but_keeps_a_live_spare() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let sim_iface = fabric.interface("10.0.0.1:1".parse().unwrap(), NatModel::Public);
        let net = Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]);
        let peer = node();
        let (old, spare, fresh) = (
            NeighborId("10.0.0.2:1#old".into()),
            NeighborId("10.0.0.2:1#spare".into()),
            NeighborId("10.0.0.2:1#fresh".into()),
        );
        let t0 = Instant::now();
        for n in [&old, &spare, &fresh] {
            net.verified_neighbors.lock().insert(n.clone(), peer);
            net.last_seen.lock().insert(n.clone(), t0);
            let _ = net.register_path(
                peer,
                n.clone(),
                sim_iface.clone() as InterfaceHandle,
                PathQuality::Direct,
                true,
            );
        }

        let probed = net.mark_superseded_direct_paths(peer, &fresh, t0);
        let mut probed: Vec<NeighborId> = probed.into_iter().map(|(n, _)| n).collect();
        probed.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(probed, vec![old.clone(), spare.clone()], "only the older paths are probed");

        // The spare answers the ping; the old one stays silent.
        net.last_seen.lock().insert(spare.clone(), t0 + Duration::from_millis(50));
        assert!(
            net.superseded_plan(t0 + Duration::from_secs(1)).is_empty(),
            "nothing is judged before the window ends"
        );
        let dead = net.superseded_plan(t0 + SUPERSEDED_PROBE_WINDOW);
        assert_eq!(dead, vec![old.clone()], "the silent old path, and only it");
        assert!(
            SUPERSEDED_PROBE_WINDOW < LIVENESS_DEAD_AFTER,
            "a restart is noticed well before plain liveness would"
        );
        for n in dead {
            net.prune_path(&n);
        }
        let remaining: Vec<NeighborId> = net.peer_paths.lock()[&peer]
            .lock()
            .paths
            .iter()
            .map(|p| p.neighbor.clone())
            .collect();
        assert!(remaining.contains(&fresh) && remaining.contains(&spare), "{remaining:?}");
        assert!(!remaining.contains(&old), "{remaining:?}");
        assert!(net.superseded_probes.lock().is_empty(), "every probe settled");
    }

    // --- end-to-end relay-peer liveness (relay-only-peer presence honesty) ---
    #[tokio::test]
    async fn relay_only_peer_probed_then_pruned_when_silent_but_kept_while_heard() {
        // The fix for "online via relay to a dead peer" (docs/bugs/relay-presence-false-online-dead-peer.md):
        // a peer reached ONLY over a relay is never our path neighbour, so per-neighbour liveness can't
        // see it die. `relay_peer_liveness_plan` probes it end-to-end (routed PeerPing), keeps it while
        // it answers (PeerPong refreshes `peer_last_seen`), and prunes it once silent past the dead
        // window — and a peer with a DIRECT path is left entirely to neighbour liveness.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]));

        // Relay-only peer: a Relay path whose neighbour is the relay LINK, not the peer itself.
        let peer = node();
        let relay_n = NeighborId("20.0.0.1:1".into());
        let t0 = Instant::now();
        net.peer_last_seen.lock().insert(peer, t0); // anchor (register_path's or_insert won't override)
        let _ = net.register_path(
            peer,
            relay_n.clone(),
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );

        // Fresh: neither probe nor prune.
        let (probe, prune) = net.relay_peer_liveness_plan(t0 + Duration::from_secs(1));
        assert!(probe.is_empty() && prune.is_empty(), "fresh relay peer untouched");

        // End-to-end-quiet past PING_AFTER (not dead): probe it, routed over the relay neighbour.
        let (probe, prune) =
            net.relay_peer_liveness_plan(t0 + LIVENESS_PING_AFTER + Duration::from_secs(1));
        assert_eq!(probe.len(), 1, "quiet relay peer probed end-to-end");
        assert_eq!(probe[0].0, peer);
        assert_eq!(probe[0].1, relay_n, "probe routes over the relay neighbour");
        assert!(prune.is_empty(), "not dead yet");
        // The caller sends that probe and remembers its nonce.
        net.peer_ping_nonce.lock().insert(peer, 1);

        // Silent past RELAY_PEER_DEAD_AFTER with the probe unanswered: scheduled for prune.
        let (_probe, prune) =
            net.relay_peer_liveness_plan(t0 + RELAY_PEER_DEAD_AFTER + Duration::from_secs(1));
        assert_eq!(prune, vec![peer], "silent relay peer scheduled for prune");

        // Pruned; its frames come back through the relay and re-register the path. It is asked
        // first, not pruned on sight (that kept a peer reachable only through a relay «down» for good).
        net.prune_relay_only_peer(peer);
        let _ = net.register_path(peer, relay_n.clone(), sim_iface.clone() as InterfaceHandle, PathQuality::Relay { hops: 1 }, false);
        let (probe, prune) = net.relay_peer_liveness_plan(t0 + RELAY_PEER_DEAD_AFTER + Duration::from_secs(2));
        assert!(prune.is_empty(), "a re-registered silent peer is probed before any prune");
        assert_eq!(probe.iter().map(|(p, _, _)| *p).collect::<Vec<_>>(), vec![peer]);

        // A PeerPong refreshes peer_last_seen → NOT pruned past the dead window (no flapping of a
        // healthy relay link). Simulate the refresh by re-anchoring (handle_peer_pong → note_peer_seen).
        let t1 = t0 + RELAY_PEER_DEAD_AFTER + Duration::from_secs(2);
        net.peer_last_seen.lock().insert(peer, t1);
        let (_probe, prune) = net.relay_peer_liveness_plan(t1 + Duration::from_secs(1));
        assert!(prune.is_empty(), "a peer still answering probes is kept");

        // A peer with a DIRECT path is never relay-probed/pruned here (neighbour liveness owns it),
        // even with an ancient incidental peer_last_seen.
        let direct_peer = node();
        net.peer_last_seen.lock().insert(direct_peer, t0);
        let _ = net.register_path(
            direct_peer,
            NeighborId("30.0.0.1:1".into()),
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Direct,
            true,
        );
        let (probe, prune) =
            net.relay_peer_liveness_plan(t0 + RELAY_PEER_DEAD_AFTER + Duration::from_secs(5));
        assert!(!prune.contains(&direct_peer), "a direct peer is never relay-pruned");
        assert!(
            !probe.iter().any(|(p, _, _)| *p == direct_peer),
            "a direct peer is never relay-probed"
        );

        // Acting on a prune drops the peer's paths + surfaces peer_down so presence goes offline.
        net.prune_relay_only_peer(peer);
        assert!(net.peer_paths.lock().get(&peer).is_none(), "peer paths removed");
        assert!(net.peer_link_quality(&peer).is_none(), "pruned relay peer has no link quality");
        assert_eq!(net.try_recv_peer_down(), Some(peer), "peer_down emitted for the app");

        // Fix 2 — NO re-grace: the prune KEEPS the (now stale) peer_last_seen, so a re-registered relay
        // path (autoconnect re-dial between ticks) does NOT read back online — `peer_link_quality` stays
        // None for a peer whose last proven liveness is older than the dead window. Without keeping it,
        // register_path's `or_insert` would re-seed FRESH and the peer would FLAP back to relay:1.
        assert!(
            net.peer_last_seen.lock().get(&peer).is_some(),
            "prune keeps the stale peer_last_seen so a re-register can't re-grace"
        );
        let _ = net.register_path(
            peer,
            relay_n.clone(),
            sim_iface.clone() as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        let stale_now = t1 + RELAY_PEER_DEAD_AFTER + Duration::from_secs(1);
        assert!(
            net.peer_link_quality_at(&peer, stale_now).is_none(),
            "a re-registered relay path with stale liveness stays offline (no flap-back)"
        );
    }

    /// A slow network: the answer to an earlier probe comes back after the next probe went out. It
    /// counts — as alive at the moment THAT probe left — while an unknown nonce and one pushed out of
    /// memory do not (stand 2026-10-15: a live contact behind a relay pruned every 25 s on bad Wi-Fi).
    #[tokio::test]
    async fn a_late_pong_to_an_earlier_probe_still_counts_as_of_its_probe() {
        let net = Network::new(fresh_signing(), vec![]);
        let peer = node();
        let t0 = Instant::now();
        net.peer_last_seen.lock().insert(peer, t0);
        let first_at = t0 + Duration::from_secs(5);
        net.remember_peer_ping(peer, 1, first_at);
        net.peer_ping_nonce.lock().insert(peer, 2);
        net.remember_peer_ping(peer, 2, t0 + Duration::from_secs(9));

        net.note_peer_pong(peer, 1);
        assert_eq!(net.peer_last_seen.lock().get(&peer), Some(&first_at), "alive as of the first probe");
        net.note_peer_pong(peer, 77);
        assert_eq!(net.peer_last_seen.lock().get(&peer), Some(&first_at), "an unknown nonce changes nothing");

        for n in 3..3 + PEER_PING_REMEMBERED as u64 {
            net.peer_ping_nonce.lock().insert(peer, n);
            net.remember_peer_ping(peer, n, t0 + Duration::from_secs(10 + n));
        }
        net.note_peer_pong(peer, 2);
        assert_eq!(net.peer_last_seen.lock().get(&peer), Some(&first_at), "a forgotten probe's answer is stale");
    }

    #[tokio::test]
    async fn relay_peer_pong_refreshes_only_on_matching_nonce() {
        // The flap fix (docs/bugs/relay-presence-false-online-dead-peer.md): a routed PeerPong refreshes
        // end-to-end liveness ONLY if its nonce matches the probe we last sent that peer — a stale /
        // duplicate / relay-buffered pong (old nonce) for a dead peer must NOT re-seed it alive.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface as InterfaceHandle]));
        let peer = node();

        // liveness_loop recorded that we sent a probe with this nonce.
        net.peer_ping_nonce.lock().insert(peer, 0xABCD);
        assert!(net.peer_last_seen.lock().get(&peer).is_none(), "no liveness yet");

        // A pong with the WRONG nonce is ignored (the post-death reseed we must reject).
        net.note_peer_pong(peer, 0x1234);
        assert!(
            net.peer_last_seen.lock().get(&peer).is_none(),
            "stale-nonce pong does not refresh liveness"
        );

        // A pong with the matching nonce proves the peer answered THIS probe → refresh.
        net.note_peer_pong(peer, 0xABCD);
        assert!(
            net.peer_last_seen.lock().get(&peer).is_some(),
            "matching-nonce pong refreshes liveness"
        );
    }

    // --- DHT-over-peer: a request addressed to us is delivered to the app to serve (2e-1b) ---
    #[tokio::test]
    async fn dht_proxy_request_to_us_is_delivered_to_the_app() {
        use crate::protocol::{DhtProxyOp, DhtProxyRequestFrame};
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface as InterfaceHandle]));
        let me = net.local_address();
        let requester = node();
        // The arrival neighbour must be verified or the handler drops the frame (anti-spoof gate).
        let neighbor = NeighborId("20.0.0.1:1".into());
        net.verified_neighbors.lock().insert(neighbor.clone(), requester);

        // A request addressed to US is delivered for the app to serve via its local omni-dht-helper.
        let req = DhtProxyRequestFrame {
            from: requester,
            to: me,
            req_id: 42,
            op: DhtProxyOp::Get,
            key: [7u8; 32],
            salt: b"0".to_vec(),
            value: Vec::new(),
            hop_limit: 6,
        };
        net.handle_dht_proxy_request(req.clone(), &neighbor).await;
        assert_eq!(
            net.try_recv_dht_proxy_request(),
            Some(req),
            "a DhtProxyRequest addressed to us is delivered to the app"
        );

        // A request for ANOTHER peer we have no route to is forwarded/dropped — NOT served locally.
        let other = node();
        let req2 = DhtProxyRequestFrame {
            from: requester,
            to: other,
            req_id: 43,
            op: DhtProxyOp::Get,
            key: [7u8; 32],
            salt: b"0".to_vec(),
            value: Vec::new(),
            hop_limit: 6,
        };
        net.handle_dht_proxy_request(req2, &neighbor).await;
        assert!(
            net.try_recv_dht_proxy_request().is_none(),
            "a request for another peer is not served locally"
        );
    }

    #[tokio::test]
    async fn dht_proxy_response_completes_the_pending_requester() {
        use crate::protocol::DhtProxyResponseFrame;
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface as InterfaceHandle]));
        let me = net.local_address();
        let proxy = node();
        let neighbor = NeighborId("20.0.0.1:1".into());
        net.verified_neighbors.lock().insert(neighbor.clone(), proxy);

        // Register a pending requester (as `dht_proxy_op` does) and deliver the matching response.
        let (tx, rx) = oneshot::channel();
        net.dht_proxy_pending.lock().insert(77, tx);
        let resp = DhtProxyResponseFrame {
            from: proxy,
            to: me,
            req_id: 77,
            ok: true,
            value: b"slot-value".to_vec(),
            hop_limit: 6,
        };
        net.handle_dht_proxy_response(resp, &neighbor).await;
        assert_eq!(
            rx.await.unwrap(),
            (true, b"slot-value".to_vec()),
            "the matching response completes the awaiting requester"
        );
        assert!(net.dht_proxy_pending.lock().is_empty(), "pending entry consumed");

        // A stray / late req_id finds no pending entry and is dropped (no panic).
        let stray = DhtProxyResponseFrame {
            from: proxy,
            to: me,
            req_id: 999,
            ok: true,
            value: Vec::new(),
            hop_limit: 6,
        };
        net.handle_dht_proxy_response(stray, &neighbor).await;
    }

    #[tokio::test]
    async fn dht_get_over_peer_returns_none_without_a_route() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface as InterfaceHandle]));
        // No path to this proxy → the request can't be routed → prompt `None` (no hang), and the
        // pending entry is cleaned up rather than leaked.
        let got = net
            .dht_get_over_peer(node(), [9u8; 32], b"0".to_vec(), Duration::from_millis(200))
            .await;
        assert!(got.is_none(), "no route to the proxy → None");
        assert!(net.dht_proxy_pending.lock().is_empty(), "pending entry cleaned up on no-route");
    }

    #[tokio::test]
    async fn peer_advertises_dht_proxy_only_from_a_capability_announce() {
        // Role-based proxy selection (R7 2e-4): we learn a peer's DHT-proxy role from its SIGNED
        // announce capabilities — not from a plain announce, and not for unknown peers. See
        // docs/decisions/dht-over-peer.md.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]));
        let iface = sim_iface as InterfaceHandle;
        let neighbor = NeighborId("20.0.0.1:1".into());

        // A peer whose verified announce declares the DHT-proxy role → advertises it.
        let proxy_key = fresh_signing();
        let proxy = NodeAddress::from_pubkey(&proxy_key.verifying_key());
        net.verified_neighbors.lock().insert(neighbor.clone(), proxy);
        let proxy_announce = Announce::sign_new_with_capabilities(
            &proxy_key,
            vec![],
            current_unix_ms(),
            300,
            crate::announce::ANNOUNCE_CAP_DHT_PROXY,
            0,
        );
        net.handle_announce(proxy_announce, &iface, &neighbor).await.expect("handle proxy announce");
        assert!(net.peer_advertises_dht_proxy(&proxy), "DHT_PROXY announce → role advertised");

        // A peer whose announce declares no capabilities → not a proxy.
        let plain_key = fresh_signing();
        let plain = NodeAddress::from_pubkey(&plain_key.verifying_key());
        let plain_announce = Announce::sign_new(&plain_key, vec![], current_unix_ms(), 300);
        net.handle_announce(plain_announce, &iface, &neighbor).await.expect("handle plain announce");
        assert!(!net.peer_advertises_dht_proxy(&plain), "plain announce → no role");

        // An unknown peer (no record) → not a proxy.
        assert!(!net.peer_advertises_dht_proxy(&node()), "unknown peer → no role");
    }

    #[tokio::test]
    async fn friend_relay_capability_and_per_friend_consent() {
        // Friend-proxy Phase A (Axis 2): the friend-relay role is learned from a peer's SIGNED announce
        // capabilities (consumer selector), and per-friend carriage consent is a SEPARATE,
        // off-by-default local opt-in — advertising the role ≠ consenting to carry everyone. See
        // docs/decisions/friend-proxy-by-consent.md.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let sim_iface = fabric.interface(addr, NatModel::Public);
        let net = Arc::new(Network::new(fresh_signing(), vec![sim_iface.clone() as InterfaceHandle]));
        let iface = sim_iface as InterfaceHandle;
        let neighbor = NeighborId("20.0.0.1:1".into());

        // A peer whose verified announce declares the friend-relay role → advertises it + is the candidate.
        let relay_key = fresh_signing();
        let relay = NodeAddress::from_pubkey(&relay_key.verifying_key());
        net.verified_neighbors.lock().insert(neighbor.clone(), relay);
        let relay_announce = Announce::sign_new_with_capabilities(
            &relay_key,
            vec![],
            current_unix_ms(),
            300,
            crate::announce::ANNOUNCE_CAP_FRIEND_RELAY,
            0,
        );
        net.handle_announce(relay_announce, &iface, &neighbor).await.expect("handle relay announce");
        assert!(net.peer_advertises_friend_relay(&relay), "FRIEND_RELAY announce → role advertised");
        assert_eq!(net.friend_relay_candidate(), Some(relay), "the advertiser is the candidate");

        // A plain announce → no role; still not a candidate; unknown peer → no role.
        let plain_key = fresh_signing();
        let plain = NodeAddress::from_pubkey(&plain_key.verifying_key());
        let plain_announce = Announce::sign_new(&plain_key, vec![], current_unix_ms(), 300);
        net.handle_announce(plain_announce, &iface, &neighbor).await.expect("handle plain announce");
        assert!(!net.peer_advertises_friend_relay(&plain), "plain announce → no role");
        assert!(!net.peer_advertises_friend_relay(&node()), "unknown peer → no role");
        assert_eq!(net.friend_relay_candidate(), Some(relay), "plain peer is not a candidate");

        // Per-friend carriage consent: off by default, set + revocable, independent of the advertised bit.
        let friend = node();
        assert!(!net.relay_consent_for(&friend), "consent is OFF by default");
        net.consent_to_relay_for(friend);
        assert!(net.relay_consent_for(&friend), "consent recorded after opt-in");
        net.revoke_relay_consent(&friend);
        assert!(!net.relay_consent_for(&friend), "consent cleared after revoke");
    }

    /// Friend-proxy Phase B end-to-end (Axis 2): A reaches C ONLY through friend B. B carries A's
    /// opaque payload to C **iff** B has consented to relay for A — without consent it drops; with
    /// consent, C receives it as `Data` FROM A. Gated entirely via the net-sim. See
    /// docs/decisions/friend-proxy-by-consent.md.
    #[tokio::test]
    async fn friend_relay_carries_only_for_a_consented_origin() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let c_addr: SocketAddr = "10.0.0.3:1".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (a_pub, b_pub, c_pub) =
            (a_key.verifying_key(), b_key.verifying_key(), c_key.verifying_key());

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c = Arc::new(Network::new(
            c_key,
            vec![fabric.interface(c_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let (a_node, b_node, c_node) = (a.local_address(), b.local_address(), c.local_address());

        // Adjacent links only: A↔B and B↔C. A never links C directly — B is A's only path to C.
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        seed_peer(&b, c_pub, vec![c_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);
        a.start();
        b.start();
        c.start();
        let _a_b = a.dial(b_node).await.expect("A links B");
        let _b_from_a = timeout(Duration::from_secs(5), b.accept())
            .await
            .expect("B accepts A")
            .expect("link");
        let _b_c = b.dial(c_node).await.expect("B links C");
        let _c_from_b = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts B")
            .expect("link");

        // WITHOUT consent: B drops A's carriage → C gets nothing.
        a.carry_via_friend(b_node, c_node, b"no-consent".to_vec())
            .await
            .expect("A sends the carriage frame to B");
        assert!(
            timeout(Duration::from_secs(1), c.accept()).await.is_err(),
            "no consent → B must NOT carry → C receives nothing"
        );

        // WITH consent: B carries → C receives it as Data FROM A.
        b.consent_to_relay_for(a_node);
        a.carry_via_friend(b_node, c_node, b"routed-via-friend-B".to_vec())
            .await
            .expect("A sends the carriage frame to B");
        let c_from_a = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts the on-demand link from A within 5s")
            .expect("link");
        assert_eq!(c_from_a.destination(), a_node, "carried payload appears to C as FROM A");
        let got = timeout(Duration::from_secs(5), c_from_a.recv())
            .await
            .expect("recv within 5s")
            .expect("recv ok");
        assert_eq!(got, b"routed-via-friend-B", "C receives exactly what A sent via B");

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    #[tokio::test]
    async fn admit_friend_relay_gates_on_consent_and_caps_per_origin() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let net = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let origin = node();
        let now = 1_000u64;

        // No consent → rejected regardless of the cap.
        assert!(!net.admit_friend_relay(&origin, now), "no consent → reject");

        // Consented → admitted up to the per-origin cap within the window, then rejected.
        net.consent_to_relay_for(origin);
        for i in 0..FRIEND_RELAY_MAX_CARRIES_PER_WINDOW {
            assert!(net.admit_friend_relay(&origin, now), "under cap must admit (i={i})");
        }
        assert!(!net.admit_friend_relay(&origin, now), "over the per-origin cap → reject in-window");

        // A later window resets the budget.
        assert!(
            net.admit_friend_relay(&origin, now + FRIEND_RELAY_WINDOW_MS + 1),
            "next window admits again"
        );
    }

    #[tokio::test]
    async fn admit_friend_exit_gates_on_consent_destinations_and_bytes_per_origin() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let net = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let origin = node();
        let now = 1_000u64;
        let dest = |i: usize| i.to_le_bytes().to_vec();

        // No consent → rejected regardless of caps.
        assert!(
            !net.admit_friend_exit(&origin, &dest(0), 1, now),
            "no exit consent → reject"
        );

        // Consented → distinct destinations admitted up to the cap, then a NEW destination is
        // rejected while an already-seen one still flows (bytes small, well under the byte cap).
        net.consent_to_exit_for(origin);
        for i in 0..FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW {
            assert!(net.admit_friend_exit(&origin, &dest(i), 1, now), "new dest under cap (i={i})");
        }
        assert!(
            !net.admit_friend_exit(&origin, &dest(FRIEND_EXIT_MAX_DESTINATIONS_PER_WINDOW), 1, now),
            "a new destination over the distinct-destination cap → reject"
        );
        assert!(
            net.admit_friend_exit(&origin, &dest(0), 1, now),
            "an already-seen destination in-window still admits"
        );

        // Byte cap: a fresh origin/window, one destination, egress exactly to the cap then over it.
        let heavy = node();
        net.consent_to_exit_for(heavy);
        let base = now + FRIEND_EXIT_WINDOW_MS * 10;
        assert!(
            net.admit_friend_exit(&heavy, &dest(0), FRIEND_EXIT_MAX_BYTES_PER_WINDOW, base),
            "egress up to the byte cap admits"
        );
        assert!(
            !net.admit_friend_exit(&heavy, &dest(0), 1, base),
            "one more byte over the cap → reject in-window"
        );
        assert!(
            net.admit_friend_exit(&heavy, &dest(0), 1, base + FRIEND_EXIT_WINDOW_MS + 1),
            "next window resets the byte budget"
        );

        // Revocation is effective immediately.
        net.revoke_exit_consent(&origin);
        assert!(
            !net.admit_friend_exit(&origin, &dest(0), 1, now),
            "revoked consent → reject"
        );
    }

    // --- rendezvous dial path: dial at an out-of-band endpoint (no announce) ---
    #[tokio::test]
    async fn dial_at_endpoints_connects_without_an_announce() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let a = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(a_addr, NatModel::Public) as InterfaceHandle],
        ));
        let b = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(b_addr, NatModel::Public) as InterfaceHandle],
        ));
        a.start();
        b.start();
        // No seed_peer / no announce — A only knows B's endpoint out-of-band (rendezvous).
        let link = a
            .dial_at_endpoints(b.local_address(), vec![b_addr])
            .await
            .expect("dial via the injected endpoint + mutual challenge");
        assert_eq!(link.destination(), b.local_address());
        assert!(
            a.verified_neighbors.lock().values().any(|p| *p == b.local_address()),
            "B is a verified neighbour after dialing its rendezvous endpoint"
        );
    }

    /// Our hop's carrier: TCP never dials a peer's raw UDP address (only one routed over its door),
    /// QUIC never takes a door, and a punch request is ignored in TCP. Auto dials as before.
    #[tokio::test]
    async fn the_carrier_of_our_hops_gates_udp_dials_and_doors() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let a = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(a_addr, NatModel::Public) as InterfaceHandle],
        ));
        let b = Arc::new(Network::new(
            fresh_signing(),
            vec![fabric.interface(b_addr, NatModel::Public) as InterfaceHandle],
        ));
        a.start();
        b.start();

        a.set_carrier_preference(CarrierPreference::Tcp);
        assert_eq!(a.carrier_preference(), CarrierPreference::Tcp);
        let tcp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            a.dial_at_endpoints(b.local_address(), vec![b_addr]),
        )
        .await
        .expect("a refused dial does not hang");
        assert!(tcp.is_err(), "TCP: no dial to a raw UDP address");
        assert!(
            !a.verified_neighbors.lock().values().any(|p| *p == b.local_address()),
            "and nothing came up"
        );

        a.set_carrier_preference(CarrierPreference::Quic);
        assert!(a.dial_over_door(b.local_address(), b_addr).await.is_err(), "QUIC: no door");
        assert!(!a.route_peer_over_door(&b.local_address(), b_addr).await.unwrap(), "nor a move onto one");

        a.set_carrier_preference(CarrierPreference::Auto);
        a.dial_at_endpoints(b.local_address(), vec![b_addr])
            .await
            .expect("Auto dials the UDP address");
    }

    /// The carrier of our hops binds a dialer too: a TCP node refuses a raw-UDP dial, a QUIC node a
    /// TCP one — «tcp — auto» rides TCP, «quic — auto» QUIC, «quic — tcp» nothing (owner, 2026-10-16).
    #[tokio::test]
    async fn the_carrier_of_our_hops_refuses_a_dialer_on_the_other_carrier() {
        use crate::sim::{NatModel, SimFabric};
        async fn dial(kind: InterfaceKind, theirs: CarrierPreference) -> bool {
            let fabric = SimFabric::new();
            let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
            let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
            let a = Arc::new(Network::new(
                fresh_signing(),
                vec![fabric.interface_with_kind(a_addr, NatModel::Public, kind) as InterfaceHandle],
            ));
            let b = Arc::new(Network::new(
                fresh_signing(),
                vec![fabric.interface_with_kind(b_addr, NatModel::Public, kind) as InterfaceHandle],
            ));
            a.start();
            b.start();
            b.set_carrier_preference(theirs);
            let dialed = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                a.dial_at_endpoints(b.local_address(), vec![b_addr]),
            )
            .await
            .expect("a refused dial does not hang");
            let linked = b.verified_neighbors.lock().values().any(|p| *p == a.local_address());
            assert_eq!(dialed.is_ok(), linked, "the dialer and the dialed agree");
            linked
        }
        assert!(!dial(InterfaceKind::QuicOverUdp, CarrierPreference::Tcp).await, "TCP refuses raw UDP");
        assert!(dial(InterfaceKind::QuicOverUdp, CarrierPreference::Auto).await, "Auto takes UDP");
        assert!(dial(InterfaceKind::QuicOverUdp, CarrierPreference::Quic).await, "QUIC takes UDP");
        assert!(!dial(InterfaceKind::QuicOverTcp, CarrierPreference::Quic).await, "QUIC refuses TCP");
        assert!(dial(InterfaceKind::QuicOverTcp, CarrierPreference::Tcp).await, "TCP takes TCP");
    }

    /// A carrier change on a running network applies at once: a link on the carrier it refuses, with
    /// nowhere to move (no door known, nothing to migrate back), is closed; Auto refuses nothing; and
    /// the link comes back once the carrier allows it again.
    #[tokio::test]
    async fn a_carrier_change_closes_at_once_the_links_it_refuses() {
        use crate::sim::{NatModel, SimFabric};
        async fn linked_pair(kind: InterfaceKind) -> (Arc<Network>, Arc<Network>, SocketAddr) {
            let fabric = SimFabric::new();
            let a_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
            let b_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
            let a = Arc::new(Network::new(
                fresh_signing(),
                vec![fabric.interface_with_kind(a_addr, NatModel::Public, kind) as InterfaceHandle],
            ));
            let b = Arc::new(Network::new(
                fresh_signing(),
                vec![fabric.interface_with_kind(b_addr, NatModel::Public, kind) as InterfaceHandle],
            ));
            a.start();
            b.start();
            a.dial_at_endpoints(b.local_address(), vec![b_addr]).await.expect("Auto — Auto links");
            (a, b, b_addr)
        }
        let linked = |net: &Network, peer: NodeAddress| net.verified_neighbors.lock().values().any(|p| *p == peer);
        let no_doors = HashMap::new();

        let (a, b, b_addr) = linked_pair(InterfaceKind::QuicOverUdp).await;
        let kept = a.switch_carrier_preference(CarrierPreference::Auto, &no_doors).await;
        assert_eq!(kept, 0, "Auto refuses nothing");
        assert!(linked(&a, b.local_address()));

        let to_tcp = a.switch_carrier_preference(CarrierPreference::Tcp, &no_doors).await;
        assert_eq!(to_tcp, 1, "the raw UDP link is closed");
        assert!(!linked(&a, b.local_address()), "the UDP link is gone at once, not at the next start");

        a.switch_carrier_preference(CarrierPreference::Auto, &no_doors).await;
        let back = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            a.dial_at_endpoints(b.local_address(), vec![b_addr]),
        )
        .await
        .expect("the dial does not hang");
        assert!(back.is_ok(), "back on Auto, UDP links again");

        let (a, b, _) = linked_pair(InterfaceKind::QuicOverTcp).await;
        let to_quic = a.switch_carrier_preference(CarrierPreference::Quic, &no_doors).await;
        assert_eq!(to_quic, 1, "a TCP-carried link under QUIC — closed");
        assert!(!linked(&a, b.local_address()));
    }

    // --- AutoNAT reachability dial-back (P2) ---
    #[tokio::test]
    async fn autonat_dial_back_confirms_reachable_over_sim() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let n_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let v_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let n_iface = fabric.interface(n_addr, NatModel::Public);
        let v_iface = fabric.interface(v_addr, NatModel::Public);
        let n = Arc::new(Network::new(
            fresh_signing(),
            vec![n_iface.clone() as InterfaceHandle],
        ));
        let v = Arc::new(Network::new(fresh_signing(), vec![v_iface as InterfaceHandle]));
        n.start();
        v.start();

        // N knows a path to verifier V (pairing is covered by other tests).
        let v_node = node();
        let _ = n.register_path(
            v_node,
            NeighborId(v_addr.to_string()),
            n_iface.clone() as InterfaceHandle,
            PathQuality::Direct,
            true,
        );

        assert_eq!(n.reachability_state(), ReachabilityState::Unknown);

        // N asks V to dial it back at its own public address.
        n.request_dial_back(&v_node, n_addr)
            .await
            .expect("send dial-back request");

        let mut reachable = false;
        for _ in 0..100 {
            if n.reachability_state() == ReachabilityState::Reachable {
                reachable = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(reachable, "dial-back probe confirmed direct reachability");
        assert_eq!(n.reachable_address(), Some(n_addr));
    }

    #[test]
    fn verified_wan_peer_picks_a_direct_wan_neighbour_not_lan_or_relayed() {
        // The AutoNAT verifier must be a peer we reach DIRECTLY over a routable-WAN address (so it
        // observes our public source IP — the precondition for its dial-back anti-amplification
        // guard to fire). A LAN neighbour (sees our private IP) and a relayed peer (presents the
        // relay's IP) are both ineligible. In practice the eligible one is the bootstrap relay.
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let sim = fabric.interface("10.0.0.9:1".parse().unwrap(), NatModel::Public);
        let net = Network::new(fresh_signing(), vec![sim.clone() as InterfaceHandle]);
        let relay = node();
        let lan_peer = node();
        let relayed = node();

        // DIRECT + verified + WAN addr → the eligible verifier.
        let relay_n = NeighborId("203.0.113.7:443".into());
        net.verified_neighbors.lock().insert(relay_n.clone(), relay);
        net.register_path(relay, relay_n, sim.clone() as InterfaceHandle, PathQuality::Direct, true);
        // DIRECT + verified but a PRIVATE (LAN) addr → sees our private IP, not a WAN verifier.
        let lan_n = NeighborId("192.168.1.5:7920".into());
        net.verified_neighbors.lock().insert(lan_n.clone(), lan_peer);
        net.register_path(lan_peer, lan_n, sim.clone() as InterfaceHandle, PathQuality::Direct, true);
        // RELAYED (non-direct) WAN path → a relayed verifier presents the relay's IP, not ours.
        let relayed_n = NeighborId("198.51.100.9:443".into());
        net.verified_neighbors.lock().insert(relayed_n.clone(), relayed);
        net.register_path(
            relayed,
            relayed_n,
            sim.clone() as InterfaceHandle,
            PathQuality::Relay { hops: 1 },
            false,
        );
        // DIRECT + verified + WAN but over the TCP BRIDGE → the verifier sees our loopback (the bridge
        // tunnels to its 127.0.0.1 QUIC), not our public IP → ineligible (AutoNAT must use UDP).
        let tcp_iface =
            fabric.interface_with_kind("10.0.0.8:1".parse().unwrap(), NatModel::Public, InterfaceKind::QuicOverTcp);
        let tcp_peer = node();
        let tcp_n = NeighborId("198.51.100.50:8443".into());
        net.verified_neighbors.lock().insert(tcp_n.clone(), tcp_peer);
        net.register_path(tcp_peer, tcp_n, tcp_iface as InterfaceHandle, PathQuality::Direct, true);

        assert_eq!(
            net.verified_wan_peer(),
            Some(relay),
            "only a DIRECT + QuicOverUdp + verified + WAN neighbour is a valid AutoNAT verifier (LAN, \
             relayed, and TCP-bridge paths are all excluded)"
        );
    }

    #[test]
    fn reachability_tracker_concludes_unreachable_after_failures() {
        let mut t = ReachabilityTracker::default();
        let cand: SocketAddr = "10.0.0.1:1".parse().unwrap();
        t.issue(1, cand);
        t.issue(2, cand);
        assert_eq!(t.state, ReachabilityState::Unknown);
        t.on_timeout(1);
        assert_eq!(t.state, ReachabilityState::Unknown, "one failure is not enough");
        t.on_timeout(2);
        assert_eq!(t.state, ReachabilityState::Unreachable, "threshold reached");
    }

    #[test]
    fn reachability_probe_beats_later_failures() {
        let mut t = ReachabilityTracker::default();
        let cand: SocketAddr = "10.0.0.1:1".parse().unwrap();
        t.issue(1, cand);
        t.issue(2, cand);
        assert!(t.on_probe(1), "matching nonce confirms reachable");
        assert_eq!(t.state, ReachabilityState::Reachable);
        t.on_timeout(2); // a later failure must NOT downgrade a proven-reachable node
        assert_eq!(t.state, ReachabilityState::Reachable);
        assert!(!t.on_probe(999), "unknown nonce does not match");
    }

    #[test]
    fn advertised_addresses_gate_reflexive_and_strip_wan() {
        let lan: SocketAddr = "192.168.1.5:8443".parse().unwrap();
        let wan_reflexive: SocketAddr = "203.0.113.7:51820".parse().unwrap();

        // Not confirmed reachable → reflexive withheld; the private LAN addr is kept.
        assert_eq!(
            select_advertised_addresses(vec![lan], Some(wan_reflexive), false),
            vec![lan]
        );
        // Confirmed reachable → the reflexive would be added, but address privacy strips it (routable WAN);
        // only the private LAN addr is gossiped.
        assert_eq!(
            select_advertised_addresses(vec![lan], Some(wan_reflexive), true),
            vec![lan]
        );
        // A WAN explicit address (e.g. a relay's public bind addr) is stripped too → empty.
        assert_eq!(
            select_advertised_addresses(vec![wan_reflexive], Some(wan_reflexive), true),
            Vec::<SocketAddr>::new()
        );
        // No reflexive inferred → just the private explicit.
        assert_eq!(
            select_advertised_addresses(vec![lan], None, true),
            vec![lan]
        );
    }

    /// Address-privacy CI invariant: the open gossip plane must NEVER carry a routable
    /// WAN endpoint a hostile mesh member could harvest (via forwarded announces / FoF) and
    /// IP-block. `select_advertised_addresses` builds every signed, hop-forwarded announce, so it is
    /// the choke point — across reachability states and a spread of WAN/CGNAT/private inputs its
    /// output must contain no enumerable address.
    #[test]
    fn announce_never_gossips_a_wan_endpoint() {
        let private: Vec<SocketAddr> = vec![
            "192.168.1.5:8443".parse().unwrap(),
            "10.0.0.9:443".parse().unwrap(),
            "172.16.4.4:443".parse().unwrap(),
            "100.64.1.1:443".parse().unwrap(), // CGNAT — also not enumerable
        ];
        let wan: Vec<SocketAddr> = vec![
            "203.0.113.7:51820".parse().unwrap(),
            "198.51.100.213:443".parse().unwrap(), // a public WAN address
            "[2001:db8::42]:8443".parse().unwrap(),
        ];
        for reachable in [false, true] {
            for w in &wan {
                let mut explicit = private.clone();
                explicit.push(*w);
                let advertised = select_advertised_addresses(explicit, Some(*w), reachable);
                for a in &advertised {
                    assert!(
                        !is_enumerable_wan(a),
                        "gossip announce leaked WAN {a} (reachable={reachable})"
                    );
                }
                assert!(!advertised.contains(w), "WAN {w} must be stripped from the announce");
            }
        }
        // Classifier sanity (the gate the invariant rests on).
        for w in ["203.0.113.7:1", "198.51.100.213:443", "[2001:db8::42]:1"] {
            assert!(is_enumerable_wan(&w.parse().unwrap()), "{w} is WAN");
        }
        for p in ["192.168.1.5:1", "10.0.0.1:1", "172.16.0.1:1", "100.64.0.1:1", "127.0.0.1:1", "[::1]:1", "[fc00::1]:1", "[fe80::1]:1"] {
            assert!(!is_enumerable_wan(&p.parse().unwrap()), "{p} is NOT WAN");
        }
    }

    use crate::cert::respond_to_challenge;
    use crate::interface::Interface;
    use crate::quic_interface::QuicOverUdpInterface;
    use rand_core::OsRng;
    use tokio::time::timeout;

    fn fresh_signing() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    /// Seed the address book as if we'd received and verified `pubkey`'s
    /// announce. Lets QUIC-path tests run hermetically without a live
    /// multicast adapter (discovery is covered by its own tests).
    fn seed_peer(net: &Network, pubkey: VerifyingKey, addresses: Vec<SocketAddr>) {
        let dest = NodeAddress::from_pubkey(&pubkey);
        net.peer_book.lock().insert(
            dest,
            PeerRecord {
                pubkey: Some(pubkey),
                advertised_addresses: addresses,
                capabilities: 0,
                door_port: 0,
                heard_direct: false,
            },
        );
    }

    fn bind_quic(key: &SigningKey) -> Arc<QuicOverUdpInterface> {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        QuicOverUdpInterface::bind(addr, key).expect("quic bind on loopback")
    }

    /// The NeighborId of `net`'s verified connection to `peer` (the connection
    /// over which `peer` proved its identity). Used to seed routing tables in
    /// multi-hop tests with the real per-connection neighbor ids.
    fn neighbor_for(net: &Network, peer: NodeAddress) -> NeighborId {
        net.verified_neighbors
            .lock()
            .iter()
            .find(|(_, addr)| **addr == peer)
            .map(|(n, _)| n.clone())
            .expect("a verified neighbor connection for peer")
    }

    #[test]
    fn network_constructs_with_no_interfaces() {
        let identity = fresh_signing();
        let net = Network::new(identity, vec![]);
        assert_eq!(net.interface_count(), 0);
    }

    #[test]
    fn local_address_derives_from_identity() {
        let identity = fresh_signing();
        let expected = NodeAddress::from_pubkey(&identity.verifying_key());
        let net = Network::new(identity, vec![]);
        assert_eq!(net.local_address(), expected);
    }

    /// Task 3 — a TCP front door is confirmed ONLY by a result whose nonce matches the in-flight
    /// request AND reports reachable; a wrong nonce or an unreachable verdict never confirms (so we
    /// never advertise a phantom door). See docs/decisions/tcp-direct-peer-fallback.md.
    #[test]
    fn tcp_reachability_result_confirms_only_a_matching_reachable_nonce() {
        use crate::protocol::TcpReachabilityResultFrame;
        let net = Network::new(fresh_signing(), vec![]);
        let candidate: SocketAddr = "203.0.113.7:8443".parse().unwrap();

        // Wrong nonce → ignored (pending untouched, nothing confirmed).
        *net.pending_tcp_confirm.lock() = Some((42, candidate));
        net.handle_tcp_reachability_result(TcpReachabilityResultFrame { nonce: 7, reachable: true });
        assert_eq!(net.confirmed_tcp_address(), None);

        // Matching nonce but UNREACHABLE → request cleared, still not confirmed.
        net.handle_tcp_reachability_result(TcpReachabilityResultFrame { nonce: 42, reachable: false });
        assert_eq!(net.confirmed_tcp_address(), None);

        // Matching + reachable → confirmed.
        *net.pending_tcp_confirm.lock() = Some((43, candidate));
        net.handle_tcp_reachability_result(TcpReachabilityResultFrame { nonce: 43, reachable: true });
        assert_eq!(net.confirmed_tcp_address(), Some(candidate));
    }

    #[test]
    fn dial_self_returns_error() {
        let identity = fresh_signing();
        let local_addr = NodeAddress::from_pubkey(&identity.verifying_key());
        let net = Network::new(identity, vec![]);
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(net.dial(local_addr));
        assert!(matches!(result, Err(NetworkError::DialSelf)));
    }

    #[test]
    fn dial_with_no_known_address_returns_error() {
        let identity = fresh_signing();
        let target = NodeAddress::from_pubkey(&fresh_signing().verifying_key());
        let net = Network::new(identity, vec![]);
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(net.dial(target));
        assert!(matches!(result, Err(NetworkError::NoPathToDestination(_))));
    }

    /// End-to-end Phase α-3 test: two Networks each with a QUIC interface.
    /// peer_book seeded directly (hermetic — no multicast adapter needed, so
    /// this runs everywhere including hostless CI). Alice dials Bob → QUIC
    /// connect + **mutual** identity challenge → verified Link. Bytes flow
    /// both ways over the point-to-point QUIC connection, never broadcast.
    #[tokio::test]
    async fn verified_quic_link_exchanges_bytes_both_ways() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let alice_addr = alice_quic.local_addr().unwrap();
        let bob_addr = bob_quic.local_addr().unwrap();

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));

        // Simulate discovery: each side learned the other's signed announce.
        seed_peer(&alice, bob_pub, vec![bob_addr]);
        seed_peer(&bob, alice_pub, vec![alice_addr]);

        alice.start();
        bob.start();

        let alice_to_bob = alice
            .dial(bob.local_address())
            .await
            .expect("dial + mutual challenge должны пройти");
        assert_eq!(alice_to_bob.destination(), bob.local_address());

        alice_to_bob.send(b"hello bob").await.unwrap();

        let bob_link = timeout(Duration::from_secs(5), bob.accept())
            .await
            .expect("bob должен получить verified incoming link")
            .expect("accept не должен вернуть None");
        assert_eq!(bob_link.destination(), alice.local_address());

        let received = timeout(Duration::from_secs(5), bob_link.recv())
            .await
            .expect("recv в течение 5с")
            .expect("recv ok");
        assert_eq!(received, b"hello bob");

        bob_link.send(b"hello alice").await.unwrap();
        let reply = timeout(Duration::from_secs(5), alice_to_bob.recv())
            .await
            .expect("alice получает reply")
            .expect("recv ok");
        assert_eq!(reply, b"hello alice");

        alice.shutdown();
        bob.shutdown();
    }

    /// МЁРТВЫЙ АДРЕС ВПЕРЕДИ ЖИВОГО НЕ ДЕРЖИТ ДОЗВОН (RFC 8305, Happy Eyeballs).
    ///
    /// `dial` раньше ждал QUIC-рукопожатие на каждом адресе по очереди, и мёртвый адрес стоил
    /// полного таймаута рукопожатия, прежде чем пробовался следующий. Живые замеры: 30.1 с на
    /// мёртвом STUN-адресе при поиске по Omni-ID, 130 с на попытку при восстановлении пары
    /// (`a_fresh_beacon_is_dialled_before_the_ports_the_peer_has_already_lost`).
    #[tokio::test]
    async fn a_dead_endpoint_ahead_of_the_live_one_does_not_stall_the_dial() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let alice_addr = alice_quic.local_addr().unwrap();
        let bob_addr = bob_quic.local_addr().unwrap();
        // Сокет, который глотает всё и не отвечает никогда: QUIC Initial туда пропадает так же, как в
        // порт, который NAT пира уже закрыл.
        let black_hole = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dead = black_hole.local_addr().unwrap();

        let alice = Arc::new(Network::new(alice_key, vec![alice_quic as Arc<dyn Interface>]));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&bob, alice_pub, vec![alice_addr]);
        alice.start();
        bob.start();

        // Отрицательная половина: на одном мёртвом адресе дозвон через 3 с всё ещё висит. Значит,
        // яма действительно съедает рукопожатие, и положительная половина проходит не потому, что
        // мёртвый адрес отказывает быстро.
        seed_peer(&alice, bob_pub, vec![dead]);
        assert!(
            timeout(Duration::from_secs(3), alice.dial(bob.local_address()))
                .await
                .is_err(),
            "дозвон на один мёртвый адрес обязан висеть, пока QUIC не сдастся"
        );

        // Мёртвый адрес первым, живой вторым — порядок, в котором их и кладёт свежий маячок.
        seed_peer(&alice, bob_pub, vec![dead, bob_addr]);
        let started = Instant::now();
        let link = timeout(Duration::from_secs(5), alice.dial(bob.local_address()))
            .await
            .expect("живой адрес не обязан ждать таймаута мёртвого")
            .expect("dial + mutual challenge через живой адрес");
        assert_eq!(link.destination(), bob.local_address());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "мёртвый адрес стоит одной задержки попытки, а не рукопожатия: {:?}",
            started.elapsed()
        );
        drop(black_hole);

        alice.shutdown();
        bob.shutdown();
    }

    /// Relay directory stage 2: a searcher holding only `ip:port` (Mainline shelf) dials «whoever you
    /// are», learns the relay's identity from its key and gets its card; a node that is not a relay
    /// tells it nothing and closes at once.
    #[tokio::test]
    async fn a_bare_address_reveals_a_relay_and_its_card_but_not_an_ordinary_node() {
        let (relay_key, plain_key, seeker_key) = (fresh_signing(), fresh_signing(), fresh_signing());
        let relay_node = NodeAddress::from_pubkey(&relay_key.verifying_key());
        let (relay_quic, plain_quic, seeker_quic) =
            (bind_quic(&relay_key), bind_quic(&plain_key), bind_quic(&seeker_key));
        let (relay_addr, plain_addr) = (relay_quic.local_addr().unwrap(), plain_quic.local_addr().unwrap());
        let relay = Arc::new(Network::new(relay_key, vec![relay_quic as Arc<dyn Interface>]));
        let plain = Arc::new(Network::new(plain_key, vec![plain_quic as Arc<dyn Interface>]));
        let seeker = Arc::new(Network::new(seeker_key, vec![seeker_quic as Arc<dyn Interface>]));
        relay.set_relay_card(Some(b"signed-entry".to_vec()));
        relay.start();
        plain.start();
        seeker.start();

        let (who, _link) = seeker.dial_unknown(relay_addr).await.expect("the relay answers «who are you»");
        assert_eq!(who, relay_node, "the identity comes from the key that answered");
        assert_eq!(
            seeker.request_relay_card(who, Duration::from_secs(3)).await.as_deref(),
            Some(b"signed-entry".as_slice())
        );

        let started = Instant::now();
        assert!(seeker.dial_unknown(plain_addr).await.is_err(), "an ordinary node does not answer");
        assert!(started.elapsed() < Duration::from_secs(3), "and says so at once: {:?}", started.elapsed());

        relay.shutdown();
        plain.shutdown();
        seeker.shutdown();
    }

    /// A goes only through its relay R (direct links only with R): a contact C that knows A's
    /// address and is linked to R must still reach A — refused directly, then routed through R by
    /// path discovery, no route seeded by hand (stand 2026-10-14: the contact stayed «searching»).
    #[tokio::test]
    async fn a_contact_reaches_a_relay_only_node_through_its_relay() {
        let (a_key, r_key, c_key) = (fresh_signing(), fresh_signing(), fresh_signing());
        let (a_pub, r_pub, c_pub) = (a_key.verifying_key(), r_key.verifying_key(), c_key.verifying_key());
        let (a_quic, r_quic, c_quic) = (bind_quic(&a_key), bind_quic(&r_key), bind_quic(&c_key));
        let (a_addr, r_addr, c_addr) =
            (a_quic.local_addr().unwrap(), r_quic.local_addr().unwrap(), c_quic.local_addr().unwrap());
        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let r = Arc::new(Network::new(r_key, vec![r_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));
        seed_peer(&a, r_pub, vec![r_addr]);
        seed_peer(&r, a_pub, vec![a_addr]);
        seed_peer(&r, c_pub, vec![c_addr]);
        seed_peer(&c, r_pub, vec![r_addr]);
        seed_peer(&c, a_pub, vec![a_addr]); // C knows A's address from an old beacon
        a.start();
        r.start();
        c.start();
        let (a_node, r_node) = (a.local_address(), r.local_address());
        a.keep_direct_only_with(Some(std::collections::HashSet::from([r_node]))).await;
        a.dial(r_node).await.expect("A links to its relay");
        c.dial(r_node).await.expect("C links to the relay");

        let link = timeout(Duration::from_secs(10), c.dial(a_node))
            .await
            .expect("C's dial to A settles within 10 s")
            .expect("C reaches A");
        drop(link);
        assert!(
            matches!(c.peer_link_quality(&a_node), Some(LinkQuality::Relay { .. })),
            "through the relay, not direct: {:?}",
            c.peer_link_quality(&a_node)
        );
        a.shutdown();
        r.shutdown();
        c.shutdown();
    }

    /// The other way round: A, going only through its relay R, dials a contact C linked to R — by
    /// path discovery through R, no route seeded (stand 2026-10-15: the pinned laptop had «no live
    /// path» to a contact device and could not ring it).
    #[tokio::test]
    async fn a_relay_only_node_reaches_a_contact_through_its_relay() {
        let (a_key, r_key, c_key) = (fresh_signing(), fresh_signing(), fresh_signing());
        let (a_pub, r_pub, c_pub) = (a_key.verifying_key(), r_key.verifying_key(), c_key.verifying_key());
        let (a_quic, r_quic, c_quic) = (bind_quic(&a_key), bind_quic(&r_key), bind_quic(&c_key));
        let (a_addr, r_addr, c_addr) =
            (a_quic.local_addr().unwrap(), r_quic.local_addr().unwrap(), c_quic.local_addr().unwrap());
        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let r = Arc::new(Network::new(r_key, vec![r_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));
        seed_peer(&a, r_pub, vec![r_addr]);
        seed_peer(&a, c_pub, vec![c_addr]); // A knows C's address — but keeps no direct link with it
        seed_peer(&r, a_pub, vec![a_addr]);
        seed_peer(&r, c_pub, vec![c_addr]);
        seed_peer(&c, r_pub, vec![r_addr]);
        a.start();
        r.start();
        c.start();
        let (r_node, c_node) = (r.local_address(), c.local_address());
        a.keep_direct_only_with(Some(std::collections::HashSet::from([r_node]))).await;
        a.dial(r_node).await.expect("A links to its relay");
        c.dial(r_node).await.expect("C links to the relay");

        let link = timeout(Duration::from_secs(10), a.dial(c_node))
            .await
            .expect("A's dial to C settles within 10 s")
            .expect("A reaches C");
        drop(link);
        assert!(matches!(a.peer_link_quality(&c_node), Some(LinkQuality::Relay { .. })));
        a.shutdown();
        r.shutdown();
        c.shutdown();
    }

    /// A relay-only peer pruned after a silent stretch keeps its inbox; its next routed frame must
    /// bring the path back (it used to land in the inbox while the peer stayed unreachable for good —
    /// never probed again: «no live path» to a contact whose frames were arriving, stand 2026-10-15).
    #[tokio::test]
    async fn a_pruned_relay_only_peer_comes_back_with_its_next_frame() {
        let (a_key, r_key, c_key) = (fresh_signing(), fresh_signing(), fresh_signing());
        let (a_pub, r_pub, c_pub) = (a_key.verifying_key(), r_key.verifying_key(), c_key.verifying_key());
        let (a_quic, r_quic, c_quic) = (bind_quic(&a_key), bind_quic(&r_key), bind_quic(&c_key));
        let (a_addr, r_addr, c_addr) =
            (a_quic.local_addr().unwrap(), r_quic.local_addr().unwrap(), c_quic.local_addr().unwrap());
        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let r = Arc::new(Network::new(r_key, vec![r_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));
        seed_peer(&a, r_pub, vec![r_addr]);
        seed_peer(&r, a_pub, vec![a_addr]);
        seed_peer(&r, c_pub, vec![c_addr]);
        seed_peer(&c, r_pub, vec![r_addr]);
        a.start();
        r.start();
        c.start();
        let (r_node, c_node, a_node) = (r.local_address(), c.local_address(), a.local_address());
        a.dial(r_node).await.expect("A links to the relay");
        c.dial(r_node).await.expect("C links to the relay");
        let a_to_c = timeout(Duration::from_secs(10), a.dial(c_node)).await.unwrap().expect("A reaches C via R");
        a_to_c.send(b"first").await.expect("first frame");
        let _ = timeout(Duration::from_secs(5), c.accept()).await.expect("C gets A's link");
        assert!(c.peer_link_quality(&a_node).is_some());

        c.prune_relay_only_peer(a_node);
        assert!(c.peer_link_quality(&a_node).is_none(), "pruned");
        a_to_c.send(b"still here").await.expect("next frame");
        timeout(Duration::from_secs(5), async {
            while c.peer_link_quality(&a_node).is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the next routed frame brings A's path back");
        a.shutdown();
        r.shutdown();
        c.shutdown();
    }

    /// Phase β multi-hop, end-to-end: A — B — C where A has **no** direct link
    /// to C. A dials C (routed via its verified link to B), sends a frame; B
    /// relays it to C; C accepts an **on-demand** incoming Link and receives the
    /// bytes; then C replies back along the reverse path C → B → A. Exercises the
    /// originating half (`dial_multihop`), the relay (`decide_data_forward`), and
    /// the relaxed delivery gate (route by origin identity + on-demand incoming
    /// Link). См. ADR mesh-multihop-identity-delivery.md.
    #[tokio::test]
    async fn multi_hop_data_delivered_a_via_b_to_c() {
        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (a_pub, b_pub, c_pub) = (
            a_key.verifying_key(),
            b_key.verifying_key(),
            c_key.verifying_key(),
        );

        let a_quic = bind_quic(&a_key);
        let b_quic = bind_quic(&b_key);
        let c_quic = bind_quic(&c_key);
        let a_addr = a_quic.local_addr().unwrap();
        let b_addr = b_quic.local_addr().unwrap();
        let c_addr = c_quic.local_addr().unwrap();
        let a_iface_id = a_quic.id();
        let b_iface_id = b_quic.id();

        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let b = Arc::new(Network::new(b_key, vec![b_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));

        // Discovery seeded for the *adjacent* hops only: A↔B and B↔C. A never
        // learns C's address — it can only reach C multi-hop.
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        seed_peer(&b, c_pub, vec![c_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);

        a.start();
        b.start();
        c.start();

        // Bring up the two direct links; accept on the responder side so both
        // ends are mutually verified before we read their neighbor ids.
        let _a_to_b = a.dial(b.local_address()).await.expect("A dials B");
        let _b_from_a = timeout(Duration::from_secs(5), b.accept())
            .await
            .expect("B accepts A within 5s")
            .expect("accept not None");
        let _b_to_c = b.dial(c.local_address()).await.expect("B dials C");
        let _c_from_b = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts B within 5s")
            .expect("accept not None");

        let a_node = a.local_address();
        let c_node = c.local_address();

        // Seed routing as multi-hop announce propagation eventually would:
        // A→C (cost 2) via its B-neighbor; B→C (cost 1) direct; B→A (cost 1)
        // direct (the reverse path so C's reply can route back).
        let now = Instant::now();
        let ttl = Duration::from_secs(300);
        a.path_table().update(
            c_node,
            a_iface_id.clone(),
            neighbor_for(&a, b.local_address()),
            2,
            ttl,
            now,
        );
        b.path_table().update(
            c_node,
            b_iface_id.clone(),
            neighbor_for(&b, c.local_address()),
            1,
            ttl,
            now,
        );
        b.path_table().update(
            a_node,
            b_iface_id.clone(),
            neighbor_for(&b, a.local_address()),
            1,
            ttl,
            now,
        );

        // A dials C — no direct address, so this resolves the multi-hop route
        // over the verified B-neighbor without challenging C.
        let a_to_c = a
            .dial(c.local_address())
            .await
            .expect("A dials C over multi-hop route");
        assert_eq!(a_to_c.destination(), c.local_address());

        a_to_c.send(b"hi C, from A via B").await.unwrap();

        // C accepts an on-demand incoming Link for origin A and receives bytes.
        let c_from_a = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts on-demand multi-hop link within 5s")
            .expect("accept not None");
        assert_eq!(c_from_a.destination(), a_node);
        let got = timeout(Duration::from_secs(5), c_from_a.recv())
            .await
            .expect("recv within 5s")
            .expect("recv ok");
        assert_eq!(got, b"hi C, from A via B");

        // Reply along the reverse path C → B → A.
        c_from_a.send(b"ack from C").await.unwrap();
        let reply = timeout(Duration::from_secs(5), a_to_c.recv())
            .await
            .expect("A receives reply within 5s")
            .expect("recv ok");
        assert_eq!(reply, b"ack from C");

        // R2: the same multi-hop path carries MEDIA too. A sends a datagram to C; B relays it (the
        // datagram Forward branch, mirroring decide_data_forward); C delivers it to the routed Link
        // by the envelope's `from` (its datagram inbox is now wired in register_routed_link_channels,
        // which dropped the sender before R2.3). Loopback QUIC datagram loss is ~nil, so one send.
        let media = vec![0x5Au8; 160];
        a_to_c.send_datagram(&media).await.unwrap();
        let got_media = timeout(Duration::from_secs(5), c_from_a.recv_datagram())
            .await
            .expect("C receives relayed media datagram within 5s")
            .expect("recv_datagram ok");
        assert_eq!(got_media, media, "relayed media bytes preserved over A→B→C");

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// R3 reactive discovery, end-to-end: A — B — C where A has **no** seeded route to C (unlike
    /// `multi_hop_data_delivered_a_via_b_to_c`, which hand-seeds the path_tables). A dials C →
    /// `discover_path` floods a PathRequest → B relays it → C (the target) replies → the reply routes
    /// back, filling A's route → `dial_multihop` reaches C and a frame is delivered. This is the proof
    /// that "one connected peer → reach the whole mesh" works ORGANICALLY (announce is single-hop, so
    /// the path_table never learns cost≥2 routes on its own). See docs/decisions/omni-network-model.md.
    #[tokio::test]
    async fn reactive_path_discovery_reaches_c_with_no_seeded_route() {
        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (a_pub, b_pub, c_pub) = (
            a_key.verifying_key(),
            b_key.verifying_key(),
            c_key.verifying_key(),
        );

        let a_quic = bind_quic(&a_key);
        let b_quic = bind_quic(&b_key);
        let c_quic = bind_quic(&c_key);
        let a_addr = a_quic.local_addr().unwrap();
        let b_addr = b_quic.local_addr().unwrap();
        let c_addr = c_quic.local_addr().unwrap();

        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let b = Arc::new(Network::new(b_key, vec![b_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));

        // Adjacent hops only: A↔B and B↔C. A never learns C's address.
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        seed_peer(&b, c_pub, vec![c_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);

        a.start();
        b.start();
        c.start();

        // Bring up the two direct links (mutually verified).
        let _a_to_b = a.dial(b.local_address()).await.expect("A dials B");
        let _b_from_a = timeout(Duration::from_secs(5), b.accept())
            .await
            .expect("B accepts A within 5s")
            .expect("accept not None");
        let _b_to_c = b.dial(c.local_address()).await.expect("B dials C");
        let _c_from_b = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts B within 5s")
            .expect("accept not None");

        // NO path_table seeding — A must DISCOVER the route to C reactively.
        assert!(
            a.path_table().lookup(&c.local_address(), Instant::now()).is_none(),
            "precondition: A has no seeded route to C"
        );

        // A dials C: no direct address + no known route → flood PathRequest → discover via B →
        // dial_multihop over the learned route.
        let a_to_c = timeout(Duration::from_secs(8), a.dial(c.local_address()))
            .await
            .expect("dial completes within 8s")
            .expect("A reaches C purely via reactive path discovery");
        assert_eq!(a_to_c.destination(), c.local_address());

        a_to_c.send(b"hi C, discovered via B").await.unwrap();
        let c_from_a = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts the on-demand multi-hop link within 5s")
            .expect("accept not None");
        let got = timeout(Duration::from_secs(5), c_from_a.recv())
            .await
            .expect("recv within 5s")
            .expect("recv ok");
        assert_eq!(got, b"hi C, discovered via B");

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// R2 durable relay session: a relay must carry the WHOLE omni-session like a
    /// direct link — reliable chat AND the media-datagram (audio/video) plane — and
    /// keep carrying BOTH long after the pairing burst, not just while the R3 route is
    /// fresh. Topology A—B(relay)—C (A and C never link directly). After establishing
    /// the relayed Link we let real time pass beyond the liveness dead-window
    /// (`LIVENESS_DEAD_AFTER`), so the "one-shot path, not a maintained link" decay
    /// (docs/bugs/relay-pairing-approval-gap.md) would surface as a dropped second
    /// frame / datagram. Guards the invariant "relay carries the whole session like a
    /// direct link" (CONTRIBUTING.md). ~22 s real-time (liveness ticks are wall-clock).
    #[tokio::test]
    async fn relay_carries_whole_session_durably_past_liveness_window() {
        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (a_pub, b_pub, c_pub) = (
            a_key.verifying_key(),
            b_key.verifying_key(),
            c_key.verifying_key(),
        );

        let a_quic = bind_quic(&a_key);
        let b_quic = bind_quic(&b_key);
        let c_quic = bind_quic(&c_key);
        let a_addr = a_quic.local_addr().unwrap();
        let b_addr = b_quic.local_addr().unwrap();
        let c_addr = c_quic.local_addr().unwrap();

        let a = Arc::new(Network::new(a_key, vec![a_quic as Arc<dyn Interface>]));
        let b = Arc::new(Network::new(b_key, vec![b_quic as Arc<dyn Interface>]));
        let c = Arc::new(Network::new(c_key, vec![c_quic as Arc<dyn Interface>]));

        // Adjacent hops only: A↔B and B↔C. A never learns C's address directly.
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&b, a_pub, vec![a_addr]);
        seed_peer(&b, c_pub, vec![c_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);

        a.start();
        b.start();
        c.start();

        // Bring up the two direct links (mutually verified), then reactively discover
        // the route A→C through B and open the relayed Link.
        let _a_to_b = a.dial(b.local_address()).await.expect("A dials B");
        let _b_from_a = timeout(Duration::from_secs(5), b.accept())
            .await
            .expect("B accepts A within 5s")
            .expect("accept not None");
        let _b_to_c = b.dial(c.local_address()).await.expect("B dials C");
        let _c_from_b = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts B within 5s")
            .expect("accept not None");

        let a_to_c = timeout(Duration::from_secs(8), a.dial(c.local_address()))
            .await
            .expect("dial completes within 8s")
            .expect("A reaches C via reactive path discovery");

        // Sanity: the relay carries the first reliable frame.
        a_to_c.send(b"chat #1 (fresh route)").await.unwrap();
        let c_from_a = timeout(Duration::from_secs(5), c.accept())
            .await
            .expect("C accepts the relayed link within 5s")
            .expect("accept not None");
        let got1 = timeout(Duration::from_secs(5), c_from_a.recv())
            .await
            .expect("recv #1 within 5s")
            .expect("recv ok");
        assert_eq!(got1, b"chat #1 (fresh route)");

        // Let real time pass beyond the liveness dead-window. A self-maintaining relay
        // link keeps A↔B and B↔C alive (ping/pong) and B's route to C fresh, so the
        // session must still flow. A "one-shot path" would have decayed by now.
        tokio::time::sleep(LIVENESS_DEAD_AFTER + Duration::from_secs(5)).await;

        // The relay still believes it can reach C (route maintained, not expired).
        assert!(
            b.path_table()
                .lookup(&c.local_address(), Instant::now())
                .is_some(),
            "relay B must still hold a route to C past the liveness window"
        );

        // Reliable chat STILL rides the relay.
        a_to_c.send(b"chat #2 (after idle)").await.unwrap();
        let got2 = timeout(Duration::from_secs(5), c_from_a.recv())
            .await
            .expect("recv #2 within 5s — chat must still ride the relay after the idle window")
            .expect("recv ok");
        assert_eq!(got2, b"chat #2 (after idle)");

        // The media-datagram plane (audio/video) ALSO still rides the relay.
        a_to_c
            .send_datagram(b"media after idle")
            .await
            .unwrap();
        let dg = timeout(Duration::from_secs(5), c_from_a.recv_datagram())
            .await
            .expect("recv datagram within 5s — media must ride the relay like a direct link")
            .expect("recv_datagram ok");
        assert_eq!(dg, b"media after idle");

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// P1 reflexive discovery over real QUIC: Alice dials Bob; Bob (the acceptor)
    /// reports back the source address it saw Alice connect from; Alice records
    /// it as a reflexive observation. (Two nodes give one observer each, so NAT
    /// class stays Unknown — cone-vs-symmetric needs the multi-node net-sim; here
    /// we prove the observe→report→record wire mechanism.) Review §4.1A.
    #[tokio::test]
    async fn acceptor_reports_reflexive_address_to_dialer() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let alice_addr = alice_quic.local_addr().unwrap();
        let bob_addr = bob_quic.local_addr().unwrap();

        let alice = Arc::new(Network::new(alice_key, vec![alice_quic as Arc<dyn Interface>]));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));

        seed_peer(&alice, bob_pub, vec![bob_addr]);
        seed_peer(&bob, alice_pub, vec![alice_addr]);

        alice.start();
        bob.start();

        let _link = alice
            .dial(bob.local_address())
            .await
            .expect("dial + mutual challenge");

        // Bob reports its observation after it verifies Alice (just after Alice's
        // dial returns) — poll until Alice has recorded it.
        let observed = timeout(Duration::from_secs(5), async {
            loop {
                let got = { alice.reflexive_observations.lock().values().next().copied() };
                if let Some(addr) = got {
                    break addr;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("alice should receive an ObservedAddress within 5s");

        // It's Alice's own source address as Bob saw it — loopback in this test.
        assert!(
            observed.ip().is_loopback(),
            "expected loopback observed addr, got {observed}"
        );

        alice.shutdown();
        bob.shutdown();
    }

    /// Net-sim harness smoke: two real Networks pair + exchange bytes over the
    /// in-process `SimFabric` (no sockets). Proves the sim is a valid Interface
    /// for the full dial + mutual-challenge + accept + data path — the foundation
    /// for deterministic NAT/relay/partition gates (P1/P2). См. external review §5.
    #[tokio::test]
    async fn pairs_and_exchanges_bytes_over_sim_fabric() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let alice_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let bob_addr: SocketAddr = "10.0.0.2:1".parse().unwrap();

        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice = Arc::new(Network::new(
            alice_key,
            vec![fabric.interface(alice_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(
            bob_key,
            vec![fabric.interface(bob_addr, NatModel::Public) as Arc<dyn Interface>],
        ));

        seed_peer(&alice, bob_pub, vec![bob_addr]);
        seed_peer(&bob, alice_pub, vec![alice_addr]);

        alice.start();
        bob.start();

        let a2b = alice
            .dial(bob.local_address())
            .await
            .expect("dial + mutual challenge over sim fabric");
        a2b.send(b"hi over sim").await.unwrap();

        let blink = timeout(Duration::from_secs(5), bob.accept())
            .await
            .expect("bob accepts within 5s")
            .expect("link");
        let got = timeout(Duration::from_secs(5), blink.recv())
            .await
            .expect("recv within 5s")
            .expect("recv ok");
        assert_eq!(got, b"hi over sim");

        alice.shutdown();
        bob.shutdown();
    }

    /// Drive a node behind `nat` dialing two public observers over the sim
    /// fabric; return the NAT class it infers from their reflexive reports. This
    /// is the P1 gate: end-to-end cone-vs-symmetric detection (review §4.1A).
    async fn observed_nat_class(nat: crate::sim::NatModel) -> NatClass {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let n_addr: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let m1_addr: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let m2_addr: SocketAddr = "20.0.0.2:1".parse().unwrap();

        let n_key = fresh_signing();
        let m1_key = fresh_signing();
        let m2_key = fresh_signing();
        let m1_pub = m1_key.verifying_key();
        let m2_pub = m2_key.verifying_key();

        let n = Arc::new(Network::new(
            n_key,
            vec![fabric.interface(n_addr, nat) as Arc<dyn Interface>],
        ));
        let m1 = Arc::new(Network::new(
            m1_key,
            vec![fabric.interface(m1_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let m2 = Arc::new(Network::new(
            m2_key,
            vec![fabric.interface(m2_addr, NatModel::Public) as Arc<dyn Interface>],
        ));

        // N knows the two public observers; it dials each (it's the NAT'd party,
        // so each acceptor sees N's mapped source and reports it back).
        seed_peer(&n, m1_pub, vec![m1_addr]);
        seed_peer(&n, m2_pub, vec![m2_addr]);

        n.start();
        m1.start();
        m2.start();

        n.dial(m1.local_address()).await.expect("dial m1");
        n.dial(m2.local_address()).await.expect("dial m2");

        timeout(Duration::from_secs(5), async {
            loop {
                if n.reflexive_observations.lock().len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("two reflexive observations within 5s");

        let class = infer_reflexive(&n.reflexive_observations.lock()).class;
        n.shutdown();
        m1.shutdown();
        m2.shutdown();
        class
    }

    #[tokio::test]
    async fn net_sim_detects_cone_as_endpoint_independent() {
        let external: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let class = observed_nat_class(crate::sim::NatModel::Cone { external }).await;
        assert_eq!(class, NatClass::EndpointIndependent);
    }

    #[tokio::test]
    async fn net_sim_detects_symmetric_as_address_dependent() {
        let class = observed_nat_class(crate::sim::NatModel::Symmetric {
            external_ip: "203.0.113.9".parse().unwrap(),
            base_port: 40000,
        })
        .await;
        assert_eq!(class, NatClass::AddressDependent);
    }

    /// Net-sim NAT filtering (groundwork for hole-punching): a filtering NAT
    /// drops UNSOLICITED inbound, but accepts a source once it has sent to it
    /// (the pinhole) — exactly the property hole-punching arranges. Review §4.1.
    #[tokio::test]
    async fn sim_nat_filtering_drops_unsolicited_then_allows_after_pinhole() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let p_addr: SocketAddr = "1.1.1.1:1".parse().unwrap();
        let n_addr: SocketAddr = "2.2.2.2:1".parse().unwrap();
        let p = fabric.interface(p_addr, NatModel::Public);
        let n = fabric.interface(
            n_addr,
            NatModel::Cone {
                external: "9.9.9.9:5".parse().unwrap(),
            },
        );
        let p_nb = NeighborId(p_addr.to_string());
        let n_nb = NeighborId(n_addr.to_string());

        // Unsolicited: P → N is filtered (N has never sent to P).
        p.send_frame(&n_nb, b"unsolicited").await.unwrap();
        assert!(
            timeout(Duration::from_millis(200), n.recv_frame())
                .await
                .is_err(),
            "unsolicited inbound to a filtering NAT must be dropped"
        );

        // N sends to P — opens N's pinhole toward P (public P receives it).
        n.send_frame(&p_nb, b"open").await.unwrap();
        let _ = timeout(Duration::from_secs(1), p.recv_frame())
            .await
            .expect("public P receives N's frame");

        // Now P → N is solicited and gets through.
        p.send_frame(&n_nb, b"now allowed").await.unwrap();
        let (_, got) = timeout(Duration::from_secs(1), n.recv_frame())
            .await
            .expect("N now accepts P (pinhole open)")
            .unwrap();
        assert_eq!(got, b"now allowed");
    }

    /// D2: the hole-punch dialer is elected deterministically by address order — lower dials,
    /// higher listens — and both sides agree, so a symmetric coordination never double-dials.
    #[test]
    fn decide_punch_role_lower_address_dials() {
        let low = NodeAddress::from_bytes([1u8; 20]);
        let high = NodeAddress::from_bytes([2u8; 20]);
        assert_eq!(decide_punch_role(&low, &high), PunchRole::Dialer);
        assert_eq!(decide_punch_role(&high, &low), PunchRole::Listener);
        // Both peers derive complementary roles from the SAME pair — never both dialers.
        assert_ne!(decide_punch_role(&low, &high), decide_punch_role(&high, &low));
        // A tie defaults to Listener — never a self-dial.
        assert_eq!(decide_punch_role(&low, &low), PunchRole::Listener);
        // Ordering is over the full 20 bytes (a mid-array difference still decides).
        let (mut a, mut b) = ([5u8; 20], [5u8; 20]);
        a[10] = 3;
        b[10] = 4;
        assert_eq!(
            decide_punch_role(&NodeAddress::from_bytes(a), &NodeAddress::from_bytes(b)),
            PunchRole::Dialer
        );
    }

    /// P2 hole-punch end-to-end (the vision's direct-without-public-IP): A and C are
    /// both behind a filtering Cone NAT, both linked to a public relay B but never
    /// to each other. `a.request_punch(C)` makes B coordinate; both sides probe each
    /// other's reflexive endpoint, opening pinholes; the direct dial A→C then passes
    /// C's NAT filter and yields a direct verified Link. (Without the punch the same
    /// dial is filtered — see sim_nat_filtering_*.) Gated entirely on one machine via
    /// the net-sim. См. docs/decisions/mesh-hole-punching.md.
    #[tokio::test]
    async fn punch_opens_pinholes_enabling_direct_dial_between_nats() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_int: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let c_int: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let a_ext: SocketAddr = "1.1.1.1:7".parse().unwrap();
        let c_ext: SocketAddr = "2.2.2.2:9".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (b_pub, c_pub) = (b_key.verifying_key(), c_key.verifying_key());

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_int, NatModel::Cone { external: a_ext }) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c = Arc::new(Network::new(
            c_key,
            vec![fabric.interface(c_int, NatModel::Cone { external: c_ext }) as Arc<dyn Interface>],
        ));
        let c_node = c.local_address();

        // A and C both link to the public relay B.
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);
        // A knows C's reflexive endpoint (as it would from C's relayed announce); the
        // point under test is that the dial only succeeds once punch opens the pinhole.
        seed_peer(&a, c_pub, vec![c_ext]);

        a.start();
        b.start();
        c.start();
        a.dial(b.local_address()).await.expect("A dials relay B");
        c.dial(b.local_address()).await.expect("C dials relay B");

        // Coordinate punch (opens pinholes) then dial direct; retry to absorb the
        // async ordering of the probe vs the dial.
        let linked = timeout(Duration::from_secs(8), async {
            loop {
                a.request_punch(c_node).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
                if let Ok(Ok(link)) = timeout(Duration::from_millis(400), a.dial(c_node)).await {
                    break link;
                }
            }
        })
        .await
        .expect("direct A→C dial should succeed once punch opens the pinholes");
        assert_eq!(linked.destination(), c_node);

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// The RUNTIME auto-upgrade (not a hand-placed `request_punch`): A reaches C only over a
    /// relay (a `Relay` path via B). `upgrade_relay_links_via_punch()` — what the mesh-runtime
    /// calls on the reconcile timer — must coordinate the punch + direct-dial on its own and
    /// leave A with a DIRECT path to C. This is the wiring that takes hole-punch out of
    /// tests-only and into the real cross-NAT path. См. docs/decisions/mesh-hole-punching.md.
    #[tokio::test]
    async fn auto_punch_upgrades_relay_only_peer_to_direct() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_int: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let c_int: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let a_ext: SocketAddr = "1.1.1.1:7".parse().unwrap();
        let c_ext: SocketAddr = "2.2.2.2:9".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (b_pub, c_pub) = (b_key.verifying_key(), c_key.verifying_key());

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_int, NatModel::Cone { external: a_ext }) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c = Arc::new(Network::new(
            c_key,
            vec![fabric.interface(c_int, NatModel::Cone { external: c_ext }) as Arc<dyn Interface>],
        ));
        let b_node = b.local_address();
        let c_node = c.local_address();

        // A and C both link the public relay B; A knows C's reflexive endpoint (relayed announce).
        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);
        seed_peer(&a, c_pub, vec![c_ext]);
        a.start();
        b.start();
        c.start();
        a.dial(b_node).await.expect("A dials relay B");
        c.dial(b_node).await.expect("C dials relay B");

        // Give A a RELAY-ONLY path to C via B (what routing-to-C-via-B produces), so C is in
        // the relay-only set the auto-upgrade scans. (Path registration is tested elsewhere;
        // here we isolate the upgrade.)
        let (b_neighbor, b_iface) = {
            let map = a.peer_paths.lock();
            let best = map.get(&b_node).unwrap().lock().best().unwrap();
            (best.neighbor, best.iface)
        };
        a.register_path(c_node, b_neighbor, b_iface, PathQuality::Relay { hops: 1 }, false);

        // The auto-upgrade (runtime calls this on a timer) must punch + dial C to direct.
        let upgraded = timeout(Duration::from_secs(8), async {
            loop {
                a.upgrade_relay_links_via_punch().await;
                let direct = a
                    .peer_paths
                    .lock()
                    .get(&c_node)
                    .and_then(|set| set.lock().best())
                    .map(|p| matches!(p.quality, PathQuality::Direct))
                    .unwrap_or(false);
                if direct {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
        assert!(
            upgraded.is_ok(),
            "auto-punch should upgrade the relay-only peer C to a DIRECT path"
        );

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// «Только через релей» / a pinned relay: direct links are kept only with the relays. The direct
    /// link already up goes at once, a new direct dial is refused when it verifies, and lifting the
    /// rule lets it through again (stand 2026-10-14: a pinned laptop kept its direct link to a contact).
    #[tokio::test]
    async fn direct_links_are_kept_only_with_the_relays() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_addr: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "20.0.0.2:1".parse().unwrap();
        let (a_key, b_key) = (fresh_signing(), fresh_signing());
        let b_pub = b_key.verifying_key();
        let a = Arc::new(Network::new(a_key, vec![fabric.interface(a_addr, NatModel::Public) as Arc<dyn Interface>]));
        let b = Arc::new(Network::new(b_key, vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>]));
        let b_node = b.local_address();
        seed_peer(&a, b_pub, vec![b_addr]);
        a.start();
        b.start();
        a.dial(b_node).await.expect("A dials B directly");
        let a_holds_b = |a: &Network| a.verified_neighbors.lock().values().any(|peer| *peer == b_node);
        assert!(a_holds_b(&a));

        a.keep_direct_only_with(Some(std::collections::HashSet::new())).await;
        assert!(!a_holds_b(&a), "the direct link to a non-relay goes at once");
        assert!(a.dial(b_node).await.is_err(), "a new direct link is refused");
        assert!(!a_holds_b(&a));
        // A dialer is refused before A proves itself: it never sees the link up, even for a moment
        // (it used to, and kept redialing every few seconds instead of moving onto the relay).
        // (A fresh C: the sim does not really close B's old connection to A.)
        let c_addr: SocketAddr = "20.0.0.3:1".parse().unwrap();
        let c = Arc::new(Network::new(fresh_signing(), vec![fabric.interface(c_addr, NatModel::Public) as Arc<dyn Interface>]));
        c.start();
        let a_node = a.local_address();
        seed_peer(&c, a.identity.verifying_key(), vec![a_addr]);
        assert!(
            timeout(Duration::from_secs(3), c.dial(a_node)).await.map_or(true, |dial| dial.is_err()),
            "the dialer is refused"
        );
        assert!(!c.verified_neighbors.lock().values().any(|peer| *peer == a_node), "C never verified A");
        c.shutdown();

        a.keep_direct_only_with(Some(std::collections::HashSet::from([b_node]))).await;
        a.dial(b_node).await.expect("a relay keeps its direct link");
        assert!(a_holds_b(&a));

        a.shutdown();
        b.shutdown();
    }

    /// RelayOnly privacy (follow-up d): with the master direct-upgrade gate OFF
    /// (`set_allow_direct_punch(false)`), the runtime auto-upgrade must NOT climb a relay-only peer to
    /// direct — the data path stays relay-mediated so the peer never learns our IP. Same topology as
    /// `auto_punch_upgrades_relay_only_peer_to_direct` (which DOES upgrade), inverted by the gate: C
    /// must REMAIN relay-only across repeated sweeps. См. docs/decisions/connection-quality-ladder.md.
    #[tokio::test]
    async fn relay_only_gate_suppresses_the_relay_to_direct_punch_upgrade() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let a_int: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b_addr: SocketAddr = "20.0.0.1:1".parse().unwrap();
        let c_int: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let a_ext: SocketAddr = "1.1.1.1:7".parse().unwrap();
        let c_ext: SocketAddr = "2.2.2.2:9".parse().unwrap();

        let a_key = fresh_signing();
        let b_key = fresh_signing();
        let c_key = fresh_signing();
        let (b_pub, c_pub) = (b_key.verifying_key(), c_key.verifying_key());

        let a = Arc::new(Network::new(
            a_key,
            vec![fabric.interface(a_int, NatModel::Cone { external: a_ext }) as Arc<dyn Interface>],
        ));
        let b = Arc::new(Network::new(
            b_key,
            vec![fabric.interface(b_addr, NatModel::Public) as Arc<dyn Interface>],
        ));
        let c = Arc::new(Network::new(
            c_key,
            vec![fabric.interface(c_int, NatModel::Cone { external: c_ext }) as Arc<dyn Interface>],
        ));
        let b_node = b.local_address();
        let c_node = c.local_address();

        seed_peer(&a, b_pub, vec![b_addr]);
        seed_peer(&c, b_pub, vec![b_addr]);
        seed_peer(&a, c_pub, vec![c_ext]);
        a.start();
        b.start();
        c.start();
        a.dial(b_node).await.expect("A dials relay B");
        c.dial(b_node).await.expect("C dials relay B");

        let (b_neighbor, b_iface) = {
            let map = a.peer_paths.lock();
            let best = map.get(&b_node).unwrap().lock().best().unwrap();
            (best.neighbor, best.iface)
        };
        a.register_path(c_node, b_neighbor, b_iface, PathQuality::Relay { hops: 1 }, false);

        // RelayOnly: forbid the direct upgrade. The sweep that upgraded C above must now be a no-op.
        a.set_allow_direct_punch(false);
        assert!(!a.allows_direct_punch(), "gate reads back off");
        for _ in 0..5 {
            a.upgrade_relay_links_via_punch().await;
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let still_relay_only = a
            .peer_paths
            .lock()
            .get(&c_node)
            .and_then(|set| set.lock().best())
            .map(|p| !matches!(p.quality, PathQuality::Direct))
            .unwrap_or(false);
        assert!(
            still_relay_only,
            "RelayOnly (allow_direct_punch=false) must keep C relay-only — no direct-upgrade"
        );

        a.shutdown();
        b.shutdown();
        c.shutdown();
    }

    /// Media plane over a verified mesh Link: after the mutual handshake,
    /// unreliable datagrams (opus/RTP-sized) flow both directions over the
    /// same QUIC connection. This is what the audio path will ride.
    #[tokio::test]
    async fn verified_quic_link_carries_media_datagrams() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let alice_addr = alice_quic.local_addr().unwrap();
        let bob_addr = bob_quic.local_addr().unwrap();

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&alice, bob_pub, vec![bob_addr]);
        seed_peer(&bob, alice_pub, vec![alice_addr]);
        alice.start();
        bob.start();

        let alice_to_bob = alice
            .dial(bob.local_address())
            .await
            .expect("dial должен пройти");
        let bob_link = timeout(Duration::from_secs(5), bob.accept())
            .await
            .expect("bob incoming link")
            .expect("accept not None");

        // Alice → Bob: an opus-sized media datagram.
        let frame = vec![0x5Au8; 160];
        alice_to_bob.send_datagram(&frame).await.unwrap();
        let got = timeout(Duration::from_secs(5), bob_link.recv_datagram())
            .await
            .expect("bob receives media datagram within 5s")
            .expect("recv_datagram ok");
        assert_eq!(got, frame);

        // Bob → Alice reply on the same connection.
        bob_link.send_datagram(b"media-pong").await.unwrap();
        let reply = timeout(Duration::from_secs(5), alice_to_bob.recv_datagram())
            .await
            .expect("alice receives media reply within 5s")
            .expect("recv_datagram ok");
        assert_eq!(reply, b"media-pong");

        alice.shutdown();
        bob.shutdown();
    }

    /// A UDP connection that was MIGRATED onto the door (Meta-B failover) comes back to UDP through
    /// the upgrade. Dialing its UDP address returns the same, door-carried connection, so the old
    /// upgrade "succeeded" over the door and left it there forever (live 2026-10-06).
    #[tokio::test]
    async fn migrated_door_connection_comes_back_to_udp() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let bob_udp = bob_quic.local_addr().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = crate::tcp_transport::run_tcp_bridge_on(listener, bob_udp).await;
        });

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&alice, bob_pub, vec![bob_udp]);
        seed_peer(&bob, alice_pub, Vec::new());
        alice.start();
        bob.start();

        let link = timeout(Duration::from_secs(15), alice.dial(bob.local_address()))
            .await
            .expect("udp dial completes")
            .expect("udp dial succeeds");
        assert!(alice
            .route_peer_over_door(&bob.local_address(), door)
            .await
            .expect("route over door"));
        assert!(alice.peer_on_door(&bob.local_address()), "migrated onto the door");

        let upgraded = timeout(
            Duration::from_secs(15),
            alice.upgrade_peer_to_udp(bob.local_address(), bob_udp),
        )
        .await
        .expect("upgrade finishes");
        assert!(upgraded, "UDP works, so the connection must come back to it");
        assert!(
            !alice.peer_on_door(&bob.local_address()),
            "back on UDP, not merely re-proven over the door"
        );

        let bob_link = timeout(Duration::from_secs(10), bob.accept())
            .await
            .expect("bob accepts")
            .expect("accept not None");
        link.send(b"back-on-udp").await.unwrap();
        let got = timeout(Duration::from_secs(10), bob_link.recv())
            .await
            .expect("bob receives after migrating back")
            .expect("recv ok");
        assert_eq!(got, b"back-on-udp");

        alice.shutdown();
        bob.shutdown();
    }

    /// A live UDP link moved onto the peer's door keeps carrying frames, and so it does when moved
    /// back onto UDP — also when a simultaneous dial left two connections between the pair, as on
    /// the stand. `dual` binds both sockets
    /// dual-stack `[::]` like the product does.
    async fn door_move_carries_frames(dual: bool, both_dial: bool) {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();
        let bind = |key: &SigningKey| {
            let addr: SocketAddr = if dual { "[::]:0".parse().unwrap() } else { "127.0.0.1:0".parse().unwrap() };
            QuicOverUdpInterface::bind(addr, key).expect("bind")
        };
        let alice_quic = bind(&alice_key);
        let bob_quic = bind(&bob_key);
        let lo = |port: u16| -> SocketAddr { format!("127.0.0.1:{port}").parse().unwrap() };
        let alice_udp = lo(alice_quic.local_addr().unwrap().port());
        let bob_udp = lo(bob_quic.local_addr().unwrap().port());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        let bridge_target = bob_quic.local_addr().unwrap();
        let bridge_target = if dual { lo(bridge_target.port()) } else { bridge_target };
        tokio::spawn(async move {
            let _ = crate::tcp_transport::run_tcp_bridge_on(listener, bridge_target).await;
        });
        let alice = Arc::new(Network::new(alice_key, vec![alice_quic as Arc<dyn Interface>]));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&alice, bob_pub, vec![bob_udp]);
        seed_peer(&bob, alice_pub, vec![alice_udp]);
        alice.start();
        bob.start();
        let (a_link, b_link) = if both_dial {
            let (a, b) = tokio::join!(alice.dial(bob.local_address()), bob.dial(alice.local_address()));
            (a.unwrap(), b.ok())
        } else {
            (timeout(Duration::from_secs(15), alice.dial(bob.local_address())).await.unwrap().unwrap(), None)
        };
        let b_in = match b_link {
            Some(link) => link,
            None => timeout(Duration::from_secs(10), bob.accept()).await.unwrap().unwrap(),
        };
        a_link.send(b"warm").await.unwrap();
        assert_eq!(timeout(Duration::from_secs(5), b_in.recv()).await.unwrap().unwrap(), b"warm");
        assert!(alice.route_peer_over_door(&bob.local_address(), door).await.unwrap());
        let mut got = 0;
        for i in 0..20u8 {
            let _ = a_link.send(&[i]).await;
            if let Ok(Ok(_)) = timeout(Duration::from_millis(500), b_in.recv()).await {
                got += 1;
            }
        }
        assert!(got >= 18, "dual={dual} both_dial={both_dial}: only {got}/20 frames crossed after the move");

        // And back: UDP works, so the sweep's upgrade migrates the same connection back onto it.
        let upgraded = timeout(Duration::from_secs(15), alice.upgrade_peer_to_udp(bob.local_address(), bob_udp))
            .await
            .expect("the upgrade finishes");
        assert!(upgraded, "dual={dual} both_dial={both_dial}: back onto UDP");
        assert!(!alice.peer_on_door(&bob.local_address()));
        let mut back = 0;
        for i in 0..20u8 {
            let _ = a_link.send(&[i]).await;
            if let Ok(Ok(_)) = timeout(Duration::from_millis(500), b_in.recv()).await {
                back += 1;
            }
        }
        alice.shutdown();
        bob.shutdown();
        assert!(back >= 18, "dual={dual} both_dial={both_dial}: only {back}/20 frames crossed back on UDP");
    }

    #[tokio::test]
    async fn a_link_moved_onto_the_door_keeps_carrying_frames() {
        door_move_carries_frames(false, false).await;
        door_move_carries_frames(true, false).await;
    }

    /// Which of the two connections carries the link depends on the order of the two node
    /// addresses, so several fresh pairs cover both: before the fix 5 of 6 runs lost every frame.
    #[tokio::test]
    async fn a_link_moved_onto_the_door_keeps_carrying_frames_with_two_connections() {
        for dual in [false, true, false, true] {
            door_move_carries_frames(dual, true).await;
        }
    }

    /// FIRST CONTACT over a TCP front door, with NO UDP path in existence.
    ///
    /// This is the relay-less case that used to be impossible: `route_peer_over_door` migrates an
    /// existing UDP path, so on a carrier where UDP never lands there was never anything to migrate
    /// and the door — the only reachable address — went unused. Alice here knows Bob's identity and
    /// ONLY his door; the whole QUIC handshake has to travel the TLS-over-TCP tunnel.
    #[tokio::test]
    async fn door_carries_first_contact_when_no_udp_path_exists() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let bob_udp = bob_quic.local_addr().unwrap();

        // Bob's front door: a TCP listener bridging into his quinn socket.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = crate::tcp_transport::run_tcp_bridge_on(listener, bob_udp).await;
        });

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        // Alice is given Bob's DOOR as his only address — no UDP endpoint anywhere.
        seed_peer(&alice, bob_pub, vec![door]);
        seed_peer(&bob, alice_pub, Vec::new());
        alice.start();
        bob.start();

        let link = timeout(
            Duration::from_secs(15),
            alice.dial_over_door(bob.local_address(), door),
        )
        .await
        .expect("first contact over the door completes within 15s")
        .expect("dial_over_door succeeds");
        assert_eq!(link.destination(), bob.local_address());

        // And it is a working link, not just a handshake: a frame crosses it.
        let bob_link = timeout(Duration::from_secs(10), bob.accept())
            .await
            .expect("bob accepts the door-borne link")
            .expect("accept not None");
        link.send(b"over-the-door").await.unwrap();
        let got = timeout(Duration::from_secs(10), bob_link.recv())
            .await
            .expect("bob receives the frame")
            .expect("recv ok");
        assert_eq!(got, b"over-the-door");

        alice.shutdown();
        bob.shutdown();
    }

    /// Only an announce heard straight from the peer gives a LAN door: one a relay forwarded may
    /// carry another network's LAN addresses. Without a door port there is no door at all.
    #[tokio::test]
    async fn a_lan_door_comes_only_from_the_peers_own_announce() {
        use crate::sim::{NatModel, SimFabric};
        let fabric = SimFabric::new();
        let lan = fabric.interface_with_kind("10.0.0.1:1".parse().unwrap(), NatModel::Public, InterfaceKind::Mdns)
            as InterfaceHandle;
        let link = fabric.interface("10.0.0.1:2".parse().unwrap(), NatModel::Public) as InterfaceHandle;
        let net = Arc::new(Network::new(fresh_signing(), vec![lan.clone(), link.clone()]));
        let neighbor = NeighborId("10.0.0.9:1".into());
        let announce_of = |key: &SigningKey, door_port: u16| {
            Announce::sign_new_with_capabilities(
                key,
                vec!["192.168.1.20:40000".parse().unwrap()],
                current_unix_ms(),
                300,
                0,
                door_port,
            )
        };

        // Our own LAN address, 192.168.1.5, as the announce loop gives it.
        net.announce(vec!["192.168.1.5:40000".parse().unwrap()]).await.unwrap();
        let heard = fresh_signing();
        net.handle_announce(announce_of(&heard, 51515), &lan, &neighbor).await.unwrap();
        assert_eq!(
            net.lan_door(&NodeAddress::from_pubkey(&heard.verifying_key())),
            Some("192.168.1.20:51515".parse().unwrap()),
            "the announced door port at the address the peer announced on our LAN"
        );

        let forwarded = fresh_signing();
        net.handle_announce(announce_of(&forwarded, 51515).with_incremented_hop(), &lan, &neighbor)
            .await
            .unwrap();
        assert_eq!(net.lan_door(&NodeAddress::from_pubkey(&forwarded.verifying_key())), None);

        // Straight from the peer over a direct link counts too — inside our /24.
        let direct = fresh_signing();
        net.handle_announce(announce_of(&direct, 51515), &link, &neighbor).await.unwrap();
        assert_eq!(
            net.lan_door(&NodeAddress::from_pubkey(&direct.verifying_key())),
            Some("192.168.1.20:51515".parse().unwrap())
        );

        // A peer linked straight over the internet announces its own home address: not our LAN.
        let elsewhere = fresh_signing();
        let away = Announce::sign_new_with_capabilities(
            &elsewhere,
            vec!["192.168.77.7:40000".parse().unwrap()],
            current_unix_ms(),
            300,
            0,
            51515,
        );
        net.handle_announce(away, &link, &neighbor).await.unwrap();
        assert_eq!(net.lan_door(&NodeAddress::from_pubkey(&elsewhere.verifying_key())), None);

        let doorless = fresh_signing();
        net.handle_announce(announce_of(&doorless, 0), &lan, &neighbor).await.unwrap();
        assert_eq!(net.lan_door(&NodeAddress::from_pubkey(&doorless.verifying_key())), None);
    }

    /// A contact on our LAN with no UPnP: in TCP mode the dial goes to its LAN door (the raw UDP
    /// address is refused), and after a switch to TCP closes a live UDP link the redial comes up over
    /// that door. Before, there was no door to use: `tcp_front_door ... advertised=[]`.
    #[tokio::test]
    async fn a_contact_on_our_lan_is_reached_over_its_lan_door_in_tcp_mode() {
        async fn pair() -> (Arc<Network>, Arc<Network>, SocketAddr) {
            let alice_key = fresh_signing();
            let bob_key = fresh_signing();
            let alice_pub = alice_key.verifying_key();
            let bob_pub = bob_key.verifying_key();
            let alice_quic = bind_quic(&alice_key);
            let bob_quic = bind_quic(&bob_key);
            let bob_udp = bob_quic.local_addr().unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let door = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = crate::tcp_transport::run_tcp_bridge_on(listener, bob_udp).await;
            });
            let alice = Arc::new(Network::new(alice_key, vec![alice_quic as Arc<dyn Interface>]));
            let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
            // What Bob's multicast announce leaves in Alice's book: his LAN address and door port.
            seed_peer(&alice, bob_pub, vec![bob_udp]);
            {
                let mut book = alice.peer_book.lock();
                let record = book.get_mut(&NodeAddress::from_pubkey(&bob_pub)).unwrap();
                record.door_port = door.port();
                record.heard_direct = true;
            }
            seed_peer(&bob, alice_pub, Vec::new());
            alice.start();
            bob.start();
            (alice, bob, door)
        }
        async fn crosses(alice: &Network, bob: &Network, link: &Link, payload: &[u8]) {
            let bob_link = timeout(Duration::from_secs(10), bob.accept())
                .await
                .expect("bob accepts the link")
                .expect("accept not None");
            link.send(payload).await.unwrap();
            let got = timeout(Duration::from_secs(10), bob_link.recv())
                .await
                .expect("bob receives the frame")
                .expect("recv ok");
            assert_eq!(got, payload);
            let _ = alice;
        }

        // A fresh dial in TCP mode.
        let (alice, bob, _door) = pair().await;
        alice.set_carrier_preference(CarrierPreference::Tcp);
        let link = timeout(Duration::from_secs(15), alice.dial(bob.local_address()))
            .await
            .expect("the dial completes within 15s")
            .expect("TCP mode reaches the LAN contact over its door");
        assert!(alice.peer_on_door(&bob.local_address()), "the link rides the door, not raw UDP");
        crosses(&alice, &bob, &link, b"over-the-lan-door").await;
        alice.shutdown();
        bob.shutdown();

        // A live UDP link, then the switch to TCP.
        let (alice, bob, _door) = pair().await;
        let link = timeout(Duration::from_secs(15), alice.dial(bob.local_address()))
            .await
            .expect("the dial completes within 15s")
            .expect("Auto links over UDP");
        assert!(!alice.peer_on_door(&bob.local_address()));
        crosses(&alice, &bob, &link, b"before-the-switch").await;
        let closed = alice.switch_carrier_preference(CarrierPreference::Tcp, &HashMap::new()).await;
        assert_eq!(closed, 1, "the raw UDP link is closed, not migrated");
        let link = timeout(Duration::from_secs(15), alice.dial(bob.local_address()))
            .await
            .expect("the redial completes within 15s")
            .expect("the redial reaches Bob over his LAN door");
        assert!(alice.peer_on_door(&bob.local_address()), "the new link rides the door");
        crosses(&alice, &bob, &link, b"after-the-switch").await;
        alice.shutdown();
        bob.shutdown();
    }

    /// "Start reliable, then move to fast": a link that came up over the door upgrades onto direct
    /// UDP once UDP actually works, and keeps working across the switch. The upgrade is additive —
    /// the door path stays underneath — so a failed upgrade could never cost us the link.
    #[tokio::test]
    async fn door_link_upgrades_to_direct_udp_when_udp_works() {
        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let bob_udp = bob_quic.local_addr().unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = crate::tcp_transport::run_tcp_bridge_on(listener, bob_udp).await;
        });

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&alice, bob_pub, vec![door]);
        seed_peer(&bob, alice_pub, Vec::new());
        alice.start();
        bob.start();

        let link = timeout(
            Duration::from_secs(15),
            alice.dial_over_door(bob.local_address(), door),
        )
        .await
        .expect("first contact over the door completes")
        .expect("dial_over_door succeeds");
        assert!(
            alice.peer_on_door(&bob.local_address()),
            "the link must start out carried by the door"
        );

        // Now UDP is reachable (loopback stands in for a carrier that started passing UDP).
        let upgraded = timeout(
            Duration::from_secs(15),
            alice.upgrade_peer_to_udp(bob.local_address(), bob_udp),
        )
        .await
        .expect("upgrade probe finishes");
        assert!(upgraded, "a working UDP endpoint must upgrade the carrier");
        // Upgraded means TRAFFIC moved, not just that a second path exists: the door path and the
        // new UDP path used to tie in the ranking, and the older door path kept carrying.
        assert!(
            !alice.peer_on_door(&bob.local_address()),
            "after the upgrade the best path must be UDP"
        );

        // The link still works after the switch — the app must not notice the carrier change.
        let bob_link = timeout(Duration::from_secs(10), bob.accept())
            .await
            .expect("bob accepts")
            .expect("accept not None");
        link.send(b"after-upgrade").await.unwrap();
        let got = timeout(Duration::from_secs(10), bob_link.recv())
            .await
            .expect("bob receives after the upgrade")
            .expect("recv ok");
        assert_eq!(got, b"after-upgrade");

        alice.shutdown();
        bob.shutdown();
    }

    /// The bulk plane (attachment transfer): a verified link carries a dedicated ORDERED stream on
    /// the peer's existing connection, and the receiver learns WHO opened it. Ordering is the whole
    /// point — it is what lets a receiver write a multi-gigabyte file straight through instead of
    /// holding out-of-order gaps in RAM — so this asserts the bytes arrive in the order written,
    /// across more than one write.
    #[tokio::test]
    async fn verified_quic_link_carries_a_bulk_stream() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let alice_key = fresh_signing();
        let bob_key = fresh_signing();
        let alice_pub = alice_key.verifying_key();
        let bob_pub = bob_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let bob_quic = bind_quic(&bob_key);
        let alice_addr = alice_quic.local_addr().unwrap();
        let bob_addr = bob_quic.local_addr().unwrap();

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        let bob = Arc::new(Network::new(bob_key, vec![bob_quic as Arc<dyn Interface>]));
        seed_peer(&alice, bob_pub, vec![bob_addr]);
        seed_peer(&bob, alice_pub, vec![alice_addr]);
        alice.start();
        bob.start();

        let alice_to_bob = alice
            .dial(bob.local_address())
            .await
            .expect("dial должен пройти");
        let _bob_link = timeout(Duration::from_secs(5), bob.accept())
            .await
            .expect("bob incoming link")
            .expect("accept not None");

        let mut out = alice_to_bob
            .open_bulk_stream()
            .await
            .expect("alice opens a bulk stream over the verified link");
        // Two separate writes: the reader must see one continuous, correctly ordered byte run.
        out.send.write_all(b"chunk-one").await.unwrap();
        out.send.write_all(b"chunk-two").await.unwrap();
        out.send.flush().await.unwrap();

        let (peer, mut incoming) = timeout(Duration::from_secs(5), bob.recv_bulk_stream())
            .await
            .expect("bob receives the bulk stream within 5s")
            .expect("recv_bulk_stream ok");
        assert_eq!(
            peer,
            alice.local_address(),
            "the stream is attributed to the identity that opened it"
        );

        let mut got = [0_u8; 18];
        timeout(Duration::from_secs(5), incoming.recv.read_exact(&mut got))
            .await
            .expect("bob reads the bulk payload within 5s")
            .expect("read_exact ok");
        assert_eq!(&got, b"chunk-onechunk-two");

        alice.shutdown();
        bob.shutdown();
    }

    /// Anti-impersonation: Alice was told (falsely) that `victim` lives at an
    /// address actually served by an impostor. The impostor *answers* the
    /// challenge, but signs with its own key — which doesn't hash to `victim`.
    /// `verify_challenge_response` catches the mismatch and dial is rejected.
    #[tokio::test]
    async fn dial_rejects_responder_with_mismatched_identity() {
        let alice_key = fresh_signing();
        let victim_key = fresh_signing();
        let impostor_key = fresh_signing();
        let victim_pub = victim_key.verifying_key();
        let impostor_pub = impostor_key.verifying_key();

        let alice_quic = bind_quic(&alice_key);
        let impostor_quic = bind_quic(&impostor_key);
        let impostor_addr = impostor_quic.local_addr().unwrap();

        let alice = Arc::new(Network::new(
            alice_key,
            vec![alice_quic as Arc<dyn Interface>],
        ));
        alice.start();

        // Alice believes victim is reachable at the impostor's address.
        seed_peer(&alice, victim_pub, vec![impostor_addr]);

        // Dishonest raw responder: accept the connection, read Alice's
        // ChallengeRequest, reply signing with impostor_key (≠ victim).
        let responder = Arc::clone(&impostor_quic);
        tokio::spawn(async move {
            if let Ok((neighbor, bytes)) = responder.recv_frame().await {
                if let Ok(NetworkFrame::ChallengeRequest(req)) =
                    NetworkFrame::decode_from_wire(&bytes)
                {
                    let response = respond_to_challenge(&impostor_key, &req.nonce);
                    let frame = NetworkFrame::ChallengeResponse(ChallengeResponseFrame {
                        nonce: req.nonce,
                        from: NodeAddress::from_pubkey(&impostor_pub),
                        to: req.from,
                        response,
                    });
                    let _ = responder.send_frame(&neighbor, &frame.encode_to_wire()).await;
                }
            }
        });

        let result = alice.dial(NodeAddress::from_pubkey(&victim_pub)).await;
        assert!(
            matches!(result, Err(NetworkError::ChallengeRejected(_))),
            "responder с mismatched identity должен быть отвергнут, got {result:?}"
        );

        alice.shutdown();
    }
}
