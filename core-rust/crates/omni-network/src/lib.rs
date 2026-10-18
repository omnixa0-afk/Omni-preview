// omni-network — spike crate (2026-05-26).
//
// Реализует Reticulum-style self-organizing mesh overlay для Omni.
// См. docs/architecture/networking-overlay.md для архитектуры,
// docs/decisions/networking-mesh-overlay-adoption.md для locked decisions.
//
// Это spike: проверка архитектурных гипотез до commit'а полного Phase α.
// Модули:
// - `address`: NodeAddress = blake3(pubkey)[:20], anti-impersonation gate
// - `announce`: signed periodic self-advertisement
// - `interface`: link-layer abstraction (Interface trait)
// - `quic_interface`: QuicOverUdpInterface — primary v1 transport
// - `path_table`: soft-state path cache с TTL'ed entries
//
// День 2 добавит: MdnsInterface, integration test двух процессов.

pub mod address;
pub mod announce;
pub mod cert;
mod tcp_probe;
pub mod interface;
pub mod media_datagram;
pub mod multicast_interface;
pub mod network;
pub mod tcp_transport;
pub mod mux_socket;
pub mod path_table;
pub mod durable;
pub mod protocol;
pub mod quic_interface;
pub mod stun_socket;

/// Deterministic in-process network simulator (test harness). Lets real
/// `Network` instances be wired over a virtual fabric with a programmable NAT
/// model — the gate for P1 (cone vs symmetric) and all of P2 (relay/punch).
#[cfg(test)]
mod sim;

pub use address::NodeAddress;
pub use announce::{
    Announce, AnnounceError, ANNOUNCE_CAP_DHT_PROXY, ANNOUNCE_CAP_FRIEND_EXIT,
    ANNOUNCE_CAP_FRIEND_RELAY, ANNOUNCE_WIRE_VERSION, MAX_HOP_COUNT,
};
pub use durable::{
    group_destination_address, group_feed_id, group_msg_id, FeedId, ReconcileEngine,
    ReconcileState,
};
pub use cert::{
    derive_cert_from_signing_key, fresh_challenge_nonce, respond_to_challenge,
    verify_challenge_response, CertError, ChallengeResponse, CHALLENGE_NONCE_BYTES,
    IDENTITY_CHALLENGE_DOMAIN,
};
pub use interface::{
    BulkStream, Interface, InterfaceHandle, InterfaceId, InterfaceKind, NeighborId,
};
pub use media_datagram::{MediaDatagram, MEDIA_DATAGRAM_DEFAULT_HOP_LIMIT, MEDIA_DATAGRAM_VERSION};
pub use multicast_interface::{
    MulticastConfig, MulticastInterface, OMNI_MULTICAST_GROUP, OMNI_MULTICAST_PORT,
};
pub use network::{
    CarrierPreference, Link, LinkQuality, Network, NetworkError, PeerLinkFacts, ReachabilityState,
    DEFAULT_ANNOUNCE_TTL_SECONDS,
};
pub use path_table::{PathEntry, PathTable};
pub use protocol::{
    ChallengeRequest, ChallengeResponseFrame, DataFrame, DhtBootReqFrame, DhtBootRespFrame,
    DhtProxyOp, DhtProxyRequestFrame, DhtProxyResponseFrame, DurableDataFrame, FrameKind,
    NetworkFrame, NetworkFrameError, RelayCardReqFrame, pack_ipv4_peers, unpack_ipv4_peers, RelayCardRespFrame, DHT_BOOT_MAX_NODES,
    MAX_DATA_PAYLOAD, RELAY_CARD_MAX,
};
pub use quic_interface::QuicOverUdpInterface;
pub use tcp_transport::tcp_trace as diag_trace;
