// Network address derivation. См. docs/architecture/networking-overlay.md
// §«Destination = address» и locked decision Q1 (20 байт, blake3).
//
// Свойства:
// - Address = blake3(pubkey)[..20]. 160 бит = ~10^48 пар коллизий.
// - Любой узел может verify: дай pubkey, я пересчитаю blake3, сравню.
//   Никто не может «занять» чужой address без приватного ключа owner'а.
// - Backward-compatible с существующим X3DH long-term keypair Omni'я.

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::fmt;

/// 160-bit cryptographic network address derived from peer's pubkey.
// `Ord` is byte-lexicographic (derived over the 20-byte array) — a stable, total, symmetric
// order used to elect a single hole-punch dialer (`decide_punch_role`: lower address dials).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeAddress([u8; 20]);

impl NodeAddress {
    /// «Whoever you are»: the `to` of a challenge sent to a bare address whose identity we do not
    /// know yet (an `ip:port` from the Mainline relay shelves). Only a node holding a relay card
    /// answers it (`Network::set_relay_card`). No key hashes to all zeroes in practice.
    pub const ANY: NodeAddress = NodeAddress([0u8; 20]);

    pub const SIZE: usize = 20;

    /// Derive deterministic address from peer's Ed25519 public key.
    /// blake3(pubkey_bytes)[..20].
    pub fn from_pubkey(pubkey: &VerifyingKey) -> Self {
        let hash = blake3::hash(pubkey.as_bytes());
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hash.as_bytes()[..20]);
        Self(bytes)
    }

    /// Verify that this address actually corresponds to the given pubkey.
    /// Used при handshake: peer claims to be address A, sends pubkey K,
    /// we check A == derive(K). Если нет — drop, либо impersonation,
    /// либо misconfiguration.
    pub fn verify_for_pubkey(&self, pubkey: &VerifyingKey) -> bool {
        let expected = Self::from_pubkey(pubkey);
        self.0 == expected.0
    }

    /// Raw bytes — для serialization / wire format.
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Только первые 8 hex символов — для удобства log'ов.
        // Полные 40 символов в Display.
        write!(f, "Addr({})", hex_short(&self.0))
    }
}

impl fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Полные 40 hex символов для wire / log identity.
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn hex_short(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(8);
    for byte in bytes.iter().take(4) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    #[test]
    fn address_deterministic_from_same_pubkey() {
        let key = SigningKey::generate(&mut OsRng);
        let pubkey = key.verifying_key();

        let addr1 = NodeAddress::from_pubkey(&pubkey);
        let addr2 = NodeAddress::from_pubkey(&pubkey);

        assert_eq!(addr1, addr2);
    }

    #[test]
    fn address_differs_for_different_pubkeys() {
        let key1 = SigningKey::generate(&mut OsRng);
        let key2 = SigningKey::generate(&mut OsRng);

        let addr1 = NodeAddress::from_pubkey(&key1.verifying_key());
        let addr2 = NodeAddress::from_pubkey(&key2.verifying_key());

        assert_ne!(addr1, addr2);
    }

    #[test]
    fn verify_for_pubkey_accepts_correct_key() {
        // H2 (positive): correct pubkey проходит verification.
        let key = SigningKey::generate(&mut OsRng);
        let pubkey = key.verifying_key();
        let addr = NodeAddress::from_pubkey(&pubkey);

        assert!(addr.verify_for_pubkey(&pubkey));
    }

    #[test]
    fn verify_for_pubkey_rejects_wrong_key() {
        // H2 (negative — критическая anti-impersonation проверка):
        // address claim'енный для key1 НЕ проходит verification
        // с key2. Без этого attacker может объявить address известного
        // peer'а и подсунуть свой pubkey.
        let key1 = SigningKey::generate(&mut OsRng);
        let key2 = SigningKey::generate(&mut OsRng);
        let addr = NodeAddress::from_pubkey(&key1.verifying_key());

        assert!(!addr.verify_for_pubkey(&key2.verifying_key()),
            "verify_for_pubkey должен отвергать чужой pubkey — это anti-impersonation gate");
    }

    #[test]
    fn display_is_40_hex_chars() {
        let key = SigningKey::generate(&mut OsRng);
        let addr = NodeAddress::from_pubkey(&key.verifying_key());
        let display = format!("{addr}");
        assert_eq!(display.len(), 40, "20 байт = 40 hex chars: got {display:?}");
        assert!(display.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn round_trip_bytes() {
        let key = SigningKey::generate(&mut OsRng);
        let addr1 = NodeAddress::from_pubkey(&key.verifying_key());
        let bytes = *addr1.as_bytes();
        let addr2 = NodeAddress::from_bytes(bytes);
        assert_eq!(addr1, addr2);
    }
}
