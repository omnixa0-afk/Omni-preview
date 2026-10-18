// Signed announce protocol. См. docs/architecture/networking-overlay.md
// §«Announce protocol».
//
// Announce = self-advertisement. Каждый узел периодически broadcast'ит
// announce'ы через свои active links: «я существую, моя identity такая,
// мои directly-reachable addresses такие, я в N хопах от источника».
//
// Подпись announce'а — anti-impersonation: только владелец приватного
// ключа может опубликовать announce от имени соответствующего address'а.

use crate::address::NodeAddress;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use thiserror::Error;

/// Wire format version byte. Bump if encoding rules меняются.
/// При несовместимости — старые узлы получают `WireVersion` error
/// при decode и просто drop'ают frame.
pub const ANNOUNCE_WIRE_VERSION: u8 = 3;

/// `capabilities` bit: this node opts in as a DHT proxy (DHT-as-a-service over the mesh, R7 Phase 2
/// mode b) — peers may delegate `dht_get/put_over_peer` to it. Advertised in the (signed) announce so
/// a requester picks a capable peer instead of probing blindly. See docs/decisions/dht-over-peer.md.
pub const ANNOUNCE_CAP_DHT_PROXY: u8 = 1 << 0;

/// `capabilities` bit: this node opts in as a **friend-proxy** (Axis 2) — willing to carry a
/// consenting friend's traffic through it (friend-as-relay / friend-as-exit). Advertised so a friend
/// can discover a willing carrier; *whom* it actually carries is gated by per-friend consent, not the
/// bit. See docs/decisions/friend-proxy-by-consent.md + tunnel-agnostic-connectivity.md §Axis 2.
pub const ANNOUNCE_CAP_FRIEND_RELAY: u8 = 1 << 1;

/// `capabilities` bit: this node opts in as a **friend-exit** (Axis 2, Phase D) — willing to let a
/// consenting friend's traffic **egress its network** toward a destination (the friend appears to
/// come from this node's address). A heavier, separate commitment than friend-relay (real IP +
/// liability lent for egress), so it is its OWN bit + its OWN per-friend consent + destination/byte
/// caps — advertising it says "willing"; *whom* + *how much* it egresses is gated separately. See
/// docs/decisions/friend-proxy-by-consent.md (Phase D).
pub const ANNOUNCE_CAP_FRIEND_EXIT: u8 = 1 << 2;

/// Periodic self-advertisement broadcast via active links.
///
/// **Wire format**: canonical hand-rolled binary encoding с явным
/// `ANNOUNCE_WIRE_VERSION` префиксом. Не используем serde-based
/// форматы потому что:
/// 1. Canonical: bytes deterministic для конкретного content'а — нет
///    surprise отличий между реализациями
/// 2. Stable: version-byte позволяет нам upgrade'ить wire без ломки
///    старых клиентов (которые видят unknown version и drop'ают)
/// 3. Auditable: encoder/decoder — короткий код который легко проверить
/// 4. Zero external dep: нет risk'а supply-chain compromise через
///    serialization library
///
/// **Signing format**: тот же canonical encoding но с hop_count=0
/// (originating value), без signature trailer'а. См.
/// `AnnounceFields::encode_for_sign()`. Forwarder может изменять
/// hop_count при relay'е не ломая подпись потому что подпись
/// computed над originator's hop_count=0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announce {
    /// Network address derived from `pubkey`.
    pub destination: NodeAddress,
    /// Raw pubkey for verification of address + signature.
    pub pubkey: VerifyingKey,
    /// Directly reachable network addresses, if any (may be empty for
    /// NAT'ed peers that only reachable via mesh). Wire-format socket
    /// addrs.
    pub advertised_addresses: Vec<SocketAddr>,
    /// 0 = origin (self). Incremented by forwarders.
    pub hop_count: u8,
    /// Unix milliseconds at originating peer's clock.
    pub timestamp: i64,
    /// TTL hint (after which the receiver may consider the announce stale).
    pub ttl_seconds: u32,
    /// Capability bitfield (signed, immutable by forwarders) — declared roles of the originator, e.g.
    /// `ANNOUNCE_CAP_DHT_PROXY`. Lets peers learn who offers a role from the announce they already
    /// process. See docs/decisions/dht-over-peer.md.
    pub capabilities: u8,
    /// The port of the originator's TCP front door (its bridge into its own quinn), 0 = no door.
    /// Signed. A peer on the same LAN dials it at the originator's LAN address: without UPnP the
    /// door is never in the beacon (`tcp_direct=[]`), so a contact on the same Wi-Fi in TCP mode had
    /// no door to reach at all. Only the port travels — the addresses are the announce's own, which
    /// never carry a WAN endpoint (address privacy). See docs/decisions/qa-2026-10-17-followups.md, step 3.
    pub door_port: u16,
    /// Ed25519 signature over all above fields (signed by `pubkey`'s
    /// secret).
    pub signature: Signature,
}

impl Announce {
    /// Build and sign a fresh announce with no declared capabilities (the common case).
    pub fn sign_new(
        signing_key: &SigningKey,
        advertised_addresses: Vec<SocketAddr>,
        timestamp: i64,
        ttl_seconds: u32,
    ) -> Self {
        Self::sign_new_with_capabilities(signing_key, advertised_addresses, timestamp, ttl_seconds, 0, 0)
    }

    /// Build and sign a fresh announce declaring `capabilities` (a signed bitfield of roles, e.g.
    /// `ANNOUNCE_CAP_DHT_PROXY`, see docs/decisions/dht-over-peer.md) and the port of our TCP front
    /// door (`door_port`, 0 = none).
    pub fn sign_new_with_capabilities(
        signing_key: &SigningKey,
        advertised_addresses: Vec<SocketAddr>,
        timestamp: i64,
        ttl_seconds: u32,
        capabilities: u8,
        door_port: u16,
    ) -> Self {
        let pubkey = signing_key.verifying_key();
        let destination = NodeAddress::from_pubkey(&pubkey);
        let unsigned = AnnounceFields {
            destination,
            pubkey,
            advertised_addresses: advertised_addresses.clone(),
            hop_count: 0,
            timestamp,
            ttl_seconds,
            capabilities,
            door_port,
        };
        let signature = signing_key.sign(&unsigned.encode_for_sign());
        Self {
            destination,
            pubkey,
            advertised_addresses,
            hop_count: 0,
            timestamp,
            ttl_seconds,
            capabilities,
            door_port,
            signature,
        }
    }

    /// Validate: signature правильная, destination соответствует pubkey'у,
    /// hop_count в разумных пределах, timestamp не из будущего.
    ///
    /// `now_ms` — current local clock; используется для проверки что
    /// announce не помечен будущим временем (sanity, не security).
    pub fn verify(&self, now_ms: i64) -> Result<(), AnnounceError> {
        if !self.destination.verify_for_pubkey(&self.pubkey) {
            return Err(AnnounceError::AddressPubkeyMismatch);
        }
        if self.hop_count > MAX_HOP_COUNT {
            return Err(AnnounceError::HopCountTooHigh(self.hop_count));
        }
        // Sanity: announce не должен быть из будущего больше чем на CLOCK_SKEW_TOLERANCE.
        if self.timestamp > now_ms + CLOCK_SKEW_TOLERANCE_MS {
            return Err(AnnounceError::TimestampFromFuture);
        }
        // Signature: bytes для верификации — те же что были signed by sender'ом,
        // но с hop_count = 0 (sender подписывал свой оригинальный announce).
        // Forwarder менять подпись не может — он только увеличивает hop_count
        // в локальной копии при relay.
        let fields = AnnounceFields {
            destination: self.destination,
            pubkey: self.pubkey,
            advertised_addresses: self.advertised_addresses.clone(),
            hop_count: 0,
            timestamp: self.timestamp,
            ttl_seconds: self.ttl_seconds,
            capabilities: self.capabilities,
            door_port: self.door_port,
        };
        self.pubkey
            .verify(&fields.encode_for_sign(), &self.signature)
            .map_err(|_| AnnounceError::InvalidSignature)?;
        Ok(())
    }

    /// Increment hop_count в локальной копии перед forward'ом.
    /// Подпись не trogается — она всегда соответствует hop_count=0
    /// оригинала. Verify'я по pubkey всегда работает.
    pub fn with_incremented_hop(mut self) -> Self {
        self.hop_count = self.hop_count.saturating_add(1);
        self
    }

    /// Serialize для transmission через Interface.broadcast_frame() /
    /// send_frame().
    ///
    /// Wire layout (canonical, big-endian):
    /// - 1 byte:  `ANNOUNCE_WIRE_VERSION`
    /// - 20 bytes: destination
    /// - 32 bytes: pubkey
    /// - 1 byte:  hop_count (current, mutable by forwarders)
    /// - 8 bytes: timestamp (i64 be)
    /// - 4 bytes: ttl_seconds (u32 be)
    /// - 1 byte:  capabilities (signed bitfield)
    /// - 2 bytes: door_port (u16 be, 0 = no TCP front door)
    /// - 4 bytes: address_count (u32 be)
    /// - per address: 1 byte family (4=v4, 6=v6) + addr bytes + 2 bytes port
    /// - 64 bytes: signature
    pub fn encode_to_wire(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(150);
        buf.push(ANNOUNCE_WIRE_VERSION);
        buf.extend_from_slice(self.destination.as_bytes());
        buf.extend_from_slice(self.pubkey.as_bytes());
        buf.push(self.hop_count);
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ttl_seconds.to_be_bytes());
        buf.push(self.capabilities);
        buf.extend_from_slice(&self.door_port.to_be_bytes());
        buf.extend_from_slice(&(self.advertised_addresses.len() as u32).to_be_bytes());
        for addr in &self.advertised_addresses {
            encode_socket_addr(&mut buf, addr);
        }
        buf.extend_from_slice(&self.signature.to_bytes());
        buf
    }

    /// Deserialize from wire bytes. Reverse of `encode_to_wire`.
    /// Не делает signature verification — caller должен вызвать `.verify()`
    /// отдельно.
    pub fn decode_from_wire(bytes: &[u8]) -> Result<Self, AnnounceError> {
        let mut cursor = WireCursor::new(bytes);
        let version = cursor.take_u8()?;
        if version != ANNOUNCE_WIRE_VERSION {
            return Err(AnnounceError::WireVersion(version));
        }

        let dest_bytes: [u8; 20] = cursor.take_n(20)?.try_into().unwrap();
        let destination = NodeAddress::from_bytes(dest_bytes);

        let pubkey_bytes: [u8; 32] = cursor.take_n(32)?.try_into().unwrap();
        let pubkey = VerifyingKey::from_bytes(&pubkey_bytes)
            .map_err(|e| AnnounceError::WireDecode(format!("invalid pubkey: {e}")))?;

        let hop_count = cursor.take_u8()?;
        let timestamp = i64::from_be_bytes(cursor.take_n(8)?.try_into().unwrap());
        let ttl_seconds = u32::from_be_bytes(cursor.take_n(4)?.try_into().unwrap());
        let capabilities = cursor.take_u8()?;
        let door_port = u16::from_be_bytes(cursor.take_n(2)?.try_into().unwrap());

        let address_count = u32::from_be_bytes(cursor.take_n(4)?.try_into().unwrap()) as usize;
        // Sanity bound: announce должен помещаться в multicast frame,
        // 64 advertised addresses — заведомо много для реалистичного use case
        // и достаточно для anti-DoS guard'а.
        if address_count > 64 {
            return Err(AnnounceError::WireDecode(format!(
                "address_count {address_count} exceeds limit 64"
            )));
        }
        let mut advertised_addresses = Vec::with_capacity(address_count);
        for _ in 0..address_count {
            advertised_addresses.push(decode_socket_addr(&mut cursor)?);
        }

        let sig_bytes: [u8; 64] = cursor.take_n(64)?.try_into().unwrap();
        let signature = Signature::from_bytes(&sig_bytes);

        if !cursor.is_exhausted() {
            return Err(AnnounceError::WireDecode(format!(
                "trailing bytes: {} unconsumed",
                cursor.remaining()
            )));
        }

        Ok(Self {
            destination,
            pubkey,
            advertised_addresses,
            hop_count,
            timestamp,
            ttl_seconds,
            capabilities,
            door_port,
            signature,
        })
    }
}

fn encode_socket_addr(buf: &mut Vec<u8>, addr: &SocketAddr) {
    match addr {
        SocketAddr::V4(v4) => {
            buf.push(4);
            buf.extend_from_slice(&v4.ip().octets());
            buf.extend_from_slice(&v4.port().to_be_bytes());
        }
        SocketAddr::V6(v6) => {
            buf.push(6);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
}

fn decode_socket_addr(cursor: &mut WireCursor) -> Result<SocketAddr, AnnounceError> {
    let family = cursor.take_u8()?;
    match family {
        4 => {
            let octets: [u8; 4] = cursor.take_n(4)?.try_into().unwrap();
            let port = u16::from_be_bytes(cursor.take_n(2)?.try_into().unwrap());
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        6 => {
            let octets: [u8; 16] = cursor.take_n(16)?.try_into().unwrap();
            let port = u16::from_be_bytes(cursor.take_n(2)?.try_into().unwrap());
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => Err(AnnounceError::WireDecode(format!(
            "unknown addr family {other}"
        ))),
    }
}

/// Minimal byte cursor для decode helpers.
struct WireCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> WireCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take_n(&mut self, n: usize) -> Result<&'a [u8], AnnounceError> {
        if self.pos + n > self.bytes.len() {
            return Err(AnnounceError::WireDecode(format!(
                "premature EOF: need {n} at offset {}, have {}",
                self.pos,
                self.bytes.len() - self.pos
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn take_u8(&mut self) -> Result<u8, AnnounceError> {
        Ok(self.take_n(1)?[0])
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn is_exhausted(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

/// Maximum hop count we'll accept — anything beyond is fishy
/// (or just stale beyond reasonable mesh diameter).
pub const MAX_HOP_COUNT: u8 = 16;

/// How much clock-skew мы терпим. Announce'ы from peers с slightly
/// fast clocks могут иметь timestamp немного впереди — это нормально.
pub const CLOCK_SKEW_TOLERANCE_MS: i64 = 30_000;

/// Helper: fields подписываются в каноническом byte-formate.
/// Не используем serde здесь — explicit encoding для cross-version
/// signature stability.
struct AnnounceFields {
    destination: NodeAddress,
    pubkey: VerifyingKey,
    advertised_addresses: Vec<SocketAddr>,
    hop_count: u8,
    timestamp: i64,
    ttl_seconds: u32,
    capabilities: u8,
    door_port: u16,
}

/// Domain-separation tag prefixed to the announce signing input. The same
/// Ed25519 identity key also signs identity challenges (cert.rs) and X3DH prekey
/// bundles (omni-crypto); a distinct constant context tag ensures a signature
/// minted in one protocol can never be replayed as valid in another. Cheap to
/// add now while signed formats are few. См. external review §2.1.
const ANNOUNCE_SIGN_DOMAIN: &[u8] = b"omni-announce-v1";

impl AnnounceFields {
    fn encode_for_sign(&self) -> Vec<u8> {
        // Stable canonical encoding. Если будем менять format, должен
        // быть version byte в начале, чтобы старые подписи остались
        // верифицируемыми.
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(ANNOUNCE_SIGN_DOMAIN);
        buf.push(2u8); // format version (within the announce domain) — v1 added capabilities, v2 the door port
        buf.extend_from_slice(self.destination.as_bytes());
        buf.extend_from_slice(self.pubkey.as_bytes());
        buf.push(self.hop_count);
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ttl_seconds.to_be_bytes());
        buf.push(self.capabilities);
        buf.extend_from_slice(&self.door_port.to_be_bytes());
        // Addresses в порядке как переданы.
        buf.extend_from_slice(&(self.advertised_addresses.len() as u32).to_be_bytes());
        for addr in &self.advertised_addresses {
            // Sort-agnostic: считаем порядок частью announce'а.
            // Wire-format: 1 байт family (4=v4, 6=v6) + bytes.
            match addr {
                SocketAddr::V4(v4) => {
                    buf.push(4);
                    buf.extend_from_slice(&v4.ip().octets());
                    buf.extend_from_slice(&v4.port().to_be_bytes());
                }
                SocketAddr::V6(v6) => {
                    buf.push(6);
                    buf.extend_from_slice(&v6.ip().octets());
                    buf.extend_from_slice(&v6.port().to_be_bytes());
                }
            }
        }
        buf
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AnnounceError {
    #[error("address doesn't match pubkey: anti-impersonation gate failed")]
    AddressPubkeyMismatch,
    #[error("hop count {0} exceeds MAX_HOP_COUNT")]
    HopCountTooHigh(u8),
    #[error("timestamp is from future beyond clock-skew tolerance")]
    TimestampFromFuture,
    #[error("Ed25519 signature verification failed")]
    InvalidSignature,
    #[error("wire decode failed: {0}")]
    WireDecode(String),
    #[error("unknown wire version (expected {})", ANNOUNCE_WIRE_VERSION)]
    WireVersion(u8),
}

// Manual Serialize/Deserialize для wire-формата может пойти позже —
// для спайка in-process использование без serialization достаточно.

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use std::net::{IpAddr, Ipv4Addr};

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn local_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    #[test]
    fn fresh_announce_verifies() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        assert_eq!(announce.verify(1_000_000), Ok(()));
    }

    #[test]
    fn announce_with_no_addresses_still_verifies() {
        // NAT'ed peer без directly-reachable address'ов — валидный кейс.
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_000_000, 300);
        assert_eq!(announce.verify(1_000_000), Ok(()));
    }

    #[test]
    fn modified_destination_fails_verification() {
        // Attacker берёт valid announce и подменяет destination на свой.
        let key = fresh_key();
        let attacker = fresh_key();
        let mut announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        announce.destination = NodeAddress::from_pubkey(&attacker.verifying_key());
        // Подпись теперь не соответствует ни старому ни новому полю.
        assert_eq!(
            announce.verify(1_000_000),
            Err(AnnounceError::AddressPubkeyMismatch)
        );
    }

    #[test]
    fn modified_pubkey_fails_verification() {
        // Attacker меняет pubkey но не пересчитывает destination → mismatch.
        let key = fresh_key();
        let attacker = fresh_key();
        let mut announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        announce.pubkey = attacker.verifying_key();
        assert_eq!(
            announce.verify(1_000_000),
            Err(AnnounceError::AddressPubkeyMismatch)
        );
    }

    #[test]
    fn modified_addresses_fails_verification() {
        // Attacker меняет advertised addresses, подпись становится invalid.
        let key = fresh_key();
        let mut announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        announce.advertised_addresses.push(local_addr(9999));
        assert_eq!(
            announce.verify(1_000_000),
            Err(AnnounceError::InvalidSignature)
        );
    }

    #[test]
    fn forwarded_announce_still_verifies() {
        // Promoter увеличил hop_count при relay'е — подпись всё равно
        // валидна потому что мы verify-им против hop_count=0 в каноническом
        // фиксированном виде.
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        let forwarded = announce.with_incremented_hop().with_incremented_hop();
        assert_eq!(forwarded.hop_count, 2);
        assert_eq!(forwarded.verify(1_000_000), Ok(()));
    }

    #[test]
    fn announce_from_far_future_rejected() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 10_000_000, 300);
        // Локальные часы говорят что сейчас 5_000_000ms — announce из
        // будущего на 5 секунд. Должен быть rejected.
        assert_eq!(
            announce.verify(5_000_000),
            Err(AnnounceError::TimestampFromFuture)
        );
    }

    #[test]
    fn announce_with_excessive_hop_count_rejected() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_000_000, 300);
        let mut overshot = announce;
        overshot.hop_count = MAX_HOP_COUNT + 1;
        assert_eq!(
            overshot.verify(1_000_000),
            Err(AnnounceError::HopCountTooHigh(MAX_HOP_COUNT + 1))
        );
    }

    #[test]
    fn verify_speed_benchmark() {
        // H3 (verify speed): должны verify'ить тысячи announce'ов в секунду
        // без напряга. Это нам нужно для path-request flood'а или большой
        // mesh'а.
        //
        // Ed25519 verify в debug сборке ~100x медленнее чем в release
        // (dalek's curve ops без оптимизаций). Threshold отдельный
        // для debug / release.
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_000_000, 300);
        let now = 1_000_000;

        let start = std::time::Instant::now();
        for _ in 0..1000 {
            announce.verify(now).expect("announce должен verify'иться");
        }
        let elapsed = start.elapsed();

        #[cfg(debug_assertions)]
        let limit_ms = 15_000; // debug: ~70/sec acceptable
        #[cfg(not(debug_assertions))]
        let limit_ms = 200; // release: ≥5000/sec expected

        assert!(
            elapsed.as_millis() < limit_ms,
            "1000 verify за {elapsed:?} — слишком медленно (limit {limit_ms}ms)"
        );
    }

    #[test]
    fn signature_size_is_64_bytes() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_000_000, 300);
        assert_eq!(announce.signature.to_bytes().len(), ed25519_dalek::SIGNATURE_LENGTH);
    }

    #[test]
    fn wire_round_trip_preserves_all_fields() {
        let key = fresh_key();
        let announce = Announce::sign_new(
            &key,
            vec![local_addr(8443), local_addr(9999)],
            1_700_000_000_000,
            600,
        );
        let bytes = announce.encode_to_wire();
        let decoded = Announce::decode_from_wire(&bytes).expect("decode should work");
        assert_eq!(announce, decoded);
        // Decoded announce должен всё ещё verify (signature не сломалась).
        assert_eq!(decoded.verify(1_700_000_000_000), Ok(()));
    }

    #[test]
    fn capabilities_round_trip_and_are_signed() {
        // A declared role (DHT-proxy) round-trips and is covered by the signature — a forwarder can't
        // forge it. See docs/decisions/dht-over-peer.md (R7 2e-4 advertised role).
        let key = fresh_key();
        let announce = Announce::sign_new_with_capabilities(
            &key,
            vec![local_addr(8443)],
            1_700_000_000_000,
            600,
            ANNOUNCE_CAP_DHT_PROXY,
            0,
        );
        assert_eq!(announce.capabilities, ANNOUNCE_CAP_DHT_PROXY);
        let decoded = Announce::decode_from_wire(&announce.encode_to_wire()).expect("decode");
        assert_eq!(decoded, announce);
        assert_eq!(decoded.capabilities, ANNOUNCE_CAP_DHT_PROXY);
        assert_eq!(decoded.verify(1_700_000_000_000), Ok(()));

        // Tampering the capabilities (without re-signing) breaks verification — it is signed, not a
        // free-text hint a relay could flip to redirect proxy traffic.
        let mut forged = decoded.clone();
        forged.capabilities = 0;
        assert_eq!(forged.verify(1_700_000_000_000), Err(AnnounceError::InvalidSignature));

        // sign_new (no caps) leaves the field zero.
        let plain = Announce::sign_new(&key, vec![], 1_700_000_000_000, 600);
        assert_eq!(plain.capabilities, 0);
        assert_eq!(plain.door_port, 0);
    }

    #[test]
    fn the_door_port_round_trips_and_is_signed() {
        // A relay forwarding the announce must not be able to point a LAN peer's TCP dial at another
        // port of the originator.
        let key = fresh_key();
        let announce =
            Announce::sign_new_with_capabilities(&key, vec![local_addr(8443)], 1_700_000_000_000, 600, 0, 51515);
        let decoded = Announce::decode_from_wire(&announce.encode_to_wire()).expect("decode");
        assert_eq!(decoded.door_port, 51515);
        assert_eq!(decoded.verify(1_700_000_000_000), Ok(()));

        let mut forged = decoded;
        forged.door_port = 22;
        assert_eq!(forged.verify(1_700_000_000_000), Err(AnnounceError::InvalidSignature));
    }

    #[test]
    fn wire_round_trip_with_no_addresses() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_700_000_000_000, 600);
        let bytes = announce.encode_to_wire();
        let decoded = Announce::decode_from_wire(&bytes).unwrap();
        assert_eq!(announce, decoded);
    }

    #[test]
    fn wire_round_trip_with_ipv6_address() {
        // Verifies IPv6 encoding path работает.
        use std::net::Ipv6Addr;
        let key = fresh_key();
        let v6 = SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x42)),
            8443,
        );
        let announce = Announce::sign_new(&key, vec![v6], 1_700_000_000_000, 600);
        let bytes = announce.encode_to_wire();
        let decoded = Announce::decode_from_wire(&bytes).unwrap();
        assert_eq!(announce, decoded);
        assert_eq!(decoded.verify(1_700_000_000_000), Ok(()));
    }

    #[test]
    fn wire_format_is_reasonably_compact() {
        // Sanity check: typical announce должен fit'нуться в multicast frame.
        // Структура: 1 + 20 + 32 + 1 + 8 + 4 + 4 + 1(capabilities) + 2(door_port) + (1+4+2 = 7 для one v4 addr) + 64 = 144 байта.
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_700_000_000_000, 600);
        let bytes = announce.encode_to_wire();
        assert_eq!(
            bytes.len(),
            144,
            "wire size должна быть ровно 144 байта для one-v4-addr announce'а, got {}",
            bytes.len()
        );
    }

    #[test]
    fn wire_format_starts_with_version_byte() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_700_000_000_000, 600);
        let bytes = announce.encode_to_wire();
        assert_eq!(bytes[0], ANNOUNCE_WIRE_VERSION);
    }

    #[test]
    fn decode_unknown_version_returns_error() {
        // Wire format со старшим version байтом → graceful rejection,
        // не crash.
        let bytes = vec![0xFF; 200];
        let result = Announce::decode_from_wire(&bytes);
        assert!(matches!(result, Err(AnnounceError::WireVersion(0xFF))));
    }

    #[test]
    fn decode_truncated_bytes_returns_error() {
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_700_000_000_000, 600);
        let full = announce.encode_to_wire();
        // Усекаем середину.
        let truncated = &full[..full.len() / 2];
        let result = Announce::decode_from_wire(truncated);
        assert!(matches!(result, Err(AnnounceError::WireDecode(_))));
    }

    #[test]
    fn decode_with_trailing_bytes_returns_error() {
        // Strict-decode: extra bytes в конце = ошибка (защита от
        // malleability — attacker не может pad'нуть message).
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![], 1_700_000_000_000, 600);
        let mut bytes = announce.encode_to_wire();
        bytes.push(0xAB);
        let result = Announce::decode_from_wire(&bytes);
        assert!(matches!(result, Err(AnnounceError::WireDecode(_))));
    }

    #[test]
    fn tampered_wire_bytes_fail_verify() {
        // Flip middle byte. Либо decode failure, либо decode success +
        // verify failure. В любом случае tampered announce не должен
        // пройти конечную проверку.
        let key = fresh_key();
        let announce = Announce::sign_new(&key, vec![local_addr(8443)], 1_700_000_000_000, 600);
        let mut bytes = announce.encode_to_wire();
        let idx = bytes.len() / 2;
        bytes[idx] ^= 0xFF;
        match Announce::decode_from_wire(&bytes) {
            Ok(decoded) => assert!(
                decoded.verify(1_700_000_000_000).is_err(),
                "tamper'нутые bytes должны не verify'иться"
            ),
            Err(_) => {
                // Tamper привёл к decode failure — тоже ок.
            }
        }
    }

    #[test]
    fn forwarded_announce_wire_round_trip() {
        // hop_count изменяется forwarder'ом; round-trip должен это preserve'ить.
        let key = fresh_key();
        let original = Announce::sign_new(&key, vec![], 1_700_000_000_000, 600);
        let forwarded = original.with_incremented_hop().with_incremented_hop();

        let bytes = forwarded.encode_to_wire();
        let decoded = Announce::decode_from_wire(&bytes).unwrap();
        assert_eq!(decoded.hop_count, 2);
        // Signature по-прежнему верифицируется (canonical signing format
        // фиксирует hop_count=0).
        assert_eq!(decoded.verify(1_700_000_000_000), Ok(()));
    }

    #[test]
    fn excessive_address_count_in_wire_rejected() {
        // Anti-DoS: malicious peer puts address_count = 1_000_000 в wire,
        // мы не должны allocate'ить такой Vec.
        let mut bytes = vec![ANNOUNCE_WIRE_VERSION];
        bytes.extend_from_slice(&[0u8; 20]);  // destination
        bytes.extend_from_slice(&[0u8; 32]);  // pubkey
        bytes.push(0);                         // hop_count
        bytes.extend_from_slice(&[0u8; 8]);    // timestamp
        bytes.extend_from_slice(&[0u8; 4]);    // ttl
        bytes.extend_from_slice(&1_000_000u32.to_be_bytes());  // address_count
        let result = Announce::decode_from_wire(&bytes);
        assert!(matches!(result, Err(AnnounceError::WireDecode(_))));
    }
}
