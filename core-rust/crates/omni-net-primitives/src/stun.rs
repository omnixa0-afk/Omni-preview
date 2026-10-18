//! Minimal STUN (RFC 5389) binding client — discover our **reflexive** transport address
//! (the public `ip:port` a NAT maps us to) using FREE PUBLIC STUN servers, so a cold-start peer
//! with no mesh neighbour yet can still learn where it is on the internet. No owned infrastructure:
//! any of dozens of free public STUN servers works, and we cross-check ≥2 so a single malicious
//! STUN can't lie us into advertising a wrong address (the same ≥2-agreement idea the mesh's
//! `infer_reflexive` uses). Once any mesh peer connects, `ObservedAddress` supersedes this — STUN
//! is the cold-start bootstrap only. See docs/decisions/serverless-bootstrap-rendezvous.md.
//!
//! This module is pure (no sockets): request building, response parsing, telling STUN apart from
//! QUIC, and the consensus. The queries themselves go out from the MESH's own QUIC socket
//! (`omni-network::stun_socket`) — the only socket whose mapping is worth advertising. They used to
//! come from a throwaway socket in the rendezvous helper, whose port was closed before anyone could
//! dial it. See docs/bugs/every-nat-was-classified-symmetric.md.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};


/// RFC 5389 magic cookie — also how STUN packets are told apart from QUIC on a shared UDP socket.
pub const STUN_MAGIC: u32 = 0x2112_A442;

/// A diverse default set of FREE PUBLIC STUN servers across independent operators (no owned infra).
/// Diversity matters: in live testing Google's STUN timed out while Cloudflare's and Nextcloud's
/// answered — the ≥2-operator consensus is what makes reflexive discovery robust. Callers may
/// override/extend this (user-addable, like rendezvous providers).
pub const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.nextcloud.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Our reflexive address + whether the NAT mapping looked **endpoint-independent** (cone/public:
/// every server saw the same `ip:port`) vs not (symmetric: same IP, different ports → relay needed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reflexive {
    pub addr: SocketAddr,
    pub endpoint_independent: bool,
}

/// Build a 20-byte STUN Binding Request for the given transaction id.
pub fn build_binding_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut req = [0u8; 20];
    req[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // message length = 0 (no attributes)
    req[4..8].copy_from_slice(&STUN_MAGIC.to_be_bytes());
    req[8..20].copy_from_slice(txid);
    req
}

/// Parse a STUN Binding Success Response and return our mapped address. Prefers the XOR-MAPPED-
/// ADDRESS attribute, falls back to the legacy MAPPED-ADDRESS. Validates the magic cookie + the
/// transaction id (so an off-path packet for a different request is rejected). Pure / no I/O.
pub fn parse_mapped_address(resp: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if resp.len() < 20 {
        return None;
    }
    if u16::from_be_bytes([resp[0], resp[1]]) != BINDING_SUCCESS {
        return None;
    }
    if u32::from_be_bytes([resp[4], resp[5], resp[6], resp[7]]) != STUN_MAGIC {
        return None;
    }
    if resp[8..20] != txid[..] {
        return None;
    }
    let msg_len = u16::from_be_bytes([resp[2], resp[3]]) as usize;
    let end = (20 + msg_len).min(resp.len());
    let mut i = 20;
    let mut fallback: Option<SocketAddr> = None;
    while i + 4 <= end {
        let atype = u16::from_be_bytes([resp[i], resp[i + 1]]);
        let alen = u16::from_be_bytes([resp[i + 2], resp[i + 3]]) as usize;
        let vstart = i + 4;
        let vend = vstart + alen;
        if vend > resp.len() {
            break;
        }
        let val = &resp[vstart..vend];
        match atype {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = decode_address(val, true, txid) {
                    return Some(addr); // XOR form is authoritative
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if let Some(addr) = decode_address(val, false, txid) {
                    fallback = Some(addr);
                }
            }
            _ => {}
        }
        // Attributes are padded to a 4-byte boundary.
        i = vstart + alen.div_ceil(4) * 4;
    }
    fallback
}

fn decode_address(val: &[u8], xor: bool, txid: &[u8; 12]) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let mut port = u16::from_be_bytes([val[2], val[3]]);
    if xor {
        port ^= (STUN_MAGIC >> 16) as u16;
    }
    let magic = STUN_MAGIC.to_be_bytes();
    match family {
        0x01 => {
            if val.len() < 8 {
                return None;
            }
            let mut a = [val[4], val[5], val[6], val[7]];
            if xor {
                for k in 0..4 {
                    a[k] ^= magic[k];
                }
            }
            Some(SocketAddr::from((Ipv4Addr::from(a), port)))
        }
        0x02 => {
            if val.len() < 20 {
                return None;
            }
            let mut a = [0u8; 16];
            a.copy_from_slice(&val[4..20]);
            if xor {
                for k in 0..4 {
                    a[k] ^= magic[k];
                }
                for k in 0..12 {
                    a[4 + k] ^= txid[k];
                }
            }
            Some(SocketAddr::from((Ipv6Addr::from(a), port)))
        }
        _ => None,
    }
}

/// A fresh transaction id (time + a process-local counter). Uniqueness is what matters for
/// matching the response; off-path spoofing is additionally caught by checking the answer came
/// from the server we asked, by the ≥2-server consensus and, ultimately, by the mesh connect-time
/// identity challenge.
pub fn fresh_txid() -> [u8; 12] {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut txid = [0u8; 12];
    txid[0..8].copy_from_slice(&nanos.to_be_bytes());
    txid[8..12].copy_from_slice(&counter.to_be_bytes());
    txid
}

/// The transaction id of a datagram that is SHAPED like a STUN message, `None` otherwise.
///
/// This is the demultiplexing test for a socket shared with QUIC (RFC 7983 §7): a STUN message
/// starts with two zero bits, carries the magic cookie at bytes 4..8 and is at least a 20-byte
/// header. A QUIC packet sets the fixed bit (0x40) — but a peer may grease that bit (RFC 9287), so
/// shape alone is not proof: the caller consumes a datagram only if its txid is one it is waiting
/// for AND it came from the server that was asked. Anything else goes on to QUIC untouched.
pub fn stun_transaction_id(datagram: &[u8]) -> Option<[u8; 12]> {
    if datagram.len() < 20 || datagram[0] & 0xC0 != 0 {
        return None;
    }
    if u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]) != STUN_MAGIC {
        return None;
    }
    datagram[8..20].try_into().ok()
}

/// The IPv4 address of `server` (`host:port`). `send_to("host:port")` would take the FIRST resolved
/// address — on a dual-stack host that is the AAAA record, and every query failed at once (live
/// 2026-09-24, laptop after it got a global IPv6). IPv6 has no NAT to discover; its endpoint is
/// advertised separately. Pure over the resolver's answer so the choice is testable.
pub fn first_ipv4(server: &str, resolved: impl IntoIterator<Item = SocketAddr>) -> io::Result<SocketAddr> {
    resolved.into_iter().find(SocketAddr::is_ipv4).ok_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, format!("{server} has no IPv4 address"))
    })
}

/// Cross-check the answers of several servers: trust an IP only if ≥2 of them agree on it (a single
/// malicious/buggy STUN can't dictate our advertised address). If those servers also agree on the
/// **port**, the NAT mapping is endpoint-independent (cone/public); same IP but differing ports ⇒
/// symmetric. `None` if fewer than two agree.
///
/// The port comparison means something ONLY if every answer describes the same local socket
/// (RFC 4787 §4.1, the RFC 5780 mapping test). Asking each server from a fresh socket, as the old
/// helper did, made every NAT — cone included — look "symmetric".
/// See docs/bugs/every-nat-was-classified-symmetric.md.
pub fn reflexive_consensus(answers: &[SocketAddr]) -> Option<Reflexive> {
    for candidate in answers {
        let same_ip: Vec<&SocketAddr> = answers.iter().filter(|r| r.ip() == candidate.ip()).collect();
        if same_ip.len() >= 2 {
            let endpoint_independent = same_ip.iter().all(|r| r.port() == candidate.port());
            return Some(Reflexive { addr: *candidate, endpoint_independent });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_request_has_valid_header() {
        let txid = [7u8; 12];
        let req = build_binding_request(&txid);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(u16::from_be_bytes([req[2], req[3]]), 0); // no attributes
        assert_eq!(u32::from_be_bytes([req[4], req[5], req[6], req[7]]), STUN_MAGIC);
        assert_eq!(&req[8..20], &txid);
    }

    /// Build a Binding Success Response carrying an XOR-MAPPED-ADDRESS for `addr` and confirm we
    /// decode it back exactly.
    #[test]
    fn parses_xor_mapped_ipv4() {
        let txid = [0x11u8; 12];
        let ip = Ipv4Addr::new(203, 0, 113, 7);
        let port: u16 = 51820;
        // XOR-encode
        let xport = port ^ (STUN_MAGIC >> 16) as u16;
        let magic = STUN_MAGIC.to_be_bytes();
        let octets = ip.octets();
        let xaddr = [
            octets[0] ^ magic[0],
            octets[1] ^ magic[1],
            octets[2] ^ magic[2],
            octets[3] ^ magic[3],
        ];
        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&12u16.to_be_bytes()); // attr: 4 header + 8 value
        resp.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        resp.extend_from_slice(&txid);
        resp.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.push(0); // reserved
        resp.push(0x01); // IPv4
        resp.extend_from_slice(&xport.to_be_bytes());
        resp.extend_from_slice(&xaddr);

        let got = parse_mapped_address(&resp, &txid).expect("should parse");
        assert_eq!(got, SocketAddr::from((ip, port)));
    }

    #[test]
    fn rejects_wrong_transaction_id() {
        let txid = [0x11u8; 12];
        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&0u16.to_be_bytes());
        resp.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        resp.extend_from_slice(&[0xFFu8; 12]); // different txid
        assert!(parse_mapped_address(&resp, &txid).is_none());
    }

    #[test]
    fn rejects_bad_magic_and_short() {
        let txid = [0x11u8; 12];
        assert!(parse_mapped_address(&[0u8; 8], &txid).is_none());
        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&0u16.to_be_bytes());
        resp.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes()); // bad magic
        resp.extend_from_slice(&txid);
        assert!(parse_mapped_address(&resp, &txid).is_none());
    }

    fn binding_response(txid: &[u8], mapped: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(v4) = mapped else { panic!("fake server answers IPv4 only") };
        let magic = STUN_MAGIC.to_be_bytes();
        let mut resp = Vec::new();
        resp.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        resp.extend_from_slice(&12u16.to_be_bytes());
        resp.extend_from_slice(&magic);
        resp.extend_from_slice(txid);
        resp.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        resp.extend_from_slice(&8u16.to_be_bytes());
        resp.extend_from_slice(&[0, 0x01]);
        resp.extend_from_slice(&(v4.port() ^ (STUN_MAGIC >> 16) as u16).to_be_bytes());
        for (octet, m) in v4.ip().octets().iter().zip(magic) {
            resp.push(octet ^ m);
        }
        resp
    }

    fn v4(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Two servers that saw the same `ip:port` of one socket → the mapping is endpoint-independent.
    #[test]
    fn two_answers_with_the_same_mapping_are_a_cone() {
        let r = reflexive_consensus(&[v4("203.0.113.7:8452"), v4("203.0.113.7:8452")]).unwrap();
        assert_eq!(r.addr, v4("203.0.113.7:8452"));
        assert!(r.endpoint_independent);
    }

    /// Negative half: the same IP on different ports is a per-destination mapping.
    #[test]
    fn the_same_ip_on_different_ports_is_symmetric() {
        let r = reflexive_consensus(&[v4("203.0.113.7:8452"), v4("203.0.113.7:9001")]).unwrap();
        assert!(!r.endpoint_independent);
    }

    /// One answer (or two that disagree on the IP) is not a consensus — a lone server can't dictate
    /// what we advertise.
    #[test]
    fn a_single_or_disagreeing_answer_is_no_consensus() {
        assert_eq!(reflexive_consensus(&[v4("203.0.113.7:8452")]), None);
        assert_eq!(
            reflexive_consensus(&[v4("203.0.113.7:8452"), v4("198.51.100.1:8452")]),
            None
        );
    }

    /// The demux test on a socket shared with QUIC: a binding response is STUN, a QUIC Initial is not,
    /// and a greased QUIC short header (fixed bit cleared) without the magic cookie is not either.
    #[test]
    fn stun_is_told_apart_from_quic_by_shape() {
        let txid = [9u8; 12];
        let resp = binding_response(&txid, v4("203.0.113.7:8452"));
        assert_eq!(stun_transaction_id(&resp), Some(txid));

        let mut quic_initial = vec![0xc3u8, 0x00, 0x00, 0x00, 0x01];
        quic_initial.resize(1200, 0);
        assert_eq!(stun_transaction_id(&quic_initial), None);

        let mut greased_short = vec![0x01u8; 1200]; // top bits 00, but no magic cookie
        greased_short[4..8].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(stun_transaction_id(&greased_short), None);
    }

    /// A dual-stack resolver answers AAAA first; the IPv4 address must be the one taken, and a
    /// v6-only server is an error for that server alone.
    #[test]
    fn the_ipv4_answer_is_taken_even_when_ipv6_comes_first() {
        let v6: SocketAddr = "[2606:4700:49::]:3478".parse().unwrap();
        let v4addr = v4("162.159.207.0:3478");
        assert_eq!(first_ipv4("stun", [v6, v4addr]).unwrap(), v4addr);
        assert!(first_ipv4("stun", [v6]).is_err());
    }
}