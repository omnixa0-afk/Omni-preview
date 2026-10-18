// Account key chain — one Omni-ID on several equal devices
// (docs/decisions/multi-device-equal-nodes.md).
//
//   master key   — derived from the root secret (recovery phrase). Signs the working key.
//                  Its public half IS the account: the account id and the Omni-ID are hashes of it.
//   working key  — version n, derived from the master seed. Lives in the profile folder in portable
//                  form, so a copied folder can sign itself a new device without the phrase. A thief
//                  holding it cannot compute version n+1 (that needs the master seed), which is what
//                  lets the owner move the account to a fresh working key and keep the Omni-ID.
//   device       — today's `LocalIdentityKeyPair` + device key, random per installation. Its Ed25519
//                  half gives the mesh node address, so two devices of one account are two nodes.
//
// NB: `include!`d into lib.rs — the key types, Ed25519 aliases and helpers are already in scope.

const WORKING_KEY_DERIVATION_LABEL: &[u8] = b"omni-account-working-key-v1";
const ACCOUNT_RENDEZVOUS_LABEL: &[u8] = b"omni-account-rendezvous-v1";
const WORKING_KEY_CERTIFICATE_DOMAIN: &[u8] = b"omni-working-key-certificate-v1";
const DEVICE_CERTIFICATE_DOMAIN: &[u8] = b"omni-device-certificate-v2";
const DEVICE_REVOCATION_DOMAIN: &[u8] = b"omni-device-revocation-v1";
const CERTIFICATE_ENCODING_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountMasterPublicKey(pub [u8; 32]);

impl AccountMasterPublicKey {
    pub fn account_id(&self) -> AccountId {
        omni_identity::account_id_from_master_public_key(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkingPublicKey(pub [u8; 32]);

#[derive(Clone, PartialEq, Eq)]
pub struct AccountMasterKey {
    seed: [u8; 32],
    public_key: AccountMasterPublicKey,
}

impl std::fmt::Debug for AccountMasterKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountMasterKey")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl AccountMasterKey {
    pub fn derive(root_secret: &AccountRootSecret) -> OmniResult<Self> {
        let seed = omni_identity::derive_account_master_seed(root_secret)?;
        let public_key =
            AccountMasterPublicKey(Ed25519SigningKey::from_bytes(&seed).verifying_key().to_bytes());
        Ok(Self { seed, public_key })
    }

    pub fn public_key(&self) -> AccountMasterPublicKey {
        self.public_key
    }

    /// The working key of `version` (from 1). Deterministic: every holder of the root secret gets
    /// the same key, so a device restored from the phrase never forks the account's version.
    pub fn working_key(&self, version: u64) -> OmniResult<WorkingKey> {
        if version == 0 {
            return Err(OmniError::InvalidInput(
                "working key version starts at 1".to_string(),
            ));
        }
        let mut info = WORKING_KEY_DERIVATION_LABEL.to_vec();
        info.extend_from_slice(&version.to_be_bytes());
        let seed = hkdf_expand(None, &self.seed, &info)?;
        Ok(WorkingKey::from_seed(version, seed))
    }

    fn sign(&self, message: &[u8]) -> Signature {
        Signature {
            bytes: Ed25519SigningKey::from_bytes(&self.seed)
                .sign(message)
                .to_bytes()
                .to_vec(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct WorkingKey {
    version: u64,
    seed: [u8; 32],
    public_key: WorkingPublicKey,
}

impl std::fmt::Debug for WorkingKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkingKey")
            .field("version", &self.version)
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl WorkingKey {
    /// Rebuild from the stored seed — how a device without the root secret (a copied folder)
    /// holds the working key.
    pub fn from_seed(version: u64, seed: [u8; 32]) -> Self {
        let public_key =
            WorkingPublicKey(Ed25519SigningKey::from_bytes(&seed).verifying_key().to_bytes());
        Self {
            version,
            seed,
            public_key,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn public_key(&self) -> WorkingPublicKey {
        self.public_key
    }

    /// The secret of the account's own rendezvous topic: every device holding this working key
    /// publishes its beacon there and finds the others — how two devices of one account that have
    /// never met (a phrase restore, a folder copied to another network) find each other. Only the
    /// account's devices have it; a new working key version moves the topic away from a revoked one.
    pub fn account_rendezvous_secret(&self) -> OmniResult<[u8; 32]> {
        hkdf_expand(None, &self.seed, ACCOUNT_RENDEZVOUS_LABEL)
    }

    fn sign(&self, message: &[u8]) -> Signature {
        Signature {
            bytes: Ed25519SigningKey::from_bytes(&self.seed)
                .sign(message)
                .to_bytes()
                .to_vec(),
        }
    }
}

/// The master key vouches for working key `version`. Among valid certificates the highest version
/// is the current one; anything signed by an older working key stops counting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingKeyCertificateV1 {
    pub master_public_key: AccountMasterPublicKey,
    pub working_public_key: WorkingPublicKey,
    pub version: u64,
    pub issued_at_unix_ms: i64,
    pub signature_by_master: Signature,
}

impl WorkingKeyCertificateV1 {
    pub fn account_id(&self) -> AccountId {
        self.master_public_key.account_id()
    }
}

/// The working key vouches for one device of the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCertificateV1 {
    pub account_id: AccountId,
    pub device_id: DeviceId,
    pub identity_public_key: IdentityPublicKey,
    pub identity_signing_public_key: IdentitySigningPublicKey,
    pub device_public_key: DevicePublicKey,
    pub working_key_version: u64,
    pub issued_at_unix_ms: i64,
    pub signature_by_working_key: Signature,
}

/// What a device presents to prove it belongs to an account: master → working → device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCertificateChainV1 {
    pub working: WorkingKeyCertificateV1,
    pub device: DeviceCertificateV1,
}

impl DeviceCertificateChainV1 {
    pub fn account_id(&self) -> AccountId {
        self.working.account_id()
    }

    /// The chain in ~470 bytes instead of ~1.4 KB of JSON — for a public beacon that has to fit a
    /// text broker's message (ntfy.sh: 4096 characters of hex, so 2 KB of beacon). The device's
    /// account id is left out: it is the master key's, and [`Self::from_compact`] puts it back
    /// (a chain claiming another account would not verify anyway).
    ///
    /// Layout: version u8 · master 32 · working 32 · working version u64 · issued i64 · master
    /// signature (u8 len + bytes) · device id (u8 len + utf-8) · identity 32 · signing 32 · device 32
    /// · device issued i64 · working-key signature (u8 len + bytes). Integers little-endian.
    pub fn to_compact(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(480);
        out.push(COMPACT_CHAIN_VERSION);
        out.extend_from_slice(&self.working.master_public_key.0);
        out.extend_from_slice(&self.working.working_public_key.0);
        out.extend_from_slice(&self.working.version.to_le_bytes());
        out.extend_from_slice(&self.working.issued_at_unix_ms.to_le_bytes());
        push_short_bytes(&mut out, &self.working.signature_by_master.bytes);
        push_short_bytes(&mut out, self.device.device_id.0.as_bytes());
        out.extend_from_slice(&self.device.identity_public_key.0);
        out.extend_from_slice(&self.device.identity_signing_public_key.0);
        out.extend_from_slice(&self.device.device_public_key.0);
        out.extend_from_slice(&self.device.issued_at_unix_ms.to_le_bytes());
        push_short_bytes(&mut out, &self.device.signature_by_working_key.bytes);
        out
    }

    /// Reads [`Self::to_compact`]; `None` for anything malformed. Says nothing about validity —
    /// verify with [`verify_device_certificate_chain`].
    pub fn from_compact(bytes: &[u8]) -> Option<Self> {
        let mut reader = CompactReader(bytes);
        if reader.take(1)?[0] != COMPACT_CHAIN_VERSION {
            return None;
        }
        let master_public_key = AccountMasterPublicKey(reader.array()?);
        let working_public_key = WorkingPublicKey(reader.array()?);
        let version = u64::from_le_bytes(reader.array()?);
        let issued_at_unix_ms = i64::from_le_bytes(reader.array()?);
        let signature_by_master = Signature { bytes: reader.short_bytes()?.to_vec() };
        let device_id = DeviceId(String::from_utf8(reader.short_bytes()?.to_vec()).ok()?);
        let identity_public_key = IdentityPublicKey(reader.array()?);
        let identity_signing_public_key = IdentitySigningPublicKey(reader.array()?);
        let device_public_key = DevicePublicKey(reader.array()?);
        let device_issued_at_unix_ms = i64::from_le_bytes(reader.array()?);
        let signature_by_working_key = Signature { bytes: reader.short_bytes()?.to_vec() };
        if !reader.0.is_empty() {
            return None;
        }
        let working = WorkingKeyCertificateV1 {
            master_public_key,
            working_public_key,
            version,
            issued_at_unix_ms,
            signature_by_master,
        };
        let device = DeviceCertificateV1 {
            account_id: working.account_id(),
            device_id,
            identity_public_key,
            identity_signing_public_key,
            device_public_key,
            working_key_version: version,
            issued_at_unix_ms: device_issued_at_unix_ms,
            signature_by_working_key,
        };
        Some(Self { working, device })
    }
}

const COMPACT_CHAIN_VERSION: u8 = 1;
const COMPACT_BUNDLE_VERSION: u8 = 1;

impl PrekeyBundle {
    /// What a prekey bundle adds to its device's certificate chain, in ~180 bytes: the prekeys and
    /// the time. Account, device and identity keys are the chain's — a public beacon carries the
    /// chain anyway (compact), so it carries them once ([`Self::from_compact`] puts them back). The
    /// JSON bundle was ~2.5 KB and held the chain a second time (live 2026-10-13: an identity beacon
    /// of 4.5 KB, over what ntfy reads).
    ///
    /// Layout: version u8 · suite u16 · signed prekey id u32 · key 32 · signature (u8 len + bytes) ·
    /// expires (u8 flag + i64) · one-time prekey (u8 flag + id u32 + key 32) · generated i64.
    pub fn to_compact(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(192);
        out.push(COMPACT_BUNDLE_VERSION);
        out.extend_from_slice(&self.suite_version.to_le_bytes());
        out.extend_from_slice(&self.signed_prekey.prekey_id.to_le_bytes());
        out.extend_from_slice(&self.signed_prekey.public_key.0);
        push_short_bytes(&mut out, &self.signed_prekey.signature.bytes);
        match self.signed_prekey.expires_at_unix_ms {
            Some(expires) => {
                out.push(1);
                out.extend_from_slice(&expires.to_le_bytes());
            }
            None => out.push(0),
        }
        match &self.one_time_prekey {
            Some(one_time) => {
                out.push(1);
                out.extend_from_slice(&one_time.prekey_id.to_le_bytes());
                out.extend_from_slice(&one_time.public_key.0);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&self.generated_at_unix_ms.to_le_bytes());
        out
    }

    /// Reads [`Self::to_compact`] back into the bundle of `chain`'s device. `None` for anything
    /// malformed; validity is the caller's (the chain verifies, the signed prekey verifies).
    pub fn from_compact(bytes: &[u8], chain: &DeviceCertificateChainV1) -> Option<Self> {
        let mut reader = CompactReader(bytes);
        if reader.take(1)?[0] != COMPACT_BUNDLE_VERSION {
            return None;
        }
        let suite_version = u16::from_le_bytes(reader.array()?);
        let prekey_id = u32::from_le_bytes(reader.array()?);
        let public_key = PrekeyPublicKey(reader.array()?);
        let signature = Signature { bytes: reader.short_bytes()?.to_vec() };
        let expires_at_unix_ms = match reader.take(1)?[0] {
            0 => None,
            1 => Some(i64::from_le_bytes(reader.array()?)),
            _ => return None,
        };
        let one_time_prekey = match reader.take(1)?[0] {
            0 => None,
            1 => Some(OneTimePrekeyDescriptor {
                prekey_id: u32::from_le_bytes(reader.array()?),
                public_key: PrekeyPublicKey(reader.array()?),
            }),
            _ => return None,
        };
        let generated_at_unix_ms = i64::from_le_bytes(reader.array()?);
        if !reader.0.is_empty() {
            return None;
        }
        Some(Self {
            suite_version,
            account_id: chain.account_id(),
            device_id: chain.device.device_id.clone(),
            identity_key: chain.device.identity_public_key,
            identity_signing_key: chain.device.identity_signing_public_key,
            device_key: chain.device.device_public_key,
            signed_prekey: SignedPrekeyDescriptor { prekey_id, public_key, signature, expires_at_unix_ms },
            one_time_prekey,
            generated_at_unix_ms,
            device_certificate_chain: Some(chain.clone()),
        })
    }
}

fn push_short_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    // Signatures are 64 bytes and device ids short; anything past 255 is cut, and then the chain
    // simply fails to verify rather than encoding something ambiguous.
    let len = bytes.len().min(u8::MAX as usize);
    out.push(len as u8);
    out.extend_from_slice(&bytes[..len]);
}

struct CompactReader<'a>(&'a [u8]);

impl<'a> CompactReader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.0.len() < len {
            return None;
        }
        let (head, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn short_bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.take(1)?[0] as usize;
        self.take(len)
    }
}

/// The account takes one of its devices back: every other device stops sending it messages and
/// accepting anything from it, and it wipes itself when it learns of this
/// (docs/decisions/multi-device-equal-nodes.md, «Устройства»). Signed by the working key, so any
/// device of the account can issue it; one signed by an older working key than the account's
/// current one stops counting, exactly like the certificates it revokes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevocationV1 {
    pub working: WorkingKeyCertificateV1,
    pub account_id: AccountId,
    pub device_id: DeviceId,
    pub revoked_at_unix_ms: i64,
    pub signature_by_working_key: Signature,
}

pub fn issue_device_revocation(
    working_certificate: &WorkingKeyCertificateV1,
    working: &WorkingKey,
    device_id: &DeviceId,
    revoked_at_unix_ms: i64,
) -> OmniResult<DeviceRevocationV1> {
    if working_certificate.working_public_key != working.public_key()
        || working_certificate.version != working.version()
    {
        return Err(OmniError::InvalidInput(
            "working key does not match its certificate".to_string(),
        ));
    }
    require_non_empty("device_id", &device_id.0)?;
    let mut revocation = DeviceRevocationV1 {
        working: working_certificate.clone(),
        account_id: working_certificate.account_id(),
        device_id: device_id.clone(),
        revoked_at_unix_ms,
        signature_by_working_key: Signature { bytes: Vec::new() },
    };
    revocation.signature_by_working_key = working.sign(&device_revocation_bytes(&revocation));
    Ok(revocation)
}

/// The master vouches for the working key, the working key signed this revocation, and the
/// revocation names the account that master hashes to. Says nothing about whether the working key
/// is still the account's current one: that is the receiver's to compare.
pub fn verify_device_revocation(revocation: &DeviceRevocationV1) -> OmniResult<()> {
    verify_working_key_certificate(&revocation.working)?;
    if revocation.account_id != revocation.working.account_id() {
        return Err(OmniError::InvalidInput(
            "device revocation claims another account".to_string(),
        ));
    }
    require_non_empty("device_id", &revocation.device_id.0)?;
    verify_ed25519(
        &revocation.working.working_public_key.0,
        &device_revocation_bytes(revocation),
        &revocation.signature_by_working_key,
    )
}

/// The owner moves the account to a new working key after the old one was stolen
/// (docs/decisions/multi-device-equal-nodes.md, «Переезд при краже рабочего ключа»). The master —
/// from the phrase, which a thief holding only the working key does not have — vouches for working
/// key version n+1 and re-certifies the devices it keeps. Everything the old key signed stops
/// counting: a device that is not in `devices` is out of the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountKeyMoveV1 {
    pub working: WorkingKeyCertificateV1,
    pub devices: Vec<DeviceCertificateChainV1>,
}

impl AccountKeyMoveV1 {
    pub fn account_id(&self) -> AccountId {
        self.working.account_id()
    }
}

/// Build a move: working key `new_working` (version above the old one) certified by `master`, and a
/// fresh certificate for each device in `keep`, copied from its old certificate (the keys are public;
/// only the signature changes).
pub fn issue_account_key_move(
    master: &AccountMasterKey,
    new_working: &WorkingKey,
    keep: &[DeviceCertificateChainV1],
    issued_at_unix_ms: i64,
) -> OmniResult<AccountKeyMoveV1> {
    let working = issue_working_key_certificate(master, new_working, issued_at_unix_ms);
    let mut devices = Vec::with_capacity(keep.len());
    for old in keep {
        if old.account_id() != working.account_id() {
            return Err(OmniError::InvalidInput(
                "a device to keep belongs to another account".to_string(),
            ));
        }
        devices.push(issue_device_certificate(
            &working,
            new_working,
            &old.device.device_id,
            old.device.identity_public_key,
            old.device.identity_signing_public_key,
            old.device.device_public_key,
            issued_at_unix_ms,
        )?);
    }
    Ok(AccountKeyMoveV1 { working, devices })
}

/// The master vouches for the new working key and every kept device is certified by exactly that key
/// for the same account. Whether the version is newer than the account's current one is the
/// receiver's to compare.
pub fn verify_account_key_move(key_move: &AccountKeyMoveV1) -> OmniResult<()> {
    verify_working_key_certificate(&key_move.working)?;
    for chain in &key_move.devices {
        verify_device_certificate_chain(chain)?;
        if chain.working != key_move.working {
            return Err(OmniError::InvalidInput(
                "a device in the move is certified by another working key".to_string(),
            ));
        }
    }
    Ok(())
}

pub fn issue_working_key_certificate(
    master: &AccountMasterKey,
    working: &WorkingKey,
    issued_at_unix_ms: i64,
) -> WorkingKeyCertificateV1 {
    let mut certificate = WorkingKeyCertificateV1 {
        master_public_key: master.public_key(),
        working_public_key: working.public_key(),
        version: working.version(),
        issued_at_unix_ms,
        signature_by_master: Signature { bytes: Vec::new() },
    };
    certificate.signature_by_master = master.sign(&working_key_certificate_bytes(&certificate));
    certificate
}

pub fn verify_working_key_certificate(certificate: &WorkingKeyCertificateV1) -> OmniResult<()> {
    if certificate.version == 0 {
        return Err(OmniError::InvalidInput(
            "working key version starts at 1".to_string(),
        ));
    }
    verify_ed25519(
        &certificate.master_public_key.0,
        &working_key_certificate_bytes(certificate),
        &certificate.signature_by_master,
    )
}

#[allow(clippy::too_many_arguments)] // the certificate's fields
pub fn issue_device_certificate(
    working_certificate: &WorkingKeyCertificateV1,
    working: &WorkingKey,
    device_id: &DeviceId,
    identity_public_key: IdentityPublicKey,
    identity_signing_public_key: IdentitySigningPublicKey,
    device_public_key: DevicePublicKey,
    issued_at_unix_ms: i64,
) -> OmniResult<DeviceCertificateChainV1> {
    if working_certificate.working_public_key != working.public_key()
        || working_certificate.version != working.version()
    {
        return Err(OmniError::InvalidInput(
            "working key does not match its certificate".to_string(),
        ));
    }
    require_non_empty("device_id", &device_id.0)?;
    let mut device = DeviceCertificateV1 {
        account_id: working_certificate.account_id(),
        device_id: device_id.clone(),
        identity_public_key,
        identity_signing_public_key,
        device_public_key,
        working_key_version: working.version(),
        issued_at_unix_ms,
        signature_by_working_key: Signature { bytes: Vec::new() },
    };
    device.signature_by_working_key = working.sign(&device_certificate_bytes(&device));
    Ok(DeviceCertificateChainV1 {
        working: working_certificate.clone(),
        device,
    })
}

/// The whole chain: the master signed the working key, the working key signed the device, and the
/// device claims the account the master key hashes to.
pub fn verify_device_certificate_chain(chain: &DeviceCertificateChainV1) -> OmniResult<()> {
    verify_working_key_certificate(&chain.working)?;
    if chain.device.account_id != chain.working.account_id() {
        return Err(OmniError::InvalidInput(
            "device certificate claims another account".to_string(),
        ));
    }
    if chain.device.working_key_version != chain.working.version {
        return Err(OmniError::InvalidInput(
            "device certificate is signed by another working key version".to_string(),
        ));
    }
    require_non_empty("device_id", &chain.device.device_id.0)?;
    verify_ed25519(
        &chain.working.working_public_key.0,
        &device_certificate_bytes(&chain.device),
        &chain.device.signature_by_working_key,
    )
}

/// A received prekey bundle must carry its device's certificate, and the certificate must vouch for
/// exactly the keys the bundle carries — otherwise a valid certificate of one device could dress up
/// another device's keys. Mandatory: with per-device fan-out the certificate is what says a device
/// belongs to the account (docs/decisions/multi-device-equal-nodes.md, 2.6).
pub fn verify_prekey_bundle_device_certificate(bundle: &PrekeyBundle) -> OmniResult<()> {
    let Some(chain) = bundle.device_certificate_chain.as_ref() else {
        return Err(OmniError::InvalidInput(
            "the prekey bundle carries no device certificate".to_string(),
        ));
    };
    verify_device_certificate_chain(chain)?;
    let device = &chain.device;
    if device.account_id != bundle.account_id
        || device.device_id != bundle.device_id
        || device.identity_public_key != bundle.identity_key
        || device.identity_signing_public_key != bundle.identity_signing_key
        || device.device_public_key != bundle.device_key
    {
        return Err(OmniError::InvalidInput(
            "the device certificate vouches for other keys than the prekey bundle carries"
                .to_string(),
        ));
    }
    Ok(())
}

fn working_key_certificate_bytes(certificate: &WorkingKeyCertificateV1) -> Vec<u8> {
    let mut bytes = certificate_domain_prefix(WORKING_KEY_CERTIFICATE_DOMAIN);
    bytes.extend_from_slice(&certificate.master_public_key.0);
    bytes.extend_from_slice(&certificate.working_public_key.0);
    bytes.extend_from_slice(&certificate.version.to_be_bytes());
    bytes.extend_from_slice(&certificate.issued_at_unix_ms.to_be_bytes());
    bytes
}

fn device_certificate_bytes(certificate: &DeviceCertificateV1) -> Vec<u8> {
    let mut bytes = certificate_domain_prefix(DEVICE_CERTIFICATE_DOMAIN);
    push_length_prefixed(&mut bytes, certificate.account_id.0.as_bytes());
    push_length_prefixed(&mut bytes, certificate.device_id.0.as_bytes());
    bytes.extend_from_slice(&certificate.identity_public_key.0);
    bytes.extend_from_slice(&certificate.identity_signing_public_key.0);
    bytes.extend_from_slice(&certificate.device_public_key.0);
    bytes.extend_from_slice(&certificate.working_key_version.to_be_bytes());
    bytes.extend_from_slice(&certificate.issued_at_unix_ms.to_be_bytes());
    bytes
}

fn device_revocation_bytes(revocation: &DeviceRevocationV1) -> Vec<u8> {
    let mut bytes = certificate_domain_prefix(DEVICE_REVOCATION_DOMAIN);
    push_length_prefixed(&mut bytes, revocation.account_id.0.as_bytes());
    push_length_prefixed(&mut bytes, revocation.device_id.0.as_bytes());
    bytes.extend_from_slice(&revocation.working.version.to_be_bytes());
    bytes.extend_from_slice(&revocation.revoked_at_unix_ms.to_be_bytes());
    bytes
}

fn certificate_domain_prefix(domain: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(domain.len() + 3);
    bytes.extend_from_slice(domain);
    bytes.push(0);
    bytes.extend_from_slice(&CERTIFICATE_ENCODING_VERSION.to_be_bytes());
    bytes
}

fn push_length_prefixed(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value);
}

fn verify_ed25519(public_key: &[u8; 32], message: &[u8], signature: &Signature) -> OmniResult<()> {
    let verifying_key = Ed25519VerifyingKey::from_bytes(public_key).map_err(|error| {
        OmniError::InvalidInput(format!("invalid Ed25519 public key: {error}"))
    })?;
    let signature = Ed25519Signature::from_bytes(&slice_to_array::<64>(&signature.bytes)?);
    verifying_key
        .verify(message, &signature)
        .map_err(|error| OmniError::InvalidInput(format!("signature verification failed: {error}")))
}

fn require_non_empty(field: &str, value: &str) -> OmniResult<()> {
    if value.trim().is_empty() {
        return Err(OmniError::InvalidInput(format!("{field} must not be empty")));
    }
    Ok(())
}

#[cfg(test)]
mod account_key_tests {
    use super::*;

    fn root(byte: u8) -> AccountRootSecret {
        AccountRootSecret(hex::encode([byte; 32]))
    }

    fn device_keys(byte: u8) -> (IdentityPublicKey, IdentitySigningPublicKey, DevicePublicKey) {
        (
            IdentityPublicKey([byte; 32]),
            IdentitySigningPublicKey([byte.wrapping_add(1); 32]),
            DevicePublicKey([byte.wrapping_add(2); 32]),
        )
    }

    fn chain_for(root_secret: &AccountRootSecret, version: u64, device: &str) -> DeviceCertificateChainV1 {
        let master = AccountMasterKey::derive(root_secret).unwrap();
        let working = master.working_key(version).unwrap();
        let working_certificate = issue_working_key_certificate(&master, &working, 1_000);
        let (identity, signing, device_key) = device_keys(7);
        issue_device_certificate(
            &working_certificate,
            &working,
            &DeviceId(device.to_string()),
            identity,
            signing,
            device_key,
            2_000,
        )
        .unwrap()
    }

    #[test]
    fn the_compact_chain_round_trips_small_and_still_verifies() {
        let chain = chain_for(&root(4), 2, "dev-compact");
        let compact = chain.to_compact();
        assert!(compact.len() < 500, "compact chain is {} bytes", compact.len());
        let back = DeviceCertificateChainV1::from_compact(&compact).unwrap();
        assert_eq!(back, chain);
        verify_device_certificate_chain(&back).unwrap();
        // Truncated or padded bytes are not a chain.
        assert!(DeviceCertificateChainV1::from_compact(&compact[..compact.len() - 1]).is_none());
        let mut longer = compact.clone();
        longer.push(0);
        assert!(DeviceCertificateChainV1::from_compact(&longer).is_none());
    }

    #[test]
    fn a_compact_prekey_bundle_comes_back_whole_from_its_chain() {
        let master = AccountMasterKey::derive(&root(5)).unwrap();
        let working = master.working_key(1).unwrap();
        let working_certificate = issue_working_key_certificate(&master, &working, 1_000);
        let device = DeviceId("dev-bundle".to_string());
        let hierarchy = generate_local_key_hierarchy(&master.public_key().account_id(), &device, 1_000, 1).unwrap();
        let chain = issue_device_certificate(
            &working_certificate,
            &working,
            &device,
            hierarchy.identity_key.public_key,
            hierarchy.identity_key.signing_public_key,
            hierarchy.device_key.public_key,
            2_000,
        )
        .unwrap();
        let mut bundle = build_prekey_bundle(
            &hierarchy.identity_key,
            &hierarchy.device_key,
            &hierarchy.signed_prekey,
            hierarchy.one_time_prekeys.first(),
            3_000,
        )
        .unwrap();
        bundle.device_certificate_chain = Some(chain.clone());
        let compact = bundle.to_compact();
        assert!(compact.len() < 200, "compact bundle is {} bytes", compact.len());
        let back = PrekeyBundle::from_compact(&compact, &chain).unwrap();
        assert_eq!(back, bundle);
        verify_prekey_bundle(&back).unwrap();
        assert!(PrekeyBundle::from_compact(&compact[..compact.len() - 1], &chain).is_none());
    }

    #[test]
    fn a_device_chain_verifies_and_names_the_account_of_the_phrase() {
        let chain = chain_for(&root(1), 1, "dev-a");
        verify_device_certificate_chain(&chain).unwrap();
        assert_eq!(
            chain.account_id(),
            omni_identity::derive_account_id_from_root(&root(1)).unwrap()
        );
    }

    #[test]
    fn the_phrase_gives_the_same_master_and_working_key_on_every_device() {
        let first = AccountMasterKey::derive(&root(3)).unwrap();
        let restored = AccountMasterKey::derive(&root(3)).unwrap();
        assert_eq!(first.public_key(), restored.public_key());
        assert_eq!(
            first.working_key(4).unwrap().public_key(),
            restored.working_key(4).unwrap().public_key()
        );
        assert_ne!(
            first.working_key(4).unwrap().public_key(),
            first.working_key(5).unwrap().public_key()
        );
    }

    #[test]
    fn a_copied_working_key_rebuilt_from_its_seed_signs_valid_devices() {
        let master = AccountMasterKey::derive(&root(2)).unwrap();
        let working = master.working_key(1).unwrap();
        let certificate = issue_working_key_certificate(&master, &working, 1_000);
        let copied = WorkingKey::from_seed(1, *working.seed());
        let (identity, signing, device_key) = device_keys(9);
        let chain = issue_device_certificate(
            &certificate,
            &copied,
            &DeviceId("dev-copy".to_string()),
            identity,
            signing,
            device_key,
            3_000,
        )
        .unwrap();
        verify_device_certificate_chain(&chain).unwrap();
    }

    #[test]
    fn a_working_key_not_signed_by_the_accounts_master_is_rejected() {
        // A thief's own key passed off as the account's working key.
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let thief = AccountMasterKey::derive(&root(9)).unwrap().working_key(2).unwrap();
        let mut forged = issue_working_key_certificate(&master, &thief, 1_000);
        forged.version = 3;
        verify_working_key_certificate(&forged).unwrap_err();

        let mut chain = chain_for(&root(1), 1, "dev-a");
        chain.working.working_public_key = thief.public_key();
        verify_device_certificate_chain(&chain).unwrap_err();
    }

    #[test]
    fn any_tampered_device_field_breaks_the_chain() {
        let base = chain_for(&root(1), 1, "dev-a");
        let tamper: [fn(&mut DeviceCertificateChainV1); 6] = [
            |c| c.device.device_id = DeviceId("dev-b".to_string()),
            |c| c.device.identity_signing_public_key.0[0] ^= 1,
            |c| c.device.identity_public_key.0[0] ^= 1,
            |c| c.device.device_public_key.0[0] ^= 1,
            |c| c.device.issued_at_unix_ms += 1,
            |c| c.device.account_id = omni_identity::derive_account_id_from_root(&root(2)).unwrap(),
        ];
        for change in tamper {
            let mut chain = base.clone();
            change(&mut chain);
            verify_device_certificate_chain(&chain).unwrap_err();
        }
    }

    #[test]
    fn a_device_signed_by_an_older_working_key_does_not_pass_as_the_newer() {
        let old = chain_for(&root(1), 1, "dev-a");
        let new = chain_for(&root(1), 2, "dev-a");
        let mut spliced = old.clone();
        spliced.working = new.working.clone();
        verify_device_certificate_chain(&spliced).unwrap_err();
    }

    fn certified_bundle(root_byte: u8, device: &str) -> PrekeyBundle {
        let master = AccountMasterKey::derive(&root(root_byte)).unwrap();
        let working = master.working_key(1).unwrap();
        let working_certificate = issue_working_key_certificate(&master, &working, 1_000);
        let device_id = DeviceId(device.to_string());
        let hierarchy = generate_local_key_hierarchy(
            &master.public_key().account_id(),
            &device_id,
            1_000,
            1,
        )
        .unwrap();
        let mut bundle = build_prekey_bundle(
            &hierarchy.identity_key,
            &hierarchy.device_key,
            &hierarchy.signed_prekey,
            hierarchy.one_time_prekeys.first(),
            1_000,
        )
        .unwrap();
        bundle.device_certificate_chain = Some(
            issue_device_certificate(
                &working_certificate,
                &working,
                &device_id,
                bundle.identity_key,
                bundle.identity_signing_key,
                bundle.device_key,
                1_000,
            )
            .unwrap(),
        );
        bundle
    }

    #[test]
    fn a_bundle_certificate_must_vouch_for_the_bundles_own_keys() {
        let bundle = certified_bundle(1, "dev-a");
        verify_prekey_bundle_device_certificate(&bundle).unwrap();

        // A valid certificate of another device of the same account, on this device's keys.
        let mut dressed_up = bundle.clone();
        dressed_up.device_certificate_chain = certified_bundle(1, "dev-b").device_certificate_chain;
        verify_prekey_bundle_device_certificate(&dressed_up).unwrap_err();

        // Keys swapped under an intact certificate.
        let mut swapped = bundle.clone();
        swapped.identity_signing_key.0[0] ^= 1;
        verify_prekey_bundle_device_certificate(&swapped).unwrap_err();

        // No certificate: nothing says the device belongs to the account.
        let mut bare = bundle;
        bare.device_certificate_chain = None;
        verify_prekey_bundle_device_certificate(&bare).unwrap_err();
    }

    #[test]
    fn every_device_of_the_account_shares_its_rendezvous_topic_and_a_new_version_moves_it() {
        let master = AccountMasterKey::derive(&root(6)).unwrap();
        let copied = WorkingKey::from_seed(1, *master.working_key(1).unwrap().seed());
        let secret = master.working_key(1).unwrap().account_rendezvous_secret().unwrap();
        assert_eq!(copied.account_rendezvous_secret().unwrap(), secret);
        assert_ne!(master.working_key(2).unwrap().account_rendezvous_secret().unwrap(), secret);
        assert_ne!(&secret, copied.seed());
    }

    fn revocation_for(root_secret: &AccountRootSecret, version: u64, device: &str) -> DeviceRevocationV1 {
        let master = AccountMasterKey::derive(root_secret).unwrap();
        let working = master.working_key(version).unwrap();
        let certificate = issue_working_key_certificate(&master, &working, 1_000);
        issue_device_revocation(&certificate, &working, &DeviceId(device.to_string()), 5_000).unwrap()
    }

    #[test]
    fn any_device_holding_the_working_key_can_revoke_another_device() {
        let revocation = revocation_for(&root(1), 1, "dev-lost");
        verify_device_revocation(&revocation).unwrap();
        assert_eq!(
            revocation.account_id,
            omni_identity::derive_account_id_from_root(&root(1)).unwrap()
        );

        // A copied folder holds only the working key's seed, not the master: still enough.
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let certificate = issue_working_key_certificate(&master, &master.working_key(1).unwrap(), 1_000);
        let copied = WorkingKey::from_seed(1, *master.working_key(1).unwrap().seed());
        let from_copy =
            issue_device_revocation(&certificate, &copied, &DeviceId("dev-lost".to_string()), 6_000).unwrap();
        verify_device_revocation(&from_copy).unwrap();
    }

    #[test]
    fn any_tampered_revocation_field_breaks_it() {
        let base = revocation_for(&root(1), 1, "dev-lost");
        let tamper: [fn(&mut DeviceRevocationV1); 4] = [
            |r| r.device_id = DeviceId("dev-other".to_string()),
            |r| r.revoked_at_unix_ms += 1,
            |r| r.account_id = omni_identity::derive_account_id_from_root(&root(2)).unwrap(),
            |r| r.signature_by_working_key.bytes[0] ^= 1,
        ];
        for change in tamper {
            let mut revocation = base.clone();
            change(&mut revocation);
            verify_device_revocation(&revocation).unwrap_err();
        }
    }

    #[test]
    fn a_revocation_signed_by_a_key_the_master_did_not_vouch_for_is_rejected() {
        // A thief's own working key passed off as the account's.
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let thief = AccountMasterKey::derive(&root(9)).unwrap().working_key(1).unwrap();
        let thief_certificate = issue_working_key_certificate(
            &AccountMasterKey::derive(&root(9)).unwrap(),
            &thief,
            1_000,
        );
        let mut forged =
            issue_device_revocation(&thief_certificate, &thief, &DeviceId("dev-a".to_string()), 5_000).unwrap();
        // Claiming the victim's account under the thief's own chain.
        forged.account_id = master.public_key().account_id();
        verify_device_revocation(&forged).unwrap_err();
    }

    #[test]
    fn a_revocation_cannot_be_issued_with_a_working_key_that_does_not_match_its_certificate() {
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let certificate = issue_working_key_certificate(&master, &master.working_key(1).unwrap(), 1_000);
        let other = master.working_key(2).unwrap();
        issue_device_revocation(&certificate, &other, &DeviceId("dev-a".to_string()), 5_000).unwrap_err();
    }

    fn kept_chains(root_secret: &AccountRootSecret, devices: &[&str]) -> Vec<DeviceCertificateChainV1> {
        devices.iter().map(|device| chain_for(root_secret, 1, device)).collect()
    }

    #[test]
    fn a_key_move_certifies_the_new_working_key_and_the_kept_devices_under_it() {
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let new_working = master.working_key(2).unwrap();
        let keep = kept_chains(&root(1), &["dev-a", "dev-b"]);
        let key_move = issue_account_key_move(&master, &new_working, &keep, 9_000).unwrap();
        verify_account_key_move(&key_move).unwrap();
        assert_eq!(key_move.working.version, 2);
        assert_eq!(key_move.devices.len(), 2);
        for (old, new) in keep.iter().zip(&key_move.devices) {
            assert_eq!(old.device.device_id, new.device.device_id);
            assert_eq!(old.device.identity_signing_public_key, new.device.identity_signing_public_key);
            assert_eq!(new.device.working_key_version, 2);
            assert_ne!(old.device.signature_by_working_key, new.device.signature_by_working_key);
        }
    }

    #[test]
    fn a_thief_with_the_old_working_key_cannot_make_a_move() {
        // The thief holds working key 1 and can sign devices with it, but not the master.
        let master = AccountMasterKey::derive(&root(1)).unwrap();
        let thief_master = AccountMasterKey::derive(&root(9)).unwrap();
        let keep = kept_chains(&root(1), &["dev-a"]);
        // A move "for" the victim's account signed by the thief's own master: another account.
        let forged = issue_account_key_move(&thief_master, &thief_master.working_key(2).unwrap(), &[], 9_000).unwrap();
        assert_ne!(forged.account_id(), master.public_key().account_id());
        // Keeping a device of the victim's account in a move of another account is refused.
        issue_account_key_move(&thief_master, &thief_master.working_key(2).unwrap(), &keep, 9_000).unwrap_err();
        // A move whose working certificate is swapped for the old, differently-signed one fails.
        let mut spliced = issue_account_key_move(&master, &master.working_key(2).unwrap(), &keep, 9_000).unwrap();
        spliced.working = keep[0].working.clone();
        verify_account_key_move(&spliced).unwrap_err();
    }

    #[test]
    fn working_key_version_zero_does_not_exist() {
        AccountMasterKey::derive(&root(1)).unwrap().working_key(0).unwrap_err();
    }
}
