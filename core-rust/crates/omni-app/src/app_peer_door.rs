// Per-peer direct-TCP door registry (S3 honest failover) — remembers each peer's
// advertised direct-TCP front door (`beacon.tcp_direct`, learned during pairing and on
// every approved-contact rendezvous sweep) so a phantom-drop can MIGRATE that peer's ONE
// quinn connection onto its door (`route_peer_over_door`) instead of the inert UDP demote.
//
// In-memory, profile-scoped (keyed by db_path), cleared on profile stop — this is live
// observation refreshed from the peer's beacon, not durable state. Mirrors the
// NAT-class registry (`app_nat_class.rs`). See docs/bugs/relay-less-mobile-transport-findings.md
// §THE REMAINING BLOCKER + connection-quality-ladder.md (reliable-base fail-back).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};

static PEER_DOOR_REGISTRY: OnceLock<Mutex<HashMap<String, HashMap<String, SocketAddr>>>> =
    OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, HashMap<String, SocketAddr>>> {
    PEER_DOOR_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn node_hex(node: &[u8; 20]) -> String {
    let mut out = String::with_capacity(40);
    for b in node {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Record a peer's advertised direct-TCP door (the first `tcp_direct` endpoint from its beacon).
/// Refreshed each time we learn it — a later beacon with a rotated/UPnP-remapped door overwrites
/// the stale one, so a failover always migrates onto the endpoint the peer currently advertises.
pub(crate) fn set_peer_door(db_path: &str, peer_node: &[u8; 20], door: SocketAddr) {
    if let Ok(mut reg) = registry().lock() {
        reg.entry(db_path.to_string())
            .or_default()
            .insert(node_hex(peer_node), door);
    }
}

/// The peer's last-learned direct-TCP door, if any (`None` ⇒ never advertised one / CGNAT peer).
pub(crate) fn peer_door(db_path: &str, peer_node: &[u8; 20]) -> Option<SocketAddr> {
    let hex = node_hex(peer_node);
    registry()
        .lock()
        .ok()
        .and_then(|reg| reg.get(db_path).and_then(|m| m.get(&hex).copied()))
}

/// Every door we know for the profile's peers, as `(node hex, door)`.
pub(crate) fn peer_doors(db_path: &str) -> Vec<(String, SocketAddr)> {
    registry()
        .lock()
        .ok()
        .and_then(|reg| reg.get(db_path).map(|m| m.iter().map(|(node, door)| (node.clone(), *door)).collect()))
        .unwrap_or_default()
}

pub(crate) fn clear_peer_doors_for_profile(db_path: &str) {
    if let Ok(mut reg) = registry().lock() {
        reg.remove(db_path);
    }
}

#[cfg(test)]
mod peer_door_tests {
    use super::*;

    #[test]
    fn roundtrip_overwrite_and_clear() {
        let db = "test-peer-door-db-1";
        let peer = [5u8; 20];
        assert_eq!(peer_door(db, &peer), None);

        let door1: SocketAddr = "198.51.100.20:51515".parse().unwrap();
        set_peer_door(db, &peer, door1);
        assert_eq!(peer_door(db, &peer), Some(door1));

        // A rotated door overwrites the stale one (validate-before-advertise refreshes it).
        let door2: SocketAddr = "198.51.100.20:40000".parse().unwrap();
        set_peer_door(db, &peer, door2);
        assert_eq!(peer_door(db, &peer), Some(door2));

        // A different peer is isolated.
        let other = [9u8; 20];
        assert_eq!(peer_door(db, &other), None);

        clear_peer_doors_for_profile(db);
        assert_eq!(peer_door(db, &peer), None);
    }

    #[test]
    fn profiles_are_isolated_by_db_path() {
        let db_a = "test-peer-door-db-a";
        let db_b = "test-peer-door-db-b";
        let peer = [1u8; 20];
        let door: SocketAddr = "198.51.100.7:8443".parse().unwrap();
        set_peer_door(db_a, &peer, door);
        assert_eq!(peer_door(db_a, &peer), Some(door));
        assert_eq!(peer_door(db_b, &peer), None);
        clear_peer_doors_for_profile(db_a);
    }
}
