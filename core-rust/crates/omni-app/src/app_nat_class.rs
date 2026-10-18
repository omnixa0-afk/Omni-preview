// NAT-class registry (connectivity Phase C) — tracks our own NAT class (from the
// rendezvous helper's STUN consensus) and each peer's NAT class (from their opened
// beacon) — plus whether each side advertises an endpoint dialable regardless of its
// class — so the connection-state derivation can be HONEST about a pair where a direct
// link is genuinely impossible (`direct_is_impossible_for_pair`).
//
// In-memory, profile-scoped (keyed by db_path), cleared on profile stop — this is
// live observation, not durable state (a fresh STUN/beacon refreshes it). See
// docs/decisions/connectivity-upgrade-via-peer-relay.md (Phase C).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use omni_rendezvous::NatType;

#[derive(Debug, Default, Clone)]
struct NatClassState {
    local: Option<NatType>,
    /// Whether WE advertise an endpoint dialable regardless of our NAT class (a forward or a TCP
    /// door). See [`direct_is_impossible_for_pair`] for why the class alone is not enough.
    local_nat_independent: bool,
    peers: HashMap<String, PeerNat>, // key = peer node address, hex
}

#[derive(Debug, Default, Clone, Copy)]
struct PeerNat {
    nat: Option<NatType>,
    nat_independent: bool,
}

static NAT_CLASS_REGISTRY: OnceLock<Mutex<HashMap<String, NatClassState>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, NatClassState>> {
    NAT_CLASS_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn node_hex(node: &[u8; 20]) -> String {
    let mut out = String::with_capacity(40);
    for b in node {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Record our own NAT class (from the STUN consensus in `beacon_reflexive`) and whether our beacon
/// carries an endpoint that is dialable regardless of it.
pub(crate) fn set_local_nat_class(db_path: &str, nat: NatType, nat_independent: bool) {
    if let Ok(mut reg) = registry().lock() {
        let state = reg.entry(db_path.to_string()).or_default();
        // Unknown carries no signal about the class — don't overwrite a known one with it. The
        // endpoint flag is a fresh observation either way, so it is always taken.
        if nat != NatType::Unknown {
            state.local = Some(nat);
        }
        state.local_nat_independent = nat_independent;
    }
}

/// Record a peer's NAT class (from their opened rendezvous beacon) and whether that beacon carries
/// an endpoint dialable regardless of it.
pub(crate) fn set_peer_nat_class(
    db_path: &str,
    peer_node: &[u8; 20],
    nat: NatType,
    nat_independent: bool,
) {
    if let Ok(mut reg) = registry().lock() {
        let entry = reg
            .entry(db_path.to_string())
            .or_default()
            .peers
            .entry(node_hex(peer_node))
            .or_default();
        if nat != NatType::Unknown {
            entry.nat = Some(nat);
        }
        entry.nat_independent = nat_independent;
    }
}


/// True when a DIRECT link between us and this peer is genuinely impossible: both NAT classes are
/// known-symmetric AND neither side advertises an endpoint that is dialable regardless of NAT class.
///
/// The second half is the whole fix. Symmetric×symmetric only rules out punching through the two
/// STUN addresses; a UPnP forward, a native IPv6 or a TCP door on EITHER side is reachable by the
/// other one outbound, whatever its own NAT does. This used to be `is_symmetric_pair`, which checked
/// the classes alone — and live 2026-09-24 the PC showed `cannot_reach` for two minutes while the
/// laptop was linking to it through the PC's TCP door. An Unknown on either side is not enough to
/// claim "no route" either (we may just not have observed it yet).
/// See docs/bugs/a-forwarded-port-was-dropped-as-symmetric.md.
pub(crate) fn direct_is_impossible_for_pair(db_path: &str, peer_node: &[u8; 20]) -> bool {
    let hex = node_hex(peer_node);
    let Ok(reg) = registry().lock() else {
        return false;
    };
    let Some(state) = reg.get(db_path) else {
        return false;
    };
    let Some(peer) = state.peers.get(&hex) else {
        return false;
    };
    state.local == Some(NatType::Symmetric)
        && peer.nat == Some(NatType::Symmetric)
        && !state.local_nat_independent
        && !peer.nat_independent
}

pub(crate) fn clear_nat_class_for_profile(db_path: &str) {
    if let Ok(mut reg) = registry().lock() {
        reg.remove(db_path);
    }
}

#[cfg(test)]
mod nat_class_tests {
    use super::*;

    // Production reads the registry only through `direct_is_impossible_for_pair`; these readers
    // exist so the tests can pin what was stored.
    fn local_nat_class(db_path: &str) -> Option<NatType> {
        registry()
            .lock()
            .ok()
            .and_then(|reg| reg.get(db_path).and_then(|s| s.local))
    }

    fn peer_nat_class(db_path: &str, peer_node: &[u8; 20]) -> Option<NatType> {
        let hex = node_hex(peer_node);
        registry()
            .lock()
            .ok()
            .and_then(|reg| reg.get(db_path).and_then(|s| s.peers.get(&hex).and_then(|p| p.nat)))
    }

    #[test]
    fn local_and_peer_roundtrip_and_a_truly_closed_pair() {
        let db = "test-nat-db-1";
        let peer = [7u8; 20];
        assert_eq!(local_nat_class(db), None);
        assert!(!direct_is_impossible_for_pair(db, &peer));

        set_local_nat_class(db, NatType::Symmetric, false);
        set_peer_nat_class(db, &peer, NatType::Symmetric, false);
        assert_eq!(local_nat_class(db), Some(NatType::Symmetric));
        assert_eq!(peer_nat_class(db, &peer), Some(NatType::Symmetric));
        assert!(direct_is_impossible_for_pair(db, &peer));

        // A cone peer is punchable.
        let cone_peer = [9u8; 20];
        set_peer_nat_class(db, &cone_peer, NatType::Cone, false);
        assert!(!direct_is_impossible_for_pair(db, &cone_peer));

        clear_nat_class_for_profile(db);
        assert_eq!(local_nat_class(db), None);
        assert!(!direct_is_impossible_for_pair(db, &peer));
    }

    /// The live case of 2026-09-24: both classes symmetric, and the peer's TCP door (or a forward)
    /// is reachable anyway. The negative half is the test above — without a forward on either side
    /// the verdict must still be "impossible", or this would pass on a function that always says no.
    #[test]
    fn a_forward_on_either_side_keeps_a_symmetric_pair_directly_reachable() {
        let db = "test-nat-db-3";
        let peer = [5u8; 20];

        set_local_nat_class(db, NatType::Symmetric, false);
        set_peer_nat_class(db, &peer, NatType::Symmetric, true);
        assert!(
            !direct_is_impossible_for_pair(db, &peer),
            "the peer's forward is dialable from behind our symmetric NAT"
        );

        set_local_nat_class(db, NatType::Symmetric, true);
        set_peer_nat_class(db, &peer, NatType::Symmetric, false);
        assert!(
            !direct_is_impossible_for_pair(db, &peer),
            "our forward is dialable from behind theirs"
        );

        set_local_nat_class(db, NatType::Symmetric, false);
        assert!(
            direct_is_impossible_for_pair(db, &peer),
            "the forward went away on the next sweep — the verdict follows it"
        );
        clear_nat_class_for_profile(db);
    }

    #[test]
    fn unknown_never_overwrites_a_known_class() {
        let db = "test-nat-db-2";
        let peer = [3u8; 20];
        set_local_nat_class(db, NatType::Cone, false);
        set_local_nat_class(db, NatType::Unknown, false); // class ignored
        assert_eq!(local_nat_class(db), Some(NatType::Cone));
        set_peer_nat_class(db, &peer, NatType::Symmetric, false);
        set_peer_nat_class(db, &peer, NatType::Unknown, false); // class ignored
        assert_eq!(peer_nat_class(db, &peer), Some(NatType::Symmetric));
        clear_nat_class_for_profile(db);
    }
}
