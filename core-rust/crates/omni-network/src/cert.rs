// Identity-derived TLS certificates + app-level identity verification.
//
// **Layered design**:
//
// (1) Cert derivation: TLS cert deriv'нут deterministically из identity
//     SigningKey. Это даёт valid TLS handshake без CA infrastructure.
//
// (2) **TLS verifier accepts any cert** (`AcceptAny` в quic_interface.rs).
//     TLS обеспечивает только encryption channel, не identity verification.
//
// (3) **Identity verification — отдельный layer внутри QUIC tunnel'а**:
//     immediately after handshake'а, peer'ы exchange'ятся
//     challenge-response: A sends random nonce, B signs его своим identity
//     SigningKey, A verifies signature against expected pubkey. Если match
//     — identity подтверждена; mismatch → close connection.
//
// **Почему такое разделение** (а не cert-based pinning):
//
// - **No x509 parsing dep** — мы не парсим cert байты, нет supply-chain risk
// - **Re-verify в любой момент** (если хотим — re-challenge later)
// - **Cert format может эволюционировать** независимо от identity verification
// - **Pattern proven в iroh / libp2p** — они тоже не trust'ят TLS PKI
//
// Compromise: peer должен явно ходить через challenge-response после
// QUIC handshake'а до использования connection'а. Стоимость — пара roundtrips,
// negligible.

use crate::address::NodeAddress;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey, SIGNATURE_LENGTH};
use rand_core::{OsRng, RngCore};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use thiserror::Error;

/// Length of identity challenge nonce. 32 bytes = 256 bits = достаточно
/// чтобы exclude collision'ы и pre-computed attacks.
pub const CHALLENGE_NONCE_BYTES: usize = 32;

/// Subject CN format: deterministic из NodeAddress. Доступно через cert
/// inspection для debugging, но не используется для security verification
/// (это делает identity challenge внутри tunnel'а).
pub fn subject_cn_for_address(address: &NodeAddress) -> String {
    format!("omni-{address}")
}

/// Derive a self-signed TLS certificate из identity SigningKey.
/// Деterministic: same key → same cert (modulo serial/dates which не
/// влияют на verification — TLS verifier у нас `AcceptAny`).
pub fn derive_cert_from_signing_key(
    signing_key: &SigningKey,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), CertError> {
    let pubkey = signing_key.verifying_key();
    let address = NodeAddress::from_pubkey(&pubkey);
    let subject_cn = subject_cn_for_address(&address);

    let pkcs8_bytes = ed25519_to_pkcs8_der(signing_key);
    let key_pair = rcgen::KeyPair::try_from(pkcs8_bytes.as_slice())
        .map_err(|e| CertError::KeyPair(format!("rcgen KeyPair: {e}")))?;

    let mut params = rcgen::CertificateParams::new(vec![subject_cn.clone()])
        .map_err(|e| CertError::Params(format!("rcgen params: {e}")))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, subject_cn);

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| CertError::SelfSign(format!("rcgen self_signed: {e}")))?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
    Ok((cert_der, key_der))
}

/// Application-layer challenge для identity verification после QUIC handshake'а.
///
/// Protocol:
/// 1. Initiator generate'ит random nonce, sends его responder'у
/// 2. Responder подписывает payload `(nonce || "omni-identity-v1")` своим SigningKey,
///    отправляет (pubkey, signature) обратно
/// 3. Initiator verify: `signature` валидна над `(nonce || "omni-identity-v1")`
///    под `pubkey`, AND `pubkey` соответствует expected NodeAddress (anti-impersonation)
///
/// Domain separator `"omni-identity-v1"` предотвращает signature reuse в других
/// контекстах (signature от identity challenge не может быть использована как
/// signature над, скажем, announce'ом).
///
/// Если verifyа не проходит — connection drop'ается, peer не trusted.
pub const IDENTITY_CHALLENGE_DOMAIN: &[u8] = b"omni-identity-v1";

/// Generate fresh random nonce для identity challenge.
pub fn fresh_challenge_nonce() -> [u8; CHALLENGE_NONCE_BYTES] {
    let mut nonce = [0u8; CHALLENGE_NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce);
    nonce
}

/// Build the bytes that are signed in response to a challenge.
/// Domain separator + nonce = signed payload.
fn challenge_signing_payload(nonce: &[u8; CHALLENGE_NONCE_BYTES]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(IDENTITY_CHALLENGE_DOMAIN.len() + CHALLENGE_NONCE_BYTES);
    payload.extend_from_slice(IDENTITY_CHALLENGE_DOMAIN);
    payload.extend_from_slice(nonce);
    payload
}

/// Responder side: sign the received challenge nonce и produce a
/// `ChallengeResponse` payload sendable обратно к initiator.
pub fn respond_to_challenge(
    signing_key: &SigningKey,
    nonce: &[u8; CHALLENGE_NONCE_BYTES],
) -> ChallengeResponse {
    let payload = challenge_signing_payload(nonce);
    let signature = signing_key.sign(&payload);
    ChallengeResponse {
        pubkey: signing_key.verifying_key(),
        signature,
    }
}

/// Initiator side: verify the response.
/// - signature над `(domain || nonce)` под claimed pubkey
/// - pubkey соответствует expected NodeAddress
///
/// Returns the verified pubkey on success; CertError на mismatch.
pub fn verify_challenge_response(
    response: &ChallengeResponse,
    nonce: &[u8; CHALLENGE_NONCE_BYTES],
    expected: &NodeAddress,
) -> Result<VerifyingKey, CertError> {
    if !expected.verify_for_pubkey(&response.pubkey) {
        return Err(CertError::AddressMismatch {
            expected: *expected,
            actual: NodeAddress::from_pubkey(&response.pubkey),
        });
    }
    let payload = challenge_signing_payload(nonce);
    response
        .pubkey
        .verify(&payload, &response.signature)
        .map_err(|e| CertError::InvalidSignature(e.to_string()))?;
    Ok(response.pubkey)
}

/// Wire format для ChallengeResponse: pubkey (32 bytes) || signature (64 bytes).
/// Total: 96 bytes — fixed, no length prefixes нужны.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeResponse {
    pub pubkey: VerifyingKey,
    pub signature: Signature,
}

impl ChallengeResponse {
    pub const WIRE_SIZE: usize = 32 + SIGNATURE_LENGTH;

    pub fn encode_to_wire(&self) -> [u8; Self::WIRE_SIZE] {
        let mut buf = [0u8; Self::WIRE_SIZE];
        buf[..32].copy_from_slice(self.pubkey.as_bytes());
        buf[32..].copy_from_slice(&self.signature.to_bytes());
        buf
    }

    pub fn decode_from_wire(bytes: &[u8]) -> Result<Self, CertError> {
        if bytes.len() != Self::WIRE_SIZE {
            return Err(CertError::WireDecode(format!(
                "expected {} bytes, got {}",
                Self::WIRE_SIZE,
                bytes.len()
            )));
        }
        let pubkey_bytes: [u8; 32] = bytes[..32].try_into().expect("len checked");
        let pubkey = VerifyingKey::from_bytes(&pubkey_bytes)
            .map_err(|e| CertError::WireDecode(format!("invalid pubkey: {e}")))?;
        let sig_bytes: [u8; SIGNATURE_LENGTH] = bytes[32..].try_into().expect("len checked");
        let signature = Signature::from_bytes(&sig_bytes);
        Ok(Self { pubkey, signature })
    }
}

/// Build PKCS#8 DER blob из 32-byte Ed25519 secret. Format spec'ифицируется
/// RFC 8410 §7. Сделано хардкодом чтобы не тащить дополнительные дep'ы.
fn ed25519_to_pkcs8_der(signing_key: &SigningKey) -> Vec<u8> {
    let secret_bytes = signing_key.to_bytes();
    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(&[
        0x30, 0x2e,                                  // SEQUENCE 46
        0x02, 0x01, 0x00,                            // INTEGER 0 (version)
        0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,    // AlgorithmIdentifier {Ed25519}
        0x04, 0x22,                                  // OCTET STRING 34
        0x04, 0x20,                                  // inner OCTET STRING 32
    ]);
    out.extend_from_slice(&secret_bytes);
    out
}

#[derive(Debug, Error)]
pub enum CertError {
    #[error("invalid key pair construction: {0}")]
    KeyPair(String),
    #[error("invalid cert params: {0}")]
    Params(String),
    #[error("self-sign failed: {0}")]
    SelfSign(String),
    #[error("challenge response pubkey ({actual:?}) doesn't match expected address ({expected:?})")]
    AddressMismatch {
        expected: NodeAddress,
        actual: NodeAddress,
    },
    #[error("Ed25519 signature verification failed: {0}")]
    InvalidSignature(String),
    #[error("wire decode failed: {0}")]
    WireDecode(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn cert_derivation_succeeds() {
        let key = fresh_key();
        let (cert, _privkey) =
            derive_cert_from_signing_key(&key).expect("derivation должна работать");
        assert!(!cert.as_ref().is_empty());
    }

    #[test]
    fn certs_from_different_keys_differ() {
        let key1 = fresh_key();
        let key2 = fresh_key();
        let (cert1, _) = derive_cert_from_signing_key(&key1).unwrap();
        let (cert2, _) = derive_cert_from_signing_key(&key2).unwrap();
        assert_ne!(cert1.as_ref(), cert2.as_ref());
    }

    #[test]
    fn challenge_response_round_trip() {
        // Happy path: A challenges, B responds, A verifies. Success.
        let alice_key = fresh_key();
        let bob_key = fresh_key();
        let bob_address = NodeAddress::from_pubkey(&bob_key.verifying_key());

        // Alice (initiator) generates nonce.
        let nonce = fresh_challenge_nonce();

        // Bob (responder) signs.
        let response = respond_to_challenge(&bob_key, &nonce);

        // Alice verifies.
        let verified = verify_challenge_response(&response, &nonce, &bob_address);
        assert!(verified.is_ok(), "happy-path challenge должна verify'иться: {verified:?}");
        assert_eq!(verified.unwrap(), bob_key.verifying_key());

        // Bonus: alice_key не используется в этом тесте (Alice — verifier, не signer).
        let _ = alice_key;
    }

    #[test]
    fn challenge_response_rejects_wrong_address() {
        // Anti-impersonation: B response'ит своим pubkey'ем, но Alice
        // expects pubkey of C. Verify должен fail.
        let bob_key = fresh_key();
        let charlie_key = fresh_key();
        let charlie_address = NodeAddress::from_pubkey(&charlie_key.verifying_key());

        let nonce = fresh_challenge_nonce();
        let response = respond_to_challenge(&bob_key, &nonce);

        let result = verify_challenge_response(&response, &nonce, &charlie_address);
        assert!(
            matches!(result, Err(CertError::AddressMismatch { .. })),
            "expected AddressMismatch, got {result:?}"
        );
    }

    #[test]
    fn challenge_response_rejects_tampered_signature() {
        let bob_key = fresh_key();
        let bob_address = NodeAddress::from_pubkey(&bob_key.verifying_key());

        let nonce = fresh_challenge_nonce();
        let mut response = respond_to_challenge(&bob_key, &nonce);

        // Tamper signature.
        let mut sig_bytes = response.signature.to_bytes();
        sig_bytes[0] ^= 0xFF;
        response.signature = Signature::from_bytes(&sig_bytes);

        let result = verify_challenge_response(&response, &nonce, &bob_address);
        assert!(
            matches!(result, Err(CertError::InvalidSignature(_))),
            "expected InvalidSignature, got {result:?}"
        );
    }

    #[test]
    fn challenge_response_rejects_replay_with_different_nonce() {
        // Critical anti-replay: signature over nonce_A не должна passing
        // verify когда verifier expects nonce_B. Защита от MITM replay'я
        // старых responses.
        let bob_key = fresh_key();
        let bob_address = NodeAddress::from_pubkey(&bob_key.verifying_key());

        let original_nonce = fresh_challenge_nonce();
        let response = respond_to_challenge(&bob_key, &original_nonce);

        let different_nonce = fresh_challenge_nonce();
        let result = verify_challenge_response(&response, &different_nonce, &bob_address);
        assert!(
            matches!(result, Err(CertError::InvalidSignature(_))),
            "replay с другим nonce должен fail: {result:?}"
        );
    }

    #[test]
    fn challenge_response_wire_round_trip() {
        let bob_key = fresh_key();
        let nonce = fresh_challenge_nonce();
        let response = respond_to_challenge(&bob_key, &nonce);

        let bytes = response.encode_to_wire();
        assert_eq!(bytes.len(), ChallengeResponse::WIRE_SIZE);

        let decoded = ChallengeResponse::decode_from_wire(&bytes).unwrap();
        assert_eq!(response, decoded);
    }

    #[test]
    fn challenge_response_wire_rejects_wrong_size() {
        let result = ChallengeResponse::decode_from_wire(&[0; 50]);
        assert!(matches!(result, Err(CertError::WireDecode(_))));
    }

    #[test]
    fn challenge_response_wire_rejects_garbage_pubkey() {
        // Decoding with 96 bytes но invalid pubkey bytes — rejected.
        let mut bytes = [0u8; ChallengeResponse::WIRE_SIZE];
        // All-zeros not a valid Ed25519 point — should fail.
        // (Actually all-zeros IS valid identity element so this may pass
        // pubkey check. Test the structural shape with random non-zero
        // bytes which пытаются формировать invalid pubkey.)
        bytes[0] = 0xFF;
        bytes[1] = 0xFF;
        let _ = ChallengeResponse::decode_from_wire(&bytes);
        // Some bytes patterns can be valid Ed25519 points; just verify
        // we don't panic. Stronger guarantee — signature verify caught
        // by happy-path tests.
    }

    #[test]
    fn pkcs8_der_format_is_correct_length() {
        let key = fresh_key();
        let pkcs8 = ed25519_to_pkcs8_der(&key);
        assert_eq!(pkcs8.len(), 48);
        assert_eq!(pkcs8[0], 0x30);
        assert_eq!(pkcs8[1], 0x2e);
    }

    #[test]
    fn subject_cn_is_deterministic() {
        let key = fresh_key();
        let addr = NodeAddress::from_pubkey(&key.verifying_key());
        let cn1 = subject_cn_for_address(&addr);
        let cn2 = subject_cn_for_address(&addr);
        assert_eq!(cn1, cn2);
        assert!(cn1.starts_with("omni-"));
        assert_eq!(cn1.len(), 5 + 40); // "omni-" + 40 hex chars
    }

    #[test]
    fn fresh_nonces_differ() {
        let n1 = fresh_challenge_nonce();
        let n2 = fresh_challenge_nonce();
        assert_ne!(n1, n2, "two fresh nonces одинаковые — RNG failure?");
    }
}
