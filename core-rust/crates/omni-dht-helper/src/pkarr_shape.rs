//! Our BEP44 values in the shape of a pkarr record — the signed DNS packets that pkarr (Pubky) and
//! iroh's address lookup keep in Mainline — so that a node storing one, or a crawler of the DHT, sees
//! one more iroh endpoint record and not an odd blob (docs/decisions/libp2p-kad-vs-mainline.md,
//! option A).
//!
//! What iroh publishes (`iroh-dns` 1.3, `SignedPacket::from_txt_strings`): a BEP44 mutable item
//! without a salt, `seq` = the time in microseconds, value = a DNS reply (flags: response only, id 0)
//! whose answers are TXT records named `_iroh.<z-base-32 public key>`, TTL 30 s, one
//! `key=value` string each; names after the first are compressed to a pointer at offset 12. The
//! attribute for application data is `user-data` (at most 245 bytes a string). Ours: the sealed
//! blob, base64url, in as many `user-data` strings as it takes.
//!
//! The transport stays plain bencode — an on-path observer still sees a BEP44 put — but the record
//! itself is a well-formed pkarr packet.

/// The TXT name iroh publishes under (`iroh_dns::attrs::IROH_TXT_NAME`).
const TXT_NAME: &str = "_iroh";

/// iroh's default record TTL (`iroh::address_lookup::pkarr::DEFAULT_PKARR_TTL`).
const TTL_SECS: u32 = 30;

/// iroh's attribute for application data and its longest value (`UserData::MAX_LENGTH`).
const USER_DATA: &str = "user-data=";
const USER_DATA_MAX: usize = 245;

/// The largest DNS packet a pkarr record may carry — BEP44's value cap.
pub(crate) const MAX_PACKET: usize = 1000;

const TYPE_TXT: u16 = 16;
const CLASS_IN: u16 = 1;
/// The header's flags of `Packet::new_reply`: response, standard query, no error.
const FLAGS_REPLY: u16 = 0x8000;
/// A compressed name pointing at the first answer's name (right after the 12-byte header).
const NAME_POINTER: [u8; 2] = [0xC0, 0x0C];

const Z32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

/// z-base-32 of a public key, as pkarr names its zone (52 characters for 32 bytes).
pub(crate) fn z32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(Z32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(Z32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn from_base64url(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).ok()
}

/// The pkarr-shaped DNS packet carrying `payload` for the zone of `public_key`, or `None` when it
/// would not fit a BEP44 value.
pub(crate) fn wrap(public_key: &[u8; 32], payload: &[u8]) -> Option<Vec<u8>> {
    let encoded = base64url(payload);
    // A whole `user-data=…` string is one TXT character-string (≤255 bytes); iroh caps the value at
    // 245, which leaves the prefix room.
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(USER_DATA_MAX)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64url is ASCII"))
        .collect();
    let mut packet = Vec::with_capacity(MAX_PACKET);
    packet.extend_from_slice(&0u16.to_be_bytes()); // id
    packet.extend_from_slice(&FLAGS_REPLY.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes()); // questions
    packet.extend_from_slice(&(chunks.len().max(1) as u16).to_be_bytes()); // answers
    packet.extend_from_slice(&0u16.to_be_bytes()); // name servers
    packet.extend_from_slice(&0u16.to_be_bytes()); // additional records
    let zone = z32(public_key);
    let strings: Vec<String> = if chunks.is_empty() {
        vec![USER_DATA.to_string()]
    } else {
        chunks.iter().map(|chunk| format!("{USER_DATA}{chunk}")).collect()
    };
    for (index, text) in strings.iter().enumerate() {
        if index == 0 {
            for label in [TXT_NAME, zone.as_str()] {
                packet.push(label.len() as u8);
                packet.extend_from_slice(label.as_bytes());
            }
            packet.push(0);
        } else {
            packet.extend_from_slice(&NAME_POINTER);
        }
        packet.extend_from_slice(&TYPE_TXT.to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        packet.extend_from_slice(&TTL_SECS.to_be_bytes());
        // One character-string: a length byte and at most 255 bytes.
        let bytes = text.as_bytes();
        packet.extend_from_slice(&((bytes.len() + 1) as u16).to_be_bytes());
        packet.push(bytes.len() as u8);
        packet.extend_from_slice(bytes);
    }
    (packet.len() <= MAX_PACKET).then_some(packet)
}

/// The payload of a packet [`wrap`] made for the zone of `public_key`; `None` for anything else —
/// somebody's real pkarr record, garbage, another zone.
pub(crate) fn unwrap(public_key: &[u8; 32], packet: &[u8]) -> Option<Vec<u8>> {
    let read_u16 = |at: usize| -> Option<u16> { Some(u16::from_be_bytes(packet.get(at..at + 2)?.try_into().ok()?)) };
    if packet.len() < 12 || read_u16(2)? & 0x8000 == 0 || read_u16(4)? != 0 {
        return None;
    }
    let answers = read_u16(6)?;
    let zone = z32(public_key);
    let mut at = 12;
    let mut encoded = String::new();
    for index in 0..answers {
        // The name: the full `_iroh.<zone>` first, a pointer to it after.
        if index == 0 {
            for label in [TXT_NAME, zone.as_str()] {
                let len = *packet.get(at)? as usize;
                if packet.get(at + 1..at + 1 + len)? != label.as_bytes() {
                    return None;
                }
                at += 1 + len;
            }
            if *packet.get(at)? != 0 {
                return None;
            }
            at += 1;
        } else {
            if packet.get(at..at + 2)? != NAME_POINTER {
                return None;
            }
            at += 2;
        }
        if read_u16(at)? != TYPE_TXT || read_u16(at + 2)? != CLASS_IN {
            return None;
        }
        let rdata_len = read_u16(at + 8)? as usize;
        at += 10;
        let rdata = packet.get(at..at + rdata_len)?;
        at += rdata_len;
        let text_len = *rdata.first()? as usize;
        let text = std::str::from_utf8(rdata.get(1..1 + text_len)?).ok()?;
        encoded.push_str(text.strip_prefix(USER_DATA)?);
    }
    (at == packet.len()).then_some(())?;
    from_base64url(&encoded)
}

/// The most raw payload [`wrap`] fits into one record.
pub(crate) fn max_payload() -> usize {
    let key = [0u8; 32];
    (0..=MAX_PACKET).rev().find(|len| wrap(&key, &vec![0u8; *len]).is_some()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The z-base-32 of pkarr: 52 characters for a key, the alphabet's first letter for zero bits.
    #[test]
    fn z32_names_a_key_in_52_characters() {
        assert_eq!(z32(&[0u8; 32]), "y".repeat(52));
        assert_eq!(z32(&[0xffu8; 32]).len(), 52);
        // Most significant bits first, no padding (iroh-base's `Z_BASE_32`): 00001 → 'b', 000(00) → 'y'.
        assert_eq!(z32(&[0b0000_1000]), "by");
    }

    /// Any payload a beacon can be comes back unchanged; the packet is a DNS reply of `_iroh` TXT
    /// records with `user-data`, and someone else's zone or a changed byte reads as nothing.
    #[test]
    fn a_payload_round_trips_through_a_pkarr_shaped_packet() {
        let key = [7u8; 32];
        for len in [0usize, 1, 183, 284, 540, max_payload()] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let packet = wrap(&key, &payload).unwrap_or_else(|| panic!("{len} B fits"));
            assert!(packet.len() <= MAX_PACKET);
            assert_eq!(&packet[2..4], &FLAGS_REPLY.to_be_bytes());
            let name = format!("_iroh.{}", z32(&key));
            assert!(packet.windows(z32(&key).len()).any(|w| w == z32(&key).as_bytes()), "{name}");
            assert_eq!(unwrap(&key, &packet), Some(payload.clone()), "{len} B");
            assert_eq!(unwrap(&[8u8; 32], &packet), None, "another zone");
        }
        assert!(wrap(&key, &vec![0u8; max_payload() + 1]).is_none(), "too big for one record");
        // `omni_rendezvous::DHT_MAX_BLOB` (the client's guard) is this number; the helper holds no
        // rendezvous code, so the two meet here.
        assert_eq!(max_payload(), 628, "a sealed beacon of the 512-byte class (540 B) fits");
    }

    /// The packet parses as DNS the way pkarr readers parse it: header counts, one full name then
    /// pointers, TXT/IN, TTL 30, one character-string each.
    #[test]
    fn the_packet_is_well_formed_dns() {
        let key = [3u8; 32];
        let packet = wrap(&key, &[1u8; 540]).unwrap();
        let answers = u16::from_be_bytes([packet[6], packet[7]]);
        assert_eq!(answers, 3, "540 B = 720 base64url characters = three user-data strings");
        assert_eq!(&packet[4..6], &[0, 0], "no questions");
        let first_name_len = 1 + TXT_NAME.len() + 1 + 52 + 1;
        let ttl_at = 12 + first_name_len + 4;
        assert_eq!(&packet[ttl_at..ttl_at + 4], &TTL_SECS.to_be_bytes());
        assert_eq!(packet.windows(2).filter(|w| *w == NAME_POINTER).count(), 2, "later names are pointers");
    }
}
