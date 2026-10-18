//! Routable envelope for the media/datagram plane (R2 — relay carrying media).
//!
//! The reliable plane relays at the frame layer (`DataFrame{from,to,hop_limit,…}` →
//! `decide_data_forward` → next-hop). The datagram plane has no such envelope today — datagrams are
//! raw bytes delivered to the *arrival neighbour*'s inbox, so a relay can't know the final
//! destination and `register_routed_link_channels` can't feed a routed Link's `recv_datagram`. This
//! type gives a media datagram the same `{from, to, hop_limit}` routing header as `DataFrame`, so the
//! datagram dispatcher can forward it hop-by-hop (mirroring the reliable plane) and deliver it by
//! identity (`from`). The `payload` is the opaque, E2E-encrypted media (RTP from the shell) — a relay
//! reads only the routing header, never the payload (content-blind). Wrapped/unwrapped INSIDE
//! omni-network so the media layer above is unchanged. See docs/decisions/omni-network-model.md (G2)
//! and docs/notes/nat-relay-CONTINUE.md (R2 design).

use crate::address::NodeAddress;

/// Wire version of the media-datagram envelope. Bumped on any layout change (pre-v1: no back-compat).
pub const MEDIA_DATAGRAM_VERSION: u8 = 1;

/// Default TTL stamped by a sender. Direct delivery doesn't consult it (the destination is the
/// neighbour, `to == self` on arrival); it bounds RELAY depth (media relay is shallow — usually 1
/// hop). Mirrors `DataFrame`'s hop bound but smaller, since deep media relay is undesirable.
pub const MEDIA_DATAGRAM_DEFAULT_HOP_LIMIT: u8 = 8;

/// Fixed header: version(1) + from(20) + to(20) + hop_limit(1). Payload follows.
const HEADER_LEN: usize = 1 + 20 + 20 + 1;

/// A media datagram with a routing header, so a relay can forward it toward `to` and the
/// destination can route it to the right Link by `from`. `payload` is opaque E2E media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDatagram {
    /// Logical source (the originating node) — keys the destination's datagram inbox by identity.
    pub from: NodeAddress,
    /// Logical destination (the final node) — the relay forwards toward this.
    pub to: NodeAddress,
    /// TTL, decremented at each relay hop (loop-bound, mirrors `DataFrame::hop_limit`).
    pub hop_limit: u8,
    /// Opaque, E2E-encrypted media payload (RTP). A relay never reads this.
    pub payload: Vec<u8>,
}

impl MediaDatagram {
    /// Encode to wire: `[version][from:20][to:20][hop_limit][payload…]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.payload.len());
        buf.push(MEDIA_DATAGRAM_VERSION);
        buf.extend_from_slice(self.from.as_bytes());
        buf.extend_from_slice(self.to.as_bytes());
        buf.push(self.hop_limit);
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode from wire. `None` if too short or the version byte doesn't match (so a stray /
    /// pre-R2 raw datagram is rejected rather than misparsed).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN || bytes[0] != MEDIA_DATAGRAM_VERSION {
            return None;
        }
        let mut from = [0u8; 20];
        from.copy_from_slice(&bytes[1..21]);
        let mut to = [0u8; 20];
        to.copy_from_slice(&bytes[21..41]);
        let hop_limit = bytes[41];
        Some(Self {
            from: NodeAddress::from_bytes(from),
            to: NodeAddress::from_bytes(to),
            hop_limit,
            payload: bytes[HEADER_LEN..].to_vec(),
        })
    }

    /// The envelope for the next relay hop: same endpoints, `hop_limit` decremented. `None` when the
    /// TTL is exhausted (drop instead of forwarding — loop bound). The destination check is the
    /// caller's (mirrors `decide_data_forward`).
    pub fn forwarded(&self) -> Option<Self> {
        let hop_limit = self.hop_limit.checked_sub(1)?;
        if hop_limit == 0 {
            return None;
        }
        Some(Self {
            from: self.from,
            to: self.to,
            hop_limit,
            payload: self.payload.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(seed: u8) -> NodeAddress {
        NodeAddress::from_bytes([seed; 20])
    }

    #[test]
    fn encode_decode_round_trips() {
        let dg = MediaDatagram {
            from: node(0xAA),
            to: node(0xBB),
            hop_limit: 4,
            payload: vec![1, 2, 3, 4, 5],
        };
        let wire = dg.encode();
        assert_eq!(wire[0], MEDIA_DATAGRAM_VERSION);
        assert_eq!(wire.len(), HEADER_LEN + 5);
        assert_eq!(MediaDatagram::decode(&wire), Some(dg));
    }

    #[test]
    fn empty_payload_round_trips() {
        let dg = MediaDatagram {
            from: node(1),
            to: node(2),
            hop_limit: 1,
            payload: Vec::new(),
        };
        assert_eq!(MediaDatagram::decode(&dg.encode()), Some(dg));
    }

    #[test]
    fn decode_rejects_short_or_wrong_version() {
        assert_eq!(MediaDatagram::decode(&[]), None);
        assert_eq!(MediaDatagram::decode(&[0u8; HEADER_LEN - 1]), None);
        // Right length, wrong version byte → rejected (not misparsed as a raw/pre-R2 datagram).
        let mut wire = MediaDatagram {
            from: node(1),
            to: node(2),
            hop_limit: 1,
            payload: vec![9],
        }
        .encode();
        wire[0] = MEDIA_DATAGRAM_VERSION.wrapping_add(1);
        assert_eq!(MediaDatagram::decode(&wire), None);
    }

    #[test]
    fn forwarded_decrements_then_drops_at_zero() {
        let dg = MediaDatagram {
            from: node(1),
            to: node(2),
            hop_limit: 3,
            payload: vec![7],
        };
        let next = dg.forwarded().expect("3 -> 2");
        assert_eq!(next.hop_limit, 2);
        // 2 -> 1 is the last forwardable hop; 1 -> would be 0 ⇒ drop.
        let last = MediaDatagram { hop_limit: 2, ..dg.clone() }.forwarded().expect("2 -> 1");
        assert_eq!(last.hop_limit, 1);
        assert_eq!(MediaDatagram { hop_limit: 1, ..dg.clone() }.forwarded(), None);
        assert_eq!(MediaDatagram { hop_limit: 0, ..dg }.forwarded(), None);
    }
}
