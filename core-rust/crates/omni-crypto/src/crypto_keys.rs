/// Return the raw Ed25519 identity *signing* key bytes for seeding the mesh
/// overlay identity (`omni-network` NodeAddress = blake3 of the public half).
///
/// SENSITIVE: these are private key bytes. The caller must keep them in-process
/// only to construct the local mesh node identity — never persist, log, or
/// transmit them. The public half is already published in `PrekeyBundleV1` as
/// `identity_signing_public_key`, so peers derive our NodeAddress without it.
pub fn mesh_identity_signing_key_bytes(
    identity_key: &LocalIdentityKeyPair,
) -> OmniResult<[u8; 32]> {
    let signing_key = unprotect_ed25519_signing_key(&identity_key.protected_signing_private_key)?;
    Ok(signing_key.to_bytes())
}

pub fn generate_at_rest_master_key() -> OmniResult<[u8; 32]> {
    let bytes = random_bytes(32)?;
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// The key that wraps a database's key for one account: what the recovery phrase opens when the
/// folder came from another machine (DPAPI cannot). `root_secret` is the account root the phrase
/// carries — high-entropy, so HKDF is enough (no password KDF). See
/// docs/decisions/db-at-rest-encryption.md.
pub fn derive_db_key_wrap_key(root_secret: &[u8], account_id: &str) -> OmniResult<[u8; 32]> {
    hkdf_expand(None, root_secret, format!("omni-db-dek-wrap-v1|{account_id}").as_bytes())
}

pub fn generate_local_key_hierarchy(
    account_id: &AccountId,
    device_id: &DeviceId,
    now_unix_ms: i64,
    one_time_prekey_count: usize,
) -> OmniResult<LocalCryptoHierarchy> {
    if account_id.0.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "account_id must not be empty".to_string(),
        ));
    }
    if device_id.0.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "device_id must not be empty".to_string(),
        ));
    }

    let identity_private = generate_x25519_secret();
    let identity_public = IdentityPublicKey(X25519PublicKey::from(&identity_private).to_bytes());
    let signing_key = Ed25519SigningKey::generate(&mut OsRng);
    let signing_public = IdentitySigningPublicKey(signing_key.verifying_key().to_bytes());
    let identity_key = LocalIdentityKeyPair {
        account_id: account_id.clone(),
        public_key: identity_public,
        signing_public_key: signing_public,
        protected_private_key: protect_bytes(identity_private.to_bytes().as_ref())?,
        protected_signing_private_key: protect_bytes(signing_key.to_bytes().as_ref())?,
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };

    let device_private = generate_x25519_secret();
    let device_key = LocalDeviceKeyPair {
        device_id: device_id.clone(),
        account_id: account_id.clone(),
        public_key: DevicePublicKey(X25519PublicKey::from(&device_private).to_bytes()),
        protected_private_key: protect_bytes(device_private.to_bytes().as_ref())?,
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
    };

    let signed_prekey_private = generate_x25519_secret();
    let signed_prekey_public = PrekeyPublicKey(X25519PublicKey::from(&signed_prekey_private).to_bytes());
    let signed_prekey_message = signed_prekey_signature_message(
        account_id,
        device_id,
        identity_public,
        signing_public,
        signed_prekey_public,
    );
    let signed_prekey = SignedPrekey {
        prekey_id: next_prekey_id(),
        device_id: device_id.clone(),
        public_key: signed_prekey_public,
        protected_private_key: protect_bytes(signed_prekey_private.to_bytes().as_ref())?,
        signature: sign_message(&identity_key.protected_signing_private_key, &signed_prekey_message)?,
        suite_version: CRYPTO_SUITE_VERSION_CURRENT,
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: None,
        updated_at_unix_ms: now_unix_ms,
    };

    let one_time_prekeys =
        generate_one_time_prekeys(device_id, now_unix_ms, one_time_prekey_count)?;

    Ok(LocalCryptoHierarchy {
        identity_key,
        device_key,
        signed_prekey,
        one_time_prekeys,
    })
}

/// Одноразовые предключи — РАСХОДНИК, а не часть фундамента: каждый входящий bootstrap съедает один.
/// Раньше пул выдавался ровно один раз при создании профиля (`DEFAULT_ONE_TIME_PREKEY_COUNT` = 8) и
/// не пополнялся никогда. После восьми входящих знакомств профиль переставал принимать новые
/// сессии, а на следующем входе `ensure_local_crypto_foundation` видел «ключи есть, одноразовых
/// нет», считал это повреждением и отказывался поднимать профиль — приложение уходило на экран
/// онбординга с целой, но неоткрываемой базой. Поймано живьём 2026-08-14.
///
/// Функция отделена от генерации иерархии именно для пополнения: идентичность и подписанный
/// предключ при этом НЕ трогаются, иначе пополнение меняло бы личность.
pub fn generate_one_time_prekeys(
    device_id: &DeviceId,
    now_unix_ms: i64,
    count: usize,
) -> OmniResult<Vec<OneTimePrekey>> {
    let mut one_time_prekeys = Vec::with_capacity(count);
    for _ in 0..count {
        let private = generate_x25519_secret();
        one_time_prekeys.push(OneTimePrekey {
            prekey_id: next_prekey_id(),
            device_id: device_id.clone(),
            public_key: PrekeyPublicKey(X25519PublicKey::from(&private).to_bytes()),
            protected_private_key: protect_bytes(private.to_bytes().as_ref())?,
            suite_version: CRYPTO_SUITE_VERSION_CURRENT,
            created_at_unix_ms: now_unix_ms,
            consumed_at_unix_ms: None,
            updated_at_unix_ms: now_unix_ms,
        });
    }
    Ok(one_time_prekeys)
}

