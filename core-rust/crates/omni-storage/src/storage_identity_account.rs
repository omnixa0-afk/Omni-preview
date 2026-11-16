const LOCAL_PROFILE_DELETED_META_KEY: &str = "local_profile_deleted";

impl Storage {
    pub fn export_database_backup(&self, destination_path: impl AsRef<Path>) -> OmniResult<()> {
        let destination_path = destination_path.as_ref();
        // TM-006: confine the caller-supplied destination. Validation-only: VACUUM INTO uses the
        // original path. See docs/decisions/tm-006-destructive-path-confinement.md.
        let _confined = confine_destructive_path(destination_path)?;
        if Self::paths_refer_to_same_location(&self.db_path, destination_path)? {
            return Err(OmniError::InvalidInput(
                "backup destination must not match the current database".to_string(),
            ));
        }
        if let Some(parent) = destination_path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to create backup directory '{}': {error}",
                    parent.display()
                ))
            })?;
        }

        // V-24 (review 2026-10-18): the export deletes nothing at the destination. It used to wipe
        // whatever a database there would own — journals, sidecars, and the `attachments/` and
        // `profile-media/` folders next to it — so a copy saved into a folder that held anything else
        // (another profile, the user's own `attachments`) took that with it. Now it writes only its
        // own files, and a destination that already has one of them is refused.
        if let Some(existing) = Self::database_copy_files(destination_path)?
            .into_iter()
            .find(|file| file.exists())
        {
            return Err(OmniError::InvalidInput(format!(
                "backup destination already has '{}': choose a new file name or an empty folder",
                existing.display()
            )));
        }
        self.connection
            .execute_batch(&format!(
                "VACUUM INTO {}",
                sqlite_string_literal(destination_path)
            ))
            .map_err(map_sqlite_error)?;
        // The copy is encrypted under this database's key: it travels with the same key file.
        self.copy_database_key_file_to(destination_path)?;

        Ok(())
    }

    pub fn inspect_account_backup_manifest(
        backup_db_path: impl AsRef<Path>,
        mnemonic: &str,
    ) -> OmniResult<omni_identity::AccountExportManifestV1> {
        // A backup made on another machine: its key is sealed there, the phrase opens it here.
        if Storage::database_lock_state(backup_db_path.as_ref())? == DatabaseLockState::NeedsRecoveryPhrase {
            Storage::unlock_database_with_mnemonic(backup_db_path.as_ref(), mnemonic)?;
        }
        let storage = Storage::open(backup_db_path)?;
        let bootstrap_summary = storage.inspect_database_bootstrap_summary()?;
        if !bootstrap_summary.supported {
            return Err(OmniError::InvalidInput(
                bootstrap_summary
                    .unsupported_reason
                    .unwrap_or_else(|| "backup database format is unsupported".to_string()),
            ));
        }

        let Some(account) = storage.get_local_account()? else {
            return Err(OmniError::InvalidInput(
                "backup database does not contain an active account".to_string(),
            ));
        };
        let Some(device) = storage.get_local_device()? else {
            return Err(OmniError::InvalidInput(
                "backup database does not contain an active device".to_string(),
            ));
        };
        let Some(local_profile_summary) = account.local_profile_summary.as_ref() else {
            return Err(OmniError::Storage(
                "backup database is missing the active profile summary".to_string(),
            ));
        };

        if account.current_device_id != device.device_id {
            return Err(OmniError::Storage(
                "backup database device state is inconsistent".to_string(),
            ));
        }

        let recovery_preview = storage.inspect_recovery_input(
            mnemonic,
            Some(&account.descriptor.display_name),
            Some(&device.device_label),
        )?;
        if recovery_preview.profile_id != local_profile_summary.profile_id {
            return Err(OmniError::InvalidInput(
                "backup database does not match the provided mnemonic".to_string(),
            ));
        }
        if recovery_preview.account_id != account.descriptor.account_id {
            return Err(OmniError::InvalidInput(
                "backup database belongs to a different account".to_string(),
            ));
        }
        omni_identity::build_account_export_manifest(&account, &device, current_unix_ms())
    }

    pub fn inspect_recovery_input(
        &self,
        mnemonic: &str,
        display_name: Option<&str>,
        device_label: Option<&str>,
    ) -> OmniResult<omni_identity::RecoveryInputSummary> {
        let recovery_phrase_v2 = omni_identity::build_recovery_phrase_v2_from_input(mnemonic)?;
        let root_secret =
            omni_identity::account_root_secret_from_recovery_phrase_v2(&recovery_phrase_v2)?;
        let account_id = omni_identity::derive_account_id_from_root(&root_secret)?;
        let profile_id =
            omni_identity::derive_local_profile_id_from_root(&root_secret, &account_id)?;
        let existing_profile = self.get_local_profile_summary(&profile_id)?;
        let active_profile_id = self.get_active_profile_id()?;

        Ok(omni_identity::build_recovery_input_summary(
            profile_id,
            account_id,
            active_profile_id.as_ref(),
            existing_profile.as_ref(),
            display_name.map(str::to_string),
            device_label.map(str::to_string),
        ))
    }

    pub fn create_local_account(
        &self,
        display_name: &str,
        device_label: &str,
        now_unix_ms: i64,
    ) -> OmniResult<LocalAccountSnapshot> {
        let (account, device, representation_v2) =
            create_account_identity_v2(display_name, device_label, now_unix_ms)?;
        self.persist_local_account_and_device(&account, &device, Some(&representation_v2))?;
        // The new profile's phrase can open this database on another machine from now on.
        self.ensure_database_key_wraps()?;
        self.mark_local_profile_deleted(false)?;

        Ok(account)
    }

    pub fn restore_local_account_from_mnemonic(
        &self,
        display_name: &str,
        mnemonic: &str,
        device_label: &str,
        now_unix_ms: i64,
    ) -> OmniResult<LocalAccountSnapshot> {
        let recovery_preview =
            self.inspect_recovery_input(mnemonic, Some(display_name), Some(device_label))?;
        if recovery_preview.conflict.is_some() {
            return Err(OmniError::InvalidInput(
                "profile already present on this device".to_string(),
            ));
        }
        let recovery_phrase_v2 = omni_identity::build_recovery_phrase_v2_from_input(mnemonic)?;
        let (mut account, device, representation_v2) = restore_account_identity_v2(
            &recovery_phrase_v2,
            display_name,
            device_label,
            now_unix_ms,
        )?;
        // The name typed at restore is a placeholder, not an edit: stamped "never", so the account's
        // real profile — from another device of ours or the passport — wins over it, instead of this
        // one taking the photo off them (live 2026-10-13: a phrase restore on the PC wiped the photo
        // on the laptop and in every group card).
        account.public_profile_updated_at_unix_ms = Some(0);
        self.persist_local_account_and_device(&account, &device, Some(&representation_v2))?;
        // The new profile's phrase can open this database on another machine from now on.
        self.ensure_database_key_wraps()?;
        self.mark_local_profile_deleted(false)?;

        Ok(account)
    }

    pub fn restore_local_account_from_export_manifest(
        &self,
        manifest: &omni_identity::AccountExportManifestV1,
        mnemonic: &str,
        now_unix_ms: i64,
    ) -> OmniResult<LocalAccountSnapshot> {
        if manifest.version != 1 {
            return Err(OmniError::InvalidInput(format!(
                "unsupported account export manifest version {}",
                manifest.version
            )));
        }

        if manifest.account_representation_version
            != omni_identity::ACCOUNT_REPRESENTATION_VERSION_V2
        {
            return Err(OmniError::InvalidInput(format!(
                "unsupported account representation version {}",
                manifest.account_representation_version
            )));
        }

        let recovery_preview = self.inspect_recovery_input(
            mnemonic,
            Some(&manifest.display_name),
            Some(&manifest.device_label),
        )?;
        if recovery_preview.profile_id != manifest.profile_id {
            return Err(OmniError::InvalidInput(
                "account export file does not match the provided mnemonic".to_string(),
            ));
        }
        if recovery_preview.account_id != manifest.account_id {
            return Err(OmniError::InvalidInput(
                "account export file belongs to a different account".to_string(),
            ));
        }
        if recovery_preview.conflict.is_some() {
            return Err(OmniError::InvalidInput(
                "profile already present on this device".to_string(),
            ));
        }

        self.restore_local_account_from_mnemonic(
            &manifest.display_name,
            mnemonic,
            &manifest.device_label,
            now_unix_ms,
        )
    }

    pub fn reopen_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<LocalProfileSummary> {
        let profile = self
            .get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        if profile.state != omni_identity::LocalProfileState::SignedIn {
            return Err(OmniError::InvalidInput(
                "profile must be signed in before it can be reopened".to_string(),
            ));
        }
        self.set_active_profile_id(Some(profile_id))?;
        Ok(profile)
    }

    pub fn get_local_account(&self) -> OmniResult<Option<LocalAccountSnapshot>> {
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Ok(None);
        };
        self.get_local_account_by_profile_id(&profile_id)
    }

    pub fn get_local_device(&self) -> OmniResult<Option<LocalDeviceSnapshot>> {
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Ok(None);
        };
        self.get_local_device_by_profile_id(&profile_id)
    }

    pub fn get_current_account_export_manifest(
        &self,
    ) -> OmniResult<Option<omni_identity::AccountExportManifestV1>> {
        let Some(account) = self.get_local_account()? else {
            return Ok(None);
        };
        let Some(device) = self.get_local_device()? else {
            return Ok(None);
        };

        let mut manifest = omni_identity::build_account_export_manifest(&account, &device, current_unix_ms())?;
        manifest.last_backup_at_unix_ms = self.last_backup_at(&account.descriptor.account_id)?;
        Ok(Some(manifest))
    }

    /// The account saved a backup file now (the settings show when, and remind after a month).
    pub fn record_backup_saved(&self, account_id: &AccountId, at_unix_ms: i64) -> OmniResult<()> {
        self.upsert_app_meta(&format!("last_backup_at_unix_ms:{}", account_id.0), &at_unix_ms.to_string())
    }

    pub fn last_backup_at(&self, account_id: &AccountId) -> OmniResult<Option<i64>> {
        Ok(self
            .get_app_meta(&format!("last_backup_at_unix_ms:{}", account_id.0))?
            .and_then(|value| value.parse().ok()))
    }

    pub fn list_local_profiles(&self) -> OmniResult<SavedProfileList> {
        self.list_profiles()
    }

    pub fn get_local_profile_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalProfileSummary>> {
        self.get_profile_by_id(profile_id)
    }

    pub fn get_password_protection_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<PasswordProtectionSummary>> {
        let Some((enabled, row_profile_id)) = self
            .connection
            .query_row(
                "
                SELECT
                    password_enabled,
                    profile_id
                FROM profile_secrets
                WHERE profile_id = ?1
                ",
                params![profile_id.0.as_str()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? != 0,
                        row.get::<_, String>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?
        else {
            return Ok(None);
        };

        Ok(Some(password_protection_summary(
            LocalProfileId(row_profile_id),
            enabled,
        )))
    }

    pub fn is_profile_signed_in(&self) -> OmniResult<bool> {
        let Some(profile_id) = self.get_active_profile_id()? else {
            return Ok(false);
        };
        let Some(profile) = self.get_local_profile_summary(&profile_id)? else {
            return Ok(false);
        };
        Ok(profile.state == omni_identity::LocalProfileState::SignedIn)
    }

    pub fn sign_out_profile(&self, profile_id: &LocalProfileId) -> OmniResult<LocalProfileSummary> {
        let Some(profile) = self.get_local_profile_summary(profile_id)? else {
            return Err(OmniError::InvalidInput("profile was not found".to_string()));
        };
        let now = current_unix_ms();
        self.update_profile_state(profile_id, omni_identity::LocalProfileState::SignedOutLocked, now)?;

        Ok(LocalProfileSummary {
            state: omni_identity::LocalProfileState::SignedOutLocked,
            ..profile
        })
    }

    pub fn reveal_profile_recovery_phrase_with_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
    ) -> OmniResult<omni_identity::RecoveryPhraseV2> {
        let password = validate_non_empty_password(password)?;
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let unlock_key = validate_profile_password_against_secrets_row(&row, password)?;
        match read_stored_profile_root(&row, Some(&unlock_key))? {
            StoredProfileRoot::Open(secrets) => Ok(secrets.recovery_phrase_v2),
            StoredProfileRoot::OtherMachine => Err(OmniError::InvalidInput(
                "this computer cannot open the stored phrase: the profile came from another computer"
                    .to_string(),
            )),
            StoredProfileRoot::Absent | StoredProfileRoot::NeedsPassword => Err(OmniError::InvalidInput(
                "this profile keeps no recovery phrase on this computer".to_string(),
            )),
        }
    }

    pub fn unlock_profile_with_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
    ) -> OmniResult<LocalProfileSummary> {
        let password = validate_non_empty_password(password)?;
        let _profile = self
            .get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        if !row.password_enabled {
            return Err(OmniError::InvalidInput(
                "password protection is not enabled for this profile".to_string(),
            ));
        }
        let unlock_key = validate_profile_password_against_secrets_row(&row, password)?;

        let now = current_unix_ms();
        self.update_profile_state(profile_id, omni_identity::LocalProfileState::SignedIn, now)?;
        self.set_active_profile_id(Some(profile_id))?;
        self.certify_local_device_with_password_key(&unlock_key, now)?;
        // A root that is not under the password yet (a password set before 2026-10-29, or a root
        // the phrase resealed) goes under it now; its database-key wrap is checked while it is open.
        if !row.profile_master_key_password_layer {
            self.rewrap_profile_root(profile_id, None, Some(&unlock_key), now)?;
        }
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        if let StoredProfileRoot::Open(secrets) = read_stored_profile_root(&row, Some(&unlock_key))? {
            self.add_database_key_wraps(vec![(AccountId(row.account_id.clone()), secrets.root_secret)])?;
        }

        self.get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::Storage("profile summary missing after unlock".to_string()))
    }

    pub fn unlock_profile_with_mnemonic(
        &self,
        profile_id: &LocalProfileId,
        mnemonic: &str,
    ) -> OmniResult<LocalProfileSummary> {
        let (account_id, root_secret) = self.take_profile_root_from_mnemonic(profile_id, mnemonic)?;

        let now = current_unix_ms();
        self.update_profile_state(profile_id, omni_identity::LocalProfileState::SignedIn, now)?;
        self.set_active_profile_id(Some(profile_id))?;
        // The phrase also derives the working key: a device whose working key waits for a password
        // is certified now.
        self.certify_local_device_with_root(&account_id, &root_secret, now)?;
        self.ensure_database_key_wraps()?;
        // A root under the profile password is not among the openable ones; the phrase has it.
        self.add_database_key_wraps(vec![(account_id, root_secret)])?;

        self.get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::Storage("profile summary missing after unlock".to_string()))
    }

    /// The phrase that opened this database from another machine (`unlock_database_with_mnemonic`)
    /// belongs to a profile in it: seal that profile's root for this machine too, so it does not ask
    /// for the phrase a second time. Does not sign the profile in — its state and password stay as
    /// they were. A phrase of no profile here is ignored: it opened the file, nothing more to do.
    pub fn adopt_profile_root_with_mnemonic(&self, mnemonic: &str) -> OmniResult<()> {
        let parsed = omni_identity::parse_mnemonic_v2(mnemonic.trim())?;
        let account_id = omni_identity::derive_account_id_from_root(&parsed.root_secret)?;
        let profile_id = omni_identity::derive_local_profile_id_from_root(&parsed.root_secret, &account_id)?;
        if self.get_profile_secrets_row(&profile_id)?.is_none() {
            return Ok(());
        }
        let (account_id, root_secret) = self.take_profile_root_from_mnemonic(&profile_id, mnemonic)?;
        // The device being certified is the active profile's: only that profile's chain may sign it.
        if self.get_active_profile_id()?.as_ref() == Some(&profile_id) {
            self.certify_local_device_with_root(&account_id, &root_secret, current_unix_ms())?;
        }
        self.ensure_database_key_wraps()
    }

    /// Check that `mnemonic` is the phrase of `profile_id` and, when the stored root does not open on
    /// this machine (a folder copied from another one), seal it again here. Returns the account and
    /// its root.
    fn take_profile_root_from_mnemonic(
        &self,
        profile_id: &LocalProfileId,
        mnemonic: &str,
    ) -> OmniResult<(AccountId, omni_identity::AccountRootSecret)> {
        let mnemonic = mnemonic.trim();
        if mnemonic.is_empty() {
            return Err(OmniError::InvalidInput("mnemonic must not be empty".to_string()));
        }
        let profile = self
            .get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let parsed_mnemonic = omni_identity::parse_mnemonic_v2(mnemonic)?;
        let root_secret = parsed_mnemonic.root_secret.clone();
        let derived_account_id = omni_identity::derive_account_id_from_root(&root_secret)?;
        if derived_account_id.0 != row.account_id {
            return Err(OmniError::InvalidInput(
                "mnemonic does not belong to this local profile".to_string(),
            ));
        }
        let derived_public = omni_identity::derive_account_public_identity(
            &root_secret,
            &profile.display_name,
        )?;
        if derived_public.account_id.0 != row.account_id {
            return Err(OmniError::InvalidInput(
                "mnemonic failed profile public-identity verification".to_string(),
            ));
        }
        // The phrase proves the account by itself. The stored copy of the root, when this machine
        // can open it, must agree; a folder copied from another machine cannot open it (sealed to
        // that machine) — the phrase seals the root and the phrase again, for this one.
        // A root under the profile password stays as it is: the phrase already proved the account,
        // and resealing it would drop the password layer.
        let account_id = AccountId(row.account_id.clone());
        match read_stored_profile_root(&row, None) {
            Ok(StoredProfileRoot::Open(stored)) => {
                if stored.root_secret != root_secret {
                    return Err(OmniError::InvalidInput(
                        "mnemonic does not match encrypted root secret for this profile".to_string(),
                    ));
                }
            }
            Ok(StoredProfileRoot::NeedsPassword) => {}
            // Resealed without the password layer; a profile with a password gets it back at the
            // next password unlock (`unlock_profile_with_password`).
            Ok(StoredProfileRoot::Absent | StoredProfileRoot::OtherMachine) | Err(_) => {
                self.reseal_profile_secrets(profile_id, &account_id, &root_secret)?
            }
        }
        Ok((account_id, root_secret))
    }

    /// Seal the root secret and its phrase to THIS machine, replacing a seal that does not open
    /// here (a folder copied from another computer).
    fn reseal_profile_secrets(
        &self,
        profile_id: &LocalProfileId,
        account_id: &AccountId,
        root_secret: &omni_identity::AccountRootSecret,
    ) -> OmniResult<()> {
        let recovery_phrase_v2 = omni_identity::derive_recovery_phrase_v2(root_secret)?;
        let recovery_version = i64::from(recovery_phrase_v2.version);
        let sealed = seal_profile_secrets(
            account_id,
            &StoredProfileSecretsV1 {
                root_secret: root_secret.clone(),
                recovery_phrase_v2,
            },
        )?;
        self.connection
            .execute(
                "
                UPDATE profile_secrets
                SET profile_master_key_protection_scheme = ?2,
                    profile_master_key_blob_hex = ?3,
                    encrypted_profile_secrets_json = ?4,
                    profile_master_key_password_layer = 0,
                    recovery_version = ?5,
                    updated_at_unix_ms = ?6
                WHERE profile_id = ?1
                ",
                params![
                    profile_id.0.as_str(),
                    sealed.profile_master_key.scheme.as_db_value(),
                    sealed.profile_master_key.blob_hex,
                    sealed.encrypted_profile_secrets_json,
                    recovery_version,
                    current_unix_ms(),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Move the root's seal between this machine alone and this machine plus the profile password,
    /// following the password: set (`None` → key), changed (old → new), removed (old → `None`). A
    /// root not under the password yet ignores `from`. A root this machine cannot open (a copied
    /// folder) is left for the phrase.
    fn rewrap_profile_root(
        &self,
        profile_id: &LocalProfileId,
        from_unlock_key: Option<&[u8; 32]>,
        to_unlock_key: Option<&[u8; 32]>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let profile_master_key = match open_profile_master_key(&row, from_unlock_key)? {
            Ok(key) => key,
            Err(StoredProfileRoot::NeedsPassword) => {
                return Err(OmniError::Storage(
                    "the root is under the profile password, but the current password was not given"
                        .to_string(),
                ))
            }
            Err(_) => return Ok(()),
        };
        let sealed = seal_profile_master_key(&row.account_id, &profile_master_key, to_unlock_key)?;
        self.connection
            .execute(
                "
                UPDATE profile_secrets
                SET profile_master_key_protection_scheme = ?2,
                    profile_master_key_blob_hex = ?3,
                    profile_master_key_password_layer = ?4,
                    updated_at_unix_ms = ?5
                WHERE profile_id = ?1
                ",
                params![
                    profile_id.0.as_str(),
                    sealed.scheme.as_db_value(),
                    sealed.blob_hex,
                    bool_to_sqlite_integer(to_unlock_key.is_some()),
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn set_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
    ) -> OmniResult<PasswordProtectionSummary> {
        self.apply_local_profile_password(profile_id, password, None)
    }

    /// Set the password; `previous_unlock_key` is the old password's key when changing it — the
    /// working key is under the old password and has to move under the new one.
    fn apply_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
        previous_unlock_key: Option<&[u8; 32]>,
    ) -> OmniResult<PasswordProtectionSummary> {
        let password = validate_non_empty_password(password)?;
        let profile = self
            .get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;

        // The password guards the working key, and the root secret with the phrase INSIDE this
        // machine's seal: DPAPI(password(root)). A copied folder plus the password still yields no
        // root (a thief could out-migrate the owner, docs/decisions/multi-device-equal-nodes.md), and
        // a process of the same user without the password no longer does either (r2 question 12).
        let salt = random_salt_hex()?;
        let unlock_key = derive_password_unlock_key(password, &salt)?;
        let verifier = password_verifier_hex(&unlock_key);
        let now = current_unix_ms();
        self.with_immediate_transaction("apply_local_profile_password", |storage| {
            storage.rewrap_working_key(
                &AccountId(row.account_id.clone()),
                previous_unlock_key,
                Some(&unlock_key),
                now,
            )?;
            storage.rewrap_profile_root(profile_id, previous_unlock_key, Some(&unlock_key), now)?;

            storage.connection
                .execute(
                    "
                    UPDATE profile_secrets
                    SET password_enabled = 1,
                        password_salt_hex = ?2,
                        password_verifier_hex = ?3,
                        updated_at_unix_ms = ?4
                    WHERE profile_id = ?1
                    ",
                    params![profile_id.0.as_str(), salt.as_str(), verifier.as_str(), now],
                )
                .map_err(map_sqlite_error)?;
            Ok(())
        })?;

        Ok(password_protection_summary(profile.profile_id, true))
    }

    pub fn change_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        current_password: &str,
        new_password: &str,
    ) -> OmniResult<PasswordProtectionSummary> {
        let previous_unlock_key = self.verify_profile_password(profile_id, current_password)?;
        self.unlock_profile_with_password(profile_id, current_password)?;
        self.apply_local_profile_password(profile_id, new_password, Some(&previous_unlock_key))
    }

    fn verify_profile_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
    ) -> OmniResult<[u8; 32]> {
        let password = validate_non_empty_password(password)?;
        let row = self
            .get_profile_secrets_row(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        validate_profile_password_against_secrets_row(&row, password)
    }

    pub fn remove_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        current_password: &str,
    ) -> OmniResult<PasswordProtectionSummary> {
        let previous_unlock_key = self.verify_profile_password(profile_id, current_password)?;
        self.unlock_profile_with_password(profile_id, current_password)?;
        let profile = self
            .get_local_profile_summary(profile_id)?
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;

        let now = current_unix_ms();
        let account_id = self
            .get_profile_secrets_row(profile_id)?
            .map(|row| AccountId(row.account_id))
            .ok_or_else(|| OmniError::InvalidInput("profile was not found".to_string()))?;
        self.with_immediate_transaction("remove_local_profile_password", |storage| {
            storage.rewrap_working_key(&account_id, Some(&previous_unlock_key), None, now)?;
            storage.rewrap_profile_root(profile_id, Some(&previous_unlock_key), None, now)?;
            storage.connection
                .execute(
                    "
                    UPDATE profile_secrets
                    SET password_enabled = 0,
                        password_salt_hex = NULL,
                        password_verifier_hex = NULL,
                        updated_at_unix_ms = ?2
                    WHERE profile_id = ?1
                    ",
                    params![profile_id.0.as_str(), now],
                )
                .map_err(map_sqlite_error)?;
            Ok(())
        })?;

        Ok(password_protection_summary(profile.profile_id, false))
    }

    pub fn delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<()> {
        let Some(profile) = self.get_local_profile_summary(profile_id)? else {
            return Ok(());
        };
        let attachment_file_paths = self.attachment_file_paths_for_profile(profile_id)?;

        self.with_immediate_transaction("delete_local_profile", |storage| {
            let account_id = profile.account_id.0.as_str();
            storage.delete_profile_rows_by_profile_id(profile_id)?;
            storage.delete_profile_auxiliary_rows(account_id)?;
            storage.delete_profile_foundation_rows(profile_id)?;
            storage.delete_profile_account_rows(account_id)?;
            storage.delete_attachment_files(attachment_file_paths.clone())?;
            storage.delete_public_profile_assets_for_profile(profile_id)?;

            storage.delete_profile_meta(profile_id)?;
            let has_remaining_profiles = !storage.list_local_profiles()?.profiles.is_empty();
            if !has_remaining_profiles {
                storage.delete_rows_of_the_last_profile()?;
            }
            storage.mark_local_profile_deleted(!has_remaining_profiles)?;

            Ok(())
        })
    }

    /// Leave one profile in a COPY of the database (never in the profile's own): every row of every
    /// other profile goes, and the key file beside the copy keeps the wrap of this profile's account
    /// alone. Rows only — the files those rows name belong to the live profiles and are not touched.
    /// The copy is then rebuilt, so the removed rows are not left in its free pages. Returns the
    /// attachment files the kept profile's rows name.
    ///
    /// A backup was a snapshot of the whole install under one profile's key: whoever held that
    /// profile's phrase read the chats and keys of every other profile on the machine (blind audit
    /// 2026-11-11, pass 14).
    pub fn keep_one_profile_in_copy(copy_db_path: &Path, keep: &LocalProfileId) -> OmniResult<Vec<PathBuf>> {
        let kept_files = {
            let copy = Storage::open(copy_db_path)?;
            let Some(kept) = copy.get_local_profile_summary(keep)? else {
                return Err(OmniError::Storage("the copy does not hold the profile it is made for".to_string()));
            };
            let others: Vec<_> =
                copy.list_local_profiles()?.profiles.into_iter().filter(|profile| profile.profile_id != *keep).collect();
            copy.with_immediate_transaction("keep_one_profile_in_copy", |copy| {
                for other in &others {
                    let account_id = other.account_id.0.as_str();
                    copy.delete_profile_rows_by_profile_id(&other.profile_id)?;
                    copy.delete_profile_auxiliary_rows(account_id)?;
                    copy.delete_profile_foundation_rows(&other.profile_id)?;
                    copy.delete_profile_account_rows(account_id)?;
                    copy.delete_profile_meta(&other.profile_id)?;
                    copy.connection
                        .execute("DELETE FROM app_meta WHERE key = ?1", params![format!("last_backup_at_unix_ms:{account_id}")])
                        .map_err(map_sqlite_error)?;
                }
                Ok(())
            })?;
            copy.connection.execute_batch("VACUUM").map_err(map_sqlite_error)?;
            // The copy is packed as one file: nothing of it may stay in a journal beside it.
            copy.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .map_err(map_sqlite_error)?;
            Self::keep_one_account_key_wrap(copy_db_path, &kept.account_id)?;
            copy.attachment_file_paths_for_profile(keep)?
        };
        Self::forget_key_of_database(copy_db_path);
        Ok(kept_files)
    }

    pub fn get_local_profile_footprint_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalProfileFootprintSummary>> {
        let Some(profile) = self.get_local_profile_summary(profile_id)? else {
            return Ok(None);
        };
        let contacts_count = self.count_profile_rows("contacts", profile_id)?;
        let sessions_count = self.count_profile_rows("sessions", profile_id)?;
        let messages_count = self.count_profile_rows("messages", profile_id)?;
        let password_summary = self.get_password_protection_summary(profile_id)?;

        Ok(Some(LocalProfileFootprintSummary {
            profile_id: profile.profile_id,
            account_id: profile.account_id,
            state: profile.state,
            contacts_count,
            sessions_count,
            messages_count,
            has_password_protection: password_summary.map(|value| value.enabled).unwrap_or(false),
        }))
    }

    pub fn preview_sign_out_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        let Some(footprint) = self.get_local_profile_footprint_summary(profile_id)? else {
            return Ok(None);
        };

        Ok(Some(DangerousActionPreview {
            profile_id: profile_id.clone(),
            action: DangerousActionKind::SignOutProfile,
            title: "Выйти из локального профиля".to_string(),
            summary: "Профиль останется на устройстве в заблокированном состоянии. Для повторного входа потребуется пароль (если задан) или фраза восстановления.".to_string(),
            consequences: vec![
                "Текущая сессия этого профиля будет завершена.".to_string(),
                "Очереди и фоновые операции для этого профиля остановятся до разблокировки.".to_string(),
                format!("Локальные данные сохранятся: {} контактов, {} сообщений.", footprint.contacts_count, footprint.messages_count),
                "Это не удаляет аккаунт глобально и не отзывает устройства.".to_string(),
            ],
            requires_confirmation: true,
            confirmation_token: Some("SIGN_OUT".to_string()),
            supported_now: true,
            unsupported_reason: None,
        }))
    }

    pub fn preview_delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        let Some(footprint) = self.get_local_profile_footprint_summary(profile_id)? else {
            return Ok(None);
        };
        // The device-level state (mesh identity, DHT peer cache, settings sidecars, attachments) is
        // shared by every profile in this db, so it only goes when the LAST one does — say which
        // case the user is in rather than promising the same thing both times.
        let is_last_profile = self.list_local_profiles()?.profiles.len() <= 1;
        let device_consequence = if is_last_profile {
            "This is the last local profile, so this device's Omni footprint goes with it: the mesh node identity (a new profile will be a NEW node), the DHT peer cache, downloaded attachments, and the connectivity/discovery settings all get wiped."
        } else {
            "Other local profiles stay intact — so this device's mesh node identity and settings stay too. This action does not delete the remote account or any remote device state."
        };

        Ok(Some(DangerousActionPreview {
            profile_id: profile_id.clone(),
            action: DangerousActionKind::DeleteLocalProfile,
            title: "Delete local profile".to_string(),
            summary: "Remove this local profile from this device. The wipe is irreversible for the profile-owned local data, but it does not delete the remote account, other profiles, or other devices.".to_string(),
            consequences: vec![
                format!(
                    "Local chat/contact history, send queue state, dedup cache, and delivery receipts will be wiped, including {} contacts, {} messages, and {} sessions.",
                    footprint.contacts_count,
                    footprint.messages_count,
                    footprint.sessions_count
                ),
                "Local profile registry, secrets, current-device metadata, route hints, and the installation-local account shadow for this profile will be removed.".to_string(),
                device_consequence.to_string(),
            ],
            requires_confirmation: true,
            confirmation_token: Some("DELETE_LOCAL_PROFILE".to_string()),
            supported_now: true,
            unsupported_reason: None,
        }))
    }

    fn delete_profile_rows_by_profile_id(&self, profile_id: &LocalProfileId) -> OmniResult<()> {
        for table_name in PROFILE_WIPE_TABLE_ORDER {
            match *table_name {
                "profile_registry"
                | "profile_secrets"
                | "profile_current_device"
                | "one_time_prekeys"
                | "signed_prekeys"
                | "device_keys"
                | "identity_keys"
                | "local_devices" => continue,
                "call_participants" => {
                    self.connection
                        .execute(
                            "
                            DELETE FROM call_participants
                            WHERE call_id IN (
                                SELECT call_id
                                FROM call_sessions
                                WHERE profile_id = ?1
                            )
                            ",
                            params![profile_id.0.as_str()],
                        )
                        .map_err(map_sqlite_error)?;
                }
                _ => {
                    if ownership_category_for_table(table_name) != Some(StorageOwnershipCategory::ProfileOwned)
                    {
                        continue;
                    }
                    self.connection
                        .execute(
                            &format!("DELETE FROM {table_name} WHERE profile_id = ?1"),
                            params![profile_id.0.as_str()],
                        )
                        .map_err(map_sqlite_error)?;
                }
            }
        }

        Ok(())
    }

    fn delete_profile_auxiliary_rows(&self, account_id: &str) -> OmniResult<()> {
        self.delete_account_key_rows(account_id)
    }

    fn delete_profile_account_rows(
        &self,
        account_id: &str,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "DELETE FROM one_time_prekeys WHERE device_id IN (SELECT device_id FROM local_devices WHERE account_id = ?1)",
                params![account_id],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM signed_prekeys WHERE device_id IN (SELECT device_id FROM local_devices WHERE account_id = ?1)",
                params![account_id],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute("DELETE FROM device_keys WHERE account_id = ?1", params![account_id])
            .map_err(map_sqlite_error)?;
        self.connection
            .execute("DELETE FROM identity_keys WHERE account_id = ?1", params![account_id])
            .map_err(map_sqlite_error)?;
        self.connection
            .execute("DELETE FROM local_devices WHERE account_id = ?1", params![account_id])
            .map_err(map_sqlite_error)?;
        self.connection
            .execute("DELETE FROM local_account WHERE account_id = ?1", params![account_id])
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn preview_revoke_current_device_and_wipe(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        let Some(_profile) = self.get_local_profile_summary(profile_id)? else {
            return Ok(None);
        };

        Ok(Some(DangerousActionPreview {
            profile_id: profile_id.clone(),
            action: DangerousActionKind::RevokeCurrentDeviceAndWipe,
            title: "Отозвать текущее устройство и очистить данные".to_string(),
            summary: "Операция пока недоступна в этом pass: требуется отдельная безопасная оркестрация с sync/device registry.".to_string(),
            consequences: vec![
                "Когда будет реализовано: устройство отзывается в реестре аккаунта.".to_string(),
                "Когда будет реализовано: локальные данные устройства очищаются после успешного подтверждения отзыва.".to_string(),
            ],
            requires_confirmation: false,
            confirmation_token: None,
            supported_now: false,
            unsupported_reason: Some(
                "Функция отзыва текущего устройства с безопасным wipe будет добавлена отдельным проходом.".to_string(),
            ),
        }))
    }

    pub(crate) fn count_profile_rows(
        &self,
        table_name: &str,
        profile_id: &LocalProfileId,
    ) -> OmniResult<usize> {
        self.connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table_name} WHERE profile_id = ?1"),
                params![profile_id.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)
            .map(|value| value.max(0) as usize)
    }

    pub fn was_local_profile_deleted(&self) -> OmniResult<bool> {
        Ok(matches!(
            self.get_app_meta(LOCAL_PROFILE_DELETED_META_KEY)?.as_deref(),
            Some("1")
        ))
    }

    /// The last profile is gone: the rows no profile column ties to one go too — message marks, keys
    /// and revocations of accounts, this node's mesh books, the profiles' own `app_meta` values. They
    /// are the deleted identity's; the install keeps only what the user set for it (relays, couriers)
    /// and its own marks. Until 2026-11-09 they stayed, though the database was called «an empty
    /// container» by then.
    fn delete_rows_of_the_last_profile(&self) -> OmniResult<()> {
        // Every row of them goes, so a reference between two of them is checked at the commit.
        self.connection.execute_batch("PRAGMA defer_foreign_keys = ON").map_err(map_sqlite_error)?;
        for entry in STORAGE_OWNERSHIP_MAP.iter().filter(|entry| entry.category != StorageOwnershipCategory::GlobalInstallScoped) {
            let exists = self
                .connection
                .query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1", params![entry.name], |_| Ok(()))
                .optional()
                .map_err(map_sqlite_error)?
                .is_some();
            if exists {
                self.connection.execute(&format!("DELETE FROM \"{}\"", entry.name), []).map_err(map_sqlite_error)?;
            }
        }
        for prefix in [PROFILE_META_PREFIX, "last_backup_at_unix_ms:"].iter().chain(LEGACY_PROFILE_META_PREFIXES) {
            self.connection
                .execute("DELETE FROM app_meta WHERE substr(key, 1, length(?1)) = ?1", params![prefix])
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Values a profile kept in `app_meta` before they carried its id (until 2026-11-09): they move to
    /// the active profile (or the only one); with none, they go. Keys under the id never match.
    fn move_legacy_profile_meta(&self) -> OmniResult<()> {
        let owner: Option<String> = match self.get_app_meta(ACTIVE_PROFILE_ID_META_KEY)? {
            Some(active) if !active.is_empty() => Some(active),
            _ => {
                let ids: Vec<String> = self
                    .connection
                    .prepare("SELECT profile_id FROM profile_registry")
                    .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
                    .map_err(map_sqlite_error)?;
                (ids.len() == 1).then(|| ids[0].clone())
            }
        };
        for prefix in LEGACY_PROFILE_META_PREFIXES {
            match &owner {
                Some(profile_id) => self
                    .connection
                    .execute(
                        "UPDATE OR REPLACE app_meta SET key = ?1 || key WHERE substr(key, 1, length(?2)) = ?2",
                        params![profile_meta_key(&LocalProfileId(profile_id.clone()), ""), prefix],
                    )
                    .map_err(map_sqlite_error)?,
                None => self
                    .connection
                    .execute("DELETE FROM app_meta WHERE substr(key, 1, length(?1)) = ?1", params![prefix])
                    .map_err(map_sqlite_error)?,
            };
        }
        Ok(())
    }

    fn mark_local_profile_deleted(&self, deleted: bool) -> OmniResult<()> {
        self.upsert_app_meta(
            LOCAL_PROFILE_DELETED_META_KEY,
            if deleted { "1" } else { "0" },
        )
    }
}

fn sqlite_string_literal(path: &Path) -> String {
    let text = path.display().to_string().replace('\'', "''");
    format!("'{}'", text)
}

fn parse_local_profile_state(value: &str) -> OmniResult<omni_identity::LocalProfileState> {
    match value {
        "signed_in" => Ok(omni_identity::LocalProfileState::SignedIn),
        "signed_out_locked" => Ok(omni_identity::LocalProfileState::SignedOutLocked),
        "deleting" => Ok(omni_identity::LocalProfileState::Deleting),
        "deleted" => Ok(omni_identity::LocalProfileState::Deleted),
        other => Err(OmniError::Storage(format!(
            "unsupported local profile state '{other}'"
        ))),
    }
}

fn validate_non_empty_password(password: &str) -> OmniResult<&str> {
    let normalized = password.trim();
    if normalized.is_empty() {
        return Err(OmniError::InvalidInput(
            "password must not be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn random_salt_hex() -> OmniResult<String> {
    let mut salt = [0u8; PASSWORD_SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    Ok(hex::encode(salt))
}

fn derive_password_unlock_key(password: &str, salt_hex: &str) -> OmniResult<[u8; 32]> {
    let salt = hex::decode(salt_hex)
        .map_err(|error| OmniError::InvalidInput(format!("invalid password salt hex: {error}")))?;
    let params = Params::new(
        PASSWORD_ARGON2_MEMORY_KIB,
        PASSWORD_ARGON2_ITERATIONS,
        PASSWORD_ARGON2_PARALLELISM,
        Some(32),
    )
    .map_err(|error| OmniError::Internal(format!("argon2 params are invalid: {error}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon2
        .hash_password_into(password.as_bytes(), &salt, &mut key)
        .map_err(|error| OmniError::Internal(format!("argon2id derivation failed: {error}")))?;
    Ok(key)
}

fn password_verifier_hex(unlock_key: &[u8; 32]) -> String {
    hex::encode(sha2::Sha256::digest(unlock_key))
}

/// Check the password and return its Argon2id unlock key (it also opens the working key).
fn validate_profile_password_against_secrets_row(
    row: &ProfileSecretsRow,
    password: &str,
) -> OmniResult<[u8; 32]> {
    if !row.password_enabled {
        return Err(OmniError::InvalidInput(
            "password protection is not enabled for this profile".to_string(),
        ));
    }

    let salt_hex = row.password_salt_hex.as_deref().ok_or_else(|| {
        OmniError::Storage("password salt is missing for protected profile".to_string())
    })?;
    let expected_verifier = row.password_verifier_hex.as_deref().ok_or_else(|| {
        OmniError::Storage("password verifier is missing for protected profile".to_string())
    })?;
    let unlock_key = derive_password_unlock_key(password, salt_hex)?;
    if password_verifier_hex(&unlock_key) != expected_verifier {
        return Err(OmniError::InvalidInput(
            "password does not unlock this profile".to_string(),
        ));
    }

    Ok(unlock_key)
}

/// Where the stored root secret of a profile stands on this machine.
enum StoredProfileRoot {
    /// No root kept in this profile.
    Absent,
    /// Sealed to another machine (a copied folder): only the phrase brings it here.
    OtherMachine,
    /// Under the profile password as well; the caller had no unlock key.
    NeedsPassword,
    Open(StoredProfileSecretsV1),
}

impl StoredProfileRoot {
    fn open(self) -> Option<StoredProfileSecretsV1> {
        match self {
            Self::Open(secrets) => Some(secrets),
            _ => None,
        }
    }
}

fn profile_master_key_password_context(account_id: &str) -> Vec<u8> {
    format!("omni-profile-master-key-password-v1|{account_id}").into_bytes()
}

/// Seal the profile master key to this machine, with the profile password inside the seal when
/// `password_unlock_key` is given: DPAPI(password(key)). A copied folder still opens nothing
/// without this machine, and a process of the same user no longer opens the root without the
/// password (r2 question 12).
fn seal_profile_master_key(
    account_id: &str,
    profile_master_key: &[u8; PROFILE_MASTER_KEY_LEN],
    password_unlock_key: Option<&[u8; 32]>,
) -> OmniResult<ProtectedSecret> {
    match password_unlock_key {
        None => protect_bytes(profile_master_key),
        Some(unlock_key) => {
            let wrapped = encrypt_at_rest_bytes(
                unlock_key,
                &profile_master_key_password_context(account_id),
                profile_master_key,
            )?;
            protect_bytes(&serde_json::to_vec(&wrapped).map_err(map_serde_error_to_storage)?)
        }
    }
}

/// The profile master key, or the state that keeps it from being had here. A wrong unlock key is
/// an error, not `NeedsPassword`.
fn open_profile_master_key(
    row: &ProfileSecretsRow,
    password_unlock_key: Option<&[u8; 32]>,
) -> OmniResult<Result<[u8; PROFILE_MASTER_KEY_LEN], StoredProfileRoot>> {
    if row.encrypted_profile_secrets_json.is_none() {
        return Ok(Err(StoredProfileRoot::Absent));
    }
    let scheme = row
        .profile_master_key_protection_scheme
        .as_deref()
        .ok_or_else(|| OmniError::Storage("profile master key scheme is missing".to_string()))?;
    let blob_hex = row
        .profile_master_key_blob_hex
        .as_deref()
        .ok_or_else(|| OmniError::Storage("profile master key blob is missing".to_string()))?;
    let protected_master_key = protected_secret_from_db(scheme, blob_hex)?;
    let Ok(sealed) = unprotect_bytes(&protected_master_key) else {
        return Ok(Err(StoredProfileRoot::OtherMachine));
    };
    let profile_master_key = if row.profile_master_key_password_layer {
        let Some(unlock_key) = password_unlock_key else {
            return Ok(Err(StoredProfileRoot::NeedsPassword));
        };
        let wrapped: AtRestEncryptedBlob =
            serde_json::from_slice(&sealed).map_err(map_serde_error_to_storage)?;
        omni_crypto::Zeroizing::new(
            decrypt_at_rest_bytes(
                unlock_key,
                &profile_master_key_password_context(&row.account_id),
                &wrapped,
            )
            .map_err(|_| OmniError::InvalidInput("password does not unlock this profile".to_string()))?,
        )
    } else {
        sealed
    };
    let profile_master_key: [u8; PROFILE_MASTER_KEY_LEN] = profile_master_key
        .as_slice()
        .try_into()
        .map_err(|_| OmniError::Storage("profile master key has unexpected length".to_string()))?;
    Ok(Ok(profile_master_key))
}

/// The root secret and the phrase of a profile. `password_unlock_key` opens a root that is under
/// the profile password; without it such a root is `NeedsPassword`.
fn read_stored_profile_root(
    row: &ProfileSecretsRow,
    password_unlock_key: Option<&[u8; 32]>,
) -> OmniResult<StoredProfileRoot> {
    let profile_master_key = match open_profile_master_key(row, password_unlock_key)? {
        Ok(key) => key,
        Err(state) => return Ok(state),
    };
    let Some(encrypted_profile_secrets_json) = row.encrypted_profile_secrets_json.as_deref() else {
        return Ok(StoredProfileRoot::Absent);
    };

    let encrypted_blob: AtRestEncryptedBlob =
        serde_json::from_str(encrypted_profile_secrets_json).map_err(map_serde_error_to_storage)?;
    let plaintext = decrypt_at_rest_bytes(
        &profile_master_key,
        format!("omni-profile-secrets-v1|{}", row.account_id).as_bytes(),
        &encrypted_blob,
    )?;
    let profile_secrets: StoredProfileSecretsV1 =
        serde_json::from_slice(&plaintext).map_err(map_serde_error_to_storage)?;

    Ok(StoredProfileRoot::Open(profile_secrets))
}


fn password_protection_summary(
    profile_id: LocalProfileId,
    enabled: bool,
) -> PasswordProtectionSummary {
    PasswordProtectionSummary {
        profile_id,
        enabled,
        algorithm: "argon2id".to_string(),
        memory_kib: PASSWORD_ARGON2_MEMORY_KIB,
        iterations: PASSWORD_ARGON2_ITERATIONS,
        parallelism: PASSWORD_ARGON2_PARALLELISM,
        protection_model: "local_unlock_gate".to_string(),
        protection_summary:
            "Protects against casual local access and UI reopening, but not malware or another process running as the same Windows user."
                .to_string(),
    }
}
