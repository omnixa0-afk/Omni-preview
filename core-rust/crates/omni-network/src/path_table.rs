// Soft-state path cache. См. docs/architecture/networking-overlay.md §«Path table».
//
// Когда мы получаем announce — добавляем/обновляем запись в таблице:
//   destination → (next_hop_interface, next_hop_neighbor, cost, last_seen)
//
// Записи expire по TTL. Истёкшая запись = не ошибка, triggers fresh
// path-request если нужно дойти до destination.
//
// Thread-safe (parking_lot Mutex'ом) для одновременного writer (announce
// handler) и reader (dialer).

use crate::address::NodeAddress;
use crate::interface::{InterfaceId, NeighborId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Cached path entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathEntry {
    pub next_hop_interface: InterfaceId,
    pub next_hop_neighbor: NeighborId,
    /// Hop count from us to destination. 0 = direct neighbor.
    pub cost: u8,
    /// When we last received an announce confirming this path.
    pub last_seen: Instant,
    /// TTL (seconds) hinted by the announce.
    pub ttl: Duration,
}

impl PathEntry {
    pub fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) > self.ttl
    }
}

/// Map of destination → best known path.
///
/// «Best» = lowest cost. Если приходит новый announce с тем же или меньшим
/// cost — обновляем. Если с большим cost — обновляем только если текущая
/// запись expired.
#[derive(Debug, Default)]
pub struct PathTable {
    entries: Mutex<HashMap<NodeAddress, PathEntry>>,
}

impl PathTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update entry если новый путь better или текущий expired.
    /// Returns true если запись действительно изменилась.
    pub fn update(
        &self,
        destination: NodeAddress,
        next_hop_interface: InterfaceId,
        next_hop_neighbor: NeighborId,
        cost: u8,
        ttl: Duration,
        now: Instant,
    ) -> bool {
        let mut entries = self.entries.lock();
        let new = PathEntry {
            next_hop_interface,
            next_hop_neighbor,
            cost,
            last_seen: now,
            ttl,
        };
        match entries.get(&destination) {
            Some(current) if !current.is_expired(now) && current.cost <= cost => {
                // Текущий путь лучше (или равный) и свежий — не меняем,
                // но обновляем last_seen чтобы продлить TTL.
                let mut refreshed = current.clone();
                refreshed.last_seen = now;
                entries.insert(destination, refreshed);
                false
            }
            _ => {
                entries.insert(destination, new);
                true
            }
        }
    }

    /// Refresh the route to a next-hop we just heard from DIRECTLY — an inbound frame
    /// arrived from this verified neighbour, so this connection IS the authoritative
    /// live path to its identity right now. Unlike [`update`](Self::update), an
    /// **equal-cost** route from a DIFFERENT next-hop REPLACES the incumbent (freshest
    /// live wins); only a STRICTLY lower-cost, unexpired route is kept over this signal.
    ///
    /// Why this matters: `NeighborId` is the peer's remote `ip:port` (QUIC), so a mobile
    /// NAT **port remap** (or any reconnect) makes the same identity arrive under a NEW
    /// NeighborId. `update`'s "keep equal-cost incumbent" rule then pinned the route to
    /// the now-dead old connection until its 300 s TTL expired, so a relay kept
    /// forwarding into a black hole even though the peer was reachable on the new
    /// connection (the live "chat stops crossing the relay after a remap" decay — see
    /// docs/bugs/relay-pairing-approval-gap.md). Returns true if the entry changed.
    pub fn refresh_live(
        &self,
        destination: NodeAddress,
        next_hop_interface: InterfaceId,
        next_hop_neighbor: NeighborId,
        cost: u8,
        ttl: Duration,
        now: Instant,
    ) -> bool {
        let mut entries = self.entries.lock();
        let new = PathEntry {
            next_hop_interface,
            next_hop_neighbor,
            cost,
            last_seen: now,
            ttl,
        };
        match entries.get(&destination) {
            // A genuinely better (strictly lower-cost) live route stays; just refresh it.
            Some(current) if !current.is_expired(now) && current.cost < cost => {
                let mut refreshed = current.clone();
                refreshed.last_seen = now;
                entries.insert(destination, refreshed);
                false
            }
            // Equal-or-worse cost (or expired/absent) → the live next-hop wins.
            Some(current)
                if current.next_hop_neighbor == new.next_hop_neighbor
                    && current.cost == new.cost =>
            {
                // Unchanged next-hop — just extend the TTL (don't report a churn).
                entries.insert(destination, new);
                false
            }
            _ => {
                entries.insert(destination, new);
                true
            }
        }
    }

    /// Lookup current path для destination'а. Returns None если нет
    /// или expired.
    pub fn lookup(&self, destination: &NodeAddress, now: Instant) -> Option<PathEntry> {
        let entries = self.entries.lock();
        entries
            .get(destination)
            .filter(|entry| !entry.is_expired(now))
            .cloned()
    }

    /// Remove expired entries. Called periodically.
    pub fn evict_expired(&self, now: Instant) -> usize {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|_, entry| !entry.is_expired(now));
        before - entries.len()
    }

    /// Remove every route whose next-hop is `neighbor` — its connection died, so all routes that
    /// forward over it are stale (a relay neighbour is the next-hop for MANY destinations). Returns
    /// how many were removed. Called from `prune_path` so a dead next-hop can't keep being chosen by
    /// `decide_data_forward` (which routes purely by `lookup`), forwarding into a black hole.
    pub fn remove_via_neighbor(&self, neighbor: &NeighborId) -> usize {
        let mut entries = self.entries.lock();
        let before = entries.len();
        entries.retain(|_, entry| &entry.next_hop_neighbor != neighbor);
        before - entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    /// Snapshot всех entries для diagnostics / debugging.
    pub fn snapshot(&self) -> Vec<(NodeAddress, PathEntry)> {
        self.entries
            .lock()
            .iter()
            .map(|(addr, entry)| (*addr, entry.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    fn fresh_addr() -> NodeAddress {
        NodeAddress::from_pubkey(&SigningKey::generate(&mut OsRng).verifying_key())
    }

    fn iface() -> InterfaceId {
        InterfaceId("test-iface".to_string())
    }

    fn neighbor(name: &str) -> NeighborId {
        NeighborId(name.to_string())
    }

    #[test]
    fn lookup_empty_returns_none() {
        let table = PathTable::new();
        let addr = fresh_addr();
        assert!(table.lookup(&addr, Instant::now()).is_none());
    }

    #[test]
    fn insert_then_lookup_returns_entry() {
        let table = PathTable::new();
        let addr = fresh_addr();
        let now = Instant::now();

        let changed = table.update(
            addr,
            iface(),
            neighbor("peer-A"),
            1,
            Duration::from_secs(300),
            now,
        );
        assert!(changed);

        let entry = table.lookup(&addr, now).expect("entry should exist");
        assert_eq!(entry.cost, 1);
        assert_eq!(entry.next_hop_neighbor.0, "peer-A");
    }

    #[test]
    fn better_path_replaces_existing() {
        // Если приходит announce с cost меньше — обновляем next-hop.
        let table = PathTable::new();
        let addr = fresh_addr();
        let now = Instant::now();

        table.update(addr, iface(), neighbor("peer-A"), 3, Duration::from_secs(300), now);
        let changed = table.update(addr, iface(), neighbor("peer-B"), 1, Duration::from_secs(300), now);
        assert!(changed, "лучший путь должен заменить старый");

        let entry = table.lookup(&addr, now).unwrap();
        assert_eq!(entry.next_hop_neighbor.0, "peer-B");
        assert_eq!(entry.cost, 1);
    }

    #[test]
    fn worse_path_does_not_replace_existing_fresh() {
        // Если приходит announce с большим cost — не меняем (но last_seen
        // обновляем чтобы продлить запись).
        let table = PathTable::new();
        let addr = fresh_addr();
        let now = Instant::now();

        table.update(addr, iface(), neighbor("peer-A"), 1, Duration::from_secs(300), now);
        let changed = table.update(addr, iface(), neighbor("peer-B"), 5, Duration::from_secs(300), now);
        assert!(!changed, "худший путь не должен заменять лучший");

        let entry = table.lookup(&addr, now).unwrap();
        assert_eq!(entry.next_hop_neighbor.0, "peer-A");
        assert_eq!(entry.cost, 1);
    }

    #[test]
    fn expired_entry_is_replaced_by_any_path() {
        // Если запись expired — заменяем любым новым, даже с большим cost.
        let table = PathTable::new();
        let addr = fresh_addr();
        let t0 = Instant::now();

        table.update(addr, iface(), neighbor("peer-A"), 1, Duration::from_millis(100), t0);

        // Через 1 секунду — entry expired.
        let later = t0 + Duration::from_secs(1);
        let changed = table.update(addr, iface(), neighbor("peer-B"), 5, Duration::from_secs(300), later);
        assert!(changed, "expired запись должна быть заменена даже худшим путём");

        let entry = table.lookup(&addr, later).unwrap();
        assert_eq!(entry.next_hop_neighbor.0, "peer-B");
    }

    #[test]
    fn expired_entry_not_returned_by_lookup() {
        let table = PathTable::new();
        let addr = fresh_addr();
        let t0 = Instant::now();

        table.update(addr, iface(), neighbor("peer-A"), 1, Duration::from_millis(100), t0);
        let later = t0 + Duration::from_secs(1);

        assert!(table.lookup(&addr, later).is_none(), "expired запись не должна возвращаться");
    }

    #[test]
    fn evict_expired_clears_old_entries() {
        let table = PathTable::new();
        let t0 = Instant::now();

        let addr1 = fresh_addr();
        let addr2 = fresh_addr();
        table.update(addr1, iface(), neighbor("peer-A"), 1, Duration::from_millis(100), t0);
        table.update(addr2, iface(), neighbor("peer-B"), 1, Duration::from_secs(3600), t0);

        let later = t0 + Duration::from_secs(1);
        let evicted = table.evict_expired(later);
        assert_eq!(evicted, 1, "должен evict'нуть только expired addr1");
        assert_eq!(table.len(), 1);

        // addr2 живой, addr1 — нет.
        assert!(table.lookup(&addr2, later).is_some());
        assert!(table.lookup(&addr1, later).is_none());
    }

    #[test]
    fn refresh_extends_ttl_without_changing_path() {
        // Equal-cost announce от того же peer'а должен продлевать last_seen
        // даже если cost не улучшился.
        let table = PathTable::new();
        let addr = fresh_addr();
        let t0 = Instant::now();

        table.update(addr, iface(), neighbor("peer-A"), 1, Duration::from_millis(500), t0);

        // Через 400ms — equal-cost refresh от того же neighbor'а.
        let mid = t0 + Duration::from_millis(400);
        table.update(addr, iface(), neighbor("peer-A"), 1, Duration::from_millis(500), mid);

        // Через ещё 400ms (total 800ms от t0) — старая запись бы expired,
        // но refresh от mid'а её продлил до mid + 500ms = 900ms от t0.
        let later = t0 + Duration::from_millis(800);
        let entry = table.lookup(&addr, later).expect("refresh должен был продлить TTL");
        assert_eq!(entry.next_hop_neighbor.0, "peer-A");
    }

    #[test]
    fn snapshot_returns_all_entries() {
        let table = PathTable::new();
        let now = Instant::now();
        let addr1 = fresh_addr();
        let addr2 = fresh_addr();
        table.update(addr1, iface(), neighbor("peer-A"), 1, Duration::from_secs(300), now);
        table.update(addr2, iface(), neighbor("peer-B"), 2, Duration::from_secs(300), now);

        let snapshot = table.snapshot();
        assert_eq!(snapshot.len(), 2);
    }

    #[test]
    fn refresh_live_repoints_equal_cost_route_to_a_reconnected_next_hop() {
        // The relay→peer black-hole after a NAT remap: NeighborId is the peer's ip:port,
        // so a port remap makes the same identity arrive under a NEW next-hop at the SAME
        // cost. `update` would keep the dead incumbent until TTL; `refresh_live` must
        // re-point to the live connection immediately. See
        // docs/bugs/relay-pairing-approval-gap.md.
        let table = PathTable::new();
        let addr = fresh_addr();
        let now = Instant::now();

        table.refresh_live(addr, iface(), neighbor("1.2.3.4:59696"), 1, Duration::from_secs(300), now);
        assert_eq!(table.lookup(&addr, now).unwrap().next_hop_neighbor.0, "1.2.3.4:59696");

        // Peer's NAT remaps the port → same identity, new next-hop, equal cost.
        let changed =
            table.refresh_live(addr, iface(), neighbor("1.2.3.4:60785"), 1, Duration::from_secs(300), now);
        assert!(changed, "equal-cost route from the live reconnected next-hop must replace the stale one");
        assert_eq!(
            table.lookup(&addr, now).unwrap().next_hop_neighbor.0,
            "1.2.3.4:60785",
            "forwarding must follow the connection the peer is using NOW, not the dead one"
        );
    }

    #[test]
    fn refresh_live_keeps_a_strictly_better_route() {
        // A genuinely better (lower-cost) live route is NOT clobbered by an equal/worse
        // direct refresh — refresh_live only wins ties, it doesn't override a real upgrade.
        let table = PathTable::new();
        let addr = fresh_addr();
        let now = Instant::now();

        table.refresh_live(addr, iface(), neighbor("direct"), 1, Duration::from_secs(300), now);
        let changed =
            table.refresh_live(addr, iface(), neighbor("relay"), 2, Duration::from_secs(300), now);
        assert!(!changed, "a worse-cost route must not replace a strictly better live one");
        assert_eq!(table.lookup(&addr, now).unwrap().next_hop_neighbor.0, "direct");
    }

    #[test]
    fn refresh_live_same_next_hop_extends_ttl_without_churn() {
        // Repeated frames from the SAME live next-hop just extend the TTL and report no
        // change (so callers don't see spurious route churn every inbound frame).
        let table = PathTable::new();
        let addr = fresh_addr();
        let t0 = Instant::now();

        table.refresh_live(addr, iface(), neighbor("peer"), 1, Duration::from_millis(500), t0);
        let mid = t0 + Duration::from_millis(400);
        let changed = table.refresh_live(addr, iface(), neighbor("peer"), 1, Duration::from_millis(500), mid);
        assert!(!changed, "same next-hop refresh is not a route change");

        let later = t0 + Duration::from_millis(800);
        assert!(table.lookup(&addr, later).is_some(), "refresh должен был продлить TTL");
    }
}
