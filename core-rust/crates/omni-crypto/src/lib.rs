use std::collections::VecDeque;

use aes_gcm::aead::{Aead, Payload as AeadPayload};
use aes_gcm::{Aes256Gcm, KeyInit as AesKeyInit, Nonce as AesNonce};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};
use ed25519_dalek::{
    Signature as Ed25519Signature, Signer, SigningKey as Ed25519SigningKey, Verifier,
    VerifyingKey as Ed25519VerifyingKey,
};
use hkdf::Hkdf;
use omni_identity::{AccountId, AccountRootSecret, DeviceId};
use omni_types::{OmniError, OmniResult};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

pub const CRYPTO_SUITE_VERSION_V2: u16 = 2;
pub const CRYPTO_SUITE_VERSION_CURRENT: u16 = CRYPTO_SUITE_VERSION_V2;
pub const DEFAULT_ONE_TIME_PREKEY_COUNT: usize = 8;

const REPLAY_WINDOW_LIMIT: usize = 64;

/// На сколько счётчик пришедшего конверта может опережать последний принятый. Больше — конверт
/// отвергается, и вернуться в строй сессия сама не может.
///
/// Публичная, потому что это ограничение ОБЕИХ сторон, а не только приёмника: отправитель обязан
/// держать число разных неподтверждённых конвертов в этих же пределах, иначе он раз за разом
/// выносит окно и сессия заклинивает (docs/bugs/session-wedged-by-counter-gap.md). Держать у двух
/// сторон два независимых числа нельзя — разъедутся.
///
/// Почему окно вообще узкое: приёмник доводит цепочку ключей до счётчика конверта ДО проверки
/// подлинности, поэтому каждый шаг — работа, которую может навязать кто угодно. 64 — это потолок
/// такой навязанной работы, а не оценка «сколько сообщений бывает в очереди».
pub const MAX_RECEIVE_GAP: u64 = 64;
/// Сколько ключей обогнанных кадров сессия помнит (см. `skip_receive_chain_to`). Два окна приёма:
/// хвост прошлой цепочки плюс разрыв в текущей. Самые старые вытесняются.
const MAX_SKIPPED_MESSAGE_KEYS: usize = 2 * MAX_RECEIVE_GAP as usize;
const CHACHA_NONCE_LEN: usize = 12;
const AES_GCM_NONCE_LEN: usize = 12;
const AEAD_TAG_LEN: usize = 16;
pub const ATTACHMENT_MEDIA_KEY_LEN_BYTES: usize = 32;
pub const ATTACHMENT_MEDIA_NONCE_SEED_LEN_BYTES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretProtectionScheme {
    DpapiV1,
    PlatformWrappedV1,
    PlaintextFallbackV1,
}

impl SecretProtectionScheme {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::DpapiV1 => "dpapi_v1",
            Self::PlatformWrappedV1 => "platform_wrapped_v1",
            Self::PlaintextFallbackV1 => "plaintext_fallback_v1",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "dpapi_v1" => Ok(Self::DpapiV1),
            "platform_wrapped_v1" => Ok(Self::PlatformWrappedV1),
            "plaintext_fallback_v1" => Ok(Self::PlaintextFallbackV1),
            _ => Err(OmniError::Storage(format!(
                "unsupported secret protection scheme '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedSecret {
    pub scheme: SecretProtectionScheme,
    pub blob_hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdentityPublicKey(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdentitySigningPublicKey(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DevicePublicKey(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrekeyPublicKey(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EphemeralPublicKey(pub [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalIdentityKeyPair {
    pub account_id: AccountId,
    pub public_key: IdentityPublicKey,
    pub signing_public_key: IdentitySigningPublicKey,
    pub protected_private_key: ProtectedSecret,
    pub protected_signing_private_key: ProtectedSecret,
    pub suite_version: u16,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDeviceKeyPair {
    pub device_id: DeviceId,
    pub account_id: AccountId,
    pub public_key: DevicePublicKey,
    pub protected_private_key: ProtectedSecret,
    pub suite_version: u16,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPrekey {
    pub prekey_id: u32,
    pub device_id: DeviceId,
    pub public_key: PrekeyPublicKey,
    pub protected_private_key: ProtectedSecret,
    pub signature: Signature,
    pub suite_version: u16,
    pub created_at_unix_ms: i64,
    pub expires_at_unix_ms: Option<i64>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneTimePrekey {
    pub prekey_id: u32,
    pub device_id: DeviceId,
    pub public_key: PrekeyPublicKey,
    pub protected_private_key: ProtectedSecret,
    pub suite_version: u16,
    pub created_at_unix_ms: i64,
    pub consumed_at_unix_ms: Option<i64>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPrekeyDescriptor {
    pub prekey_id: u32,
    pub public_key: PrekeyPublicKey,
    pub signature: Signature,
    pub expires_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneTimePrekeyDescriptor {
    pub prekey_id: u32,
    pub public_key: PrekeyPublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrekeyBundle {
    pub suite_version: u16,
    pub account_id: AccountId,
    pub device_id: DeviceId,
    pub identity_key: IdentityPublicKey,
    pub identity_signing_key: IdentitySigningPublicKey,
    pub device_key: DevicePublicKey,
    pub signed_prekey: SignedPrekeyDescriptor,
    pub one_time_prekey: Option<OneTimePrekeyDescriptor>,
    pub generated_at_unix_ms: i64,
    /// Proof that this device belongs to the account: master → working key → device
    /// (docs/decisions/multi-device-equal-nodes.md). `None` from a path that carries no
    /// certificate yet (the mesh session bootstrap); mandatory once messages fan out per device.
    #[serde(default)]
    pub device_certificate_chain: Option<DeviceCertificateChainV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAddress {
    pub remote_account_id: AccountId,
    pub remote_device_id: DeviceId,
}

impl SessionAddress {
    pub fn storage_key(&self) -> String {
        format!("{}::{}", self.remote_account_id.0, self.remote_device_id.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBootstrapState {
    InitiatorBootstrapCreated,
    ResponderBootstrapAccepted,
    Established,
}

impl SessionBootstrapState {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::InitiatorBootstrapCreated => "initiator_bootstrap_created",
            Self::ResponderBootstrapAccepted => "responder_bootstrap_accepted",
            Self::Established => "established",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "initiator_bootstrap_created" => Ok(Self::InitiatorBootstrapCreated),
            "responder_bootstrap_accepted" => Ok(Self::ResponderBootstrapAccepted),
            "established" => Ok(Self::Established),
            _ => Err(OmniError::Storage(format!(
                "unsupported session bootstrap state '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_address: SessionAddress,
    pub local_account_id: AccountId,
    pub local_device_id: DeviceId,
    pub local_identity_key: IdentityPublicKey,
    pub local_identity_signing_key: IdentitySigningPublicKey,
    pub local_device_key: DevicePublicKey,
    pub remote_identity_key: IdentityPublicKey,
    pub remote_identity_signing_key: IdentitySigningPublicKey,
    pub remote_device_key: DevicePublicKey,
    pub protected_session_key: ProtectedSecret,
    pub protected_send_chain_key: Option<ProtectedSecret>,
    pub protected_receive_chain_key: Option<ProtectedSecret>,
    /// A STABLE per-contact rendezvous secret derived once from the symmetric X3DH shared secret
    /// (NOT the ratcheting root/chain keys). Both peers derive the identical value, so it seeds the
    /// blinded serverless-rendezvous topic + beacon AEAD (omni-rendezvous). `None` on sessions
    /// bootstrapped before this field existed (recomputed on the next bootstrap). See
    /// docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
    #[serde(default)]
    pub protected_rendezvous_secret: Option<ProtectedSecret>,
    /// Ключи кадров, которые обогнал более поздний кадр (или поздние кадры прошлой цепочки):
    /// без них такой кадр не открыть никогда, а mesh и курьеры порядок не обещают.
    /// `None` — помнить нечего. См. docs/bugs/message-stuck-sending-after-restart.md.
    #[serde(default)]
    pub protected_skipped_message_keys: Option<ProtectedSecret>,
    pub protected_local_ratchet_key: ProtectedSecret,
    pub local_ratchet_public_key: EphemeralPublicKey,
    pub remote_ratchet_public_key: EphemeralPublicKey,
    pub bootstrap_state: SessionBootstrapState,
    pub send_counter: u64,
    pub receive_counter: u64,
    pub previous_send_chain_length: u64,
    pub needs_send_ratchet: bool,
    pub remote_signed_prekey_id: u32,
    pub remote_one_time_prekey_id: Option<u32>,
    pub suite_version: u16,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    /// Which stored version of this session the copy descends from — optimistic concurrency for the
    /// read → advance → write-back every encrypt/decrypt does. `0` = never stored (a session just
    /// built by a bootstrap); storage sets it on every write and refuses a write whose revision is
    /// not the stored one, so a copy taken before someone else advanced the ratchet cannot roll it
    /// back (docs/bugs/stale-session-copy-diverges-ratchet.md). Crypto never touches it.
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindowState {
    pub session_address: SessionAddress,
    pub highest_counter: u64,
    pub recent_tags: Vec<String>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintMaterial {
    pub local_account_id: AccountId,
    pub local_device_id: DeviceId,
    pub local_identity_key: IdentityPublicKey,
    pub local_identity_signing_key: IdentitySigningPublicKey,
    pub local_device_key: DevicePublicKey,
    pub remote_account_id: AccountId,
    pub remote_device_id: DeviceId,
    pub remote_identity_key: IdentityPublicKey,
    pub remote_identity_signing_key: IdentitySigningPublicKey,
    pub remote_device_key: DevicePublicKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBootstrapMessage {
    pub suite_version: u16,
    pub initiator_account_id: AccountId,
    pub initiator_device_id: DeviceId,
    pub initiator_identity_key: IdentityPublicKey,
    pub initiator_identity_signing_key: IdentitySigningPublicKey,
    pub initiator_device_key: DevicePublicKey,
    pub initiator_ephemeral_key: EphemeralPublicKey,
    pub remote_signed_prekey_id: u32,
    pub remote_one_time_prekey_id: Option<u32>,
    pub bootstrap_signature: Signature,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPayload {
    pub counter: u64,
    pub ratchet_public_key_hex: String,
    pub previous_chain_length: u64,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub mac_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptedTextMessage {
    pub message_id: String,
    pub body: String,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecryptedDeliveryReceipt {
    pub message_id: String,
    pub delivered_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCryptoHierarchy {
    pub identity_key: LocalIdentityKeyPair,
    pub device_key: LocalDeviceKeyPair,
    pub signed_prekey: SignedPrekey,
    pub one_time_prekeys: Vec<OneTimePrekey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtRestEncryptedBlob {
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub mac_hex: String,
}

include!("crypto_keys.rs");

include!("crypto_account_keys.rs");

include!("crypto_prekeys_fingerprint.rs");

include!("crypto_bootstrap.rs");

include!("crypto_ratchet.rs");

include!("crypto_wire.rs");

include!("crypto_local_protection.rs");

include!("crypto_attachment_media.rs");
include!("crypto_group.rs");
include!("crypto_omni_id.rs");
include!("crypto_backup.rs");
include!("crypto_content_tree.rs");
include!("crypto_file_at_rest.rs");

include!("internals.rs");

include!("tests.rs");
