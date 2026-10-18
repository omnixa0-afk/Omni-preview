//! Durable-delivery reconciliation engine (mesh Phase β+ / durable delivery, P4).
//!
//! The store-and-forward + anti-entropy LOGIC is transport-independent, so per the
//! ADR it is built and unit-tested in isolation now (single-hop); multi-hop just
//! reaches more holders later. This module is the pure core: no async, no network,
//! no storage — just the feed/high-water model + the "what does my peer miss"
//! computation that drives have/offer/request. Integration (a DurableData frame, a
//! bounded persistent store, the announce-driven exchange, quotas/GC) builds on top.
//!
//! Model (ADR §«Точная семантика»):
//! - Messages are per-`(sender, scope)` **append-only feeds** with a monotonic
//!   `seq` starting at 1. `FeedId` is opaque to the engine — the app derives it
//!   (sender pubkey + dialog/group id), hashed to 16 bytes.
//! - A node's sync state is a compact **high-water vector** `{feed → highest
//!   CONTIGUOUS seq held}`. "Contiguous" is load-bearing: holding seq 5 but not 4
//!   means the high-water is 3 — we still need 4.
//! - Two nodes reconcile by exchanging high-water and each sending the other only
//!   the seqs it is missing. This heals any gap (offline / lost packet / late join)
//!   and bridges non-overlapping online windows (A→B→C).
//!
//! See docs/decisions/mesh-durable-delivery.md.

use std::collections::{BTreeSet, HashMap};

use crate::address::NodeAddress;

/// Identifies an append-only feed = a `(sender, scope)` pair, hashed to 16 bytes
/// by the app (scope = 1-on-1 dialog or group id). Opaque to the engine.
pub type FeedId = [u8; 16];

/// Synthetic mesh destination for a group (G3): `blake3("omni.group.dest|" ‖
/// group_id)[..20]`. No node owns it — delivery is subscription-based
/// (`Network::subscribe_group_destination`). Stable pseudonym: a storer can link
/// the group's frames together (same class of linkage as `feed_id`) but learns
/// no member list. См. docs/decisions/group-envelope-and-transport.md §1.
pub fn group_destination_address(group_id: &str) -> NodeAddress {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"omni.group.dest|");
    hasher.update(group_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&hash.as_bytes()[..20]);
    NodeAddress::from_bytes(bytes)
}

/// Per-(group, sender) feed id. `domain` separates the chat feed
/// (`omni.group.feed`) from the membership/keys feed (`omni.group.ops`).
pub fn group_feed_id(domain: &str, group_id: &str, sender_pub: &str) -> FeedId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"|");
    hasher.update(group_id.as_bytes());
    hasher.update(b"|");
    hasher.update(sender_pub.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes
}

/// Deterministic per-entry msg_id for anti-entropy request matching.
pub fn group_msg_id(feed: &FeedId, seq: u64) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"omni.group.msg|");
    hasher.update(feed);
    hasher.update(&seq.to_le_bytes());
    let hash = hasher.finalize();
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
}

/// The compact sync state a node advertises to a peer: the highest contiguous seq
/// it holds per feed. Feeds absent from the map are at high-water 0 (nothing held).
/// This is what the `have` half of anti-entropy puts on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileState {
    marks: HashMap<FeedId, u64>,
}

impl ReconcileState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Highest contiguous seq the peer claims for `feed` (0 if unknown).
    pub fn high_water(&self, feed: &FeedId) -> u64 {
        self.marks.get(feed).copied().unwrap_or(0)
    }

    /// Set the high-water for a feed (used when (de)serializing a peer's state).
    pub fn set(&mut self, feed: FeedId, seq: u64) {
        self.marks.insert(feed, seq);
    }

    /// Iterate `(feed, high_water)` — for wire-encoding the `have` message.
    pub fn iter(&self) -> impl Iterator<Item = (&FeedId, u64)> {
        self.marks.iter().map(|(f, s)| (f, *s))
    }

    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }
}

/// What we hold for a single feed: the set of seqs present, so we can derive the
/// contiguous high-water and answer "what is the peer missing".
#[derive(Debug, Default)]
struct FeedHeld {
    present: BTreeSet<u64>,
}

impl FeedHeld {
    /// Highest `s` such that every seq in `1..=s` is present. 0 if seq 1 is absent.
    fn high_water(&self) -> u64 {
        let mut hw = 0u64;
        for &seq in &self.present {
            if seq == hw + 1 {
                hw = seq;
            } else if seq > hw + 1 {
                break; // gap — contiguity ends
            }
            // seq <= hw can't happen in a sorted set without dup, ignore.
        }
        hw
    }
}

/// Holds which `(feed, seq)` entries a node has, and computes anti-entropy offers.
/// Pure + deterministic. The real store persists the ciphertext keyed by these
/// `(feed, seq)`; this engine only tracks presence + drives reconciliation.
#[derive(Debug, Default)]
pub struct ReconcileEngine {
    feeds: HashMap<FeedId, FeedHeld>,
}

impl ReconcileEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that we now hold `(feed, seq)`. Idempotent. `seq` is 1-based.
    pub fn insert(&mut self, feed: FeedId, seq: u64) {
        if seq == 0 {
            return; // seq is 1-based; 0 means "nothing"
        }
        self.feeds.entry(feed).or_default().present.insert(seq);
    }

    /// Whether we hold `(feed, seq)`.
    pub fn contains(&self, feed: &FeedId, seq: u64) -> bool {
        self.feeds
            .get(feed)
            .is_some_and(|held| held.present.contains(&seq))
    }

    /// Our high-water vector — the state we advertise to peers (`have`).
    pub fn state(&self) -> ReconcileState {
        let mut state = ReconcileState::new();
        for (feed, held) in &self.feeds {
            let hw = held.high_water();
            if hw > 0 {
                state.set(*feed, hw);
            }
        }
        state
    }

    /// Given the PEER's advertised state, the `(feed, seq)` entries we hold that
    /// the peer is missing — i.e. everything we have past the peer's high-water.
    /// These are what we'd `offer`/send. Deterministic order (feed-major, seq-asc)
    /// for testability. Re-sending an entry the peer happens to hold above a gap is
    /// harmless (the store dedups by msg_id).
    pub fn offers_for(&self, peer: &ReconcileState) -> Vec<(FeedId, u64)> {
        let mut out = Vec::new();
        // Sort feeds for deterministic output.
        let mut feeds: Vec<&FeedId> = self.feeds.keys().collect();
        feeds.sort_unstable();
        for feed in feeds {
            let held = &self.feeds[feed];
            let peer_hw = peer.high_water(feed);
            for &seq in held.present.range((peer_hw + 1)..) {
                out.push((*feed, seq));
            }
        }
        out
    }

    /// Feeds where the peer is AHEAD of us (their high-water exceeds ours) — the
    /// feeds we should `request` from them. (We learn the actual seqs from their
    /// offer; this just flags where we're behind, e.g. for prioritising requests.)
    pub fn feeds_behind(&self, peer: &ReconcileState) -> Vec<FeedId> {
        let mut out: Vec<FeedId> = peer
            .iter()
            .filter(|(feed, their_hw)| *their_hw > self.state().high_water(feed))
            .map(|(feed, _)| *feed)
            .collect();
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(n: u8) -> FeedId {
        let mut f = [0u8; 16];
        f[0] = n;
        f
    }

    #[test]
    fn high_water_is_highest_contiguous_seq() {
        let mut e = ReconcileEngine::new();
        for s in [1, 2, 3, 5] {
            e.insert(feed(1), s);
        }
        // 1,2,3 contiguous; 5 is past the gap at 4 → high-water is 3.
        assert_eq!(e.state().high_water(&feed(1)), 3);

        e.insert(feed(1), 4); // fill the gap
        assert_eq!(e.state().high_water(&feed(1)), 5); // now 1..=5 contiguous
    }

    #[test]
    fn empty_or_missing_feed_is_high_water_zero() {
        let e = ReconcileEngine::new();
        assert_eq!(e.state().high_water(&feed(9)), 0);
        assert!(e.state().is_empty());
    }

    #[test]
    fn seq_zero_is_ignored() {
        let mut e = ReconcileEngine::new();
        e.insert(feed(1), 0);
        assert!(!e.contains(&feed(1), 0));
        assert_eq!(e.state().high_water(&feed(1)), 0);
    }

    #[test]
    fn offers_everything_a_fresh_peer_lacks() {
        let mut a = ReconcileEngine::new();
        for s in [1, 2, 3] {
            a.insert(feed(1), s);
        }
        let fresh = ReconcileState::new();
        assert_eq!(
            a.offers_for(&fresh),
            vec![(feed(1), 1), (feed(1), 2), (feed(1), 3)]
        );
    }

    #[test]
    fn offers_only_what_is_past_the_peer_high_water() {
        let mut a = ReconcileEngine::new();
        for s in [1, 2, 3, 4] {
            a.insert(feed(1), s);
        }
        // Peer already has 1..=2.
        let mut peer = ReconcileState::new();
        peer.set(feed(1), 2);
        assert_eq!(a.offers_for(&peer), vec![(feed(1), 3), (feed(1), 4)]);
    }

    #[test]
    fn feeds_behind_flags_where_peer_is_ahead() {
        let mut a = ReconcileEngine::new();
        a.insert(feed(1), 1);
        // peer: feed1 hw=3 (ahead), feed2 hw=1 (we have neither → behind)
        let mut peer = ReconcileState::new();
        peer.set(feed(1), 3);
        peer.set(feed(2), 1);
        assert_eq!(a.feeds_behind(&peer), vec![feed(1), feed(2)]);
    }

    /// The core property: exchanging offers in both directions makes two nodes
    /// converge to the same set, healing a gap and a non-overlapping hold.
    #[test]
    fn reconciliation_converges_two_nodes() {
        let mut a = ReconcileEngine::new();
        let mut b = ReconcileEngine::new();
        // A holds feed1 1..=3; B holds feed1 {1,3} (gap at 2) + feed2 {1}.
        for s in [1, 2, 3] {
            a.insert(feed(1), s);
        }
        b.insert(feed(1), 1);
        b.insert(feed(1), 3);
        b.insert(feed(2), 1);

        // One round of mutual offer/apply.
        let a_state = a.state();
        let b_state = b.state();
        for (f, s) in a.offers_for(&b_state) {
            b.insert(f, s);
        }
        for (f, s) in b.offers_for(&a_state) {
            a.insert(f, s);
        }

        // Both now hold feed1 1..=3 and feed2 1 → identical high-water vectors.
        assert_eq!(a.state(), b.state());
        assert_eq!(a.state().high_water(&feed(1)), 3);
        assert_eq!(a.state().high_water(&feed(2)), 1);
        // And nothing left to offer either way → converged.
        assert!(a.offers_for(&b.state()).is_empty());
        assert!(b.offers_for(&a.state()).is_empty());
    }
}
