// Find by name (docs/decisions/find-by-name.md): a THIRD keying of the courier machinery, next to the
// pair topics and the Omni-ID topics. The topic comes from a person's normalized display name, so
// anyone typing the same name computes it; the beacon under it is a signed POINTER — Omni-ID, name,
// a tiny photo — never endpoints or keys. The finder picks someone from the list and goes on along
// the find-by-Omni-ID path, where the person's own "findable by ID" decides again.
//
// Size is a design limit, not a detail: ntfy carries a blob as hex text and turns a message over 4096
// characters into an attachment nobody reads, so a beacon over 2 KB would reach only Nostr. Hence the
// binary layout and the compact certificate chain (`DeviceCertificateChainV1::to_compact`).

use unicode_normalization::UnicodeNormalization;

use crate::{hkdf32, Topic, TOPIC_WINDOW_SECS};

/// The largest name beacon on the wire: 2048 bytes is 4096 hex characters, ntfy's message limit;
/// a little under it for safety.
pub const NAME_BEACON_MAX_WIRE: usize = 2000;
/// The longest normalized search key, in characters.
pub const NAME_KEY_MAX_CHARS: usize = 64;
/// One beacon is posted under at most this many keys: the whole name and up to three of its words.
pub const NAME_KEYS_MAX: usize = 4;

const NAME_BEACON_MAGIC: &[u8; 4] = b"ONB1";
const NAME_BEACON_SIGNING_CONTEXT: &[u8] = b"omni-name-beacon-v1\0";
const HKDF_NAME_TOPIC_INFO: &[u8] = b"omni.name.topic.v1";
const HKDF_NAME_DHT_KEY_INFO: &[u8] = b"omni.name.dht.v1";
const NAME_KEY_IKM_PREFIX: &[u8] = b"omni.name.v1\0";

/// A name the way a search compares it: Unicode-normalized (NFKC), lower case, «ё» read as «е»,
/// letters and digits only, words separated by one space. «  Марк   Пётров » → «марк петров».
pub fn normalize_search_name(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in raw.nfkc().flat_map(char::to_lowercase) {
        let ch = match ch {
            'ё' => 'е',
            other => other,
        };
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        } else {
            pending_space = true;
        }
    }
    out.chars().take(NAME_KEY_MAX_CHARS).collect::<String>().trim_end().to_string()
}

/// The keys a display name is findable under: the whole normalized name, and — when it has more than
/// one word — each word of at least two letters (owner, 2026-10-13: first name and surname apart).
pub fn name_search_keys(display_name: &str) -> Vec<String> {
    let full = normalize_search_name(display_name);
    if full.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![full.clone()];
    let words: Vec<&str> = full.split(' ').collect();
    if words.len() > 1 {
        for word in words {
            if word.chars().count() >= 2 && !keys.iter().any(|known| known == word) && keys.len() < NAME_KEYS_MAX {
                keys.push(word.to_string());
            }
        }
    }
    keys
}

fn name_key_ikm(key: &str) -> Vec<u8> {
    let mut ikm = NAME_KEY_IKM_PREFIX.to_vec();
    ikm.extend_from_slice(key.as_bytes());
    ikm
}

/// The topics (current window + the previous one) for one normalized key, each with the per-bucket
/// seed the helper protocol wants (the BT-DHT is never asked for names: one slot per key there would be
/// a fight over who writes last — see find-by-name.md §3).
pub fn name_topics_with_keys(key: &str, now_ms: u64) -> Vec<(Topic, [u8; 32])> {
    let ikm = name_key_ikm(key);
    let bucket = now_ms / 1000 / TOPIC_WINDOW_SECS;
    [bucket, bucket.saturating_sub(1)]
        .into_iter()
        .map(|b| {
            let mut topic_info = HKDF_NAME_TOPIC_INFO.to_vec();
            topic_info.extend_from_slice(&b.to_le_bytes());
            let mut dht_info = HKDF_NAME_DHT_KEY_INFO.to_vec();
            dht_info.extend_from_slice(&b.to_le_bytes());
            (Topic(hkdf32(&ikm, &topic_info)), hkdf32(&ikm, &dht_info))
        })
        .collect()
}

/// A signed pointer from a name to an Omni-ID. An empty `display_name` is a withdrawal: "this device
/// is no longer findable by name" — newer than the last real one, it hides it at once.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NameBeacon {
    pub omni_id: [u8; 20],
    /// The publishing device's certificate chain in compact form (omni-crypto
    /// `DeviceCertificateChainV1::to_compact`); its device key signs the beacon.
    pub chain_compact: Vec<u8>,
    /// The name as the person wrote it in the profile (not normalized).
    pub display_name: String,
    /// A small JPEG of the profile photo (empty — none), so namesakes can be told apart.
    pub thumb_jpeg: Vec<u8>,
    pub ts_ms: u64,
    pub ttl_secs: u32,
    /// Freshest wins per device.
    pub seq: u64,
    pub signature: Vec<u8>,
}

impl NameBeacon {
    pub fn is_withdrawal(&self) -> bool {
        self.display_name.is_empty()
    }

    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms <= self.ts_ms.saturating_add((self.ttl_secs as u64).saturating_mul(1000))
    }

    /// Layout: magic «ONB1» · omni_id 20 · chain (u16 len) · name (u8 len, utf-8) · thumb (u16 len) ·
    /// ts u64 · ttl u32 · seq u64 · signature (u8 len). Little-endian.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.chain_compact.len() + self.thumb_jpeg.len());
        out.extend_from_slice(NAME_BEACON_MAGIC);
        out.extend_from_slice(&self.omni_id);
        push_u16_bytes(&mut out, &self.chain_compact);
        let name = truncate_utf8(&self.display_name, u8::MAX as usize);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        push_u16_bytes(&mut out, &self.thumb_jpeg);
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out.extend_from_slice(&self.ttl_secs.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        let signature = &self.signature[..self.signature.len().min(u8::MAX as usize)];
        out.push(signature.len() as u8);
        out.extend_from_slice(signature);
        out
    }

    pub fn from_wire(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > NAME_BEACON_MAX_WIRE {
            return None;
        }
        let mut rest = bytes;
        if take(&mut rest, 4)? != NAME_BEACON_MAGIC {
            return None;
        }
        let omni_id: [u8; 20] = take(&mut rest, 20)?.try_into().ok()?;
        let chain_len = u16::from_le_bytes(take(&mut rest, 2)?.try_into().ok()?) as usize;
        let chain_compact = take(&mut rest, chain_len)?.to_vec();
        let name_len = take(&mut rest, 1)?[0] as usize;
        let display_name = String::from_utf8(take(&mut rest, name_len)?.to_vec()).ok()?;
        let thumb_len = u16::from_le_bytes(take(&mut rest, 2)?.try_into().ok()?) as usize;
        let thumb_jpeg = take(&mut rest, thumb_len)?.to_vec();
        let ts_ms = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let ttl_secs = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?);
        let seq = u64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let signature_len = take(&mut rest, 1)?[0] as usize;
        let signature = take(&mut rest, signature_len)?.to_vec();
        if !rest.is_empty() {
            return None;
        }
        Some(Self { omni_id, chain_compact, display_name, thumb_jpeg, ts_ms, ttl_secs, seq, signature })
    }

    /// Whether this beacon belongs under `key`: a real one only under the keys of its own name (a
    /// beacon for «Пётр» posted under «марк» is someone's noise), a withdrawal under any.
    pub fn matches_key(&self, key: &str) -> bool {
        self.is_withdrawal() || name_search_keys(&self.display_name).iter().any(|own| own == key)
    }
}

/// What the device key signs: the beacon with an empty signature, behind a domain prefix.
pub fn name_beacon_signing_bytes(beacon: &NameBeacon) -> Vec<u8> {
    let mut unsigned = beacon.clone();
    unsigned.signature = Vec::new();
    let mut bytes = NAME_BEACON_SIGNING_CONTEXT.to_vec();
    bytes.extend_from_slice(&unsigned.to_wire());
    bytes
}

fn push_u16_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn take<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if rest.len() < len {
        return None;
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_normalize_the_way_people_type_them() {
        assert_eq!(normalize_search_name("  Марк   Пётров "), "марк петров");
        assert_eq!(normalize_search_name("МАРК-ПЕТРОВ"), "марк петров");
        assert_eq!(normalize_search_name("Ｍａｒｋ"), "mark"); // full-width letters (NFKC)
        assert_eq!(normalize_search_name("!!!"), "");
    }

    #[test]
    fn a_name_is_findable_whole_and_by_each_word() {
        assert_eq!(name_search_keys("Марк Пётров"), vec!["марк петров", "марк", "петров"]);
        assert_eq!(name_search_keys("Марк"), vec!["марк"]);
        // One-letter words are not keys of their own; the list is capped.
        assert_eq!(name_search_keys("Я Марк"), vec!["я марк", "марк"]);
        assert_eq!(name_search_keys("Аа Бб Вв Гг Дд").len(), NAME_KEYS_MAX);
        assert!(name_search_keys("  ").is_empty());
    }

    #[test]
    fn the_same_name_gives_the_same_topic_and_another_name_another() {
        let now = 1_800_000_000_000;
        let mark = name_topics_with_keys("марк", now);
        assert_eq!(mark, name_topics_with_keys("марк", now));
        assert_ne!(mark[0].0, name_topics_with_keys("пётр", now)[0].0);
        assert_eq!(mark.len(), 2);
    }

    fn beacon(name: &str, thumb: usize) -> NameBeacon {
        NameBeacon {
            omni_id: [7; 20],
            chain_compact: vec![1; 470],
            display_name: name.to_string(),
            thumb_jpeg: vec![9; thumb],
            ts_ms: 1_800_000_000_000,
            ttl_secs: 46_800,
            seq: 42,
            signature: vec![3; 64],
        }
    }

    #[test]
    fn the_wire_round_trips_and_fits_ntfy_with_a_photo() {
        let original = beacon("Марк Пётров", 1_300);
        let wire = original.to_wire();
        assert!(wire.len() <= NAME_BEACON_MAX_WIRE, "{} bytes", wire.len());
        assert_eq!(NameBeacon::from_wire(&wire), Some(original));
        assert!(NameBeacon::from_wire(&wire[..wire.len() - 1]).is_none());
        assert!(NameBeacon::from_wire(b"garbage").is_none());
    }

    #[test]
    fn a_beacon_counts_only_under_its_own_names() {
        let real = beacon("Марк Пётров", 0);
        assert!(real.matches_key("петров"));
        assert!(!real.matches_key("иван"));
        assert!(beacon("", 0).matches_key("иван"), "a withdrawal hides the device under any key");
        // The signature covers the name: change it and the signed bytes change.
        assert_ne!(name_beacon_signing_bytes(&real), name_beacon_signing_bytes(&beacon("Марк Петров", 0)));
    }
}
