use serde::{Deserialize, Serialize};

use crate::TransportMode;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalingRouteCandidateV1 {
    pub version: u16,
    pub dial_addr: String,
    #[serde(default)]
    pub transport_mode: TransportMode,
    pub source: String,
    pub priority: u8,
    pub expires_at_unix_ms: i64,
}

/// A mesh-overlay pairing invite — the minimal identity a friend needs to pair
/// with us over the mesh, with no legacy route info (the mesh discovers the
/// route via multicast announces). Decoupled from `SignalingBootstrapV1` (which
/// is legacy-signaling, requires a dial address, and rides a compact binary
/// token that can't carry this). Encoded as `om3mesh:` + base64url(JSON).
/// See docs/decisions/pairing-over-mesh-adoption.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshPairingInviteV1 {
    pub version: u16,
    /// Inviter's stable instance id (the dialog-key seed).
    pub source_instance_id: String,
    /// Hex-encoded 32-byte Ed25519 identity *signing* public key — the key the
    /// mesh derives `NodeAddress = blake3(pubkey)[:20]` from. Lets the importer
    /// dial + cryptographically verify the inviter over the mesh.
    pub signing_pubkey_hex: String,
    /// Hex-encoded 32-byte random nonce that seeds the FIRST-contact bootstrap rendezvous
    /// (`omni_crypto::derive_rendezvous_bootstrap_secret`). Cross-network first pairing has no
    /// X3DH session yet (so no durable pair secret to meet on), so both sides derive a one-time
    /// blinded bootstrap topic from this nonce, exchange current endpoints over the rendezvous
    /// providers, and connect — without a relay or same-LAN multicast. Empty when absent (e.g. a
    /// same-LAN-only invite): the importer then relies on announce/manual reach.
    /// See docs/decisions/invite-bootstrap-rendezvous.md.
    #[serde(default)]
    pub bootstrap_nonce_hex: String,
    /// The inviter's current rendezvous courier set (provider URLs, e.g. `https://ntfy.sh`,
    /// `wss://offchain.pub`) — so a friend on a restrictive network bootstraps first contact on a WORKING,
    /// SHARED provider set out of the box, not just whatever default was compiled into their build (which
    /// a network may already block). N-of-many: both sides meet if ANY listed provider is reachable. Empty
    /// when absent (older invites / same-LAN): the importer falls back to its own configured/default set.
    /// See `docs/decisions/global-identity-discovery.md` (R7 invite-provider-list) + `invite-bootstrap-rendezvous.md`.
    #[serde(default)]
    pub rendezvous_providers: Vec<String>,
    /// Custom DHT bootstrap nodes (`ip:port`) the inviter is using — CUSTOM DHT entry points for an
    /// isolated network segment where the global Mainline routers are blocked, so a friend converges on
    /// working entry points out of the box. Carried in the sealed PER-PAIR invite, NOT open gossip: a DHT
    /// bootstrap node's `ip:port` is a blockable endpoint, so it follows the same self+direct-friend trust
    /// gradient as relay IPs (the R1b anti-enumeration invariant). Empty when absent (older invites / no
    /// custom bootstrap). See `docs/decisions/serverless-connectivity-masterplan.md` §3.
    #[serde(default)]
    pub dht_bootstrap_nodes: Vec<String>,
    pub exported_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
}

pub const MESH_PAIRING_INVITE_VERSION_V1: u16 = 1;

