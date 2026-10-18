use std::collections::HashSet;
use std::sync::OnceLock;

use bip39::{Language, Mnemonic};
use omni_types::{OmniError, OmniResult};
use rand_core::{OsRng, RngCore};

pub(crate) const ACCOUNT_ROOT_SECRET_BYTES: usize = 32;
pub(crate) const RECOVERY_PHRASE_V2_WORD_COUNT: usize = 24;

const ENGLISH_WORDLIST_EXPECTED_COUNT: usize = 2048;
const ENGLISH_WORDLIST: &str = include_str!("mnemonic/english.txt");

static WORDLIST_VALIDATION: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedMnemonicV2 {
    pub normalized_mnemonic: String,
    pub root_bytes: [u8; ACCOUNT_ROOT_SECRET_BYTES],
}

pub(crate) fn generate_random_account_root_bytes() -> [u8; ACCOUNT_ROOT_SECRET_BYTES] {
    let mut bytes = [0u8; ACCOUNT_ROOT_SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

pub(crate) fn encode_root_to_mnemonic_v2_bytes(
    root_bytes: &[u8; ACCOUNT_ROOT_SECRET_BYTES],
) -> OmniResult<String> {
    ensure_wordlist_is_canonical()?;

    let mnemonic = Mnemonic::from_entropy_in(Language::English, root_bytes).map_err(|error| {
        OmniError::Internal(format!(
            "failed to encode account root secret as mnemonic: {error}"
        ))
    })?;

    Ok(mnemonic.to_string())
}

pub(crate) fn decode_mnemonic_v2_to_root_bytes(
    mnemonic: &str,
) -> OmniResult<[u8; ACCOUNT_ROOT_SECRET_BYTES]> {
    Ok(parse_mnemonic_v2(mnemonic)?.root_bytes)
}

pub(crate) fn validate_mnemonic_v2(mnemonic: &str) -> OmniResult<()> {
    parse_mnemonic_v2(mnemonic).map(|_| ())
}

pub(crate) fn normalize_mnemonic_v2(mnemonic: &str) -> OmniResult<String> {
    Ok(parse_mnemonic_v2(mnemonic)?.normalized_mnemonic)
}

pub(crate) fn parse_mnemonic_v2(mnemonic: &str) -> OmniResult<ParsedMnemonicV2> {
    ensure_wordlist_is_canonical()?;

    let normalized_input = normalize_words(mnemonic)?;
    let word_count = normalized_input.split(' ').count();
    if word_count != RECOVERY_PHRASE_V2_WORD_COUNT {
        return Err(OmniError::InvalidInput(format!(
            "recovery phrase must contain exactly {RECOVERY_PHRASE_V2_WORD_COUNT} English words"
        )));
    }

    let parsed =
        Mnemonic::parse_in_normalized(Language::English, &normalized_input).map_err(|error| {
            OmniError::InvalidInput(format!(
                "invalid recovery phrase: checksum or word format check failed ({error})"
            ))
        })?;

    let entropy = parsed.to_entropy();
    if entropy.len() != ACCOUNT_ROOT_SECRET_BYTES {
        return Err(OmniError::InvalidInput(format!(
            "recovery phrase entropy must be {ACCOUNT_ROOT_SECRET_BYTES} bytes"
        )));
    }

    let mut root_bytes = [0u8; ACCOUNT_ROOT_SECRET_BYTES];
    root_bytes.copy_from_slice(&entropy);

    Ok(ParsedMnemonicV2 {
        normalized_mnemonic: parsed.to_string(),
        root_bytes,
    })
}

fn ensure_wordlist_is_canonical() -> OmniResult<()> {
    let validation = WORDLIST_VALIDATION.get_or_init(validate_wordlist);
    match validation {
        Ok(()) => Ok(()),
        Err(error) => Err(OmniError::Internal(format!(
            "canonical mnemonic english wordlist is invalid: {error}"
        ))),
    }
}

fn validate_wordlist() -> Result<(), String> {
    let words = ENGLISH_WORDLIST.lines().map(str::trim).collect::<Vec<_>>();

    if words.len() != ENGLISH_WORDLIST_EXPECTED_COUNT {
        return Err(format!(
            "expected {ENGLISH_WORDLIST_EXPECTED_COUNT} words, got {}",
            words.len()
        ));
    }

    if words.iter().any(|word| word.is_empty()) {
        return Err("wordlist contains empty lines".to_string());
    }

    let mut unique = HashSet::with_capacity(words.len());
    for word in &words {
        if !unique.insert(*word) {
            return Err(format!("wordlist contains duplicate word '{word}'"));
        }
    }

    let canonical = Language::English.word_list();
    if canonical.len() != ENGLISH_WORDLIST_EXPECTED_COUNT {
        return Err(format!(
            "bip39 english list has unexpected length {}",
            canonical.len()
        ));
    }

    if words
        .iter()
        .zip(canonical.iter())
        .any(|(left, right)| *left != *right)
    {
        return Err("wordlist content does not match bip39 canonical English wordlist".to_string());
    }

    Ok(())
}

fn normalize_words(value: &str) -> OmniResult<String> {
    let normalized = value
        .split_whitespace()
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect::<Vec<_>>();

    if normalized.is_empty() {
        return Err(OmniError::InvalidInput(
            "recovery phrase must not be empty".to_string(),
        ));
    }

    Ok(normalized.join(" "))
}
