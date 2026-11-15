//! Whether an address is a public, internet-routable unicast one — one place for every crate
//! that asks (a relay directory entry or a contact's courier, V-22; the external address of a
//! UPnP mapping worth advertising). The special-purpose ranges are those of RFC 6890 and its
//! IANA registries; an IPv6 address that carries an IPv4 one (mapped, NAT64, 6to4) is judged by
//! the IPv4 address it reaches.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A routable unicast address: not loopback, private, link-local, CGNAT, documentation,
/// benchmarking, reserved, unspecified, broadcast or multicast — in either family, and not an
/// IPv6 form of such an IPv4 address.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                                        // 0.0.0.0/8 "this network"
        || a == 10                                  // 10.0.0.0/8 private
        || (a == 100 && (b & 0xc0) == 64)           // 100.64.0.0/10 CGNAT
        || a == 127                                 // loopback
        || (a == 169 && b == 254)                   // link-local
        || (a == 172 && (b & 0xf0) == 16)           // 172.16.0.0/12 private
        || (a == 192 && b == 0 && c == 0)           // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)           // TEST-NET-1
        || (a == 192 && b == 88 && c == 99)         // 6to4 relay anycast
        || (a == 192 && b == 168)                   // private
        || (a == 198 && (b & 0xfe) == 18)           // 198.18.0.0/15 benchmarking
        || (a == 198 && b == 51 && c == 100)        // TEST-NET-2
        || (a == 203 && b == 0 && c == 113)         // TEST-NET-3
        || a >= 224) // multicast 224/4, reserved 240/4, broadcast
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // An IPv4 address in IPv6 clothing reaches the IPv4 one.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(v4); // ::ffff:0:0/96
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return is_public_ipv4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8)); // NAT64 64:ff9b::/96
    }
    if s[0] == 0x2002 {
        return is_public_ipv4(Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8)); // 6to4 2002::/16
    }
    // Only global unicast 2000::/3 reaches the internet; inside it, the IETF protocol block
    // 2001::/23 (Teredo, ORCHID, benchmarking…) and documentation 2001:db8::/32 do not. Everything
    // else — ::/8 (unspecified, loopback, IPv4-compatible), 64:ff9b:1::/48, 100::/64 discard,
    // fc00::/7, fe80::/10, fec0::/10, ff00::/8 — is outside 2000::/3.
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] < 0x0200) && !(s[0] == 0x2001 && s[1] == 0x0db8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(text: &str) -> bool {
        is_public_ip(text.parse().unwrap())
    }

    #[test]
    fn ordinary_internet_addresses_are_public() {
        for ip in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "2606:4700:4700::1111", "2a00:1450:4001::1"] {
            assert!(public(ip), "{ip}");
        }
    }

    #[test]
    fn special_purpose_ipv4_ranges_are_not_public() {
        for ip in [
            "0.1.2.3", "10.0.0.1", "100.64.0.1", "100.127.255.254", "127.0.0.1", "169.254.1.1",
            "172.16.0.1", "172.31.255.255", "192.0.0.8", "192.0.2.1", "192.88.99.1", "192.168.1.1",
            "198.18.0.1", "198.19.255.255", "198.51.100.1", "203.0.113.1", "224.0.0.1", "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!public(ip), "{ip}");
        }
        // Neighbours of the ranges stay public.
        for ip in ["100.128.0.1", "172.32.0.1", "198.20.0.1", "192.0.1.1"] {
            assert!(public(ip), "{ip}");
        }
    }

    /// V-22: an IPv6 address that carries a private IPv4 one is not public — the check used to
    /// pass `::ffff:192.168.x.x`, 6to4 and NAT64 forms.
    #[test]
    fn ipv6_forms_of_private_ipv4_are_not_public() {
        for ip in ["::ffff:192.168.1.10", "::ffff:127.0.0.1", "64:ff9b::a00:1", "2002:c0a8:010a::1", "2002:7f00:1::1"] {
            assert!(!public(ip), "{ip}");
        }
        for ip in ["::ffff:8.8.8.8", "64:ff9b::808:808", "2002:808:808::1"] {
            assert!(public(ip), "{ip}");
        }
    }

    #[test]
    fn special_purpose_ipv6_ranges_are_not_public() {
        for ip in [
            "::", "::1", "::127.0.0.1", "64:ff9b:1::1", "100::1", "2001::1", "2001:1ff::1", "2001:db8::1",
            "fc00::1", "fd12:3456::1", "fe80::1", "fec0::1", "ff02::1",
        ] {
            assert!(!public(ip), "{ip}");
        }
    }
}
