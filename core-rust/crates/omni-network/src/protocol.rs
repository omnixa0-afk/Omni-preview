// Network-level frame protocol. Все байты которые ходят через Interface'ы
// (multicast announce'ы, QUIC streams, future LoRa packets) начинаются с
// одного байта `FrameKind` который сообщает receiver'у тип содержимого.
//
// Без этого byte'а мы не могли бы mix'ать announce'ы и data frame'ы на одном
// transport channel'е (или мы бы платили separate channels per frame type
// что усложняет API).
//
// Wire format верхнего уровня:
//   [1 byte: FrameKind tag]
//   [remainder: kind-specific encoding]
//
// FrameKind values используют high-bit area (0xA0..) чтобы не пересекаться
// с ASCII range — если кто-то случайно посылает text frame, мы immediately
// дропаем а не misparse'им.
//
// **Backward compat policy**: новые FrameKind values добавляются как новые
// tag-bytes; старые узлы видят unknown tag и drop'ают (NetworkFrameError::
// UnknownKind). Это позволяет evolve frame zoo без breaking change'а.

use crate::address::NodeAddress;
use crate::announce::{Announce, AnnounceError};
use crate::cert::{CertError, ChallengeResponse, CHALLENGE_NONCE_BYTES};
use crate::durable::FeedId;
use rand_core::{OsRng, RngCore};
use std::net::SocketAddr;
use thiserror::Error;

/// Tag byte для каждого frame type на wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    /// Periodic self-advertisement (signed). Forwarded multi-hop по
    /// mesh'у. См. `announce::Announce`.
    Announce = 0xA0,
    /// Initiator → responder: «prove you own this NodeAddress».
    /// Sent immediately after QUIC handshake'а before transmitting app data.
    ChallengeRequest = 0xC1,
    /// Responder → initiator: signed proof of identity.
    /// См. `cert::ChallengeResponse`.
    ChallengeResponse = 0xC2,
    /// Application payload между двумя destinations.
    Data = 0xD0,
    /// Reflexive-address hint: «я видел, что ты подключился с этого адреса».
    /// Шлётся по verified-линку, чтобы получатель узнал свой внешне-маппленный
    /// endpoint (NAT reflexive), агрегируя отчёты от ≥2 соседей → класс NAT.
    /// См. P1 / external review §4.1A.
    ObservedAddress = 0xB0,
    /// AutoNAT dial-back request: «дозвонись до меня по candidate-адресу и
    /// докажи это, прислав DialBackProbe с этим nonce». Шлётся по verified-линку
    /// к пиру-верификатору. P2 reachability — отличает reflexive-адрес (знаю свой
    /// внешний endpoint) от дозваниваемости (новый пир может туда дозвониться).
    DialBackRequest = 0xB1,
    /// AutoNAT dial-back probe: верификатор делает СВЕЖИЙ dial на candidate и шлёт
    /// этот кадр; получение его на fresh-inbound доказывает прямую достижимость.
    DialBackProbe = 0xB2,
    /// Durable delivery (P4): подписанный opaque per-destination кадр с тонким
    /// plaintext-envelope (to/from/feed/seq/msg_id/ttl), который хранитель/relay
    /// читает для маршрута/хранения/дедупа, но НЕ контент. В отличие от Data
    /// (0xD0, fire-and-forget single-hop) — переживает оффлайн через store-and-
    /// forward. См. docs/decisions/mesh-durable-delivery.md.
    DurableData = 0xD1,
    /// Anti-entropy «have»: high-water вектор отправителя {feed → seq}.
    AntiEntropyHave = 0xE0,
    /// Anti-entropy «offer»: какие (feed, seq, msg_id) у меня есть за твоим high-water.
    AntiEntropyOffer = 0xE1,
    /// Anti-entropy «request»: какие msg_id я хочу (которых у меня ещё нет).
    AntiEntropyRequest = 0xE2,
    /// Hole-punch request: «помоги мне пробить прямое соединение к target». Шлётся
    /// по verified-линку к общему соседу/relay, который рассылает PunchSync обоим.
    /// P2 NAT-traversal upgrade (relay-first → direct). См. review §4.1B-C.
    PunchRequest = 0xB3,
    /// Hole-punch sync: relay сообщает каждой стороне reflexive-endpoint ДРУГОЙ +
    /// общий nonce, чтобы обе пробили pinhole (probe друг к другу), после чего одна
    /// делает реальный dial (роль — по меньшему NodeAddress). См. review §4.1C.
    PunchSync = 0xB4,
    /// Mesh liveness probe (app-level): sent to a verified neighbour that has gone
    /// quiet so a live peer answers and stays marked alive, while a dead one is
    /// detected fast (vs the slow QUIC idle timeout). Single-hop, payloadless.
    Ping = 0xB5,
    /// Reply to `Ping` — proves the link + peer are alive. Payloadless.
    Pong = 0xB6,
    /// Reactive path discovery (R3): flood a request to find a route to `target` when a dial has no
    /// direct/known path. A node that IS the target, or already knows a route to it, answers with a
    /// `PathReply`; others re-broadcast it (hop-limit bounded + deduped) to verified neighbours. This
    /// is what makes "one connected peer → reach the whole mesh" real — announce is single-hop, so the
    /// path_table never learns cost≥2 routes on its own. See docs/decisions/omni-network-model.md (D6).
    PathRequest = 0xB7,
    /// Reply to a `PathRequest`: the answerer can reach `target` at `cost` hops. Relayed back toward
    /// the requester, filling each hop's path_table so the requester can then `dial_multihop`.
    PathReply = 0xB8,
    /// TCP-direct reachability request (task 3): «TCP-connect to my `candidate` front door and tell
    /// me if it's reachable». The TCP analog of `DialBackRequest`, but the proof can't be a probe the
    /// requester receives (its TCP door is a QUIC bridge, opaque to its reachability logic) — so the
    /// verifier reports the result back. See docs/decisions/tcp-direct-peer-fallback.md.
    TcpReachabilityRequest = 0xB9,
    /// Reply to `TcpReachabilityRequest`: the verifier's TCP-connect to `candidate` succeeded (or
    /// not). A `reachable=true` confirms our TCP front door before we advertise it (validate-before-
    /// advertise, like AutoNAT for UDP).
    TcpReachabilityResult = 0xBA,
    /// Routed end-to-end liveness probe (relay-path liveness): forwarded hop-by-hop toward an end-peer
    /// reached ONLY over a relay, so the prober can tell when that peer dies — the link-local `Ping`
    /// only proves the relay neighbour. See docs/bugs/relay-presence-false-online-dead-peer.md.
    PeerPing = 0xBB,
    /// Reply to `PeerPing` — routed back toward the prober; proves the end-peer is alive end-to-end.
    PeerPong = 0xBC,
    /// Routed DHT-as-a-service request (R7 Phase 2 mode b): a UDP-blocked node asks a proxy peer (whose
    /// DHT works) to run a BEP44 `put`/`get` on its behalf. Forwarded toward `to` like `PeerPing`. See
    /// docs/decisions/dht-over-peer.md.
    DhtProxyRequest = 0xBD,
    /// Reply to `DhtProxyRequest` — routed back toward the requester with the BEP44 result.
    DhtProxyResponse = 0xBE,
    /// Friend-proxy carriage (Axis 2): a consumer asks its consenting friend to carry an opaque
    /// (E2E-sealed) payload to `to` on its behalf. Sent one hop to the chosen friend, which gates on
    /// per-friend consent + a per-origin cap, then re-emits it as routed `Data`. See
    /// docs/decisions/friend-proxy-by-consent.md.
    FriendRelayCarry = 0xBF,
    /// Direct-neighbour request: "share the in-segment DHT bootstrap addresses you know" — segment
    /// cold-start seed propagation (step B). NOT routed: sent to, and answered by, a linked peer only
    /// (no `to`/hop_limit) so there is no multi-hop harvest/amplification surface. See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    DhtBootReq = 0xC3,
    /// Reply to `DhtBootReq` — up to `DHT_BOOT_MAX_NODES` `ip:port` DHT bootstrap candidates the
    /// answerer has configured, returned on the same direct link.
    DhtBootResp = 0xC4,
    /// Direct-neighbour request: "your relay card, please" — the signed relay-directory entry a node
    /// that opened itself as a relay hands to whoever reached it at a bare address. Single-hop.
    /// See docs/decisions/public-relay-directory.md (stage 2).
    RelayCardReq = 0xC5,
    /// Reply to `RelayCardReq` — the card (opaque bytes the app signed), empty when not a relay.
    RelayCardResp = 0xC6,
}

impl FrameKind {
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0xA0 => Some(Self::Announce),
            0xB0 => Some(Self::ObservedAddress),
            0xB1 => Some(Self::DialBackRequest),
            0xB2 => Some(Self::DialBackProbe),
            0xB3 => Some(Self::PunchRequest),
            0xB4 => Some(Self::PunchSync),
            0xB5 => Some(Self::Ping),
            0xB6 => Some(Self::Pong),
            0xB7 => Some(Self::PathRequest),
            0xB8 => Some(Self::PathReply),
            0xB9 => Some(Self::TcpReachabilityRequest),
            0xBA => Some(Self::TcpReachabilityResult),
            0xBB => Some(Self::PeerPing),
            0xBC => Some(Self::PeerPong),
            0xBD => Some(Self::DhtProxyRequest),
            0xBE => Some(Self::DhtProxyResponse),
            0xBF => Some(Self::FriendRelayCarry),
            0xC3 => Some(Self::DhtBootReq),
            0xC4 => Some(Self::DhtBootResp),
            0xC5 => Some(Self::RelayCardReq),
            0xC6 => Some(Self::RelayCardResp),
            0xC1 => Some(Self::ChallengeRequest),
            0xC2 => Some(Self::ChallengeResponse),
            0xD0 => Some(Self::Data),
            0xD1 => Some(Self::DurableData),
            0xE0 => Some(Self::AntiEntropyHave),
            0xE1 => Some(Self::AntiEntropyOffer),
            0xE2 => Some(Self::AntiEntropyRequest),
            _ => None,
        }
    }
}

/// Parsed network frame variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkFrame {
    Announce(Announce),
    ChallengeRequest(ChallengeRequest),
    ChallengeResponse(ChallengeResponseFrame),
    Data(DataFrame),
    ObservedAddress(ObservedAddressFrame),
    DialBackRequest(DialBackRequestFrame),
    DialBackProbe(DialBackProbeFrame),
    TcpReachabilityRequest(TcpReachabilityRequestFrame),
    TcpReachabilityResult(TcpReachabilityResultFrame),
    DurableData(DurableDataFrame),
    AntiEntropyHave(AntiEntropyHaveFrame),
    AntiEntropyOffer(AntiEntropyOfferFrame),
    AntiEntropyRequest(AntiEntropyRequestFrame),
    PunchRequest(PunchRequestFrame),
    PunchSync(PunchSyncFrame),
    /// Liveness probe / reply — payloadless, single-hop. See `FrameKind::Ping`/`Pong`.
    Ping,
    Pong,
    PathRequest(PathRequestFrame),
    PathReply(PathReplyFrame),
    /// Routed end-to-end liveness probe / reply. See `PeerPingFrame` / `PeerPongFrame`.
    PeerPing(PeerPingFrame),
    PeerPong(PeerPongFrame),
    /// Routed DHT-as-a-service request / reply (R7 Phase 2 mode b). See `DhtProxyRequestFrame` /
    /// `DhtProxyResponseFrame` + docs/decisions/dht-over-peer.md.
    DhtProxyRequest(DhtProxyRequestFrame),
    DhtProxyResponse(DhtProxyResponseFrame),
    /// Friend-proxy carriage (Axis 2). See `FriendRelayCarryFrame` + docs/decisions/friend-proxy-by-consent.md.
    FriendRelayCarry(FriendRelayCarryFrame),
    /// Direct-neighbour DHT bootstrap seed exchange (step B). See `DhtBootReqFrame` /
    /// `DhtBootRespFrame` + docs/decisions/dht-bootstrap-mesh-propagation.md.
    DhtBootReq(DhtBootReqFrame),
    DhtBootResp(DhtBootRespFrame),
    /// Relay card exchange (relay directory stage 2). See `RelayCardReqFrame` / `RelayCardRespFrame`.
    RelayCardReq(RelayCardReqFrame),
    RelayCardResp(RelayCardRespFrame),
}

/// Routed end-to-end liveness probe (relay-path liveness): `from` asks the END-peer `to` "are you
/// still there?", forwarded hop-by-hop toward `to` via the path_table (like Data). A peer reached
/// ONLY over a relay is never our path neighbour, so the link-local `Ping`/`Pong` (which the relay
/// answers) can't tell when it dies — this probe routes all the way to the end-peer, which answers
/// with a `PeerPong`. Without it a dead relay-reachable peer reads as "online via relay" forever
/// (the live relay carries the stale path). See docs/bugs/relay-presence-false-online-dead-peer.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPingFrame {
    /// The prober — the `PeerPong` is routed back toward this.
    pub from: NodeAddress,
    /// The end-peer being probed — the ping is forwarded toward this.
    pub to: NodeAddress,
    /// Correlator: dedups re-forwards + matches the pong to this ping.
    pub nonce: u64,
    /// TTL, decremented per hop (loop/flood bound). 0 ⇒ don't forward onward.
    pub hop_limit: u8,
}

/// Reply to a `PeerPing` — proves the answering end-peer is alive end-to-end. Routed back toward the
/// original prober (`to`) over the path_table; the prober refreshes its per-peer liveness for `from`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPongFrame {
    /// The answering end-peer (whose liveness this proves).
    pub from: NodeAddress,
    /// The original prober — the pong is forwarded toward this.
    pub to: NodeAddress,
    /// Echo of the ping nonce.
    pub nonce: u64,
    /// TTL, decremented per hop.
    pub hop_limit: u8,
}

/// BEP44 hard caps, re-asserted on the wire so an oversized salt/value can't ride a `DhtProxyRequest`
/// (the proxy's `omni-dht-helper` would reject it anyway; reject earlier + bound the frame).
pub const DHT_PROXY_MAX_SALT: usize = 64;
pub const DHT_PROXY_MAX_VALUE: usize = 1000;

/// The BEP44 op a `DhtProxyRequest` asks the proxy peer to run on its working DHT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhtProxyOp {
    Get,
    Put,
    /// BEP 5 `get_peers` under the info-hash carried in the first 20 bytes of `key`; the answer is the
    /// peers packed by [`pack_ipv4_peers`]. The relay directory's Mainline shelves, read through a
    /// friend when our own Mainline is dead (public-relay-directory.md, stage 2).
    Peers,
}

/// IPv4 peers (BEP 5 `get_peers` gives only those) as 6 bytes each — address, port — so a proxy's
/// answer fits `DHT_PROXY_MAX_VALUE` (up to 166 peers).
pub fn pack_ipv4_peers(peers: &[std::net::SocketAddrV4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(peers.len().min(DHT_PROXY_MAX_VALUE / 6) * 6);
    for peer in peers.iter().take(DHT_PROXY_MAX_VALUE / 6) {
        out.extend_from_slice(&peer.ip().octets());
        out.extend_from_slice(&peer.port().to_be_bytes());
    }
    out
}

/// What [`pack_ipv4_peers`] packed; a trailing partial entry is dropped.
pub fn unpack_ipv4_peers(bytes: &[u8]) -> Vec<std::net::SocketAddrV4> {
    bytes
        .as_chunks::<6>()
        .0
        .iter()
        .map(|chunk| {
            std::net::SocketAddrV4::new(
                std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]),
                u16::from_be_bytes([chunk[4], chunk[5]]),
            )
        })
        .collect()
}

impl DhtProxyOp {
    fn to_tag(self) -> u8 {
        match self {
            DhtProxyOp::Get => 0,
            DhtProxyOp::Put => 1,
            DhtProxyOp::Peers => 2,
        }
    }
    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(DhtProxyOp::Get),
            1 => Some(DhtProxyOp::Put),
            2 => Some(DhtProxyOp::Peers),
            _ => None,
        }
    }
}

/// Routed DHT-as-a-service request (R7 Phase 2 mode b): a node whose own UDP can't reach the Mainline
/// swarm asks a proxy peer (whose DHT works — the home node / a white relay) to run a BEP44 `put`/`get`
/// on its behalf, forwarded hop-by-hop toward `to` (like `PeerPing`). The proxy runs the op on its
/// local `omni-dht-helper` and routes a `DhtProxyResponse` back. Content-blind: `value` is the core's
/// AEAD-sealed beacon, `key` is the per-bucket secret-derived seed. See docs/decisions/dht-over-peer.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtProxyRequestFrame {
    /// The requester — the response is routed back toward this.
    pub from: NodeAddress,
    /// The proxy peer — the request is forwarded toward this.
    pub to: NodeAddress,
    /// Correlator: dedups re-forwards + matches the response to this request.
    pub req_id: u64,
    /// The BEP44 op to run (`Get` reads the slot, `Put` writes `value`).
    pub op: DhtProxyOp,
    /// The per-bucket `dht_key` seed; the proxy's helper derives the ed25519 keypair from it.
    pub key: [u8; 32],
    /// The BEP44 slot salt (≤ `DHT_PROXY_MAX_SALT`).
    pub salt: Vec<u8>,
    /// The sealed beacon to publish for `Put` (empty for `Get`); ≤ `DHT_PROXY_MAX_VALUE`.
    pub value: Vec<u8>,
    /// TTL, decremented per hop (loop/flood bound). 0 ⇒ don't forward onward.
    pub hop_limit: u8,
}

/// Reply to a `DhtProxyRequest` — the BEP44 result, routed back toward the requester (`to`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtProxyResponseFrame {
    /// The proxy that ran the op.
    pub from: NodeAddress,
    /// The original requester — the response is forwarded toward this.
    pub to: NodeAddress,
    /// Echo of the request correlator.
    pub req_id: u64,
    /// Whether the proxy's op succeeded (a `Get` miss is still `ok` with an empty `value`).
    pub ok: bool,
    /// The `Get` result (empty for a `Put`, or a `Get` of an empty slot); ≤ `DHT_PROXY_MAX_VALUE`.
    pub value: Vec<u8>,
    /// TTL, decremented per hop.
    pub hop_limit: u8,
}

/// Max DHT bootstrap candidates a `DhtBootResp` may carry — the shared set is a small curated list of
/// known-good in-segment entry points, not a routing-table dump. Re-asserted on the wire so an
/// oversized list can't ride the frame. See docs/decisions/dht-bootstrap-mesh-propagation.md.
pub const DHT_BOOT_MAX_NODES: usize = 16;

/// Direct-neighbour DHT bootstrap seed request (step B): "share the in-segment DHT bootstrap addresses
/// you know". Sent to a linked peer; the peer answers with a `DhtBootResp` on the same link. Carries no
/// `to`/hop_limit — it is single-hop by design (no multi-hop harvest of DHT IPs from strangers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtBootReqFrame {
    /// Correlator: matches the response to this request (a peer may have several outstanding).
    pub req_id: u64,
}

/// The largest relay card a `RelayCardResp` carries (a relay-directory entry is a few hundred bytes).
pub const RELAY_CARD_MAX: usize = 1024;

/// "Your relay card, please" — sent on a direct link to a node reached at a bare address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCardReqFrame {
    pub req_id: u64,
}

/// The answer: the card the node's app set (`Network::set_relay_card`), empty when it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayCardRespFrame {
    pub req_id: u64,
    pub card: Vec<u8>,
}

/// Reply to a `DhtBootReq` — the answerer's configured DHT bootstrap candidates (≤ `DHT_BOOT_MAX_NODES`
/// `ip:port`). Public-in-the-swarm addresses, so shared point-to-point on the established link (not in
/// the announce/gossip plane — R1b stays intact). An empty list means "I have none / not sharing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtBootRespFrame {
    /// Echo of the request correlator.
    pub req_id: u64,
    /// The bootstrap candidates (`ip:port`); may be empty.
    pub nodes: Vec<SocketAddr>,
}

/// Friend-proxy carriage (Axis 2): a consumer's request that its consenting friend carry `payload`
/// (opaque — already E2E-sealed for the final recipient) toward `to`. Sent ONE hop to the chosen
/// friend; the friend re-emits it as routed `Data` only if it has consented to relay for the origin
/// (resolved from the verified link, NOT this frame) and is under its per-origin cap. Content-blind.
/// See docs/decisions/friend-proxy-by-consent.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendRelayCarryFrame {
    /// The final recipient the friend should carry the payload toward.
    pub to: NodeAddress,
    /// Opaque payload (E2E-sealed for `to`; content-blind to the friend). ≤ `MAX_DATA_PAYLOAD`.
    pub payload: Vec<u8>,
}

/// Hole-punch coordination request (P2): asks a common relay/neighbour to set up a
/// direct connection to `target`. The relay sends a `PunchSync` to both parties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PunchRequestFrame {
    /// The peer we want to punch a direct path to.
    pub target: NodeAddress,
    /// Correlator shared by the two resulting PunchSyncs.
    pub nonce: u64,
}

/// Hole-punch sync (P2): the relay tells one side the OTHER side's reflexive
/// endpoint + the shared nonce, so both probe each other (open NAT pinholes) and
/// the role-assigned dialer then connects. See external review §4.1C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PunchSyncFrame {
    /// The other peer to punch to.
    pub peer: NodeAddress,
    /// Its reflexive endpoint to probe / dial.
    pub peer_endpoint: SocketAddr,
    /// Echo of the request nonce.
    pub nonce: u64,
}

/// Reactive path-discovery request (R3, AODV-style): `origin` asks "who can reach `target`?".
/// Flooded over verified neighbours when a dial finds no direct/known path; re-broadcast hop-by-hop
/// until `hop_limit` hits 0, deduped by `(origin, nonce)`. Each relay records a REVERSE route to
/// `origin` (origin → arrival neighbour) so the matching `PathReply` can route back. See
/// docs/decisions/omni-network-model.md (D6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRequestFrame {
    /// The requester — the reply is routed back toward this, and relays learn a route to it.
    pub origin: NodeAddress,
    /// The node `origin` is trying to find a route to.
    pub target: NodeAddress,
    /// Random correlator: dedups re-broadcasts + matches the reply to this request.
    pub nonce: u64,
    /// TTL, decremented per hop (loop/flood bound). 0 ⇒ don't re-broadcast.
    pub hop_limit: u8,
}

/// Reactive path-discovery reply (R3): the answerer can reach `target` at `cost` hops. Routed back
/// toward `origin` over the reverse route the request established; each forwarder records the
/// FORWARD route to `target` (target → arrival neighbour) so `origin` can then `dial_multihop`.
/// `cost` increments per hop, mirroring announce hop_count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathReplyFrame {
    /// The original requester — the reply is forwarded toward this.
    pub origin: NodeAddress,
    /// The node a route was found to.
    pub target: NodeAddress,
    /// Echo of the request nonce, so `origin` matches it to its pending PathRequest.
    pub nonce: u64,
    /// Hops from the replying node to `target` (0 = the replier IS the target).
    pub cost: u8,
}

/// Durable-delivery frame (P4): a signed, E2E-encrypted, per-destination message
/// whose THIN plaintext envelope a storer/relay reads (route/store/dedup/order/GC)
/// while the `ciphertext` stays opaque. Survives offline via store-and-forward.
///
/// **Wire layout is provisional** — the exact bit packing + the privacy trade for
/// `feed_id` are fixed by a dedicated envelope-ADR (group-plan §7 п.3). Here we fix
/// only the field SET the durable layer needs. See mesh-durable-delivery.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableDataFrame {
    pub to: NodeAddress,
    pub from: NodeAddress,
    pub feed_id: FeedId,
    pub seq: u64,
    pub msg_id: u64,
    pub ttl_secs: u32,
    pub ciphertext: Vec<u8>,
    /// Signature over the envelope+ciphertext by `from` (so a relay can't forge).
    /// Carried opaquely here; produce/verify wiring is a later durable increment.
    pub signature: Vec<u8>,
}

/// Anti-entropy `have`: a node's high-water vector `{feed → highest contiguous seq}`,
/// plus the destinations it is INTERESTED in (subscribed group dests + its own address).
///
/// `interests` is what lets a store-and-forward relay scope its offers WITHOUT leaking: a
/// relay can't map an opaque `feed_id` (a per-sender hash) back to a destination, so a member
/// must name the destinations it wants; the relay then offers only frames for those dests (plus
/// what it forwards for). This replaces "offer everything to everyone", which broadcast the full
/// feed inventory to untrusted route-candidates. See docs/decisions/anti-entropy-have-scoping.md.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AntiEntropyHaveFrame {
    pub marks: Vec<(FeedId, u64)>,
    pub interests: Vec<NodeAddress>,
}

/// Anti-entropy `offer`: `(feed, seq, msg_id)` entries the sender holds past the
/// peer's high-water — the peer requests the `msg_id`s it still lacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntiEntropyOfferFrame {
    pub entries: Vec<(FeedId, u64, u64)>,
}

/// Anti-entropy `request`: the `msg_id`s the sender wants delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AntiEntropyRequestFrame {
    pub msg_ids: Vec<u64>,
}

/// Anti-DoS cap on the number of entries in one anti-entropy frame.
pub const MAX_ANTI_ENTROPY_ENTRIES: usize = 8192;

/// AutoNAT dial-back request (P2 reachability). Asks the verified peer to make a
/// FRESH connection to `candidate` and send back a `DialBackProbe` carrying
/// `nonce`. Receiving that probe proves we are directly dialable at `candidate`
/// — a stronger fact than merely knowing the address (P1 reflexive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialBackRequestFrame {
    /// Random correlator: echoed in the probe so we match it to this request.
    pub nonce: u64,
    /// The address we want the peer to try to reach us at.
    pub candidate: SocketAddr,
}

/// AutoNAT dial-back probe — sent by the verifier over a fresh connection to the
/// requester's candidate address. Its arrival is the reachability proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialBackProbeFrame {
    /// Echo of the request nonce, so the requester matches it to a pending probe.
    pub nonce: u64,
}

/// TCP-direct reachability request (task 3). Asks a verified peer to TCP-connect to `candidate`
/// (our direct-TCP front door) and report back whether it succeeded — the TCP analog of
/// [`DialBackRequestFrame`]. See docs/decisions/tcp-direct-peer-fallback.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpReachabilityRequestFrame {
    /// Random correlator: echoed in the result so we match it to this request.
    pub nonce: u64,
    /// Our direct-TCP front door the peer should try to TCP-connect to.
    pub candidate: SocketAddr,
}

/// Reply to [`TcpReachabilityRequestFrame`]: whether the verifier's TCP-connect to `candidate`
/// succeeded. `reachable=true` confirms our front door is externally reachable (validate-before-
/// advertise). Reported back over the existing verified path (no probe to the candidate itself,
/// unlike the UDP `DialBackProbe`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpReachabilityResultFrame {
    /// Echo of the request nonce, so the requester matches it to a pending request.
    pub nonce: u64,
    /// Whether the verifier could TCP-connect to the candidate.
    pub reachable: bool,
}

/// «Я видел, что ты подключился с этого адреса». Получатель — за NAT — так узнаёт
/// свой внешне-маппленный endpoint, не обращаясь к STUN-серверу: агрегирует
/// отчёты от ≥2 разных соседей. Совпадающий порт у разных наблюдателей ⇒
/// endpoint-independent mapping (cone); разный ⇒ address/port-dependent
/// (symmetric). Шлётся по verified-линку. См. P1 / external review §4.1A.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAddressFrame {
    /// Адрес получателя, как его увидел отправитель (наш reflexive-кандидат).
    pub observed: SocketAddr,
}

/// Initiator sends nonce + claims expected destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeRequest {
    /// Random nonce that responder will sign.
    pub nonce: [u8; CHALLENGE_NONCE_BYTES],
    /// Initiator's own NodeAddress (so responder может позже challenge'нуть обратно).
    pub from: NodeAddress,
    /// Expected NodeAddress на other end. Anti-impersonation hint.
    pub to: NodeAddress,
}

/// Responder echoes the original nonce + signed proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeResponseFrame {
    pub nonce: [u8; CHALLENGE_NONCE_BYTES],
    pub from: NodeAddress,
    pub to: NodeAddress,
    pub response: ChallengeResponse,
}

/// Application data между двумя destinations.
///
/// `hop_limit` — оставшийся бюджет хопов (как IP TTL). Originator ставит
/// `DATA_DEFAULT_HOP_LIMIT`; каждый forwarder (Phase β multi-hop) декрементит;
/// при 0 кадр дропается — это loop/runaway guard для multi-hop forwarding.
/// Для direct (α-3) доставки поле просто игнорируется получателем.
///
/// `msg_id` — случайный 64-битный идентификатор, присваиваемый **на источнике**
/// (`origin`) и **неизменный при forward'е**. Используется для mesh-уровневого
/// dedup: replay одного и того же кадра и его дубликаты по разным путям
/// (diamond-фан-аут) несут тот же `msg_id` → отбрасываются повторно; два разных
/// send'а (даже с одинаковым payload) получают разные `msg_id` → оба доставляются.
/// Пред-готовит общий формат `msg_id` для durable-слоя. См. external review §3.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub from: NodeAddress,
    pub to: NodeAddress,
    pub hop_limit: u8,
    pub msg_id: u64,
    pub payload: Vec<u8>,
}

impl DataFrame {
    /// Construct a frame originated locally (hop budget at full, fresh random
    /// `msg_id` for dedup).
    pub fn origin(from: NodeAddress, to: NodeAddress, payload: Vec<u8>) -> Self {
        Self {
            from,
            to,
            hop_limit: DATA_DEFAULT_HOP_LIMIT,
            msg_id: OsRng.next_u64(),
            payload,
        }
    }
}

/// Стартовый hop-бюджет для originated Data. Mesh малый (друзья, 3-8 в группе),
/// 16 хопов с огромным запасом перекрывает реальные пути; служит loop guard.
pub const DATA_DEFAULT_HOP_LIMIT: u8 = 16;

impl NetworkFrame {
    /// Wire-encode any frame variant. Returns Vec<u8> ready для interface
    /// .send_frame / .broadcast_frame.
    pub fn encode_to_wire(&self) -> Vec<u8> {
        match self {
            NetworkFrame::Announce(a) => {
                let mut buf = Vec::with_capacity(1 + 150);
                buf.push(FrameKind::Announce as u8);
                buf.extend_from_slice(&a.encode_to_wire());
                buf
            }
            NetworkFrame::ChallengeRequest(req) => {
                let mut buf = Vec::with_capacity(1 + CHALLENGE_NONCE_BYTES + 20 + 20);
                buf.push(FrameKind::ChallengeRequest as u8);
                buf.extend_from_slice(&req.nonce);
                buf.extend_from_slice(req.from.as_bytes());
                buf.extend_from_slice(req.to.as_bytes());
                buf
            }
            NetworkFrame::ChallengeResponse(resp) => {
                let mut buf = Vec::with_capacity(1 + CHALLENGE_NONCE_BYTES + 20 + 20 + 96);
                buf.push(FrameKind::ChallengeResponse as u8);
                buf.extend_from_slice(&resp.nonce);
                buf.extend_from_slice(resp.from.as_bytes());
                buf.extend_from_slice(resp.to.as_bytes());
                buf.extend_from_slice(&resp.response.encode_to_wire());
                buf
            }
            NetworkFrame::Data(data) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 1 + 8 + 4 + data.payload.len());
                buf.push(FrameKind::Data as u8);
                buf.extend_from_slice(data.from.as_bytes());
                buf.extend_from_slice(data.to.as_bytes());
                buf.push(data.hop_limit);
                buf.extend_from_slice(&data.msg_id.to_be_bytes());
                buf.extend_from_slice(&(data.payload.len() as u32).to_be_bytes());
                buf.extend_from_slice(&data.payload);
                buf
            }
            NetworkFrame::ObservedAddress(o) => {
                let mut buf = Vec::with_capacity(1 + 1 + 16 + 2);
                buf.push(FrameKind::ObservedAddress as u8);
                encode_socket_addr(&mut buf, &o.observed);
                buf
            }
            NetworkFrame::DialBackRequest(req) => {
                let mut buf = Vec::with_capacity(1 + 8 + 1 + 16 + 2);
                buf.push(FrameKind::DialBackRequest as u8);
                buf.extend_from_slice(&req.nonce.to_be_bytes());
                encode_socket_addr(&mut buf, &req.candidate);
                buf
            }
            NetworkFrame::DialBackProbe(probe) => {
                let mut buf = Vec::with_capacity(1 + 8);
                buf.push(FrameKind::DialBackProbe as u8);
                buf.extend_from_slice(&probe.nonce.to_be_bytes());
                buf
            }
            NetworkFrame::TcpReachabilityRequest(req) => {
                let mut buf = Vec::with_capacity(1 + 8 + 1 + 16 + 2);
                buf.push(FrameKind::TcpReachabilityRequest as u8);
                buf.extend_from_slice(&req.nonce.to_be_bytes());
                encode_socket_addr(&mut buf, &req.candidate);
                buf
            }
            NetworkFrame::TcpReachabilityResult(res) => {
                let mut buf = Vec::with_capacity(1 + 8 + 1);
                buf.push(FrameKind::TcpReachabilityResult as u8);
                buf.extend_from_slice(&res.nonce.to_be_bytes());
                buf.push(res.reachable as u8);
                buf
            }
            NetworkFrame::PunchRequest(req) => {
                let mut buf = Vec::with_capacity(1 + 20 + 8);
                buf.push(FrameKind::PunchRequest as u8);
                buf.extend_from_slice(req.target.as_bytes());
                buf.extend_from_slice(&req.nonce.to_be_bytes());
                buf
            }
            NetworkFrame::PunchSync(sync) => {
                let mut buf = Vec::with_capacity(1 + 20 + 8 + 1 + 16 + 2);
                buf.push(FrameKind::PunchSync as u8);
                buf.extend_from_slice(sync.peer.as_bytes());
                buf.extend_from_slice(&sync.nonce.to_be_bytes());
                encode_socket_addr(&mut buf, &sync.peer_endpoint);
                buf
            }
            // Liveness probe / reply — tag only, no body.
            NetworkFrame::Ping => vec![FrameKind::Ping as u8],
            NetworkFrame::Pong => vec![FrameKind::Pong as u8],
            NetworkFrame::PathRequest(req) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 8 + 1);
                buf.push(FrameKind::PathRequest as u8);
                buf.extend_from_slice(req.origin.as_bytes());
                buf.extend_from_slice(req.target.as_bytes());
                buf.extend_from_slice(&req.nonce.to_be_bytes());
                buf.push(req.hop_limit);
                buf
            }
            NetworkFrame::PathReply(rep) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 8 + 1);
                buf.push(FrameKind::PathReply as u8);
                buf.extend_from_slice(rep.origin.as_bytes());
                buf.extend_from_slice(rep.target.as_bytes());
                buf.extend_from_slice(&rep.nonce.to_be_bytes());
                buf.push(rep.cost);
                buf
            }
            NetworkFrame::PeerPing(p) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 8 + 1);
                buf.push(FrameKind::PeerPing as u8);
                buf.extend_from_slice(p.from.as_bytes());
                buf.extend_from_slice(p.to.as_bytes());
                buf.extend_from_slice(&p.nonce.to_be_bytes());
                buf.push(p.hop_limit);
                buf
            }
            NetworkFrame::PeerPong(p) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 8 + 1);
                buf.push(FrameKind::PeerPong as u8);
                buf.extend_from_slice(p.from.as_bytes());
                buf.extend_from_slice(p.to.as_bytes());
                buf.extend_from_slice(&p.nonce.to_be_bytes());
                buf.push(p.hop_limit);
                buf
            }
            // [from 20][to 20][req_id 8][op 1][hop_limit 1][key 32][salt_len 1][salt][value_len 2][value]
            NetworkFrame::DhtProxyRequest(r) => {
                let mut buf =
                    Vec::with_capacity(1 + 20 + 20 + 8 + 1 + 1 + 32 + 1 + r.salt.len() + 2 + r.value.len());
                buf.push(FrameKind::DhtProxyRequest as u8);
                buf.extend_from_slice(r.from.as_bytes());
                buf.extend_from_slice(r.to.as_bytes());
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf.push(r.op.to_tag());
                buf.push(r.hop_limit);
                buf.extend_from_slice(&r.key);
                buf.push(r.salt.len() as u8);
                buf.extend_from_slice(&r.salt);
                buf.extend_from_slice(&(r.value.len() as u16).to_be_bytes());
                buf.extend_from_slice(&r.value);
                buf
            }
            // [from 20][to 20][req_id 8][ok 1][hop_limit 1][value_len 2][value]
            NetworkFrame::DhtProxyResponse(r) => {
                let mut buf = Vec::with_capacity(1 + 20 + 20 + 8 + 1 + 1 + 2 + r.value.len());
                buf.push(FrameKind::DhtProxyResponse as u8);
                buf.extend_from_slice(r.from.as_bytes());
                buf.extend_from_slice(r.to.as_bytes());
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf.push(u8::from(r.ok));
                buf.push(r.hop_limit);
                buf.extend_from_slice(&(r.value.len() as u16).to_be_bytes());
                buf.extend_from_slice(&r.value);
                buf
            }
            // [to 20][payload_len 4][payload]
            NetworkFrame::FriendRelayCarry(c) => {
                let mut buf = Vec::with_capacity(1 + 20 + 4 + c.payload.len());
                buf.push(FrameKind::FriendRelayCarry as u8);
                buf.extend_from_slice(c.to.as_bytes());
                buf.extend_from_slice(&(c.payload.len() as u32).to_be_bytes());
                buf.extend_from_slice(&c.payload);
                buf
            }
            // [req_id 8]
            NetworkFrame::DhtBootReq(r) => {
                let mut buf = Vec::with_capacity(1 + 8);
                buf.push(FrameKind::DhtBootReq as u8);
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf
            }
            // [req_id 8]
            NetworkFrame::RelayCardReq(r) => {
                let mut buf = Vec::with_capacity(1 + 8);
                buf.push(FrameKind::RelayCardReq as u8);
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf
            }
            // [req_id 8][len 2][card] — card capped at RELAY_CARD_MAX
            NetworkFrame::RelayCardResp(r) => {
                let card = &r.card[..r.card.len().min(RELAY_CARD_MAX)];
                let mut buf = Vec::with_capacity(1 + 8 + 2 + card.len());
                buf.push(FrameKind::RelayCardResp as u8);
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf.extend_from_slice(&(card.len() as u16).to_be_bytes());
                buf.extend_from_slice(card);
                buf
            }
            // [req_id 8][count 1][addr (1+4|16+2) ...] — count capped at DHT_BOOT_MAX_NODES
            NetworkFrame::DhtBootResp(r) => {
                let count = r.nodes.len().min(DHT_BOOT_MAX_NODES);
                let mut buf = Vec::with_capacity(1 + 8 + 1 + count * (1 + 16 + 2));
                buf.push(FrameKind::DhtBootResp as u8);
                buf.extend_from_slice(&r.req_id.to_be_bytes());
                buf.push(count as u8);
                for addr in r.nodes.iter().take(count) {
                    encode_socket_addr(&mut buf, addr);
                }
                buf
            }
            NetworkFrame::DurableData(d) => {
                let mut buf = Vec::with_capacity(
                    1 + 20 + 20 + 16 + 8 + 8 + 4 + 4 + d.ciphertext.len() + 2 + d.signature.len(),
                );
                buf.push(FrameKind::DurableData as u8);
                buf.extend_from_slice(d.to.as_bytes());
                buf.extend_from_slice(d.from.as_bytes());
                buf.extend_from_slice(&d.feed_id);
                buf.extend_from_slice(&d.seq.to_be_bytes());
                buf.extend_from_slice(&d.msg_id.to_be_bytes());
                buf.extend_from_slice(&d.ttl_secs.to_be_bytes());
                buf.extend_from_slice(&(d.ciphertext.len() as u32).to_be_bytes());
                buf.extend_from_slice(&d.ciphertext);
                buf.extend_from_slice(&(d.signature.len() as u16).to_be_bytes());
                buf.extend_from_slice(&d.signature);
                buf
            }
            NetworkFrame::AntiEntropyHave(h) => {
                // [marks count u32][marks: feed(16)+seq(8)]…[interests count u32][interests: node(20)]…
                let mut buf =
                    Vec::with_capacity(1 + 4 + h.marks.len() * 24 + 4 + h.interests.len() * 20);
                buf.push(FrameKind::AntiEntropyHave as u8);
                buf.extend_from_slice(&(h.marks.len() as u32).to_be_bytes());
                for (feed, seq) in &h.marks {
                    buf.extend_from_slice(feed);
                    buf.extend_from_slice(&seq.to_be_bytes());
                }
                buf.extend_from_slice(&(h.interests.len() as u32).to_be_bytes());
                for addr in &h.interests {
                    buf.extend_from_slice(addr.as_bytes());
                }
                buf
            }
            NetworkFrame::AntiEntropyOffer(o) => {
                let mut buf = Vec::with_capacity(1 + 4 + o.entries.len() * (16 + 8 + 8));
                buf.push(FrameKind::AntiEntropyOffer as u8);
                buf.extend_from_slice(&(o.entries.len() as u32).to_be_bytes());
                for (feed, seq, msg_id) in &o.entries {
                    buf.extend_from_slice(feed);
                    buf.extend_from_slice(&seq.to_be_bytes());
                    buf.extend_from_slice(&msg_id.to_be_bytes());
                }
                buf
            }
            NetworkFrame::AntiEntropyRequest(r) => {
                let mut buf = Vec::with_capacity(1 + 4 + r.msg_ids.len() * 8);
                buf.push(FrameKind::AntiEntropyRequest as u8);
                buf.extend_from_slice(&(r.msg_ids.len() as u32).to_be_bytes());
                for msg_id in &r.msg_ids {
                    buf.extend_from_slice(&msg_id.to_be_bytes());
                }
                buf
            }
        }
    }

    /// Decode wire frame. Returns:
    /// - `Ok(frame)` для known FrameKind values
    /// - `Err(UnknownKind)` если первый byte не из FrameKind enum'а — caller
    ///   обычно просто drop'ает и идёт дальше (forward compat behaviour)
    /// - `Err(other)` для malformed bodies of known kinds
    pub fn decode_from_wire(bytes: &[u8]) -> Result<Self, NetworkFrameError> {
        let tag = *bytes.first().ok_or(NetworkFrameError::Empty)?;
        let kind = FrameKind::from_tag(tag).ok_or(NetworkFrameError::UnknownKind(tag))?;
        let body = &bytes[1..];
        match kind {
            FrameKind::Announce => {
                let announce = Announce::decode_from_wire(body)?;
                Ok(NetworkFrame::Announce(announce))
            }
            FrameKind::ChallengeRequest => {
                if body.len() != CHALLENGE_NONCE_BYTES + 20 + 20 {
                    return Err(NetworkFrameError::BodyLength(
                        "ChallengeRequest",
                        body.len(),
                    ));
                }
                let mut nonce = [0u8; CHALLENGE_NONCE_BYTES];
                nonce.copy_from_slice(&body[..CHALLENGE_NONCE_BYTES]);
                let mut from_bytes = [0u8; 20];
                from_bytes.copy_from_slice(&body[CHALLENGE_NONCE_BYTES..CHALLENGE_NONCE_BYTES + 20]);
                let mut to_bytes = [0u8; 20];
                to_bytes.copy_from_slice(&body[CHALLENGE_NONCE_BYTES + 20..]);
                Ok(NetworkFrame::ChallengeRequest(ChallengeRequest {
                    nonce,
                    from: NodeAddress::from_bytes(from_bytes),
                    to: NodeAddress::from_bytes(to_bytes),
                }))
            }
            FrameKind::ChallengeResponse => {
                if body.len() != CHALLENGE_NONCE_BYTES + 20 + 20 + ChallengeResponse::WIRE_SIZE {
                    return Err(NetworkFrameError::BodyLength(
                        "ChallengeResponse",
                        body.len(),
                    ));
                }
                let mut offset = 0;
                let mut nonce = [0u8; CHALLENGE_NONCE_BYTES];
                nonce.copy_from_slice(&body[offset..offset + CHALLENGE_NONCE_BYTES]);
                offset += CHALLENGE_NONCE_BYTES;
                let mut from_bytes = [0u8; 20];
                from_bytes.copy_from_slice(&body[offset..offset + 20]);
                offset += 20;
                let mut to_bytes = [0u8; 20];
                to_bytes.copy_from_slice(&body[offset..offset + 20]);
                offset += 20;
                let response = ChallengeResponse::decode_from_wire(&body[offset..])?;
                Ok(NetworkFrame::ChallengeResponse(ChallengeResponseFrame {
                    nonce,
                    from: NodeAddress::from_bytes(from_bytes),
                    to: NodeAddress::from_bytes(to_bytes),
                    response,
                }))
            }
            FrameKind::Data => {
                // [from 20][to 20][hop_limit 1][msg_id 8][payload_len 4][payload]
                const DATA_HEADER: usize = 20 + 20 + 1 + 8 + 4; // = 53
                if body.len() < DATA_HEADER {
                    return Err(NetworkFrameError::BodyLength("Data", body.len()));
                }
                let mut from_bytes = [0u8; 20];
                from_bytes.copy_from_slice(&body[..20]);
                let mut to_bytes = [0u8; 20];
                to_bytes.copy_from_slice(&body[20..40]);
                let hop_limit = body[40];
                let msg_id = u64::from_be_bytes(body[41..49].try_into().unwrap());
                let payload_len = u32::from_be_bytes(body[49..53].try_into().unwrap()) as usize;
                // Anti-DoS: payload не должен быть нелепо большой даже если
                // нам говорят что таков.
                if payload_len > MAX_DATA_PAYLOAD {
                    return Err(NetworkFrameError::BodyLength(
                        "Data payload too large",
                        payload_len,
                    ));
                }
                if body.len() != DATA_HEADER + payload_len {
                    return Err(NetworkFrameError::BodyLength(
                        "Data payload length mismatch",
                        body.len(),
                    ));
                }
                let payload = body[DATA_HEADER..].to_vec();
                Ok(NetworkFrame::Data(DataFrame {
                    from: NodeAddress::from_bytes(from_bytes),
                    to: NodeAddress::from_bytes(to_bytes),
                    hop_limit,
                    msg_id,
                    payload,
                }))
            }
            FrameKind::FriendRelayCarry => {
                // [to 20][payload_len 4][payload]
                const HEADER: usize = 20 + 4;
                if body.len() < HEADER {
                    return Err(NetworkFrameError::BodyLength("FriendRelayCarry", body.len()));
                }
                let mut to_bytes = [0u8; 20];
                to_bytes.copy_from_slice(&body[..20]);
                let payload_len = u32::from_be_bytes(body[20..24].try_into().unwrap()) as usize;
                if payload_len > MAX_DATA_PAYLOAD {
                    return Err(NetworkFrameError::BodyLength(
                        "FriendRelayCarry payload too large",
                        payload_len,
                    ));
                }
                if body.len() != HEADER + payload_len {
                    return Err(NetworkFrameError::BodyLength(
                        "FriendRelayCarry payload length mismatch",
                        body.len(),
                    ));
                }
                Ok(NetworkFrame::FriendRelayCarry(FriendRelayCarryFrame {
                    to: NodeAddress::from_bytes(to_bytes),
                    payload: body[HEADER..].to_vec(),
                }))
            }
            FrameKind::ObservedAddress => {
                let observed = decode_socket_addr(body)?;
                Ok(NetworkFrame::ObservedAddress(ObservedAddressFrame { observed }))
            }
            FrameKind::DialBackRequest => {
                if body.len() < 8 {
                    return Err(NetworkFrameError::BodyLength("DialBackRequest", body.len()));
                }
                let nonce = u64::from_be_bytes(body[..8].try_into().unwrap());
                let candidate = decode_socket_addr(&body[8..])?;
                Ok(NetworkFrame::DialBackRequest(DialBackRequestFrame {
                    nonce,
                    candidate,
                }))
            }
            FrameKind::DialBackProbe => {
                if body.len() != 8 {
                    return Err(NetworkFrameError::BodyLength("DialBackProbe", body.len()));
                }
                let nonce = u64::from_be_bytes(body[..8].try_into().unwrap());
                Ok(NetworkFrame::DialBackProbe(DialBackProbeFrame { nonce }))
            }
            FrameKind::TcpReachabilityRequest => {
                if body.len() < 8 {
                    return Err(NetworkFrameError::BodyLength("TcpReachabilityRequest", body.len()));
                }
                let nonce = u64::from_be_bytes(body[..8].try_into().unwrap());
                let candidate = decode_socket_addr(&body[8..])?;
                Ok(NetworkFrame::TcpReachabilityRequest(TcpReachabilityRequestFrame { nonce, candidate }))
            }
            FrameKind::TcpReachabilityResult => {
                if body.len() != 9 {
                    return Err(NetworkFrameError::BodyLength("TcpReachabilityResult", body.len()));
                }
                let nonce = u64::from_be_bytes(body[..8].try_into().unwrap());
                Ok(NetworkFrame::TcpReachabilityResult(TcpReachabilityResultFrame {
                    nonce,
                    reachable: body[8] != 0,
                }))
            }
            FrameKind::PunchRequest => {
                if body.len() != 20 + 8 {
                    return Err(NetworkFrameError::BodyLength("PunchRequest", body.len()));
                }
                let mut target = [0u8; 20];
                target.copy_from_slice(&body[..20]);
                let nonce = u64::from_be_bytes(body[20..28].try_into().unwrap());
                Ok(NetworkFrame::PunchRequest(PunchRequestFrame {
                    target: NodeAddress::from_bytes(target),
                    nonce,
                }))
            }
            FrameKind::PunchSync => {
                if body.len() < 20 + 8 {
                    return Err(NetworkFrameError::BodyLength("PunchSync", body.len()));
                }
                let mut peer = [0u8; 20];
                peer.copy_from_slice(&body[..20]);
                let nonce = u64::from_be_bytes(body[20..28].try_into().unwrap());
                let peer_endpoint = decode_socket_addr(&body[28..])?;
                Ok(NetworkFrame::PunchSync(PunchSyncFrame {
                    peer: NodeAddress::from_bytes(peer),
                    peer_endpoint,
                    nonce,
                }))
            }
            // Liveness probe / reply — payloadless; ignore any trailing bytes.
            FrameKind::Ping => Ok(NetworkFrame::Ping),
            FrameKind::Pong => Ok(NetworkFrame::Pong),
            FrameKind::PathRequest => {
                if body.len() != 20 + 20 + 8 + 1 {
                    return Err(NetworkFrameError::BodyLength("PathRequest", body.len()));
                }
                let mut origin = [0u8; 20];
                origin.copy_from_slice(&body[..20]);
                let mut target = [0u8; 20];
                target.copy_from_slice(&body[20..40]);
                let nonce = u64::from_be_bytes(body[40..48].try_into().unwrap());
                let hop_limit = body[48];
                Ok(NetworkFrame::PathRequest(PathRequestFrame {
                    origin: NodeAddress::from_bytes(origin),
                    target: NodeAddress::from_bytes(target),
                    nonce,
                    hop_limit,
                }))
            }
            FrameKind::PathReply => {
                if body.len() != 20 + 20 + 8 + 1 {
                    return Err(NetworkFrameError::BodyLength("PathReply", body.len()));
                }
                let mut origin = [0u8; 20];
                origin.copy_from_slice(&body[..20]);
                let mut target = [0u8; 20];
                target.copy_from_slice(&body[20..40]);
                let nonce = u64::from_be_bytes(body[40..48].try_into().unwrap());
                let cost = body[48];
                Ok(NetworkFrame::PathReply(PathReplyFrame {
                    origin: NodeAddress::from_bytes(origin),
                    target: NodeAddress::from_bytes(target),
                    nonce,
                    cost,
                }))
            }
            FrameKind::PeerPing | FrameKind::PeerPong => {
                if body.len() != 20 + 20 + 8 + 1 {
                    let label = if matches!(kind, FrameKind::PeerPing) {
                        "PeerPing"
                    } else {
                        "PeerPong"
                    };
                    return Err(NetworkFrameError::BodyLength(label, body.len()));
                }
                let mut from = [0u8; 20];
                from.copy_from_slice(&body[..20]);
                let mut to = [0u8; 20];
                to.copy_from_slice(&body[20..40]);
                let nonce = u64::from_be_bytes(body[40..48].try_into().unwrap());
                let hop_limit = body[48];
                let from = NodeAddress::from_bytes(from);
                let to = NodeAddress::from_bytes(to);
                if matches!(kind, FrameKind::PeerPing) {
                    Ok(NetworkFrame::PeerPing(PeerPingFrame { from, to, nonce, hop_limit }))
                } else {
                    Ok(NetworkFrame::PeerPong(PeerPongFrame { from, to, nonce, hop_limit }))
                }
            }
            FrameKind::DhtProxyRequest => {
                // [from 20][to 20][req_id 8][op 1][hop_limit 1][key 32][salt_len 1][salt][value_len 2][value]
                const HEADER: usize = 20 + 20 + 8 + 1 + 1 + 32 + 1; // through salt_len
                if body.len() < HEADER {
                    return Err(NetworkFrameError::BodyLength("DhtProxyRequest", body.len()));
                }
                let mut off = 0;
                let mut from = [0u8; 20];
                from.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let mut to = [0u8; 20];
                to.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let req_id = u64::from_be_bytes(body[off..off + 8].try_into().unwrap());
                off += 8;
                let op = DhtProxyOp::from_tag(body[off])
                    .ok_or(NetworkFrameError::BodyLength("DhtProxyRequest op", body[off] as usize))?;
                off += 1;
                let hop_limit = body[off];
                off += 1;
                let mut key = [0u8; 32];
                key.copy_from_slice(&body[off..off + 32]);
                off += 32;
                let salt_len = body[off] as usize;
                off += 1;
                if salt_len > DHT_PROXY_MAX_SALT {
                    return Err(NetworkFrameError::BodyLength("DhtProxyRequest salt", salt_len));
                }
                if body.len() < off + salt_len + 2 {
                    return Err(NetworkFrameError::BodyLength("DhtProxyRequest", body.len()));
                }
                let salt = body[off..off + salt_len].to_vec();
                off += salt_len;
                let value_len = u16::from_be_bytes(body[off..off + 2].try_into().unwrap()) as usize;
                off += 2;
                if value_len > DHT_PROXY_MAX_VALUE {
                    return Err(NetworkFrameError::BodyLength("DhtProxyRequest value", value_len));
                }
                if body.len() != off + value_len {
                    return Err(NetworkFrameError::BodyLength(
                        "DhtProxyRequest length mismatch",
                        body.len(),
                    ));
                }
                let value = body[off..].to_vec();
                Ok(NetworkFrame::DhtProxyRequest(DhtProxyRequestFrame {
                    from: NodeAddress::from_bytes(from),
                    to: NodeAddress::from_bytes(to),
                    req_id,
                    op,
                    key,
                    salt,
                    value,
                    hop_limit,
                }))
            }
            FrameKind::DhtProxyResponse => {
                // [from 20][to 20][req_id 8][ok 1][hop_limit 1][value_len 2][value]
                const HEADER: usize = 20 + 20 + 8 + 1 + 1 + 2;
                if body.len() < HEADER {
                    return Err(NetworkFrameError::BodyLength("DhtProxyResponse", body.len()));
                }
                let mut off = 0;
                let mut from = [0u8; 20];
                from.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let mut to = [0u8; 20];
                to.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let req_id = u64::from_be_bytes(body[off..off + 8].try_into().unwrap());
                off += 8;
                let ok = body[off] != 0;
                off += 1;
                let hop_limit = body[off];
                off += 1;
                let value_len = u16::from_be_bytes(body[off..off + 2].try_into().unwrap()) as usize;
                off += 2;
                if value_len > DHT_PROXY_MAX_VALUE {
                    return Err(NetworkFrameError::BodyLength("DhtProxyResponse value", value_len));
                }
                if body.len() != off + value_len {
                    return Err(NetworkFrameError::BodyLength(
                        "DhtProxyResponse length mismatch",
                        body.len(),
                    ));
                }
                let value = body[off..].to_vec();
                Ok(NetworkFrame::DhtProxyResponse(DhtProxyResponseFrame {
                    from: NodeAddress::from_bytes(from),
                    to: NodeAddress::from_bytes(to),
                    req_id,
                    ok,
                    value,
                    hop_limit,
                }))
            }
            FrameKind::RelayCardReq => {
                if body.len() != 8 {
                    return Err(NetworkFrameError::BodyLength("RelayCardReq", body.len()));
                }
                let req_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
                Ok(NetworkFrame::RelayCardReq(RelayCardReqFrame { req_id }))
            }
            FrameKind::RelayCardResp => {
                if body.len() < 10 {
                    return Err(NetworkFrameError::BodyLength("RelayCardResp", body.len()));
                }
                let req_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
                let len = u16::from_be_bytes(body[8..10].try_into().unwrap()) as usize;
                if len > RELAY_CARD_MAX || body.len() != 10 + len {
                    return Err(NetworkFrameError::BodyLength("RelayCardResp card", body.len()));
                }
                Ok(NetworkFrame::RelayCardResp(RelayCardRespFrame { req_id, card: body[10..].to_vec() }))
            }
            FrameKind::DhtBootReq => {
                // [req_id 8]
                if body.len() != 8 {
                    return Err(NetworkFrameError::BodyLength("DhtBootReq", body.len()));
                }
                let req_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
                Ok(NetworkFrame::DhtBootReq(DhtBootReqFrame { req_id }))
            }
            FrameKind::DhtBootResp => {
                // [req_id 8][count 1][addr (1+4|16+2) ...]
                const HEADER: usize = 8 + 1;
                if body.len() < HEADER {
                    return Err(NetworkFrameError::BodyLength("DhtBootResp", body.len()));
                }
                let req_id = u64::from_be_bytes(body[0..8].try_into().unwrap());
                let count = body[8] as usize;
                if count > DHT_BOOT_MAX_NODES {
                    return Err(NetworkFrameError::BodyLength("DhtBootResp count", count));
                }
                let mut off = HEADER;
                let mut nodes = Vec::with_capacity(count);
                for _ in 0..count {
                    // The family byte tells us this address's length; slice it exactly for decode.
                    let family = *body
                        .get(off)
                        .ok_or(NetworkFrameError::BodyLength("DhtBootResp addr", body.len()))?;
                    let addr_len = match family {
                        4 => 1 + 4 + 2,
                        6 => 1 + 16 + 2,
                        _ => {
                            return Err(NetworkFrameError::BodyLength(
                                "DhtBootResp addr family",
                                family as usize,
                            ))
                        }
                    };
                    if body.len() < off + addr_len {
                        return Err(NetworkFrameError::BodyLength("DhtBootResp addr", body.len()));
                    }
                    nodes.push(decode_socket_addr(&body[off..off + addr_len])?);
                    off += addr_len;
                }
                if off != body.len() {
                    return Err(NetworkFrameError::BodyLength("DhtBootResp length mismatch", body.len()));
                }
                Ok(NetworkFrame::DhtBootResp(DhtBootRespFrame { req_id, nodes }))
            }
            FrameKind::DurableData => {
                // [to 20][from 20][feed 16][seq 8][msg_id 8][ttl 4][clen 4][ciphertext][slen 2][sig]
                const HEADER: usize = 20 + 20 + 16 + 8 + 8 + 4 + 4;
                if body.len() < HEADER {
                    return Err(NetworkFrameError::BodyLength("DurableData", body.len()));
                }
                let mut off = 0;
                let mut to = [0u8; 20];
                to.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let mut from = [0u8; 20];
                from.copy_from_slice(&body[off..off + 20]);
                off += 20;
                let mut feed_id = [0u8; 16];
                feed_id.copy_from_slice(&body[off..off + 16]);
                off += 16;
                let seq = u64::from_be_bytes(body[off..off + 8].try_into().unwrap());
                off += 8;
                let msg_id = u64::from_be_bytes(body[off..off + 8].try_into().unwrap());
                off += 8;
                let ttl_secs = u32::from_be_bytes(body[off..off + 4].try_into().unwrap());
                off += 4;
                let clen = u32::from_be_bytes(body[off..off + 4].try_into().unwrap()) as usize;
                off += 4;
                if clen > MAX_DATA_PAYLOAD {
                    return Err(NetworkFrameError::BodyLength("DurableData ciphertext", clen));
                }
                if body.len() < off + clen + 2 {
                    return Err(NetworkFrameError::BodyLength("DurableData", body.len()));
                }
                let ciphertext = body[off..off + clen].to_vec();
                off += clen;
                let slen = u16::from_be_bytes(body[off..off + 2].try_into().unwrap()) as usize;
                off += 2;
                if body.len() != off + slen {
                    return Err(NetworkFrameError::BodyLength("DurableData sig", body.len()));
                }
                let signature = body[off..off + slen].to_vec();
                Ok(NetworkFrame::DurableData(DurableDataFrame {
                    to: NodeAddress::from_bytes(to),
                    from: NodeAddress::from_bytes(from),
                    feed_id,
                    seq,
                    msg_id,
                    ttl_secs,
                    ciphertext,
                    signature,
                }))
            }
            FrameKind::AntiEntropyHave => {
                // Two count-prefixed lists back to back: marks (24 B) then interests (20 B).
                let name = "AntiEntropyHave";
                if body.len() < 4 {
                    return Err(NetworkFrameError::BodyLength(name, body.len()));
                }
                let mcount = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
                if mcount > MAX_ANTI_ENTROPY_ENTRIES {
                    return Err(NetworkFrameError::BodyLength(name, mcount));
                }
                let marks_end = 4 + mcount * 24;
                if body.len() < marks_end + 4 {
                    return Err(NetworkFrameError::BodyLength(name, body.len()));
                }
                let mut marks = Vec::with_capacity(mcount);
                for i in 0..mcount {
                    let s = 4 + i * 24;
                    let mut feed = [0u8; 16];
                    feed.copy_from_slice(&body[s..s + 16]);
                    marks.push((feed, u64::from_be_bytes(body[s + 16..s + 24].try_into().unwrap())));
                }
                let icount =
                    u32::from_be_bytes(body[marks_end..marks_end + 4].try_into().unwrap()) as usize;
                if icount > MAX_ANTI_ENTROPY_ENTRIES {
                    return Err(NetworkFrameError::BodyLength(name, icount));
                }
                let interests_end = marks_end + 4 + icount * 20;
                if body.len() != interests_end {
                    return Err(NetworkFrameError::BodyLength(name, body.len()));
                }
                let mut interests = Vec::with_capacity(icount);
                for i in 0..icount {
                    let s = marks_end + 4 + i * 20;
                    let mut node = [0u8; 20];
                    node.copy_from_slice(&body[s..s + 20]);
                    interests.push(NodeAddress::from_bytes(node));
                }
                Ok(NetworkFrame::AntiEntropyHave(AntiEntropyHaveFrame {
                    marks,
                    interests,
                }))
            }
            FrameKind::AntiEntropyOffer => {
                let entries = decode_count_prefixed(body, 32, "AntiEntropyOffer", |c| {
                    let mut feed = [0u8; 16];
                    feed.copy_from_slice(&c[..16]);
                    (
                        feed,
                        u64::from_be_bytes(c[16..24].try_into().unwrap()),
                        u64::from_be_bytes(c[24..32].try_into().unwrap()),
                    )
                })?;
                Ok(NetworkFrame::AntiEntropyOffer(AntiEntropyOfferFrame { entries }))
            }
            FrameKind::AntiEntropyRequest => {
                let msg_ids = decode_count_prefixed(body, 8, "AntiEntropyRequest", |c| {
                    u64::from_be_bytes(c[..8].try_into().unwrap())
                })?;
                Ok(NetworkFrame::AntiEntropyRequest(AntiEntropyRequestFrame {
                    msg_ids,
                }))
            }
        }
    }
}

/// Decode a `[u32 count][count × entry_size bytes]` list, parsing each fixed-size
/// entry with `parse`. Strict on total length; bounded by `MAX_ANTI_ENTROPY_ENTRIES`
/// before allocating so a forged count can't OOM us.
fn decode_count_prefixed<T>(
    body: &[u8],
    entry_size: usize,
    name: &'static str,
    mut parse: impl FnMut(&[u8]) -> T,
) -> Result<Vec<T>, NetworkFrameError> {
    if body.len() < 4 {
        return Err(NetworkFrameError::BodyLength(name, body.len()));
    }
    let count = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
    if count > MAX_ANTI_ENTROPY_ENTRIES {
        return Err(NetworkFrameError::BodyLength(name, count));
    }
    if body.len() != 4 + count * entry_size {
        return Err(NetworkFrameError::BodyLength(name, body.len()));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = 4 + i * entry_size;
        out.push(parse(&body[start..start + entry_size]));
    }
    Ok(out)
}

/// Encode a SocketAddr: `[family 1 (4=v4|6=v6)][ip 4|16][port 2 BE]`.
fn encode_socket_addr(buf: &mut Vec<u8>, addr: &SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(4);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            buf.push(6);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
}

/// Decode a SocketAddr encoded by `encode_socket_addr`. Strict on length.
fn decode_socket_addr(body: &[u8]) -> Result<SocketAddr, NetworkFrameError> {
    match body.first() {
        Some(4) => {
            if body.len() != 1 + 4 + 2 {
                return Err(NetworkFrameError::BodyLength("ObservedAddress v4", body.len()));
            }
            let ip = std::net::Ipv4Addr::new(body[1], body[2], body[3], body[4]);
            let port = u16::from_be_bytes([body[5], body[6]]);
            Ok(SocketAddr::from((ip, port)))
        }
        Some(6) => {
            if body.len() != 1 + 16 + 2 {
                return Err(NetworkFrameError::BodyLength("ObservedAddress v6", body.len()));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&body[1..17]);
            let ip = std::net::Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([body[17], body[18]]);
            Ok(SocketAddr::from((ip, port)))
        }
        _ => Err(NetworkFrameError::BodyLength(
            "ObservedAddress family",
            body.len(),
        )),
    }
}

/// Max payload bytes в одном Data frame. Frames больше этого размера
/// caller'у нужно chunk'ать на куски. 64 KB = разумный compromise
/// между MTU и practicality.
pub const MAX_DATA_PAYLOAD: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum NetworkFrameError {
    #[error("empty frame buffer")]
    Empty,
    #[error("unknown frame kind tag 0x{0:02X}")]
    UnknownKind(u8),
    #[error("body length wrong for {0}: {1}")]
    BodyLength(&'static str, usize),
    #[error("announce decode failed: {0}")]
    Announce(#[from] AnnounceError),
    #[error("cert/challenge decode failed: {0}")]
    Cert(#[from] CertError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert::{fresh_challenge_nonce, respond_to_challenge};
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn fresh_addr() -> NodeAddress {
        NodeAddress::from_pubkey(&fresh_key().verifying_key())
    }

    #[test]
    fn frame_kind_tag_round_trip() {
        for kind in [
            FrameKind::Announce,
            FrameKind::ChallengeRequest,
            FrameKind::ChallengeResponse,
            FrameKind::Data,
        ] {
            let tag = kind as u8;
            let parsed = FrameKind::from_tag(tag).expect("tag должен parse'ить обратно");
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn frame_kind_rejects_unknown_tag() {
        assert_eq!(FrameKind::from_tag(0x00), None);
        assert_eq!(FrameKind::from_tag(0xFF), None);
        assert_eq!(FrameKind::from_tag(0xA1), None); // близко к Announce 0xA0
    }

    #[test]
    fn empty_buffer_returns_empty_error() {
        let result = NetworkFrame::decode_from_wire(&[]);
        assert!(matches!(result, Err(NetworkFrameError::Empty)));
    }

    #[test]
    fn peer_ping_pong_frames_round_trip() {
        // Routed end-to-end liveness frames (relay-only-peer presence honesty). See
        // docs/bugs/relay-presence-false-online-dead-peer.md.
        let from = NodeAddress::from_bytes([0xAA; 20]);
        let to = NodeAddress::from_bytes([0xBB; 20]);
        let ping = NetworkFrame::PeerPing(PeerPingFrame {
            from,
            to,
            nonce: 0x0123_4567_89AB_CDEF,
            hop_limit: 6,
        });
        let bytes = ping.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::PeerPing as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), ping);

        let pong = NetworkFrame::PeerPong(PeerPongFrame {
            from: to,
            to: from,
            nonce: 0x0123_4567_89AB_CDEF,
            hop_limit: 5,
        });
        let bytes = pong.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::PeerPong as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), pong);

        // A wrong body length is rejected, not silently mis-parsed.
        assert!(NetworkFrame::decode_from_wire(&[FrameKind::PeerPing as u8, 1, 2, 3]).is_err());
    }

    #[test]
    fn dht_proxy_frames_round_trip() {
        // DHT-as-a-service over the mesh (R7 Phase 2 mode b). See docs/decisions/dht-over-peer.md.
        let from = NodeAddress::from_bytes([0x11; 20]);
        let to = NodeAddress::from_bytes([0x22; 20]);

        // A Put request with a real salt + value round-trips field-for-field.
        let put = NetworkFrame::DhtProxyRequest(DhtProxyRequestFrame {
            from,
            to,
            req_id: 0xDEAD_BEEF_0BAD_F00D,
            op: DhtProxyOp::Put,
            key: [0x5A; 32],
            salt: b"0".to_vec(),
            value: b"sealed-beacon-bytes".to_vec(),
            hop_limit: 6,
        });
        let bytes = put.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::DhtProxyRequest as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), put);

        // A Get request carries an empty value.
        let get = NetworkFrame::DhtProxyRequest(DhtProxyRequestFrame {
            from,
            to,
            req_id: 7,
            op: DhtProxyOp::Get,
            key: [0xA5; 32],
            salt: b"1".to_vec(),
            value: Vec::new(),
            hop_limit: 6,
        });
        assert_eq!(NetworkFrame::decode_from_wire(&get.encode_to_wire()).unwrap(), get);

        // The response round-trips, ok flag + value preserved.
        let resp = NetworkFrame::DhtProxyResponse(DhtProxyResponseFrame {
            from: to,
            to: from,
            req_id: 0xDEAD_BEEF_0BAD_F00D,
            ok: true,
            value: b"the-other-slot".to_vec(),
            hop_limit: 5,
        });
        let bytes = resp.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::DhtProxyResponse as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), resp);

        // Caps enforced on decode: an over-cap value_len is rejected, not trusted.
        let mut bad = NetworkFrame::DhtProxyResponse(DhtProxyResponseFrame {
            from,
            to,
            req_id: 1,
            ok: true,
            value: Vec::new(),
            hop_limit: 1,
        })
        .encode_to_wire();
        let vlen = (DHT_PROXY_MAX_VALUE as u16 + 1).to_be_bytes();
        let n = bad.len();
        bad[n - 2] = vlen[0];
        bad[n - 1] = vlen[1];
        assert!(NetworkFrame::decode_from_wire(&bad).is_err(), "over-cap value_len rejected");

        // A truncated request body is rejected, not mis-parsed.
        assert!(
            NetworkFrame::decode_from_wire(&[FrameKind::DhtProxyRequest as u8, 1, 2, 3]).is_err()
        );
    }

    #[test]
    fn relay_card_frames_round_trip_and_cap_the_card() {
        let req = NetworkFrame::RelayCardReq(RelayCardReqFrame { req_id: 7 });
        assert_eq!(NetworkFrame::decode_from_wire(&req.encode_to_wire()).unwrap(), req);
        let resp = NetworkFrame::RelayCardResp(RelayCardRespFrame { req_id: 7, card: b"entry".to_vec() });
        assert_eq!(NetworkFrame::decode_from_wire(&resp.encode_to_wire()).unwrap(), resp);
        let huge = NetworkFrame::RelayCardResp(RelayCardRespFrame { req_id: 1, card: vec![9; RELAY_CARD_MAX + 50] });
        let NetworkFrame::RelayCardResp(back) = NetworkFrame::decode_from_wire(&huge.encode_to_wire()).unwrap() else {
            panic!("not a card");
        };
        assert_eq!(back.card.len(), RELAY_CARD_MAX, "the encoder caps the card");
    }

    #[test]
    fn dht_boot_frames_round_trip() {
        // Direct-neighbour DHT bootstrap seed exchange (step B). See
        // docs/decisions/dht-bootstrap-mesh-propagation.md.
        let req = NetworkFrame::DhtBootReq(DhtBootReqFrame { req_id: 0xABCD_1234_5678_9AA5 });
        let bytes = req.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::DhtBootReq as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), req);

        // A response with a mix of v4 + v6 candidates round-trips field-for-field.
        let resp = NetworkFrame::DhtBootResp(DhtBootRespFrame {
            req_id: 0xABCD_1234_5678_9AA5,
            nodes: vec![
                "203.0.113.7:6881".parse().unwrap(),
                "[2001:db8::1]:6881".parse().unwrap(),
            ],
        });
        let bytes = resp.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::DhtBootResp as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&bytes).unwrap(), resp);

        // An empty candidate list ("I have none / not sharing") round-trips.
        let empty = NetworkFrame::DhtBootResp(DhtBootRespFrame { req_id: 1, nodes: Vec::new() });
        assert_eq!(NetworkFrame::decode_from_wire(&empty.encode_to_wire()).unwrap(), empty);

        // The encoder truncates an over-cap list to DHT_BOOT_MAX_NODES (curated, bounded).
        let over = NetworkFrame::DhtBootResp(DhtBootRespFrame {
            req_id: 2,
            nodes: (0..(DHT_BOOT_MAX_NODES as u16 + 5))
                .map(|i| SocketAddr::from(([10, 0, 0, 1], 6000 + i)))
                .collect(),
        });
        let decoded = NetworkFrame::decode_from_wire(&over.encode_to_wire()).unwrap();
        match decoded {
            NetworkFrame::DhtBootResp(f) => assert_eq!(f.nodes.len(), DHT_BOOT_MAX_NODES),
            other => panic!("expected DhtBootResp, got {other:?}"),
        }

        // A count byte over the cap is rejected on decode, not trusted.
        let mut bad = NetworkFrame::DhtBootResp(DhtBootRespFrame { req_id: 3, nodes: Vec::new() })
            .encode_to_wire();
        bad[1 + 8] = (DHT_BOOT_MAX_NODES as u8) + 1; // the count byte
        assert!(NetworkFrame::decode_from_wire(&bad).is_err(), "over-cap count rejected");

        // A truncated request body is rejected, not mis-parsed.
        assert!(NetworkFrame::decode_from_wire(&[FrameKind::DhtBootReq as u8, 1, 2]).is_err());
    }

    #[test]
    fn unknown_tag_byte_returns_unknown_kind() {
        let bytes = [0xFFu8, 1, 2, 3];
        let result = NetworkFrame::decode_from_wire(&bytes);
        assert!(matches!(result, Err(NetworkFrameError::UnknownKind(0xFF))));
    }

    #[test]
    fn announce_frame_round_trip() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_700_000_000_000, 300);
        let frame = NetworkFrame::Announce(announce.clone());
        let bytes = frame.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::Announce as u8);

        let decoded = NetworkFrame::decode_from_wire(&bytes).unwrap();
        match decoded {
            NetworkFrame::Announce(a) => assert_eq!(a, announce),
            other => panic!("expected Announce, got {other:?}"),
        }
    }

    #[test]
    fn challenge_request_frame_round_trip() {
        let req = ChallengeRequest {
            nonce: fresh_challenge_nonce(),
            from: fresh_addr(),
            to: fresh_addr(),
        };
        let frame = NetworkFrame::ChallengeRequest(req.clone());
        let bytes = frame.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::ChallengeRequest as u8);

        let decoded = NetworkFrame::decode_from_wire(&bytes).unwrap();
        match decoded {
            NetworkFrame::ChallengeRequest(parsed) => assert_eq!(parsed, req),
            other => panic!("expected ChallengeRequest, got {other:?}"),
        }
    }

    #[test]
    fn challenge_response_frame_round_trip() {
        let responder_key = fresh_key();
        let nonce = fresh_challenge_nonce();
        let response = respond_to_challenge(&responder_key, &nonce);

        let frame_resp = NetworkFrame::ChallengeResponse(ChallengeResponseFrame {
            nonce,
            from: NodeAddress::from_pubkey(&responder_key.verifying_key()),
            to: fresh_addr(),
            response: response.clone(),
        });
        let bytes = frame_resp.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::ChallengeResponse as u8);

        let decoded = NetworkFrame::decode_from_wire(&bytes).unwrap();
        match decoded {
            NetworkFrame::ChallengeResponse(parsed) => {
                assert_eq!(parsed.nonce, nonce);
                assert_eq!(parsed.response, response);
            }
            other => panic!("expected ChallengeResponse, got {other:?}"),
        }
    }

    #[test]
    fn data_frame_round_trip() {
        let data = DataFrame::origin(fresh_addr(), fresh_addr(), b"hello world".to_vec());
        let frame = NetworkFrame::Data(data.clone());
        let bytes = frame.encode_to_wire();
        assert_eq!(bytes[0], FrameKind::Data as u8);

        let decoded = NetworkFrame::decode_from_wire(&bytes).unwrap();
        match decoded {
            NetworkFrame::Data(parsed) => assert_eq!(parsed, data),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn path_request_and_reply_round_trip() {
        let origin = fresh_addr();
        let target = fresh_addr();
        let req = NetworkFrame::PathRequest(PathRequestFrame {
            origin,
            target,
            nonce: 0xDEAD_BEEF,
            hop_limit: 4,
        });
        let wire = req.encode_to_wire();
        assert_eq!(wire[0], FrameKind::PathRequest as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), req);

        let rep = NetworkFrame::PathReply(PathReplyFrame {
            origin,
            target,
            nonce: 0xDEAD_BEEF,
            cost: 2,
        });
        let wire = rep.encode_to_wire();
        assert_eq!(wire[0], FrameKind::PathReply as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), rep);

        // Length guard: a truncated body is rejected, not misparsed.
        assert!(matches!(
            NetworkFrame::decode_from_wire(&wire[..wire.len() - 1]),
            Err(NetworkFrameError::BodyLength(_, _))
        ));
    }

    #[test]
    fn data_frame_with_empty_payload_round_trips() {
        let data = DataFrame::origin(fresh_addr(), fresh_addr(), vec![]);
        let frame = NetworkFrame::Data(data.clone());
        let bytes = frame.encode_to_wire();
        let decoded = NetworkFrame::decode_from_wire(&bytes).unwrap();
        match decoded {
            NetworkFrame::Data(parsed) => assert_eq!(parsed.payload.len(), 0),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn data_frame_oversized_payload_rejected_at_decode() {
        // Forge wire bytes claiming payload size > MAX. Decoder должен
        // отвергнуть до allocate'а большого Vec'а.
        let mut bytes = vec![FrameKind::Data as u8];
        bytes.extend_from_slice(&[0u8; 20]); // from
        bytes.extend_from_slice(&[0u8; 20]); // to
        bytes.push(16u8); // hop_limit
        bytes.extend_from_slice(&0u64.to_be_bytes()); // msg_id
        bytes.extend_from_slice(&(MAX_DATA_PAYLOAD as u32 + 1).to_be_bytes());
        // No actual payload bytes added — decoder должен fail на length check'е.
        let result = NetworkFrame::decode_from_wire(&bytes);
        assert!(matches!(result, Err(NetworkFrameError::BodyLength(_, _))));
    }

    #[test]
    fn truncated_data_frame_rejected() {
        let data = DataFrame::origin(fresh_addr(), fresh_addr(), vec![1, 2, 3, 4]);
        let frame = NetworkFrame::Data(data);
        let full = frame.encode_to_wire();
        let truncated = &full[..full.len() - 2]; // отрезать 2 байта payload'а
        let result = NetworkFrame::decode_from_wire(truncated);
        assert!(matches!(result, Err(NetworkFrameError::BodyLength(_, _))));
    }

    #[test]
    fn observed_address_v4_round_trip() {
        let addr: std::net::SocketAddr = "203.0.113.7:51820".parse().unwrap();
        let frame = NetworkFrame::ObservedAddress(ObservedAddressFrame { observed: addr });
        let wire = frame.encode_to_wire();
        match NetworkFrame::decode_from_wire(&wire) {
            Ok(NetworkFrame::ObservedAddress(o)) => assert_eq!(o.observed, addr),
            other => panic!("expected ObservedAddress, got {other:?}"),
        }
    }

    #[test]
    fn observed_address_v6_round_trip() {
        let addr: std::net::SocketAddr = "[2001:db8::1]:443".parse().unwrap();
        let frame = NetworkFrame::ObservedAddress(ObservedAddressFrame { observed: addr });
        let wire = frame.encode_to_wire();
        match NetworkFrame::decode_from_wire(&wire) {
            Ok(NetworkFrame::ObservedAddress(o)) => assert_eq!(o.observed, addr),
            other => panic!("expected ObservedAddress, got {other:?}"),
        }
    }

    #[test]
    fn tcp_reachability_frames_round_trip() {
        // Task 3 — the TCP-reachability request (nonce + candidate, v4/v6) and result (nonce + bool)
        // survive encode → decode. See docs/decisions/tcp-direct-peer-fallback.md.
        for s in ["203.0.113.7:8443", "[2001:db8::1]:8443"] {
            let candidate: std::net::SocketAddr = s.parse().unwrap();
            let req = NetworkFrame::TcpReachabilityRequest(TcpReachabilityRequestFrame {
                nonce: 0x1122_3344_5566_7788,
                candidate,
            });
            let wire = req.encode_to_wire();
            assert_eq!(wire[0], FrameKind::TcpReachabilityRequest as u8);
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), req);
        }
        for reachable in [true, false] {
            let res = NetworkFrame::TcpReachabilityResult(TcpReachabilityResultFrame {
                nonce: 99,
                reachable,
            });
            let wire = res.encode_to_wire();
            assert_eq!(wire[0], FrameKind::TcpReachabilityResult as u8);
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), res);
        }
    }

    #[test]
    fn punch_request_round_trips() {
        let frame = NetworkFrame::PunchRequest(PunchRequestFrame {
            target: fresh_addr(),
            nonce: 0xABCD_1234_5678_9F0E,
        });
        let wire = frame.encode_to_wire();
        assert_eq!(wire[0], FrameKind::PunchRequest as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
    }

    #[test]
    fn punch_sync_round_trips_v4_and_v6() {
        for s in ["203.0.113.7:51820", "[2001:db8::1]:443"] {
            let endpoint: std::net::SocketAddr = s.parse().unwrap();
            let frame = NetworkFrame::PunchSync(PunchSyncFrame {
                peer: fresh_addr(),
                peer_endpoint: endpoint,
                nonce: 7,
            });
            let wire = frame.encode_to_wire();
            assert_eq!(wire[0], FrameKind::PunchSync as u8);
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
        }
    }

    #[test]
    fn friend_relay_carry_round_trips() {
        // Non-empty (a sealed payload) and empty both round-trip; the tag is FriendRelayCarry.
        for payload in [b"sealed-e2e-ciphertext-for-the-final-recipient".to_vec(), Vec::new()] {
            let frame = NetworkFrame::FriendRelayCarry(FriendRelayCarryFrame {
                to: fresh_addr(),
                payload,
            });
            let wire = frame.encode_to_wire();
            assert_eq!(wire[0], FrameKind::FriendRelayCarry as u8);
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
        }
    }

    #[test]
    fn ping_pong_round_trip_payloadless() {
        for (frame, tag) in [
            (NetworkFrame::Ping, FrameKind::Ping),
            (NetworkFrame::Pong, FrameKind::Pong),
        ] {
            let wire = frame.encode_to_wire();
            assert_eq!(wire, vec![tag as u8], "tag only, no body");
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
        }
        // Forward-compat: trailing bytes after the tag are ignored, not an error.
        assert_eq!(
            NetworkFrame::decode_from_wire(&[FrameKind::Ping as u8, 0xDE, 0xAD]).unwrap(),
            NetworkFrame::Ping,
        );
    }

    #[test]
    fn dial_back_request_round_trips_v4_and_v6() {
        for s in ["203.0.113.7:51820", "[2001:db8::1]:443"] {
            let candidate: std::net::SocketAddr = s.parse().unwrap();
            let frame = NetworkFrame::DialBackRequest(DialBackRequestFrame {
                nonce: 0xDEAD_BEEF_0102_0304,
                candidate,
            });
            let wire = frame.encode_to_wire();
            assert_eq!(wire[0], FrameKind::DialBackRequest as u8);
            match NetworkFrame::decode_from_wire(&wire) {
                Ok(NetworkFrame::DialBackRequest(parsed)) => {
                    assert_eq!(parsed.nonce, 0xDEAD_BEEF_0102_0304);
                    assert_eq!(parsed.candidate, candidate);
                }
                other => panic!("expected DialBackRequest, got {other:?}"),
            }
        }
    }

    #[test]
    fn dial_back_probe_round_trips() {
        let frame = NetworkFrame::DialBackProbe(DialBackProbeFrame { nonce: 42 });
        let wire = frame.encode_to_wire();
        assert_eq!(wire[0], FrameKind::DialBackProbe as u8);
        match NetworkFrame::decode_from_wire(&wire) {
            Ok(NetworkFrame::DialBackProbe(p)) => assert_eq!(p.nonce, 42),
            other => panic!("expected DialBackProbe, got {other:?}"),
        }
    }

    #[test]
    fn dial_back_probe_wrong_length_rejected() {
        let mut wire = vec![FrameKind::DialBackProbe as u8];
        wire.extend_from_slice(&[0u8; 4]); // only 4 bytes, need 8
        assert!(matches!(
            NetworkFrame::decode_from_wire(&wire),
            Err(NetworkFrameError::BodyLength(_, _))
        ));
    }

    fn fd(n: u8) -> FeedId {
        let mut f = [0u8; 16];
        f[0] = n;
        f
    }

    #[test]
    fn durable_data_frame_round_trips() {
        let frame = NetworkFrame::DurableData(DurableDataFrame {
            to: fresh_addr(),
            from: fresh_addr(),
            feed_id: fd(7),
            seq: 42,
            msg_id: 0xCAFE_F00D_1234_5678,
            ttl_secs: 3600,
            ciphertext: vec![1, 2, 3, 4, 5],
            signature: vec![9u8; 64],
        });
        let wire = frame.encode_to_wire();
        assert_eq!(wire[0], FrameKind::DurableData as u8);
        assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
    }

    #[test]
    fn durable_data_empty_ciphertext_and_sig_round_trip() {
        let frame = NetworkFrame::DurableData(DurableDataFrame {
            to: fresh_addr(),
            from: fresh_addr(),
            feed_id: fd(0),
            seq: 1,
            msg_id: 1,
            ttl_secs: 0,
            ciphertext: vec![],
            signature: vec![],
        });
        let wire = frame.encode_to_wire();
        assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
    }

    #[test]
    fn anti_entropy_frames_round_trip() {
        let have = NetworkFrame::AntiEntropyHave(AntiEntropyHaveFrame {
            marks: vec![(fd(1), 3), (fd(2), 0)],
            interests: vec![fresh_addr(), fresh_addr()],
        });
        let offer = NetworkFrame::AntiEntropyOffer(AntiEntropyOfferFrame {
            entries: vec![(fd(1), 4, 100), (fd(1), 5, 101)],
        });
        let request = NetworkFrame::AntiEntropyRequest(AntiEntropyRequestFrame {
            msg_ids: vec![100, 101, 102],
        });
        for frame in [have, offer, request] {
            let wire = frame.encode_to_wire();
            assert_eq!(NetworkFrame::decode_from_wire(&wire).unwrap(), frame);
        }
        // Empty lists round-trip too.
        let empty = NetworkFrame::AntiEntropyRequest(AntiEntropyRequestFrame { msg_ids: vec![] });
        assert_eq!(
            NetworkFrame::decode_from_wire(&empty.encode_to_wire()).unwrap(),
            empty
        );
    }

    #[test]
    fn anti_entropy_oversized_count_rejected_before_alloc() {
        // Forge a Have claiming a huge count with no entries → reject on the cap,
        // not by trying to allocate.
        let mut wire = vec![FrameKind::AntiEntropyHave as u8];
        wire.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            NetworkFrame::decode_from_wire(&wire),
            Err(NetworkFrameError::BodyLength(_, _))
        ));
    }

    #[test]
    fn observed_address_truncated_rejected() {
        let addr: std::net::SocketAddr = "203.0.113.7:51820".parse().unwrap();
        let frame = NetworkFrame::ObservedAddress(ObservedAddressFrame { observed: addr });
        let wire = frame.encode_to_wire();
        let truncated = &wire[..wire.len() - 1];
        assert!(matches!(
            NetworkFrame::decode_from_wire(truncated),
            Err(NetworkFrameError::BodyLength(_, _))
        ));
    }

    #[test]
    fn known_frame_starts_with_high_bit_tag() {
        // Anti-confusion: все наши tag'и в range 0xA0+ чтобы text-frame'ы
        // (которые обычно начинаются с printable bytes) сразу drop'ались.
        for kind in [
            FrameKind::Announce,
            FrameKind::ChallengeRequest,
            FrameKind::ChallengeResponse,
            FrameKind::Data,
        ] {
            assert!((kind as u8) >= 0xA0);
        }
    }
}
