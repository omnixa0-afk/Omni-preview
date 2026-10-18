impl Storage {
    pub fn ensure_local_crypto_foundation(&self, now_unix_ms: i64) -> OmniResult<()> {
        let Some(account) = self.get_local_account()? else {
            return Ok(());
        };
        let Some(mut device) = self.get_local_device()? else {
            return Err(OmniError::Storage(
                "local account exists without a current local device".to_string(),
            ));
        };
        if self.local_device_was_copied(&account.descriptor.account_id, &device)? {
            device = self.become_new_device(&account.descriptor.account_id, &device, now_unix_ms)?;
        }

        let has_identity_key = self
            .get_local_identity_key(&account.descriptor.account_id)?
            .is_some();
        let has_device_key = self.get_local_device_key(&device.device_id)?.is_some();
        let has_signed_prekey = self.get_active_signed_prekey(&device.device_id)?.is_some();
        let has_one_time_prekey = self
            .get_available_one_time_prekey(&device.device_id)?
            .is_some();

        // Фундамент — это идентичность, ключ устройства и подписанный предключ. Одноразовые
        // предключи в него НЕ входят: это расходник, который тратится на каждом входящем bootstrap'е.
        //
        // Раньше пустой пул считался признаком повреждения, и профиль с целыми ключами переставал
        // подниматься: ниже начиналась ветка «частично инициализирован», пересобрать её было нельзя
        // (это сменило бы личность), и вход отваливался с ошибкой — приложение уходило на онбординг
        // при живой базе. Пул при этом выдавался ровно один раз при создании профиля (8 штук) и не
        // пополнялся никогда, то есть в это состояние попадал ЛЮБОЙ профиль после восьми входящих
        // знакомств. Поймано живьём 2026-08-14.
        if has_identity_key && has_device_key && has_signed_prekey {
            if !has_one_time_prekey {
                self.replenish_one_time_prekeys(&device.device_id, now_unix_ms)?;
            }
            return self.ensure_local_device_certificate(
                &account.descriptor.account_id,
                &device,
                None,
                now_unix_ms,
            );
        }
        if has_identity_key || has_device_key || has_signed_prekey || has_one_time_prekey {
            if self.can_rebuild_local_crypto_foundation(
                &account.descriptor.account_id,
                &device.device_id,
            )? {
                self.reset_local_crypto_foundation(
                    &account.descriptor.account_id,
                    &device.device_id,
                )?;
            } else {
                return Err(OmniError::Storage(
                    "local crypto foundation is only partially initialized".to_string(),
                ));
            }
        }

        let hierarchy = generate_local_key_hierarchy(
            &account.descriptor.account_id,
            &device.device_id,
            now_unix_ms,
            DEFAULT_ONE_TIME_PREKEY_COUNT,
        )?;
        self.persist_local_crypto_hierarchy(&hierarchy)?;
        self.ensure_local_device_certificate(&account.descriptor.account_id, &device, None, now_unix_ms)
    }

    /// Долить пул одноразовых предключей. Личность не трогает — только расходник.
    ///
    /// Номера назначает ХРАНИЛИЩЕ, а не генератор: `next_prekey_id` — счётчик, живущий в процессе и
    /// начинающийся с единицы при каждом запуске. Пока пул выдавался ровно один раз при создании
    /// профиля, это было незаметно; при доливе тот же счётчик выдавал номера, которые в таблице уже
    /// лежат (израсходованные строки из неё не удаляются), и вставка падала на
    /// `UNIQUE constraint failed: one_time_prekeys.prekey_id`. Причём падала она внутри
    /// инициализации — то есть профиль после этого вообще не открывался.
    pub fn replenish_one_time_prekeys(
        &self,
        device_id: &DeviceId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let highest: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(prekey_id), 0) FROM one_time_prekeys WHERE device_id = ?1",
                params![device_id.0.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let mut prekeys = omni_crypto::generate_one_time_prekeys(
            device_id,
            now_unix_ms,
            DEFAULT_ONE_TIME_PREKEY_COUNT,
        )?;
        for (offset, prekey) in prekeys.iter_mut().enumerate() {
            prekey.prekey_id = (highest as u32).saturating_add(offset as u32 + 1);
        }
        self.persist_additional_one_time_prekeys(&prekeys)
    }

    pub fn get_local_identity_key(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Option<LocalIdentityKeyPair>> {
        self.connection
            .query_row(
                "
                SELECT
                    account_id,
                    public_key,
                    signing_public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    signing_private_key_protection_scheme,
                    signing_private_key_blob,
                    suite_version,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM identity_keys
                WHERE account_id = ?1
                ",
                params![account_id.0],
                |row| {
                    Ok(LocalIdentityKeyPair {
                        account_id: AccountId(row.get(0)?),
                        public_key: identity_public_key_from_wire(&row.get::<_, String>(1)?)
                            .map_err(to_sqlite_user_error)?,
                        signing_public_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(2)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_signing_private_key: protected_secret_from_db(
                            &row.get::<_, String>(5)?,
                            &row.get::<_, String>(6)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(7)? as u16,
                        created_at_unix_ms: row.get(8)?,
                        updated_at_unix_ms: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_local_device_key(
        &self,
        device_id: &DeviceId,
    ) -> OmniResult<Option<LocalDeviceKeyPair>> {
        self.connection
            .query_row(
                "
                SELECT
                    device_id,
                    account_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    suite_version,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM device_keys
                WHERE device_id = ?1
                ",
                params![device_id.0],
                |row| {
                    Ok(LocalDeviceKeyPair {
                        device_id: DeviceId(row.get(0)?),
                        account_id: AccountId(row.get(1)?),
                        public_key: device_public_key_from_wire(&row.get::<_, String>(2)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(5)? as u16,
                        created_at_unix_ms: row.get(6)?,
                        updated_at_unix_ms: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_active_signed_prekey(&self, device_id: &DeviceId) -> OmniResult<Option<SignedPrekey>> {
        self.connection
            .query_row(
                "
                SELECT
                    prekey_id,
                    device_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    signature_r,
                    signature_s,
                    suite_version,
                    created_at_unix_ms,
                    expires_at_unix_ms,
                    updated_at_unix_ms
                FROM signed_prekeys
                WHERE device_id = ?1
                  AND state = 'active'
                ORDER BY created_at_unix_ms DESC, prekey_id DESC
                LIMIT 1
                ",
                params![device_id.0],
                |row| {
                    Ok(SignedPrekey {
                        prekey_id: row.get::<_, i64>(0)? as u32,
                        device_id: DeviceId(row.get(1)?),
                        public_key: prekey_public_key_from_wire(&row.get::<_, String>(2)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        signature: signature_from_wire(
                            &row.get::<_, String>(5)?,
                            &row.get::<_, String>(6)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(7)? as u16,
                        created_at_unix_ms: row.get(8)?,
                        expires_at_unix_ms: row.get(9)?,
                        updated_at_unix_ms: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_available_one_time_prekey(
        &self,
        device_id: &DeviceId,
    ) -> OmniResult<Option<OneTimePrekey>> {
        self.connection
            .query_row(
                "
                SELECT
                    prekey_id,
                    device_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    suite_version,
                    created_at_unix_ms,
                    consumed_at_unix_ms,
                    updated_at_unix_ms
                FROM one_time_prekeys
                WHERE device_id = ?1
                  AND state = 'available'
                ORDER BY created_at_unix_ms ASC, prekey_id ASC
                LIMIT 1
                ",
                params![device_id.0],
                |row| {
                    Ok(OneTimePrekey {
                        prekey_id: row.get::<_, i64>(0)? as u32,
                        device_id: DeviceId(row.get(1)?),
                        public_key: prekey_public_key_from_wire(&row.get::<_, String>(2)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(5)? as u16,
                        created_at_unix_ms: row.get(6)?,
                        consumed_at_unix_ms: row.get(7)?,
                        updated_at_unix_ms: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_available_one_time_prekey_by_id(
        &self,
        device_id: &DeviceId,
        prekey_id: u32,
    ) -> OmniResult<Option<OneTimePrekey>> {
        self.connection
            .query_row(
                "
                SELECT
                    prekey_id,
                    device_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    suite_version,
                    created_at_unix_ms,
                    consumed_at_unix_ms,
                    updated_at_unix_ms
                FROM one_time_prekeys
                WHERE device_id = ?1
                  AND prekey_id = ?2
                  AND state = 'available'
                LIMIT 1
                ",
                params![device_id.0, prekey_id as i64],
                |row| {
                    Ok(OneTimePrekey {
                        prekey_id: row.get::<_, i64>(0)? as u32,
                        device_id: DeviceId(row.get(1)?),
                        public_key: prekey_public_key_from_wire(&row.get::<_, String>(2)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(5)? as u16,
                        created_at_unix_ms: row.get(6)?,
                        consumed_at_unix_ms: row.get(7)?,
                        updated_at_unix_ms: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    /// The one-time prekey an inbound bootstrap names: an available one, or one already taken by
    /// another peer within [`ONE_TIME_PREKEY_SHARED_GRACE_MS`]. Without a server that hands every
    /// asker its own prekey, one bundle reaches many — our other devices, a contact's devices — and
    /// they all name its prekey; the first to bootstrap took it, and every later one failed with
    /// «expected one-time prekey is missing» and kept resending the same bootstrap forever (a copy
    /// of a profile on the stand: the PC took the prekey 3 s before the laptop, the laptop never
    /// reached the copy, its revocation neither). The private half of a consumed prekey stays in
    /// the table regardless, so this keeps nothing longer than before; a replayed bootstrap is
    /// refused before this, by its `created_at` (`existing_session_should_absorb_incoming_bootstrap`).
    pub fn get_one_time_prekey_for_inbound_bootstrap(
        &self,
        device_id: &DeviceId,
        prekey_id: u32,
        now_unix_ms: i64,
    ) -> OmniResult<Option<OneTimePrekey>> {
        if let Some(available) = self.get_available_one_time_prekey_by_id(device_id, prekey_id)? {
            return Ok(Some(available));
        }
        self.connection
            .query_row(
                "
                SELECT
                    prekey_id,
                    device_id,
                    public_key,
                    private_key_protection_scheme,
                    private_key_blob,
                    suite_version,
                    created_at_unix_ms,
                    consumed_at_unix_ms,
                    updated_at_unix_ms
                FROM one_time_prekeys
                WHERE device_id = ?1
                  AND prekey_id = ?2
                  AND state = 'consumed'
                  AND consumed_at_unix_ms >= ?3
                LIMIT 1
                ",
                params![device_id.0, prekey_id as i64, now_unix_ms.saturating_sub(ONE_TIME_PREKEY_SHARED_GRACE_MS)],
                |row| {
                    Ok(OneTimePrekey {
                        prekey_id: row.get::<_, i64>(0)? as u32,
                        device_id: DeviceId(row.get(1)?),
                        public_key: prekey_public_key_from_wire(&row.get::<_, String>(2)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_private_key: protected_secret_from_db(
                            &row.get::<_, String>(3)?,
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        suite_version: row.get::<_, i64>(5)? as u16,
                        created_at_unix_ms: row.get(6)?,
                        consumed_at_unix_ms: row.get(7)?,
                        updated_at_unix_ms: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn mark_one_time_prekey_consumed(
        &self,
        device_id: &DeviceId,
        prekey_id: u32,
        consumed_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                UPDATE one_time_prekeys
                SET state = 'consumed',
                    consumed_at_unix_ms = ?2,
                    updated_at_unix_ms = ?2
                WHERE device_id = ?1
                  AND prekey_id = ?3
                  AND state = 'available'
                ",
                params![device_id.0.as_str(), consumed_at_unix_ms, prekey_id as i64],
            )
            .map_err(map_sqlite_error)?;

        // Доливаем СРАЗУ, как только пул опустел, а не только при следующем входе: иначе между
        // опустошением и перезапуском все входящие bootstrap'ы отваливаются с «expected one-time
        // prekey is missing», то есть с пиром, который к нам стучится, знакомство не состоится.
        if self.get_available_one_time_prekey(device_id)?.is_none() {
            self.replenish_one_time_prekeys(device_id, consumed_at_unix_ms)?;
        }

        Ok(())
    }

    fn can_rebuild_local_crypto_foundation(
        &self,
        account_id: &AccountId,
        device_id: &DeviceId,
    ) -> OmniResult<bool> {
        let session_count: i64 = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM sessions
                WHERE local_account_id = ?1
                   OR local_device_id = ?2
                ",
                params![account_id.0.as_str(), device_id.0.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(session_count == 0)
    }

    fn reset_local_crypto_foundation(
        &self,
        account_id: &AccountId,
        device_id: &DeviceId,
    ) -> OmniResult<()> {
        self.with_immediate_transaction("reset_local_crypto_foundation", |storage| {
            storage
                .connection
                .execute(
                    "DELETE FROM one_time_prekeys WHERE device_id = ?1",
                    params![device_id.0.as_str()],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "DELETE FROM signed_prekeys WHERE device_id = ?1",
                    params![device_id.0.as_str()],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "DELETE FROM device_keys WHERE device_id = ?1",
                    params![device_id.0.as_str()],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "DELETE FROM identity_keys WHERE account_id = ?1",
                    params![account_id.0.as_str()],
                )
                .map_err(map_sqlite_error)?;
            Ok(())
        })
    }

    pub fn build_local_prekey_bundle(&self, generated_at_unix_ms: i64) -> OmniResult<PrekeyBundle> {
        let account = self.get_local_account()?.ok_or_else(|| {
            OmniError::InvalidInput("local account is not initialized".to_string())
        })?;
        let device = self.get_local_device()?.ok_or_else(|| {
            OmniError::InvalidInput("local device is not initialized".to_string())
        })?;
        let identity_key = self
            .get_local_identity_key(&account.descriptor.account_id)?
            .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
        let device_key = self
            .get_local_device_key(&device.device_id)?
            .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;
        let signed_prekey = self
            .get_active_signed_prekey(&device.device_id)?
            .ok_or_else(|| OmniError::Storage("active signed prekey is missing".to_string()))?;
        let one_time_prekey = self.get_available_one_time_prekey(&device.device_id)?;

        let mut bundle = build_prekey_bundle(
            &identity_key,
            &device_key,
            &signed_prekey,
            one_time_prekey.as_ref(),
            generated_at_unix_ms,
        )?;
        bundle.device_certificate_chain = self.get_local_device_certificate_chain()?;
        Ok(bundle)
    }
}
