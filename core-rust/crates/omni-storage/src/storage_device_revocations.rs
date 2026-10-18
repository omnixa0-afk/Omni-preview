// Revoked devices (docs/decisions/multi-device-equal-nodes.md, «Устройства», stage 3).
//
// A revocation is a statement signed by the account's working key, so any device of the account can
// issue one and anybody can verify it. This layer stores them, keeps them sticky (a device that is
// revoked stays revoked, even if its certificate shows up again later) and answers "is this device
// revoked?" for the code that picks where to send. Telling the revoked device, and what it does when
// told, are the orchestration's business.

/// What applying a revocation did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRevocationOutcome {
    /// `false` when this device was already known revoked: nothing to pass on.
    pub newly_applied: bool,
    /// The revoked device is THIS one, of our own account: it has been taken back.
    pub targets_local_device: bool,
}

impl Storage {
    /// Verify `revocation` and remember it. Marks the device revoked wherever it is known.
    ///
    /// Rejected when it is not signed by a working key the account's master vouches for, or when
    /// that key is older than one we already know for the account (a working-key move makes
    /// everything the old key signed void). For a contact's account we may know no working key at
    /// all; then any validly signed revocation counts.
    pub fn apply_device_revocation(
        &self,
        revocation: &omni_crypto::DeviceRevocationV1,
        now_unix_ms: i64,
    ) -> OmniResult<DeviceRevocationOutcome> {
        omni_crypto::verify_device_revocation(revocation)?;
        let account_id = &revocation.account_id;
        if let Some(known_version) = self.highest_known_working_key_version(account_id)? {
            if revocation.working.version < known_version {
                return Err(OmniError::InvalidInput(
                    "the revocation is signed by an older working key than the account's current one"
                        .to_string(),
                ));
            }
        }
        let targets_local_device = match (self.get_local_account()?, self.get_local_device()?) {
            (Some(account), Some(device)) => {
                &account.descriptor.account_id == account_id && device.device_id == revocation.device_id
            }
            _ => false,
        };
        let revocation_json =
            serde_json::to_string(revocation).map_err(map_serde_error_to_storage)?;
        let inserted = self
            .connection
            .execute(
                "
                INSERT OR IGNORE INTO device_revocations (
                    account_id,
                    device_id,
                    revocation_json,
                    working_key_version,
                    revoked_at_unix_ms,
                    received_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    account_id.0.as_str(),
                    revocation.device_id.0.as_str(),
                    revocation_json,
                    revocation.working.version as i64,
                    revocation.revoked_at_unix_ms,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE account_devices
                SET revoked_at_unix_ms = COALESCE(revoked_at_unix_ms, ?3)
                WHERE account_id = ?1 AND device_id = ?2
                ",
                params![
                    account_id.0.as_str(),
                    revocation.device_id.0.as_str(),
                    revocation.revoked_at_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(DeviceRevocationOutcome {
            newly_applied: inserted > 0,
            targets_local_device,
        })
    }

    pub fn is_device_revoked(&self, account_id: &AccountId, device_id: &DeviceId) -> OmniResult<bool> {
        let found: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM device_revocations WHERE account_id = ?1 AND device_id = ?2",
                params![account_id.0.as_str(), device_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(found.is_some())
    }

    /// This device was taken back by its account: erase the profile it holds. Irreversible, so it
    /// is guarded here and not left to the caller: it runs only when a verified revocation of THIS
    /// device is on record, and does nothing otherwise. Returns whether it erased.
    pub fn wipe_profile_if_taken_back(&self) -> OmniResult<bool> {
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Ok(false);
        };
        if !self.is_device_revoked(&account.descriptor.account_id, &device.device_id)? {
            return Ok(false);
        }
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Ok(false);
        };
        self.delete_local_profile(&profile_id)?;
        Ok(true)
    }

    /// The owner has seen the devices of the account: none of them is "new" any more.
    pub fn acknowledge_account_devices(&self, now_unix_ms: i64) -> OmniResult<()> {
        let Some(account) = self.get_local_account()? else {
            return Ok(());
        };
        self.connection
            .execute(
                "
                UPDATE account_devices
                SET acknowledged_at_unix_ms = ?2
                WHERE account_id = ?1 AND acknowledged_at_unix_ms IS NULL
                ",
                params![account.descriptor.account_id.0.as_str(), now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Take revoked devices of our account out of the Settings list: one (`device_id`) or all of
    /// them (`None`). A device that is not revoked is never hidden — the list is where the owner
    /// sees who is in the account. Returns how many rows were hidden.
    pub fn hide_revoked_account_devices(&self, device_id: Option<&DeviceId>, now_unix_ms: i64) -> OmniResult<usize> {
        let Some(account) = self.get_local_account()? else {
            return Ok(0);
        };
        self.connection
            .execute(
                "
                UPDATE account_devices
                SET hidden_at_unix_ms = ?3
                WHERE account_id = ?1
                  AND revoked_at_unix_ms IS NOT NULL
                  AND hidden_at_unix_ms IS NULL
                  AND (?2 IS NULL OR device_id = ?2)
                ",
                params![
                    account.descriptor.account_id.0.as_str(),
                    device_id.map(|device_id| device_id.0.as_str()),
                    now_unix_ms
                ],
            )
            .map_err(map_sqlite_error)
    }

    /// Our devices that were taken back but whose dialog we still know — where the revocation has
    /// to be delivered so the device learns it.
    pub fn list_revoked_own_device_dialogs(&self) -> OmniResult<Vec<(DeviceId, String)>> {
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT device_id, dialog_key
                FROM account_devices
                WHERE account_id = ?1
                  AND device_id <> ?2
                  AND revoked_at_unix_ms IS NOT NULL
                  AND dialog_key IS NOT NULL
                ORDER BY first_seen_at_unix_ms ASC, device_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![account.descriptor.account_id.0.as_str(), device.device_id.0.as_str()],
                |row| Ok((DeviceId(row.get(0)?), row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Every revocation we hold for `account_id` — what is passed on to a device that has not heard.
    pub fn list_device_revocations(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Vec<omni_crypto::DeviceRevocationV1>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT revocation_json FROM device_revocations
                WHERE account_id = ?1 AND kind = 'revoked'
                ORDER BY revoked_at_unix_ms ASC, device_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![account_id.0.as_str()], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut revocations = Vec::new();
        for row in rows {
            let json = row.map_err(map_sqlite_error)?;
            revocations.push(serde_json::from_str(&json).map_err(map_serde_error_to_storage)?);
        }
        Ok(revocations)
    }

    /// The revocations worth passing on: those signed by the account's current working key or a newer
    /// one. What an older key signed was voided by the key move (and is refused by whoever applies it
    /// — passing it on only filled their traces with refusals).
    pub fn list_device_revocations_under_current_key(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Vec<omni_crypto::DeviceRevocationV1>> {
        let current = self.highest_known_working_key_version(account_id)?;
        Ok(self
            .list_device_revocations(account_id)?
            .into_iter()
            .filter(|revocation| current.is_none_or(|current| revocation.working.version >= current))
            .collect())
    }

    /// Revoke another device of our own account with the working key. `password_unlock_key` is the
    /// profile password's unlock key when the working key is kept under one. This device cannot
    /// revoke itself: leaving the account is signing out.
    pub fn issue_local_device_revocation(
        &self,
        device_id: &DeviceId,
        password_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<omni_crypto::DeviceRevocationV1> {
        let (Some(account), Some(local_device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Err(OmniError::InvalidInput("no local account".to_string()));
        };
        if &local_device.device_id == device_id {
            return Err(OmniError::InvalidInput(
                "this device cannot revoke itself: sign out instead".to_string(),
            ));
        }
        let account_id = &account.descriptor.account_id;
        let known: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM account_devices WHERE account_id = ?1 AND device_id = ?2",
                params![account_id.0.as_str(), device_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if known.is_none() {
            return Err(OmniError::InvalidInput(
                "the account has no such device".to_string(),
            ));
        }
        match self.read_working_key(account_id, password_unlock_key)? {
            StoredWorkingKey::Ready(working, certificate) => {
                omni_crypto::issue_device_revocation(&certificate, &working, device_id, now_unix_ms)
            }
            StoredWorkingKey::Locked => Err(OmniError::InvalidInput(
                "the working key is under the profile password: unlock it first".to_string(),
            )),
            StoredWorkingKey::Absent => Err(OmniError::InvalidInput(
                "the account has no working key".to_string(),
            )),
        }
    }

    /// [`Self::issue_local_device_revocation`] for a folder whose working key is under the profile
    /// password: the password opens it for this one signature.
    pub fn issue_local_device_revocation_with_password(
        &self,
        device_id: &DeviceId,
        password: &str,
        now_unix_ms: i64,
    ) -> OmniResult<omni_crypto::DeviceRevocationV1> {
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Err(OmniError::InvalidInput("no active profile".to_string()));
        };
        let unlock_key = self.verify_profile_password(&profile_id, password)?;
        self.issue_local_device_revocation(device_id, Some(&unlock_key), now_unix_ms)
    }

    /// The newest working key version this profile knows for `account_id`: from its own key chain
    /// when it is our account, and from the device certificates stored for it.
    fn highest_known_working_key_version(&self, account_id: &AccountId) -> OmniResult<Option<u64>> {
        let mut highest: Option<u64> = self.current_working_key_version(account_id)?;
        for record in self.list_account_devices(account_id)? {
            highest = Some(highest.map_or(record.chain.working.version, |current| {
                current.max(record.chain.working.version)
            }));
        }
        Ok(highest)
    }
}
