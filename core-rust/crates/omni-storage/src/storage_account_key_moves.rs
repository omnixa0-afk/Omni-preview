// The account moves to a new working key (docs/decisions/multi-device-equal-nodes.md, «Переезд при
// краже рабочего ключа»).
//
// The master key — from the phrase, which a thief holding only the working key does not have —
// vouches for working key version n+1 and re-certifies the devices the owner keeps. Anyone can check
// that, so the move needs no trust in whoever hands it over; what each side keeps is the highest
// working key version it has seen for an account, and everything certified by a lower one stops
// counting: a device that is not re-certified is out.

/// What applying a move did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountKeyMoveOutcome {
    /// `false` when this version was already known: nothing to pass on.
    pub newly_applied: bool,
    /// The move is for our own account.
    pub own_account: bool,
    /// Our own account only: this device is among the devices the owner kept.
    pub this_device_kept: bool,
    /// Devices that lost their place because the move did not re-certify them.
    pub devices_superseded: usize,
}

impl Storage {
    /// The newest working key version this profile knows for `account_id`: the account's own key
    /// chain when it is ours, and the highest version seen in a move or a device certificate.
    pub fn current_working_key_version(&self, account_id: &AccountId) -> OmniResult<Option<u64>> {
        let mut highest = self
            .get_account_keys(account_id)?
            .map(|keys| keys.working_certificate.version);
        let registered: Option<i64> = self
            .connection
            .query_row(
                "SELECT version FROM account_working_keys WHERE account_id = ?1",
                params![account_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if let Some(version) = registered {
            highest = Some(highest.map_or(version as u64, |current| current.max(version as u64)));
        }
        Ok(highest)
    }

    /// A working key certificate of some account showed up (inside a device certificate chain). When
    /// it is newer than the one we know, the account has moved: remember the version and, for a
    /// contact's account, drop the devices certified by an older key. Our own account's devices are
    /// only superseded by an explicit move (see `apply_account_key_move`) — a missed message must not
    /// cost a device its place.
    pub fn observe_working_key_certificate(
        &self,
        certificate: &omni_crypto::WorkingKeyCertificateV1,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        omni_crypto::verify_working_key_certificate(certificate)?;
        let account_id = certificate.account_id();
        if self
            .current_working_key_version(&account_id)?
            .is_some_and(|known| known >= certificate.version)
        {
            return Ok(());
        }
        self.register_working_key_certificate(certificate, now_unix_ms)?;
        let own = self
            .get_local_account()?
            .is_some_and(|account| account.descriptor.account_id == account_id);
        if !own {
            self.supersede_stale_remote_devices(&account_id, certificate.version, &Default::default(), now_unix_ms)?;
        }
        Ok(())
    }

    fn register_working_key_certificate(
        &self,
        certificate: &omni_crypto::WorkingKeyCertificateV1,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let certificate_json =
            serde_json::to_string(certificate).map_err(map_serde_error_to_storage)?;
        self.connection
            .execute(
                "
                INSERT INTO account_working_keys (account_id, version, certificate_json, updated_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(account_id) DO UPDATE SET
                    version = excluded.version,
                    certificate_json = excluded.certificate_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                WHERE excluded.version > account_working_keys.version
                ",
                params![
                    certificate.account_id().0.as_str(),
                    certificate.version as i64,
                    certificate_json,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Verify and apply a move. `own_working_seed` is the new working key's seed, handed over by the
    /// device that made the move to the devices it kept (never to contacts): without it this device
    /// learns the new certificates but cannot sign with the new key. `password_unlock_key` is this
    /// profile's password key when it has a password — the new key is kept under it; without it the
    /// seed waits sealed to this machine until the next unlock.
    pub fn apply_account_key_move(
        &self,
        key_move: &omni_crypto::AccountKeyMoveV1,
        own_working_seed: Option<[u8; 32]>,
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<AccountKeyMoveOutcome> {
        omni_crypto::verify_account_key_move(key_move)?;
        self.with_immediate_transaction("apply_account_key_move", |storage| {
            storage.apply_verified_account_key_move(key_move, own_working_seed, password_unlock_key, now_unix_ms)
        })
    }

    fn apply_verified_account_key_move(
        &self,
        key_move: &omni_crypto::AccountKeyMoveV1,
        own_working_seed: Option<[u8; 32]>,
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<AccountKeyMoveOutcome> {
        let account_id = key_move.account_id();
        let new_version = key_move.working.version;
        let own = match (self.get_local_account()?, self.get_local_device()?) {
            (Some(account), Some(device)) => {
                (account.descriptor.account_id == account_id).then_some(device.device_id)
            }
            _ => None,
        };
        let known = self.current_working_key_version(&account_id)?;
        if known.is_some_and(|known| new_version < known) {
            return Err(OmniError::InvalidInput(
                "the move is to an older working key than the account's current one".to_string(),
            ));
        }
        let kept: std::collections::HashSet<DeviceId> = key_move
            .devices
            .iter()
            .map(|chain| chain.device.device_id.clone())
            .collect();
        let newly_applied = known.is_none_or(|known| new_version > known);
        let this_device_kept = own.as_ref().is_some_and(|device_id| kept.contains(device_id));

        if newly_applied {
            self.register_working_key_certificate(&key_move.working, now_unix_ms)?;
        }
        let mut devices_superseded = 0;
        if own.is_some() {
            if newly_applied {
                devices_superseded =
                    self.apply_own_account_move(key_move, &kept, now_unix_ms)?;
                self.remember_own_account_move(key_move, now_unix_ms)?;
            }
            if let Some(seed) = own_working_seed {
                self.adopt_working_key_seed(&account_id, key_move, seed, password_unlock_key, now_unix_ms)?;
            }
        } else if newly_applied {
            for chain in &key_move.devices {
                self.connection
                    .execute(
                        "
                        UPDATE remote_prekey_bundles
                        SET device_certificate_chain_json = ?3
                        WHERE remote_account_id = ?1 AND remote_device_id = ?2
                        ",
                        params![
                            account_id.0.as_str(),
                            chain.device.device_id.0.as_str(),
                            serde_json::to_string(chain).map_err(map_serde_error_to_storage)?,
                        ],
                    )
                    .map_err(map_sqlite_error)?;
                self.clear_superseded(&account_id, &chain.device.device_id)?;
            }
            devices_superseded =
                self.supersede_stale_remote_devices(&account_id, new_version, &kept, now_unix_ms)?;
        }
        Ok(AccountKeyMoveOutcome {
            newly_applied,
            own_account: own.is_some(),
            this_device_kept,
            devices_superseded,
        })
    }

    /// "The account key was stolen": move this account to working key version n+1. The master comes
    /// from the root secret sealed to this machine, or from `phrase` when it is not readable here
    /// (a copied folder). `keep` are the other devices of the account that stay; this device always
    /// stays. Returns the move and the new key's seed — the move goes to everybody, the seed only to
    /// the kept devices of ours.
    ///
    /// A profile with a password needs it: the new key goes under it like the old one was.
    pub fn move_local_account_to_new_working_key(
        &self,
        keep: &[DeviceId],
        phrase: Option<&str>,
        password: Option<&str>,
        now_unix_ms: i64,
    ) -> OmniResult<(omni_crypto::AccountKeyMoveV1, [u8; 32])> {
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Err(OmniError::InvalidInput("no local account".to_string()));
        };
        let account_id = account.descriptor.account_id.clone();
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Err(OmniError::InvalidInput("no active profile".to_string()));
        };
        let Some(keys) = self.get_account_keys(&account_id)? else {
            return Err(OmniError::InvalidInput("the account has no key chain".to_string()));
        };
        let row = self
            .get_profile_secrets_row(&profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let root_secret = match phrase.map(str::trim).filter(|phrase| !phrase.is_empty()) {
            Some(phrase) => omni_identity::parse_mnemonic_v2(phrase)?.root_secret,
            None => match load_profile_secrets_from_profile_secrets_row(&row) {
                Ok(Some(stored)) => stored.root_secret,
                _ => {
                    return Err(OmniError::InvalidInput(
                        "the recovery phrase is needed: this computer cannot open the stored root".to_string(),
                    ))
                }
            },
        };
        let master = omni_crypto::AccountMasterKey::derive(&root_secret)?;
        if master.public_key() != keys.master_public_key {
            return Err(OmniError::InvalidInput(
                "the recovery phrase does not belong to this account".to_string(),
            ));
        }
        let unlock_key = if row.password_enabled {
            let password = password.ok_or_else(|| {
                OmniError::InvalidInput("the profile password is needed to move the account key".to_string())
            })?;
            Some(self.verify_profile_password(&profile_id, password)?)
        } else {
            None
        };

        let current = self.current_working_key_version(&account_id)?.unwrap_or(1);
        let new_working = master.working_key(current + 1)?;
        let mut keep_chains = Vec::new();
        for record in self.list_account_devices(&account_id)? {
            let device_id = &record.chain.device.device_id;
            let wanted = *device_id == device.device_id || keep.contains(device_id);
            if wanted && record.revoked_at_unix_ms.is_none() {
                keep_chains.push(record.chain);
            }
        }
        let key_move = omni_crypto::issue_account_key_move(&master, &new_working, &keep_chains, now_unix_ms)?;
        let seed = *new_working.seed();
        // All or nothing: a move applied here whose seed never got queued for the kept devices would
        // leave them on a key the account no longer honours.
        self.with_immediate_transaction("move_local_account_to_new_working_key", |storage| {
            storage.apply_account_key_move(&key_move, Some(seed), unlock_key.as_ref(), now_unix_ms)?;
            storage.start_key_move_delivery(&key_move, &seed, &device.device_id)
        })?;
        Ok((key_move, seed))
    }

    /// Keep our account's latest move, so it can be handed to whoever has not heard of it (a contact
    /// that was away, a device that joined later).
    pub fn remember_own_account_move(
        &self,
        key_move: &omni_crypto::AccountKeyMoveV1,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO account_key_moves (account_id, version, move_json, updated_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(account_id) DO UPDATE SET
                    version = excluded.version,
                    move_json = excluded.move_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                WHERE excluded.version > account_key_moves.version
                ",
                params![
                    key_move.account_id().0.as_str(),
                    key_move.working.version as i64,
                    serde_json::to_string(key_move).map_err(map_serde_error_to_storage)?,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The latest move of our own account, if it ever moved.
    pub fn latest_own_account_move(&self) -> OmniResult<Option<omni_crypto::AccountKeyMoveV1>> {
        let Some(account) = self.get_local_account()? else {
            return Ok(None);
        };
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT move_json FROM account_key_moves WHERE account_id = ?1",
                params![account.descriptor.account_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(map_serde_error_to_storage))
            .transpose()
    }

    /// The move and the new working key's seed, with the devices of ours that have not yet shown the
    /// new certificate — they have to be handed both. `None` when nothing is pending.
    pub fn pending_key_move_delivery(
        &self,
    ) -> OmniResult<Option<(omni_crypto::AccountKeyMoveV1, [u8; 32], Vec<(DeviceId, String)>)>> {
        let Some(account) = self.get_local_account()? else {
            return Ok(None);
        };
        let account_id = account.descriptor.account_id;
        let Some(key_move) = self.latest_own_account_move()? else {
            return Ok(None);
        };
        let seed_row: Option<(String, String)> = self
            .connection
            .query_row(
                "SELECT protection_scheme, blob_hex FROM pending_key_move_seeds WHERE account_id = ?1 AND version = ?2",
                params![account_id.0.as_str(), key_move.working.version as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((scheme, blob_hex)) = seed_row else {
            return Ok(None);
        };
        let protected = omni_crypto::ProtectedSecret {
            scheme: omni_crypto::SecretProtectionScheme::from_db_value(&scheme)?,
            blob_hex,
        };
        let Ok(seed_bytes) = omni_crypto::unprotect_bytes(&protected) else {
            return Ok(None);
        };
        let seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|_| OmniError::Storage("pending key move seed is malformed".to_string()))?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT account_devices.device_id, account_devices.dialog_key
                FROM pending_key_move_devices
                JOIN account_devices
                  ON account_devices.account_id = pending_key_move_devices.account_id
                 AND account_devices.device_id = pending_key_move_devices.device_id
                WHERE pending_key_move_devices.account_id = ?1
                  AND pending_key_move_devices.version = ?2
                  AND account_devices.revoked_at_unix_ms IS NULL
                  AND account_devices.dialog_key IS NOT NULL
                ",
            )
            .map_err(map_sqlite_error)?;
        let devices = statement
            .query_map(params![account_id.0.as_str(), key_move.working.version as i64], |row| {
                Ok((DeviceId(row.get(0)?), row.get::<_, String>(1)?))
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        if devices.is_empty() {
            return Ok(None);
        }
        Ok(Some((key_move, seed, devices)))
    }

    /// A kept device of ours showed the new certificate: it has the move and the key.
    pub(crate) fn mark_key_move_delivered(&self, chain: &omni_crypto::DeviceCertificateChainV1) -> OmniResult<()> {
        self.connection
            .execute(
                "
                DELETE FROM pending_key_move_devices
                WHERE account_id = ?1 AND device_id = ?2 AND version <= ?3
                ",
                params![
                    chain.device.account_id.0.as_str(),
                    chain.device.device_id.0.as_str(),
                    chain.working.version as i64,
                ],
            )
            .map_err(map_sqlite_error)?;
        // Nobody left to hand the key to: it is no longer kept.
        self.connection
            .execute(
                "
                DELETE FROM pending_key_move_seeds
                WHERE account_id = ?1
                  AND NOT EXISTS (SELECT 1 FROM pending_key_move_devices WHERE account_id = ?1)
                ",
                params![chain.device.account_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    fn start_key_move_delivery(
        &self,
        key_move: &omni_crypto::AccountKeyMoveV1,
        seed: &[u8; 32],
        local_device: &DeviceId,
    ) -> OmniResult<()> {
        let account_id = key_move.account_id();
        let protected = omni_crypto::protect_bytes(seed)?;
        self.connection
            .execute(
                "
                INSERT INTO pending_key_move_seeds (account_id, version, protection_scheme, blob_hex)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(account_id) DO UPDATE SET
                    version = excluded.version,
                    protection_scheme = excluded.protection_scheme,
                    blob_hex = excluded.blob_hex
                ",
                params![
                    account_id.0.as_str(),
                    key_move.working.version as i64,
                    protected.scheme.as_db_value(),
                    protected.blob_hex,
                ],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM pending_key_move_devices WHERE account_id = ?1",
                params![account_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        for chain in &key_move.devices {
            if &chain.device.device_id == local_device {
                continue;
            }
            self.connection
                .execute(
                    "
                    INSERT OR REPLACE INTO pending_key_move_devices (account_id, device_id, version)
                    VALUES (?1, ?2, ?3)
                    ",
                    params![
                        account_id.0.as_str(),
                        chain.device.device_id.0.as_str(),
                        key_move.working.version as i64,
                    ],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Our own account: kept devices get their new certificates, the others lose their place.
    fn apply_own_account_move(
        &self,
        key_move: &omni_crypto::AccountKeyMoveV1,
        kept: &std::collections::HashSet<DeviceId>,
        now_unix_ms: i64,
    ) -> OmniResult<usize> {
        let account_id = key_move.account_id();
        let existing = self.list_account_devices(&account_id)?;
        for chain in &key_move.devices {
            let label = existing
                .iter()
                .find(|record| record.chain.device.device_id == chain.device.device_id)
                .map(|record| record.device_label.clone())
                .unwrap_or_else(|| "device".to_string());
            self.store_account_device(chain, &label, now_unix_ms)?;
            self.clear_superseded(&account_id, &chain.device.device_id)?;
        }
        let mut superseded = 0;
        for record in existing {
            let device_id = record.chain.device.device_id.clone();
            if kept.contains(&device_id) || record.revoked_at_unix_ms.is_some() {
                continue;
            }
            self.mark_superseded(&account_id, &device_id, key_move.working.version, now_unix_ms)?;
            self.connection
                .execute(
                    "UPDATE account_devices SET revoked_at_unix_ms = ?3 WHERE account_id = ?1 AND device_id = ?2",
                    params![account_id.0.as_str(), device_id.0.as_str(), now_unix_ms],
                )
                .map_err(map_sqlite_error)?;
            superseded += 1;
        }
        Ok(superseded)
    }

    /// A contact's account moved: its devices we know of that the move did not re-certify (and any
    /// that still hold an older certificate) are out.
    fn supersede_stale_remote_devices(
        &self,
        account_id: &AccountId,
        new_version: u64,
        kept: &std::collections::HashSet<DeviceId>,
        now_unix_ms: i64,
    ) -> OmniResult<usize> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT remote_device_id, device_certificate_chain_json
                FROM remote_prekey_bundles
                WHERE remote_account_id = ?1
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows: Vec<(String, Option<String>)> = statement
            .query_map(params![account_id.0.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(map_sqlite_error)?
            .collect::<Result<_, _>>()
            .map_err(map_sqlite_error)?;
        let mut superseded = 0;
        for (device_id, chain_json) in rows {
            let device_id = DeviceId(device_id);
            if kept.contains(&device_id) {
                continue;
            }
            let version = chain_json
                .and_then(|json| serde_json::from_str::<omni_crypto::DeviceCertificateChainV1>(&json).ok())
                .map(|chain| chain.working.version);
            if version.is_some_and(|version| version >= new_version) {
                continue;
            }
            self.mark_superseded(account_id, &device_id, new_version, now_unix_ms)?;
            superseded += 1;
        }
        Ok(superseded)
    }

    fn mark_superseded(
        &self,
        account_id: &AccountId,
        device_id: &DeviceId,
        new_version: u64,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT OR IGNORE INTO device_revocations (
                    account_id, device_id, revocation_json, working_key_version,
                    revoked_at_unix_ms, received_at_unix_ms, kind
                )
                VALUES (?1, ?2, '', ?3, ?4, ?4, 'superseded')
                ",
                params![
                    account_id.0.as_str(),
                    device_id.0.as_str(),
                    new_version as i64,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// A device that was superseded and shows up certified by the current key is back.
    pub(crate) fn clear_superseded(&self, account_id: &AccountId, device_id: &DeviceId) -> OmniResult<()> {
        let removed = self
            .connection
            .execute(
                "DELETE FROM device_revocations WHERE account_id = ?1 AND device_id = ?2 AND kind = 'superseded'",
                params![account_id.0.as_str(), device_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        if removed > 0 {
            self.connection
                .execute(
                    "UPDATE account_devices SET revoked_at_unix_ms = NULL WHERE account_id = ?1 AND device_id = ?2",
                    params![account_id.0.as_str(), device_id.0.as_str()],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Take the new working key the device that made the move handed us.
    fn adopt_working_key_seed(
        &self,
        account_id: &AccountId,
        key_move: &omni_crypto::AccountKeyMoveV1,
        seed: [u8; 32],
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let working = omni_crypto::WorkingKey::from_seed(key_move.working.version, seed);
        if working.public_key() != key_move.working.working_public_key {
            return Err(OmniError::InvalidInput(
                "the working key seed does not match the move's certificate".to_string(),
            ));
        }
        let locked = matches!(self.read_working_key(account_id, None)?, StoredWorkingKey::Locked);
        if locked && password_unlock_key.is_none() {
            // The profile has a password and it is not at hand: keep the seed sealed to this machine
            // until the next unlock re-wraps it under the password.
            let protected = omni_crypto::protect_bytes(&seed)?;
            self.connection
                .execute(
                    "
                    INSERT INTO pending_working_keys (
                        account_id, version, certificate_json, protection_scheme, blob_hex, updated_at_unix_ms
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(account_id) DO UPDATE SET
                        version = excluded.version,
                        certificate_json = excluded.certificate_json,
                        protection_scheme = excluded.protection_scheme,
                        blob_hex = excluded.blob_hex,
                        updated_at_unix_ms = excluded.updated_at_unix_ms
                    ",
                    params![
                        account_id.0.as_str(),
                        key_move.working.version as i64,
                        serde_json::to_string(&key_move.working).map_err(map_serde_error_to_storage)?,
                        protected.scheme.as_db_value(),
                        protected.blob_hex,
                        now_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;
            return Ok(());
        }
        self.store_working_key(account_id, &working, &key_move.working, password_unlock_key, now_unix_ms)
    }

    /// After a password unlock: a working key that arrived while the password was not at hand goes
    /// under the password now.
    pub(crate) fn adopt_pending_working_key(
        &self,
        password_unlock_key: &[u8; 32],
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let Some(account) = self.get_local_account()? else {
            return Ok(());
        };
        let account_id = account.descriptor.account_id;
        let row: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT certificate_json, protection_scheme, blob_hex FROM pending_working_keys WHERE account_id = ?1",
                params![account_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((certificate_json, scheme, blob_hex)) = row else {
            return Ok(());
        };
        let certificate: omni_crypto::WorkingKeyCertificateV1 =
            serde_json::from_str(&certificate_json).map_err(map_serde_error_to_storage)?;
        let protected = omni_crypto::ProtectedSecret {
            scheme: omni_crypto::SecretProtectionScheme::from_db_value(&scheme)?,
            blob_hex,
        };
        let Ok(seed_bytes) = omni_crypto::unprotect_bytes(&protected) else {
            return Ok(()); // sealed on another machine: it cannot be opened here
        };
        let seed: [u8; 32] = seed_bytes
            .try_into()
            .map_err(|_| OmniError::Storage("pending working key is malformed".to_string()))?;
        let working = omni_crypto::WorkingKey::from_seed(certificate.version, seed);
        self.store_working_key(&account_id, &working, &certificate, Some(password_unlock_key), now_unix_ms)?;
        self.connection
            .execute(
                "DELETE FROM pending_working_keys WHERE account_id = ?1",
                params![account_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}
