// The account key chain at rest (docs/decisions/multi-device-equal-nodes.md).
//
// `account_keys` holds what a COPIED folder needs to act for the account: the public master key and
// the working key. The working key is stored portably (not under DPAPI) so the copy can certify its
// own new device on another machine; the master seed is never stored here — it is re-derived from
// the root secret, which stays in the DPAPI-sealed profile secrets.
// `account_devices` holds the certificates of the account's devices, this one included.

/// The working key in the clear. Chosen by the owner for a folder without a password: copying the
/// folder must be enough to carry the account to another computer.
const WORKING_KEY_SCHEME_PORTABLE_PLAIN: &str = "portable_plain_v1";
/// The working key under the profile password (its Argon2id unlock key) — still portable, but a
/// stolen folder is useless without the password. The password guards nothing else in the folder:
/// the root secret stays sealed to its machine.
const WORKING_KEY_SCHEME_PASSWORD_WRAPPED: &str = "password_wrapped_v1";

enum StoredWorkingKey {
    Absent,
    /// Under the profile password, and the password was not given.
    Locked,
    Ready(omni_crypto::WorkingKey, omni_crypto::WorkingKeyCertificateV1),
}

fn working_key_wrap_context(account_id: &AccountId) -> Vec<u8> {
    format!("omni-password-wrap-working-key|{}", account_id.0).into_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountKeysRecord {
    pub account_id: AccountId,
    pub master_public_key: omni_crypto::AccountMasterPublicKey,
    pub working_certificate: omni_crypto::WorkingKeyCertificateV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDeviceRecord {
    pub chain: omni_crypto::DeviceCertificateChainV1,
    pub device_label: String,
    pub first_seen_at_unix_ms: i64,
    pub revoked_at_unix_ms: Option<i64>,
    /// When the owner was told about this device; `None` = still new.
    pub acknowledged_at_unix_ms: Option<i64>,
    /// When the owner took this (revoked) device out of the Settings list; `None` = listed.
    pub hidden_at_unix_ms: Option<i64>,
}

impl Storage {
    /// Derive the master key from the root secret and store the account's first working key.
    /// Runs when an account is created or restored — both have the root secret in hand.
    fn persist_account_keys_from_root(
        &self,
        account_id: &AccountId,
        root_secret: &omni_identity::AccountRootSecret,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let master = omni_crypto::AccountMasterKey::derive(root_secret)?;
        if &master.public_key().account_id() != account_id {
            return Err(OmniError::Internal(
                "account id does not match the account's master key".to_string(),
            ));
        }
        let working = master.working_key(1)?;
        let certificate = omni_crypto::issue_working_key_certificate(&master, &working, now_unix_ms);
        self.store_working_key(account_id, &working, &certificate, None, now_unix_ms)
    }

    fn store_working_key(
        &self,
        account_id: &AccountId,
        working: &omni_crypto::WorkingKey,
        certificate: &omni_crypto::WorkingKeyCertificateV1,
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let certificate_json =
            serde_json::to_string(certificate).map_err(map_serde_error_to_storage)?;
        let (scheme, blob) = match password_unlock_key {
            None => (WORKING_KEY_SCHEME_PORTABLE_PLAIN, hex::encode(working.seed())),
            Some(unlock_key) => {
                let wrapped = encrypt_at_rest_bytes(
                    unlock_key,
                    &working_key_wrap_context(account_id),
                    working.seed(),
                )?;
                (
                    WORKING_KEY_SCHEME_PASSWORD_WRAPPED,
                    serde_json::to_string(&wrapped).map_err(map_serde_error_to_storage)?,
                )
            }
        };
        self.connection
            .execute(
                "
                INSERT INTO account_keys (
                    account_id,
                    master_public_key,
                    working_key_version,
                    working_key_protection_scheme,
                    working_key_blob,
                    working_key_certificate_json,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(account_id) DO UPDATE SET
                    master_public_key = excluded.master_public_key,
                    working_key_version = excluded.working_key_version,
                    working_key_protection_scheme = excluded.working_key_protection_scheme,
                    working_key_blob = excluded.working_key_blob,
                    working_key_certificate_json = excluded.working_key_certificate_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    account_id.0.as_str(),
                    hex::encode(certificate.master_public_key.0),
                    working.version() as i64,
                    scheme,
                    blob,
                    certificate_json,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        self.remember_account_rendezvous_secret(account_id, working)
    }

    /// Keep the account rendezvous secret of `working` under this machine's protection.
    fn remember_account_rendezvous_secret(
        &self,
        account_id: &AccountId,
        working: &omni_crypto::WorkingKey,
    ) -> OmniResult<()> {
        let protected = omni_crypto::protect_bytes(&working.account_rendezvous_secret()?)?;
        self.connection
            .execute(
                "
                UPDATE account_keys
                SET rendezvous_secret_protection_scheme = ?2, rendezvous_secret_blob = ?3
                WHERE account_id = ?1
                ",
                params![
                    account_id.0.as_str(),
                    protected.scheme.as_db_value(),
                    protected.blob_hex,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The secret of the account's own rendezvous topic, where this account's devices find each
    /// other (docs/decisions/multi-device-equal-nodes.md). `None` when it cannot be had right now:
    /// no key chain, or a copied folder whose working key waits for the password.
    pub fn get_account_rendezvous_secret(&self) -> OmniResult<Option<[u8; 32]>> {
        let Some(account) = self.get_local_account()? else {
            return Ok(None);
        };
        let account_id = &account.descriptor.account_id;
        if let StoredWorkingKey::Ready(working, _) = self.read_working_key(account_id, None)? {
            return working.account_rendezvous_secret().map(Some);
        }
        let row: Option<(Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "
                SELECT rendezvous_secret_protection_scheme, rendezvous_secret_blob
                FROM account_keys WHERE account_id = ?1
                ",
                params![account_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some(Some(protected)) = row
            .map(|(scheme, blob)| protected_secret_from_optional_db(scheme, blob))
            .transpose()?
        else {
            return Ok(None);
        };
        // Sealed on another machine (a copied folder): unreadable here until the password unlock
        // stores it again.
        Ok(omni_crypto::unprotect_bytes(&protected)
            .ok()
            .and_then(|bytes| bytes.try_into().ok()))
    }

    pub fn get_account_keys(&self, account_id: &AccountId) -> OmniResult<Option<AccountKeysRecord>> {
        let row: Option<String> = self
            .connection
            .query_row(
                "SELECT working_key_certificate_json FROM account_keys WHERE account_id = ?1",
                params![account_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some(certificate_json) = row else {
            return Ok(None);
        };
        let working_certificate: omni_crypto::WorkingKeyCertificateV1 =
            serde_json::from_str(&certificate_json).map_err(map_serde_error_to_storage)?;
        Ok(Some(AccountKeysRecord {
            account_id: account_id.clone(),
            master_public_key: working_certificate.master_public_key,
            working_certificate,
        }))
    }

    fn read_working_key(
        &self,
        account_id: &AccountId,
        password_unlock_key: Option<&[u8; 32]>,
    ) -> OmniResult<StoredWorkingKey> {
        let row: Option<(i64, String, String, String)> = self
            .connection
            .query_row(
                "
                SELECT working_key_version, working_key_protection_scheme, working_key_blob,
                       working_key_certificate_json
                FROM account_keys
                WHERE account_id = ?1
                ",
                params![account_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((version, scheme, blob, certificate_json)) = row else {
            return Ok(StoredWorkingKey::Absent);
        };
        let seed_bytes = match (scheme.as_str(), password_unlock_key) {
            (WORKING_KEY_SCHEME_PORTABLE_PLAIN, _) => hex::decode(&blob).ok(),
            (WORKING_KEY_SCHEME_PASSWORD_WRAPPED, None) => return Ok(StoredWorkingKey::Locked),
            (WORKING_KEY_SCHEME_PASSWORD_WRAPPED, Some(unlock_key)) => {
                let wrapped: AtRestEncryptedBlob =
                    serde_json::from_str(&blob).map_err(map_serde_error_to_storage)?;
                Some(decrypt_at_rest_bytes(
                    unlock_key,
                    &working_key_wrap_context(account_id),
                    &wrapped,
                )?)
            }
            (other, _) => {
                return Err(OmniError::Storage(format!(
                    "unknown working key protection scheme '{other}'"
                )))
            }
        };
        let seed: [u8; 32] = seed_bytes
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| OmniError::Storage("working key blob is malformed".to_string()))?;
        let working = omni_crypto::WorkingKey::from_seed(version as u64, seed);
        let certificate: omni_crypto::WorkingKeyCertificateV1 =
            serde_json::from_str(&certificate_json).map_err(map_serde_error_to_storage)?;
        if certificate.working_public_key != working.public_key() {
            return Err(OmniError::Storage(
                "stored working key does not match its certificate".to_string(),
            ));
        }
        Ok(StoredWorkingKey::Ready(working, certificate))
    }

    /// Move the working key between the clear and the profile password, following the password
    /// itself: set (`None` → key), changed (old → new), removed (old → `None`).
    fn rewrap_working_key(
        &self,
        account_id: &AccountId,
        from_unlock_key: Option<&[u8; 32]>,
        to_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        match self.read_working_key(account_id, from_unlock_key)? {
            StoredWorkingKey::Absent => Ok(()),
            StoredWorkingKey::Locked => Err(OmniError::InvalidInput(
                "the profile already has a password: change it with the current one".to_string(),
            )),
            StoredWorkingKey::Ready(working, certificate) => {
                self.store_working_key(account_id, &working, &certificate, to_unlock_key, now_unix_ms)
            }
        }
    }

    /// After a password unlock: the moment a password-protected working key can sign this device.
    fn certify_local_device_with_password_key(
        &self,
        password_unlock_key: &[u8; 32],
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        // A working key that arrived while the password was not at hand goes under it now.
        self.adopt_pending_working_key(password_unlock_key, now_unix_ms)?;
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Ok(());
        };
        if self.get_local_identity_key(&account.descriptor.account_id)?.is_none() {
            return Ok(()); // no crypto foundation yet; building it certifies the device itself
        }
        self.ensure_local_device_certificate(
            &account.descriptor.account_id,
            &device,
            Some(password_unlock_key),
            now_unix_ms,
        )
    }

    /// After a phrase unlock: the phrase re-derives the master key and so the working key — whether
    /// the stored one waits for a password or not — and the device is certified with it.
    fn certify_local_device_with_root(
        &self,
        account_id: &AccountId,
        root_secret: &omni_identity::AccountRootSecret,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let (Some(keys), Some(device)) = (self.get_account_keys(account_id)?, self.get_local_device()?)
        else {
            return Ok(());
        };
        if self.get_local_identity_key(account_id)?.is_none() {
            return Ok(()); // no crypto foundation yet; building it certifies the device itself
        }
        let master = omni_crypto::AccountMasterKey::derive(root_secret)?;
        if master.public_key() != keys.master_public_key {
            return Err(OmniError::InvalidInput(
                "the phrase does not belong to this account's master key".to_string(),
            ));
        }
        let working = master.working_key(keys.working_certificate.version)?;
        if working.public_key() != keys.working_certificate.working_public_key {
            return Err(OmniError::Storage(
                "the phrase's working key does not match the stored certificate".to_string(),
            ));
        }
        self.certify_local_device_with_working(
            account_id,
            &device,
            &working,
            &keys.working_certificate,
            now_unix_ms,
        )
    }

    /// Certify the current device with the working key, once. A no-op for an account without a
    /// key chain (nothing to sign with), for a device that already holds its certificate, and for a
    /// working key under a password that was not given (the password unlock certifies it).
    fn ensure_local_device_certificate(
        &self,
        account_id: &AccountId,
        device: &LocalDeviceSnapshot,
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let StoredWorkingKey::Ready(working, working_certificate) =
            self.read_working_key(account_id, password_unlock_key)?
        else {
            return Ok(());
        };
        self.certify_local_device_with_working(
            account_id,
            device,
            &working,
            &working_certificate,
            now_unix_ms,
        )
    }

    fn certify_local_device_with_working(
        &self,
        account_id: &AccountId,
        device: &LocalDeviceSnapshot,
        working: &omni_crypto::WorkingKey,
        working_certificate: &omni_crypto::WorkingKeyCertificateV1,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let already: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM account_devices WHERE account_id = ?1 AND device_id = ?2",
                params![account_id.0.as_str(), device.device_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        // A copied folder: the secret was sealed by the other machine; seal it again for this one.
        self.remember_account_rendezvous_secret(account_id, working)?;
        if already.is_some() {
            return Ok(());
        }
        let identity = self.get_local_identity_key(account_id)?.ok_or_else(|| {
            OmniError::Storage("local identity key is missing".to_string())
        })?;
        let device_key = self.get_local_device_key(&device.device_id)?.ok_or_else(|| {
            OmniError::Storage("local device key is missing".to_string())
        })?;
        let chain = omni_crypto::issue_device_certificate(
            working_certificate,
            working,
            &device.device_id,
            identity.public_key,
            identity.signing_public_key,
            device_key.public_key,
            now_unix_ms,
        )?;
        self.store_account_device(&chain, &device.device_label, now_unix_ms)
    }

    fn store_account_device(
        &self,
        chain: &omni_crypto::DeviceCertificateChainV1,
        device_label: &str,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let chain_json = serde_json::to_string(chain).map_err(map_serde_error_to_storage)?;
        self.connection
            .execute(
                "
                INSERT INTO account_devices (
                    account_id,
                    device_id,
                    certificate_chain_json,
                    device_label,
                    first_seen_at_unix_ms,
                    revoked_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, NULL)
                ON CONFLICT(account_id, device_id) DO UPDATE SET
                    certificate_chain_json = excluded.certificate_chain_json,
                    device_label = excluded.device_label
                ",
                params![
                    chain.device.account_id.0.as_str(),
                    chain.device.device_id.0.as_str(),
                    chain_json,
                    device_label,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        // Revoked stays revoked: a certificate that turns up after its revocation does not undo it.
        self.connection
            .execute(
                "
                UPDATE account_devices
                SET revoked_at_unix_ms = COALESCE(
                    revoked_at_unix_ms,
                    (SELECT revoked_at_unix_ms FROM device_revocations
                     WHERE account_id = account_devices.account_id
                       AND device_id = account_devices.device_id)
                )
                WHERE account_id = ?1 AND device_id = ?2
                ",
                params![
                    chain.device.account_id.0.as_str(),
                    chain.device.device_id.0.as_str(),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// This device's proof of membership in its account — what it shows its contacts.
    pub fn get_local_device_certificate_chain(
        &self,
    ) -> OmniResult<Option<omni_crypto::DeviceCertificateChainV1>> {
        let Some(device) = self.get_local_device()? else {
            return Ok(None);
        };
        Ok(self
            .list_account_devices(&device.account_id)?
            .into_iter()
            .find(|record| record.chain.device.device_id == device.device_id)
            .map(|record| record.chain))
    }

    pub fn list_account_devices(&self, account_id: &AccountId) -> OmniResult<Vec<AccountDeviceRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT certificate_chain_json, device_label, first_seen_at_unix_ms, revoked_at_unix_ms,
                       acknowledged_at_unix_ms, hidden_at_unix_ms
                FROM account_devices
                WHERE account_id = ?1
                ORDER BY first_seen_at_unix_ms ASC, device_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![account_id.0.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })
            .map_err(map_sqlite_error)?;
        let mut devices = Vec::new();
        for row in rows {
            let (
                chain_json,
                device_label,
                first_seen_at_unix_ms,
                revoked_at_unix_ms,
                acknowledged_at_unix_ms,
                hidden_at_unix_ms,
            ) = row.map_err(map_sqlite_error)?;
            devices.push(AccountDeviceRecord {
                chain: serde_json::from_str(&chain_json).map_err(map_serde_error_to_storage)?,
                device_label,
                first_seen_at_unix_ms,
                revoked_at_unix_ms,
                acknowledged_at_unix_ms,
                hidden_at_unix_ms,
            });
        }
        Ok(devices)
    }

    fn delete_account_key_rows(&self, account_id: &str) -> OmniResult<()> {
        for table in [
            "account_devices",
            "account_keys",
            "device_revocations",
            "account_working_keys",
            "pending_working_keys",
            "account_key_moves",
            "pending_key_move_devices",
            "pending_key_move_seeds",
        ] {
            self.connection
                .execute(
                    &format!("DELETE FROM {table} WHERE account_id = ?1"),
                    params![account_id],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }
}
