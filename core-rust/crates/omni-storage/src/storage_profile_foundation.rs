const ACTIVE_PROFILE_ID_META_KEY: &str = "active_profile_id";

struct ProfileRegistryRow {
    profile_id: String,
    account_id: String,
    display_name: String,
    public_profile_updated_at_unix_ms: i64,
    public_avatar_hash_hex: Option<String>,
    public_avatar_mime_type: Option<String>,
    account_representation_version: i64,
    profile_state: String,
    restored_from_recovery: bool,
    created_at_unix_ms: i64,
    updated_at_unix_ms: i64,
}

struct ProfileSecretsRow {
    account_id: String,
    password_enabled: bool,
    password_salt_hex: Option<String>,
    password_verifier_hex: Option<String>,
    profile_master_key_protection_scheme: Option<String>,
    profile_master_key_blob_hex: Option<String>,
    encrypted_profile_secrets_json: Option<String>,
}

impl Storage {
    pub fn list_profiles(&self) -> OmniResult<SavedProfileList> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    profile_id,
                    account_id,
                    display_name,
                    public_profile_updated_at_unix_ms,
                    public_avatar_hash_hex,
                    public_avatar_mime_type,
                    account_representation_version,
                    profile_state,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM profile_registry
                ORDER BY created_at_unix_ms ASC, profile_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(ProfileRegistryRow {
                    profile_id: row.get(0)?,
                    account_id: row.get(1)?,
                    display_name: row.get(2)?,
                    public_profile_updated_at_unix_ms: row.get(3)?,
                    public_avatar_hash_hex: row.get(4)?,
                    public_avatar_mime_type: row.get(5)?,
                    account_representation_version: row.get(6)?,
                    profile_state: row.get(7)?,
                    restored_from_recovery: row.get::<_, i64>(8)? != 0,
                    created_at_unix_ms: row.get(9)?,
                    updated_at_unix_ms: row.get(10)?,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_sqlite_error)?;

        Ok(build_saved_profile_list(
            rows.into_iter()
                .map(profile_summary_from_registry_row)
                .collect::<OmniResult<Vec<_>>>()?,
        ))
    }

    pub(crate) fn get_profile_registry_row(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileRegistryRow>> {
        self.connection
            .query_row(
                "
                SELECT
                    profile_id,
                    account_id,
                    display_name,
                    public_profile_updated_at_unix_ms,
                    public_avatar_hash_hex,
                    public_avatar_mime_type,
                    account_representation_version,
                    profile_state,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM profile_registry
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str()],
                |row| {
                    Ok(ProfileRegistryRow {
                        profile_id: row.get(0)?,
                        account_id: row.get(1)?,
                        display_name: row.get(2)?,
                        public_profile_updated_at_unix_ms: row.get(3)?,
                        public_avatar_hash_hex: row.get(4)?,
                        public_avatar_mime_type: row.get(5)?,
                        account_representation_version: row.get(6)?,
                        profile_state: row.get(7)?,
                        restored_from_recovery: row.get::<_, i64>(8)? != 0,
                        created_at_unix_ms: row.get(9)?,
                        updated_at_unix_ms: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub(crate) fn get_profile_secrets_row(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileSecretsRow>> {
        self.connection
            .query_row(
                "
                SELECT
                    account_id,
                    password_enabled,
                    password_salt_hex,
                    password_verifier_hex,
                    profile_master_key_protection_scheme,
                    profile_master_key_blob_hex,
                    encrypted_profile_secrets_json
                FROM profile_secrets
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str()],
                |row| {
                    Ok(ProfileSecretsRow {
                        account_id: row.get(0)?,
                        password_enabled: row.get::<_, i64>(1)? != 0,
                        password_salt_hex: row.get(2)?,
                        password_verifier_hex: row.get(3)?,
                        profile_master_key_protection_scheme: row.get(4)?,
                        profile_master_key_blob_hex: row.get(5)?,
                        encrypted_profile_secrets_json: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_local_device_by_id(
        &self,
        device_id: &DeviceId,
    ) -> OmniResult<Option<LocalDeviceSnapshot>> {
        self.connection
            .query_row(
                "
                SELECT
                    device_id,
                    account_id,
                    device_label,
                    is_current,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM local_devices
                WHERE device_id = ?1
                ",
                params![device_id.0.as_str()],
                |row| {
                    Ok(LocalDeviceSnapshot {
                        device_id: DeviceId(row.get(0)?),
                        account_id: AccountId(row.get(1)?),
                        device_label: row.get(2)?,
                        is_current: row.get::<_, i64>(3)? != 0,
                        restored_from_recovery: row.get::<_, i64>(4)? != 0,
                        created_at_unix_ms: row.get(5)?,
                        updated_at_unix_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_local_device_by_profile_id(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalDeviceSnapshot>> {
        let Some(device_id) = self.get_profile_current_device_id(profile_id)? else {
            return Ok(None);
        };
        self.get_local_device_by_id(&device_id)
    }

    pub fn get_profile_by_id(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalProfileSummary>> {
        self.get_profile_registry_row(profile_id)?
            .map(profile_summary_from_registry_row)
            .transpose()
    }

    pub fn profile_exists(&self, profile_id: &LocalProfileId) -> OmniResult<bool> {
        Ok(self.get_profile_registry_row(profile_id)?.is_some())
    }

    pub fn get_active_profile_id(&self) -> OmniResult<Option<LocalProfileId>> {
        Ok(self
            .get_app_meta(ACTIVE_PROFILE_ID_META_KEY)?
            .map(LocalProfileId))
    }

    pub(crate) fn require_active_profile_id(&self) -> OmniResult<LocalProfileId> {
        self.get_active_profile_id()?.ok_or_else(|| {
            OmniError::InvalidInput("no active profile is selected".to_string())
        })
    }

    pub(crate) fn resolve_transition_profile_id(&self) -> OmniResult<LocalProfileId> {
        self.require_active_profile_id()
    }

    pub fn set_active_profile_id(&self, profile_id: Option<&LocalProfileId>) -> OmniResult<()> {
        match profile_id {
            Some(profile_id) => {
                if !self.profile_exists(profile_id)? {
                    return Err(OmniError::InvalidInput(
                        "profile was not found".to_string(),
                    ));
                }
                self.upsert_app_meta(ACTIVE_PROFILE_ID_META_KEY, &profile_id.0)
            }
            None => self
                .connection
                .execute(
                    "DELETE FROM app_meta WHERE key = ?1",
                    params![ACTIVE_PROFILE_ID_META_KEY],
                )
                .map_err(map_sqlite_error)
                .map(|_| ()),
        }
    }

    pub fn get_profile_current_device_id(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DeviceId>> {
        if let Some(device_id) = self
            .connection
            .query_row(
                "
                SELECT device_id
                FROM profile_current_device
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?
        {
            return Ok(Some(DeviceId(device_id)));
        }
        Ok(None)
    }

    pub fn set_profile_current_device_id(
        &self,
        profile_id: &LocalProfileId,
        device_id: &DeviceId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let Some(profile) = self.get_local_profile_summary(profile_id)? else {
            return Err(OmniError::InvalidInput("profile was not found".to_string()));
        };
        let Some(device) = self.get_local_device_by_id(device_id)? else {
            return Err(OmniError::InvalidInput("device was not found".to_string()));
        };
        if device.account_id != profile.account_id {
            return Err(OmniError::InvalidInput(
                "device does not belong to this profile".to_string(),
            ));
        }
        self.connection
            .execute(
                "
                INSERT INTO profile_current_device (
                    profile_id,
                    device_id,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(profile_id) DO UPDATE SET
                    device_id = excluded.device_id,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![profile_id.0.as_str(), device_id.0.as_str(), now_unix_ms, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn upsert_profile_registry_from_account(
        &self,
        account: &LocalAccountSnapshot,
        now_unix_ms: i64,
    ) -> OmniResult<LocalProfileId> {
        let Some(local_profile_summary) = account.local_profile_summary.as_ref() else {
            return Err(OmniError::InvalidInput(
                "local profile summary is missing for profile registry write".to_string(),
            ));
        };
        let profile_id = local_profile_summary.profile_id.clone();
        let profile_state = local_profile_summary.state;

        self.connection
            .execute(
                "
                INSERT INTO profile_registry (
                    profile_id,
                    account_id,
                    display_name,
                    public_profile_updated_at_unix_ms,
                    public_avatar_hash_hex,
                    public_avatar_mime_type,
                    account_representation_version,
                    profile_state,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(profile_id) DO UPDATE SET
                    account_id = excluded.account_id,
                    display_name = excluded.display_name,
                    public_profile_updated_at_unix_ms = excluded.public_profile_updated_at_unix_ms,
                    public_avatar_hash_hex = excluded.public_avatar_hash_hex,
                    public_avatar_mime_type = excluded.public_avatar_mime_type,
                    account_representation_version = excluded.account_representation_version,
                    profile_state = excluded.profile_state,
                    restored_from_recovery = excluded.restored_from_recovery,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    account.descriptor.account_id.0.as_str(),
                    account.descriptor.display_name.as_str(),
                    account.public_profile_updated_at_unix_ms.unwrap_or(account.updated_at_unix_ms),
                    account.public_avatar_hash_hex.as_deref(),
                    account.public_avatar_mime_type.as_deref(),
                    i64::from(account.account_representation_version),
                    profile_state_to_db(profile_state),
                    bool_to_sqlite_integer(account.restored_from_recovery),
                    account.created_at_unix_ms,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(profile_id)
    }

    pub fn upsert_profile_secrets_from_account(
        &self,
        account: &LocalAccountSnapshot,
        representation_v2: Option<&omni_identity::AccountRepresentationV2>,
        now_unix_ms: i64,
    ) -> OmniResult<LocalProfileId> {
        let Some(local_profile_summary) = account.local_profile_summary.as_ref() else {
            return Err(OmniError::InvalidInput(
                "local profile summary is missing for profile secrets write".to_string(),
            ));
        };
        let profile_id = local_profile_summary.profile_id.clone();
        let profile_secrets_storage = representation_v2
            .map(build_profile_secrets_storage)
            .transpose()?;
        let (recovery_version, recovery_code) = (
            // 0: this device holds no phrase (a folder copied to another computer).
            account
                .recovery_phrase_v2
                .as_ref()
                .map_or(0, |phrase| i64::from(phrase.version)),
            "v2-managed-in-encrypted-secrets".to_string(),
        );

        self.connection
            .execute(
                "
                INSERT INTO profile_secrets (
                    profile_id,
                    account_id,
                    recovery_version,
                    recovery_code,
                    password_enabled,
                    password_salt_hex,
                    password_verifier_hex,
                    profile_master_key_protection_scheme,
                    profile_master_key_blob_hex,
                    encrypted_profile_secrets_json,
                    restored_from_recovery,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(profile_id) DO UPDATE SET
                    account_id = excluded.account_id,
                    recovery_version = excluded.recovery_version,
                    recovery_code = excluded.recovery_code,
                    password_enabled = excluded.password_enabled,
                    password_salt_hex = excluded.password_salt_hex,
                    password_verifier_hex = excluded.password_verifier_hex,
                    profile_master_key_protection_scheme = excluded.profile_master_key_protection_scheme,
                    profile_master_key_blob_hex = excluded.profile_master_key_blob_hex,
                    encrypted_profile_secrets_json = excluded.encrypted_profile_secrets_json,
                    restored_from_recovery = excluded.restored_from_recovery,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    account.descriptor.account_id.0.as_str(),
                    recovery_version,
                    recovery_code,
                    0,
                    Option::<String>::None,
                    Option::<String>::None,
                    profile_secrets_storage
                        .as_ref()
                        .map(|value| value.profile_master_key.scheme.as_db_value()),
                    profile_secrets_storage
                        .as_ref()
                        .map(|value| value.profile_master_key.blob_hex.clone()),
                    profile_secrets_storage
                        .as_ref()
                        .map(|value| value.encrypted_profile_secrets_json.clone()),
                    bool_to_sqlite_integer(account.restored_from_recovery),
                    account.created_at_unix_ms,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(profile_id)
    }

    pub fn update_profile_state(
        &self,
        profile_id: &LocalProfileId,
        profile_state: omni_identity::LocalProfileState,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_state = profile_state_to_db(profile_state);
        self.connection
            .execute(
                "
                UPDATE profile_registry
                SET profile_state = ?2,
                    updated_at_unix_ms = ?3
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str(), profile_state, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE local_account
                SET profile_state = ?2,
                    updated_at_unix_ms = ?3
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str(), profile_state, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_local_account_by_profile_id(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalAccountSnapshot>> {
        let Some(registry_row) = self.get_profile_registry_row(profile_id)? else {
            return Ok(None);
        };
        let Some(secrets_row) = self.get_profile_secrets_row(profile_id)? else {
            return Ok(None);
        };
        let Some(current_device_id) = self.get_profile_current_device_id(profile_id)? else {
            return Ok(None);
        };
        let Some(device) = self.get_local_device_by_id(&current_device_id)? else {
            return Ok(None);
        };
        if secrets_row.encrypted_profile_secrets_json.is_none() {
            return Ok(None);
        }
        let account_id = AccountId(registry_row.account_id.clone());
        // The root secret and the phrase are sealed to the machine that made them (DPAPI). A folder
        // copied to another computer cannot open them — and does not need to: it acts for the
        // account through the working key, and the account's public identity comes from the
        // master PUBLIC key every device holds (docs/decisions/multi-device-equal-nodes.md).
        let profile_secrets = load_profile_secrets_from_profile_secrets_row(&secrets_row)
            .ok()
            .flatten();
        let derived_public_identity = match (&profile_secrets, self.get_account_keys(&account_id)?) {
            (_, Some(keys)) => omni_identity::account_public_identity_from_master_public_key(
                &keys.master_public_key.0,
                &registry_row.display_name,
            )?,
            (Some(secrets), None) => omni_identity::derive_account_public_identity(
                &secrets.root_secret,
                &registry_row.display_name,
            )?,
            (None, None) => {
                return Err(OmniError::Storage(
                    "the profile's secrets do not open on this computer and it has no account key chain"
                        .to_string(),
                ))
            }
        };
        if derived_public_identity.account_id != account_id {
            return Err(OmniError::Storage(
                "stored profile secrets do not match persisted account identity".to_string(),
            ));
        }
        let has_root_secret = profile_secrets.is_some();

        Ok(Some(LocalAccountSnapshot {
            descriptor: PublicAccountDescriptor {
                account_id: AccountId(registry_row.account_id.clone()),
                display_name: registry_row.display_name.clone(),
            },
            recovery_phrase_v2: profile_secrets.map(|secrets| secrets.recovery_phrase_v2),
            current_device_id: device.device_id,
            restored_from_recovery: registry_row.restored_from_recovery,
            account_representation_version: registry_row.account_representation_version as u16,
            public_summary: Some(PublicAccountSummary {
                account_id: derived_public_identity.account_id.clone(),
                display_name: derived_public_identity.display_name.clone(),
                verification_payload: derived_public_identity.verification_payload,
                safety_number_material: derived_public_identity.safety_number_material,
            }),
            local_profile_summary: Some(LocalAccountProfileSummary {
                profile_id: LocalProfileId(registry_row.profile_id.clone()),
                account_id: AccountId(registry_row.account_id.clone()),
                display_name: registry_row.display_name,
                account_representation_version: registry_row.account_representation_version as u16,
                state: parse_local_profile_state(&registry_row.profile_state)?,
                has_root_secret,
                has_recovery_phrase_v2: has_root_secret,
            }),
            public_avatar_hash_hex: registry_row.public_avatar_hash_hex.clone(),
            public_avatar_mime_type: registry_row.public_avatar_mime_type.clone(),
            public_avatar_image_path: self.resolve_local_public_avatar_image_path(
                profile_id,
                registry_row.public_avatar_hash_hex.as_deref(),
            ),
            public_profile_updated_at_unix_ms: Some(registry_row.public_profile_updated_at_unix_ms),
            created_at_unix_ms: registry_row.created_at_unix_ms,
            updated_at_unix_ms: registry_row.updated_at_unix_ms,
        }))
    }

    pub fn delete_profile_foundation_rows(&self, profile_id: &LocalProfileId) -> OmniResult<()> {
        self.connection
            .execute(
                "DELETE FROM profile_current_device WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM profile_secrets WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM profile_registry WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM app_meta WHERE key = ?1 AND value = ?2",
                params![ACTIVE_PROFILE_ID_META_KEY, profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}

fn profile_summary_from_registry_row(row: ProfileRegistryRow) -> OmniResult<LocalProfileSummary> {
    Ok(LocalProfileSummary {
        profile_id: LocalProfileId(row.profile_id),
        account_id: AccountId(row.account_id),
        display_name: row.display_name,
        state: parse_local_profile_state(&row.profile_state)?,
        account_representation_version: row.account_representation_version as u16,
    })
}

fn profile_state_to_db(state: omni_identity::LocalProfileState) -> &'static str {
    match state {
        omni_identity::LocalProfileState::SignedIn => "signed_in",
        omni_identity::LocalProfileState::SignedOutLocked => "signed_out_locked",
        omni_identity::LocalProfileState::Deleting => "deleting",
        omni_identity::LocalProfileState::Deleted => "deleted",
    }
}
