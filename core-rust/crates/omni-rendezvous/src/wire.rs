//! The compact binary form of what we seal (docs/decisions/libp2p-kad-vs-mainline.md, «Что не
//! влезает»). A beacon used to be JSON with a relay bundle inside as base64 of more JSON: a 20-byte node
//! as a 70-character array of numbers, a key as 64 hex characters, `203.0.113.1:51820` as 17 bytes.
//! With one relay that came to ~800 bytes — over what a BT-DHT record holds. Binary, the same beacon is
//! a quarter of that, so it fits the DHT and falls into the same padding bucket as one without a bundle.
//!
//! Endpoints are `ip:port` bytes (a string only for what does not parse as one); the relay bundle rides
//! as its raw bytes (the `om3relay:` token is base64url of them).

use std::net::{IpAddr, SocketAddr};

const ENDPOINT_TEXT: u8 = 0;
const ENDPOINT_V4: u8 = 4;
const ENDPOINT_V6: u8 = 6;

/// Append an endpoint: `ip:port` as 1 + 6 (IPv4) or 1 + 18 (IPv6) bytes; anything else as text (at
/// most 255 bytes). `None` when it does not fit.
pub fn put_endpoint(out: &mut Vec<u8>, endpoint: &str) -> Option<()> {
    match endpoint.parse::<SocketAddr>() {
        Ok(SocketAddr::V4(addr)) => {
            out.push(ENDPOINT_V4);
            out.extend_from_slice(&addr.ip().octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
        Ok(SocketAddr::V6(addr)) => {
            out.push(ENDPOINT_V6);
            out.extend_from_slice(&addr.ip().octets());
            out.extend_from_slice(&addr.port().to_be_bytes());
        }
        Err(_) => {
            let bytes = endpoint.as_bytes();
            out.push(ENDPOINT_TEXT);
            out.push(u8::try_from(bytes.len()).ok()?);
            out.extend_from_slice(bytes);
        }
    }
    Some(())
}

/// A cursor over the bytes being decoded; every read is bounds-checked.
pub struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < len {
            return None;
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Some(head)
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    pub fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    /// Text of at most 255 bytes, length first.
    pub fn short_text(&mut self) -> Option<String> {
        let len = self.u8()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    pub fn endpoint(&mut self) -> Option<String> {
        match self.u8()? {
            ENDPOINT_V4 => {
                let ip: [u8; 4] = self.array()?;
                Some(SocketAddr::new(IpAddr::from(ip), self.u16()?).to_string())
            }
            ENDPOINT_V6 => {
                let ip: [u8; 16] = self.array()?;
                Some(SocketAddr::new(IpAddr::from(ip), self.u16()?).to_string())
            }
            ENDPOINT_TEXT => self.short_text(),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Append text of at most 255 bytes, length first. `None` when longer.
pub fn put_short_text(out: &mut Vec<u8>, text: &str) -> Option<()> {
    out.push(u8::try_from(text.len()).ok()?);
    out.extend_from_slice(text.as_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_round_trip_as_ip_bytes_or_text() {
        for endpoint in ["203.0.113.7:51820", "[2001:db8::1]:443", "relay.example:443"] {
            let mut out = Vec::new();
            put_endpoint(&mut out, endpoint).unwrap();
            let mut reader = Reader::new(&out);
            assert_eq!(reader.endpoint().as_deref(), Some(endpoint));
            assert!(reader.is_empty());
        }
        let mut v4 = Vec::new();
        put_endpoint(&mut v4, "203.0.113.7:51820").unwrap();
        assert_eq!(v4.len(), 7, "an IPv4 endpoint is 7 bytes, not 17 characters");
        assert!(put_endpoint(&mut Vec::new(), &"x".repeat(256)).is_none());
    }
}
