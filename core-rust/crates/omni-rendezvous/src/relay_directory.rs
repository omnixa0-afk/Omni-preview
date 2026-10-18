// Public relay directory (docs/decisions/public-relay-directory.md): a FOURTH keying of the courier
// machinery, next to the pair topics, the Omni-ID topics and the name topics. Every Omni app knows the
// directory topics; a node that turned on "be a relay for everyone" posts a signed entry there — how to
// reach it, nothing about whose device it is beyond its node key — and anyone looking for a relay reads
// them all, on demand only.
//
// Each courier kind has its own number of shelves (shards): a Nostr relay answers at most ~64 events a
// query, so the directory is spread over 8 topics there; ntfy returns its whole cache and carries the
// entry rarely, so one topic is enough. A relay picks its shelf by its key, the reader reads every
// shelf of the current and the previous window.

use std::net::{IpAddr, SocketAddr};

use sha2::{Digest, Sha256};

use crate::{hkdf32, Topic, TOPIC_WINDOW_SECS};

/// Shelves per courier kind (owner, 2026-10-14). Changing one is a format change for every app.
pub const RELAY_SHELVES_NOSTR: u8 = 8;
pub const RELAY_SHELVES_NTFY: u8 = 1;
/// Mainline (`announce_peer` / `get_peers`) — stage 2 of the decision.
pub const RELAY_SHELVES_MAINLINE: u8 = 4;

/// The largest entry on the wire: well inside ntfy's 4096 hex characters.
pub const RELAY_ENTRY_MAX_WIRE: usize = 1024;
/// A relay name, in characters (shown next to its id, never instead of it).
pub const RELAY_NAME_MAX_CHARS: usize = 32;
const RELAY_ENDPOINTS_MAX: usize = 4;
const RELAY_ENDPOINT_MAX_BYTES: usize = 64;

const RELAY_ENTRY_MAGIC: &[u8; 4] = b"ORD1";
const RELAY_ENTRY_SIGNING_CONTEXT: &[u8] = b"omni-relay-directory-v1\0";
const RELAY_DIRECTORY_IKM: &[u8] = b"omni.relay-directory.v1";
const HKDF_RELAY_TOPIC_INFO: &[u8] = b"omni.relay-directory.topic.v1";
const HKDF_RELAY_DHT_KEY_INFO: &[u8] = b"omni.relay-directory.dht.v1";

/// Which courier kind a shelf belongs to: the kinds keep separate topics, since their shelf counts differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelayShelfKind {
    Nostr,
    Ntfy,
    Mainline,
}

impl RelayShelfKind {
    pub fn shelves(self) -> u8 {
        match self {
            Self::Nostr => RELAY_SHELVES_NOSTR,
            Self::Ntfy => RELAY_SHELVES_NTFY,
            Self::Mainline => RELAY_SHELVES_MAINLINE,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Nostr => 1,
            Self::Ntfy => 2,
            Self::Mainline => 3,
        }
    }
}

/// The shelf a relay with `node_pubkey` posts on, among `shelves`.
pub fn relay_shelf(node_pubkey: &[u8; 32], shelves: u8) -> u8 {
    let digest = Sha256::digest(node_pubkey);
    digest[0] % shelves.max(1)
}

fn shelf_topic(kind: RelayShelfKind, shelf: u8, bucket: u64) -> (Topic, [u8; 32]) {
    let mut suffix = vec![kind.tag(), shelf];
    suffix.extend_from_slice(&bucket.to_le_bytes());
    let mut topic_info = HKDF_RELAY_TOPIC_INFO.to_vec();
    topic_info.extend_from_slice(&suffix);
    let mut dht_info = HKDF_RELAY_DHT_KEY_INFO.to_vec();
    dht_info.extend_from_slice(&suffix);
    (Topic(hkdf32(RELAY_DIRECTORY_IKM, &topic_info)), hkdf32(RELAY_DIRECTORY_IKM, &dht_info))
}

/// A Mainline shelf's info-hash (BEP 5 `announce_peer` / `get_peers`): the first 20 bytes of the
/// shelf's DHT key — the same HKDF over kind ‖ shelf ‖ window as every other shelf.
pub fn mainline_info_hash(dht_key: &[u8; 32]) -> [u8; 20] {
    dht_key[..20].try_into().expect("20 of 32")
}

/// Where a relay posts on `kind`: its own shelf, current window only.
pub fn relay_publish_topic(kind: RelayShelfKind, node_pubkey: &[u8; 32], now_ms: u64) -> (Topic, [u8; 32]) {
    shelf_topic(kind, relay_shelf(node_pubkey, kind.shelves()), now_ms / 1000 / TOPIC_WINDOW_SECS)
}

/// What a reader reads on `kind`: every shelf, current window and the previous one.
pub fn relay_read_topics(kind: RelayShelfKind, now_ms: u64) -> Vec<(Topic, [u8; 32])> {
    let bucket = now_ms / 1000 / TOPIC_WINDOW_SECS;
    let mut topics = Vec::new();
    for b in [bucket, bucket.saturating_sub(1)] {
        for shelf in 0..kind.shelves() {
            topics.push(shelf_topic(kind, shelf, b));
        }
    }
    topics
}

/// How full a relay says it is — coarse on purpose: an exact count would tell its audience.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelayLoad {
    Free,
    Busy,
    Full,
}

impl RelayLoad {
    fn to_byte(self) -> u8 {
        match self {
            Self::Free => 0,
            Self::Busy => 1,
            Self::Full => 2,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Free),
            1 => Some(Self::Busy),
            2 => Some(Self::Full),
            _ => None,
        }
    }
}

/// One relay's entry, signed by its node key (`node_pubkey`; the mesh node address is derived from
/// it, so a link to the relay proves the entry is its). No `udp`/`tcp` — a withdrawal: the relay has
/// stopped, newer than its last real entry it takes it off the list at once.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RelayDirectoryEntry {
    pub node_pubkey: [u8; 32],
    /// `ip:port` the mesh accepts QUIC on.
    pub udp: Vec<String>,
    /// `ip:port` of its TLS-over-TCP door, for a network that drops UDP.
    pub tcp: Vec<String>,
    /// Shown next to its id, never instead of it (owner, 2026-10-14).
    pub name: String,
    pub load: RelayLoad,
    /// Since when the node has been a relay without a break — the base of "seniority" (the reader
    /// trusts it only as far as its own earlier sightings go).
    pub since_ms: u64,
    pub ts_ms: u64,
    pub ttl_secs: u32,
    /// Freshest wins per relay.
    pub seq: u64,
    /// Proof of work, when it becomes mandatory (owner: once spam is seen). 0 — none.
    pub pow_nonce: u64,
    pub signature: Vec<u8>,
}

impl RelayDirectoryEntry {
    pub fn is_withdrawal(&self) -> bool {
        self.udp.is_empty() && self.tcp.is_empty()
    }

    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms <= self.ts_ms.saturating_add(u64::from(self.ttl_secs).saturating_mul(1000))
    }

    /// Layout: magic «ORD1» · pubkey 32 · udp (u8 count, each u8 len) · tcp (same) · name (u8 len,
    /// utf-8) · load u8 · since u64 · ts u64 · ttl u32 · seq u64 · pow u64 · signature (u8 len).
    /// Little-endian.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160);
        out.extend_from_slice(RELAY_ENTRY_MAGIC);
        out.extend_from_slice(&self.node_pubkey);
        push_endpoints(&mut out, &self.udp);
        push_endpoints(&mut out, &self.tcp);
        let name = truncate_chars(&self.name, RELAY_NAME_MAX_CHARS);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(self.load.to_byte());
        out.extend_from_slice(&self.since_ms.to_le_bytes());
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out.extend_from_slice(&self.ttl_secs.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.pow_nonce.to_le_bytes());
        let signature = &self.signature[..self.signature.len().min(u8::MAX as usize)];
        out.push(signature.len() as u8);
        out.extend_from_slice(signature);
        out
    }

    pub fn from_wire(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > RELAY_ENTRY_MAX_WIRE {
            return None;
        }
        let mut rest = bytes;
        if take(&mut rest, 4)? != RELAY_ENTRY_MAGIC {
            return None;
        }
        let node_pubkey: [u8; 32] = take(&mut rest, 32)?.try_into().ok()?;
        let udp = take_endpoints(&mut rest)?;
        let tcp = take_endpoints(&mut rest)?;
        let name_len = take(&mut rest, 1)?[0] as usize;
        let name = String::from_utf8(take(&mut rest, name_len)?.to_vec()).ok()?;
        if name.chars().count() > RELAY_NAME_MAX_CHARS {
            return None;
        }
        let load = RelayLoad::from_byte(take(&mut rest, 1)?[0])?;
        let since_ms = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let ts_ms = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let ttl_secs = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?);
        let seq = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let pow_nonce = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let signature_len = take(&mut rest, 1)?[0] as usize;
        let signature = take(&mut rest, signature_len)?.to_vec();
        if !rest.is_empty() {
            return None;
        }
        Some(Self { node_pubkey, udp, tcp, name, load, since_ms, ts_ms, ttl_secs, seq, pow_nonce, signature })
    }

    /// Every endpoint parses and is a public address: a directory entry pointing into someone's LAN
    /// (or at loopback) would make every reader dial it.
    pub fn endpoints_are_public(&self) -> bool {
        self.udp.iter().chain(self.tcp.iter()).all(|endpoint| {
            endpoint.parse::<SocketAddr>().is_ok_and(|addr| addr.port() != 0 && is_public_ip(addr.ip()))
        })
    }
}

/// What the node key signs: the entry with an empty signature, behind a domain prefix.
pub fn relay_entry_signing_bytes(entry: &RelayDirectoryEntry) -> Vec<u8> {
    let mut unsigned = entry.clone();
    unsigned.signature = Vec::new();
    let mut bytes = RELAY_ENTRY_SIGNING_CONTEXT.to_vec();
    bytes.extend_from_slice(&unsigned.to_wire());
    bytes
}

/// A routable unicast address: not loopback, private, link-local, CGNAT, documentation,
/// unspecified or multicast.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

fn push_endpoints(out: &mut Vec<u8>, endpoints: &[String]) {
    let kept: Vec<&String> =
        endpoints.iter().filter(|e| e.len() <= RELAY_ENDPOINT_MAX_BYTES).take(RELAY_ENDPOINTS_MAX).collect();
    out.push(kept.len() as u8);
    for endpoint in kept {
        out.push(endpoint.len() as u8);
        out.extend_from_slice(endpoint.as_bytes());
    }
}

fn take_endpoints(rest: &mut &[u8]) -> Option<Vec<String>> {
    let count = take(rest, 1)?[0] as usize;
    if count > RELAY_ENDPOINTS_MAX {
        return None;
    }
    let mut endpoints = Vec::with_capacity(count);
    for _ in 0..count {
        let len = take(rest, 1)?[0] as usize;
        if len > RELAY_ENDPOINT_MAX_BYTES {
            return None;
        }
        endpoints.push(String::from_utf8(take(rest, len)?.to_vec()).ok()?);
    }
    Some(endpoints)
}

fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if rest.len() < len {
        return None;
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> RelayDirectoryEntry {
        RelayDirectoryEntry {
            node_pubkey: [5; 32],
            udp: vec!["203.0.113.7:4433".into(), "[2a01:4f8::1]:4433".into()],
            tcp: vec!["203.0.113.7:443".into()],
            name: "berlin-relay".into(),
            load: RelayLoad::Busy,
            since_ms: 1_799_000_000_000,
            ts_ms: 1_800_000_000_000,
            ttl_secs: 1_800,
            seq: 7,
            pow_nonce: 0,
            signature: vec![9; 64],
        }
    }

    #[test]
    fn the_wire_round_trips_and_rejects_the_broken() {
        let original = entry();
        let wire = original.to_wire();
        assert!(wire.len() <= RELAY_ENTRY_MAX_WIRE, "{} bytes", wire.len());
        assert_eq!(RelayDirectoryEntry::from_wire(&wire), Some(original));
        assert!(RelayDirectoryEntry::from_wire(&wire[..wire.len() - 1]).is_none());
        assert!(RelayDirectoryEntry::from_wire(b"ORD1garbage").is_none());
    }

    #[test]
    fn a_long_name_is_cut_and_the_signature_covers_everything_else() {
        let mut long = entry();
        long.name = "р".repeat(50);
        let read = RelayDirectoryEntry::from_wire(&long.to_wire()).unwrap();
        assert_eq!(read.name.chars().count(), RELAY_NAME_MAX_CHARS);
        let mut moved = entry();
        moved.udp = vec!["198.51.100.1:4433".into()];
        assert_ne!(relay_entry_signing_bytes(&entry()), relay_entry_signing_bytes(&moved));
    }

    #[test]
    fn an_entry_without_endpoints_is_a_withdrawal_and_lan_addresses_are_not_public() {
        let mut gone = entry();
        gone.udp.clear();
        gone.tcp.clear();
        assert!(gone.is_withdrawal());
        assert!(!entry().is_withdrawal());
        assert!(!entry().endpoints_are_public(), "203.0.113.0/24 is a documentation range");
        let mut real = entry();
        real.udp = vec!["8.8.8.8:4433".into()];
        real.tcp = vec![];
        assert!(real.endpoints_are_public());
        for lan in ["192.168.1.5:4433", "10.0.0.1:1", "127.0.0.1:4433", "100.64.0.1:4433", "[fe80::1]:4433", "8.8.8.8:0"] {
            real.udp = vec![lan.into()];
            assert!(!real.endpoints_are_public(), "{lan}");
        }
    }

    #[test]
    fn each_courier_kind_has_its_own_shelves_and_a_relay_posts_on_one_the_reader_reads() {
        let now = 1_800_000_000_000;
        let pubkey = [5; 32];
        for kind in [RelayShelfKind::Nostr, RelayShelfKind::Ntfy, RelayShelfKind::Mainline] {
            let read = relay_read_topics(kind, now);
            assert_eq!(read.len(), usize::from(kind.shelves()) * 2, "{kind:?}: every shelf, two windows");
            let (topic, _) = relay_publish_topic(kind, &pubkey, now);
            assert!(read.iter().any(|(t, _)| *t == topic), "{kind:?}: the reader finds the writer's shelf");
        }
        assert_ne!(
            relay_read_topics(RelayShelfKind::Nostr, now)[0].0,
            relay_read_topics(RelayShelfKind::Ntfy, now)[0].0,
            "kinds keep separate topics"
        );
        // Relays spread over the Nostr shelves.
        let used: std::collections::HashSet<u8> =
            (0u8..64).map(|i| relay_shelf(&[i; 32], RELAY_SHELVES_NOSTR)).collect();
        assert!(used.len() > 4, "{used:?}");
    }
}
