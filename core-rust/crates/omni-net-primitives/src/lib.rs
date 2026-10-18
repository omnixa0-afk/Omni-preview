// omni-net-primitives — shared, identity-agnostic networking primitives.
//
// Extracted 2026-05-28 (Strategy B из
// docs/bugs/mesh-vs-existing-duplication-audit.md) чтобы omni-transport
// и omni-network reuse'или один QUIC + multicast stack без дублирования.
//
// Содержит ТОЛЬКО transport-level plumbing — никаких app semantics
// (instance_id handshake, NodeAddress challenge, announce, routing).
// Те layer'ы строятся поверх в omni-transport и omni-network
// соответственно.

pub mod bdp_cap;
pub mod delay_cap;
pub mod multicast;
pub mod quic;
pub mod stun;
pub mod trace;

pub use multicast::{MulticastSocket, MulticastSocketConfig};
pub use trace::{trace_enabled, trace_line};
pub use quic::{
    bind_quic_udp_socket, omni_client_config, omni_client_config_for, omni_server_config,
    omni_server_config_for, omni_transport_config, omni_transport_config_for, TransportCarrier,
    DOOR_QUEUE_BUDGET, UDP_QUEUE_BUDGET,
    OmniServerCertVerifier, QuicPrimitiveError, QUIC_DATAGRAM_RECEIVE_BUFFER_BYTES,
    QUIC_KEEP_ALIVE_INTERVAL, QUIC_MAX_BIDI_STREAMS, QUIC_MAX_IDLE_TIMEOUT, QUIC_MAX_UNI_STREAMS,
};
