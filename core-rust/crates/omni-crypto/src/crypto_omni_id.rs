// Omni-ID (global identity discovery / Model A) — a shareable address that COMMITS to the
// account's master public key, so a stranger holding only the ID can (a) compute the identity-keyed
// rendezvous topic and (b) verify any record fetched by that ID against the ID itself, with no
// trusted server.
//
// Omni-ID = sha256("omni.id.v2" ‖ master_public_key)[:20], rendered as `omni:<40 hex>`. The master
// key is derived from the recovery phrase, so the phrase gives the same Omni-ID back on any device,
// and every device of the account answers to it (docs/decisions/multi-device-equal-nodes.md). A
// record proves itself through the certificate chain master → working key → device.
//
// NB: `include!`d into lib.rs — Sha256/Digest are already in scope there.

pub const OMNI_ID_LEN_BYTES: usize = 20;
pub const OMNI_ID_PREFIX: &str = "omni:";

/// The raw 20-byte commitment to an account's master key.
pub fn omni_id_bytes(master_public_key: &AccountMasterPublicKey) -> [u8; OMNI_ID_LEN_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(b"omni.id.v2");
    hasher.update(master_public_key.0);
    let digest = hasher.finalize();
    let mut out = [0u8; OMNI_ID_LEN_BYTES];
    out.copy_from_slice(&digest[..OMNI_ID_LEN_BYTES]);
    out
}

/// The shareable string form: `omni:<40 hex>`.
pub fn omni_id_string(master_public_key: &AccountMasterPublicKey) -> String {
    format!("{OMNI_ID_PREFIX}{}", hex::encode(omni_id_bytes(master_public_key)))
}

/// Parse a shared Omni-ID string back to its 20 raw bytes. Accepts the `omni:`
/// prefix (case-insensitive prefix) and bare 40-hex; rejects anything else.
pub fn parse_omni_id(s: &str) -> Option<[u8; OMNI_ID_LEN_BYTES]> {
    let trimmed = s.trim();
    let hex_part = trimmed
        .strip_prefix(OMNI_ID_PREFIX)
        .or_else(|| trimmed.strip_prefix("OMNI:"))
        .unwrap_or(trimmed);
    if hex_part.len() != OMNI_ID_LEN_BYTES * 2 {
        return None;
    }
    let raw = hex::decode(hex_part).ok()?;
    raw.try_into().ok()
}

/// Verify that a fetched master key matches the Omni-ID a looker-up used — the "checkable
/// against the ID with no trusted server" property. A courier that swaps keys fails this.
pub fn verify_omni_id_commitment(
    omni_id: &[u8; OMNI_ID_LEN_BYTES],
    master_public_key: &AccountMasterPublicKey,
) -> bool {
    &omni_id_bytes(master_public_key) == omni_id
}

#[cfg(test)]
mod omni_id_tests {
    use super::*;

    #[test]
    fn omni_id_roundtrips_and_commits_to_the_master_key() {
        let master = AccountMasterPublicKey([1; 32]);
        let id = omni_id_string(&master);
        assert!(id.starts_with("omni:"));
        assert_eq!(id.len(), 5 + 40);

        let parsed = parse_omni_id(&id).expect("parse");
        assert_eq!(parsed, omni_id_bytes(&master));
        assert!(verify_omni_id_commitment(&parsed, &master));
        assert!(!verify_omni_id_commitment(&parsed, &AccountMasterPublicKey([9; 32])));
    }

    #[test]
    fn the_phrase_gives_the_same_omni_id_on_every_device() {
        let root = AccountRootSecret(hex::encode([5u8; 32]));
        let first = AccountMasterKey::derive(&root).unwrap().public_key();
        let restored = AccountMasterKey::derive(&root).unwrap().public_key();
        assert_eq!(omni_id_string(&first), omni_id_string(&restored));
    }

    #[test]
    fn parse_accepts_prefix_and_bare_hex_rejects_junk() {
        let bytes = omni_id_bytes(&AccountMasterPublicKey([3; 32]));
        let hex_str = hex::encode(bytes);
        assert_eq!(parse_omni_id(&format!("omni:{hex_str}")), Some(bytes));
        assert_eq!(parse_omni_id(&format!("  omni:{hex_str}  ")), Some(bytes));
        assert_eq!(parse_omni_id(&hex_str), Some(bytes)); // bare hex
        assert_eq!(parse_omni_id("omni:zzzz"), None);
        assert_eq!(parse_omni_id("not-an-id"), None);
        assert_eq!(parse_omni_id(""), None);
    }
}
