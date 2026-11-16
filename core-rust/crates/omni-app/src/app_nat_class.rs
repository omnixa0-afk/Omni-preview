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
    /// The address families our beacon advertises; `None` until a beacon of ours had any address.
    local_families: Option<AddressFamilies>,
    peers: HashMap<String, PeerNat>, // key = peer node address, hex
}

#[derive(Debug, Default, Clone, Copy)]
struct PeerNat {
    nat: Option<NatType>,
    nat_independent: bool,
    /// The address families the peer's last opened beacon advertised; `None` — none seen yet.
    families: Option<AddressFamilies>,
}

/// Which IP families a beacon's endpoints are in. An IPv4-only node and an IPv6-only node have no
/// path between them at all — no NAT class, forward or punch changes that (only a relay, or a peer
/// that has both, carries such a pair).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddressFamilies {
    pub(crate) ipv4: bool,
    pub(crate) ipv6: bool,
}

impl AddressFamilies {
    /// The families of a beacon's endpoint strings (`ip:port`, `[v6]:port`); an IPv4 address written
    /// as IPv6 (`::ffff:a.b.c.d`) counts as IPv4. Unparsable entries are ignored.
    pub(crate) fn of<'a>(endpoints: impl IntoIterator<Item = &'a String>) -> Self {
        let mut families = Self::default();
        for endpoint in endpoints {
            let Ok(address) = endpoint.parse::<std::net::SocketAddr>() else {
                continue;
            };
            match address.ip() {
                std::net::IpAddr::V4(_) => families.ipv4 = true,
                std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => families.ipv4 = true,
                std::net::IpAddr::V6(_) => families.ipv6 = true,
            }
        }
        families
    }

    /// Plus the families this host can route right now (a route lookup, no packet sent). What we
    /// publish misses an IPv4 behind NAT whenever STUN did not answer, and that IPv4 still dials out.
    pub(crate) fn with_host_routes(mut self) -> Self {
        self.ipv4 |= omni_mesh_runtime::detect_primary_lan_ipv4().is_some();
        self.ipv6 |= omni_mesh_runtime::detect_global_ipv6().is_some();
        self
    }

    fn is_empty(self) -> bool {
        !self.ipv4 && !self.ipv6
    }

    fn shares_with(self, other: Self) -> bool {
        (self.ipv4 && other.ipv4) || (self.ipv6 && other.ipv6)
    }

    /// «IPv4», «IPv6» or «IPv4 и IPv6» — for the shell, which names them to the user.
    pub(crate) fn label(self) -> &'static str {
        match (self.ipv4, self.ipv6) {
            (true, true) => "IPv4 и IPv6",
            (true, false) => "IPv4",
            (false, true) => "IPv6",
            (false, false) => "",
        }
    }
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

/// Record the families our beacon advertises (what we publish, so a RelayOnly profile that strips
/// every endpoint records nothing and claims nothing).
pub(crate) fn set_local_address_families(db_path: &str, families: AddressFamilies) {
    if let Ok(mut reg) = registry().lock() {
        reg.entry(db_path.to_string()).or_default().local_families =
            (!families.is_empty()).then_some(families);
    }
}

/// Record the families a peer's opened beacon advertises (a pair beacon or an identity beacon).
/// A beacon without addresses says nothing about them and does not erase what is known.
pub(crate) fn set_peer_address_families(db_path: &str, peer_node: &[u8; 20], families: AddressFamilies) {
    if families.is_empty() {
        return;
    }
    if let Ok(mut reg) = registry().lock() {
        reg.entry(db_path.to_string())
            .or_default()
            .peers
            .entry(node_hex(peer_node))
            .or_default()
            .families = Some(families);
    }
}

/// `(ours, theirs)` when a direct link between us is impossible for want of a common family: we have
/// no IPv4 route at all and the peer publishes no IPv6 — the case of an IPv6-only node and an IPv4-only
/// one (2026-10-23: requests «sent» on one side never arrived, with nothing on screen saying why).
///
/// Only that direction is evidence. A missing IPv4 in a beacon is not: an IPv4 behind NAT is published
/// only when STUN answered, and with no relays a false "only IPv6" would sit on most contacts. A global
/// IPv6 is always published (`advertised_ipv6_endpoint`), so its absence is. Our own side is what we
/// publish plus what the host can route (`with_host_routes`). `None` when either side is unknown.
pub(crate) fn address_family_gap(db_path: &str, peer_node: &[u8; 20]) -> Option<(AddressFamilies, AddressFamilies)> {
    let reg = registry().lock().ok()?;
    let state = reg.get(db_path)?;
    let ours = state.local_families?;
    let theirs = state.peers.get(&node_hex(peer_node))?.families?;
    (!ours.ipv4 && !theirs.ipv6 && !ours.shares_with(theirs)).then_some((ours, theirs))
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

    /// An IPv6-only node and an IPv4-only one have no direct path; a family in common, an unknown
    /// side, or an address-less beacon is no such evidence.
    #[test]
    fn a_pair_without_a_common_address_family_is_reported() {
        let db = "test-nat-db-4";
        let peer = [11u8; 20];
        let v6 = || AddressFamilies::of(&["[2a00:1e88::1]:60219".to_string()]);
        let v4 = || AddressFamilies::of(&["203.0.113.7:4433".to_string()]);
        assert_eq!(address_family_gap(db, &peer), None, "nothing known yet");

        set_local_address_families(db, v6());
        assert_eq!(address_family_gap(db, &peer), None, "the peer's side is unknown");
        set_peer_address_families(db, &peer, v4());
        let (ours, theirs) = address_family_gap(db, &peer).expect("IPv6-only and IPv4-only share nothing");
        assert_eq!((ours.label(), theirs.label()), ("IPv6", "IPv4"));

        // A beacon without addresses keeps what was seen; a dual-stack one closes the gap.
        set_peer_address_families(db, &peer, AddressFamilies::of(&[]));
        assert!(address_family_gap(db, &peer).is_some());
        set_peer_address_families(
            db,
            &peer,
            AddressFamilies::of(&["203.0.113.7:4433".to_string(), "[2001:db8::7]:4433".to_string()]),
        );
        assert_eq!(address_family_gap(db, &peer), None);

        // An IPv4 address written as IPv6 is IPv4.
        assert_eq!(AddressFamilies::of(&["[::ffff:203.0.113.7]:1".to_string()]), v4());

        // The other way round is no evidence: a peer without IPv4 in its beacon may have one behind a
        // NAT whose STUN did not answer, while a global IPv6 is always published.
        set_local_address_families(db, v4());
        set_peer_address_families(db, &peer, v6());
        assert_eq!(address_family_gap(db, &peer), None, "a missing IPv4 proves nothing");
        set_local_address_families(db, v6());

        // Our beacon losing its addresses (RelayOnly) withdraws our side.
        set_peer_address_families(db, &peer, v4());
        set_local_address_families(db, AddressFamilies::of(&[]));
        assert_eq!(address_family_gap(db, &peer), None);
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
