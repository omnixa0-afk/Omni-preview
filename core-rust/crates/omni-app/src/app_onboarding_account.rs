#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleFastSnapshot {
    pub active_profile_id: Option<LocalProfileId>,
    pub local_account: Option<SanitizedLocalAccountSnapshot>,
    pub local_device: Option<LocalDeviceSnapshot>,
    pub gate_summary: Option<ProfileGateSummary>,
    pub lifecycle_summary: Option<ProfileLifecycleSummary>,
}

impl OmniApp {
    pub fn inspect_recovery_input(
        &self,
        mnemonic: impl AsRef<str>,
        display_name: Option<&str>,
        device_label: Option<&str>,
    ) -> OmniResult<RecoveryInputSummary> {
        self.storage
            .inspect_recovery_input(mnemonic.as_ref(), display_name, device_label)
    }

    pub fn create_account(
        &self,
        display_name: impl Into<String>,
        device_label: impl Into<String>,
    ) -> OmniResult<LocalAccountSnapshot> {
        let snapshot = self.storage.create_local_account(
            &display_name.into(),
            &device_label.into(),
            current_unix_ms(),
        )?;
        self.storage
            .ensure_local_crypto_foundation(current_unix_ms())?;
        Ok(snapshot)
    }

    pub fn create_account_response(
        &self,
        display_name: impl Into<String>,
        device_label: impl Into<String>,
    ) -> OmniResult<CreateAccountResponse> {
        let snapshot = self.create_account(display_name, device_label)?;
        // A new account always has its phrase: it was just made from the root secret.
        let initial_recovery_phrase_v2 = snapshot.recovery_phrase_v2.clone().ok_or_else(|| {
            OmniError::Internal("a new account came back without its recovery phrase".to_string())
        })?;
        Ok(CreateAccountResponse {
            account: sanitize_local_account_snapshot(snapshot),
            initial_recovery_phrase_v2,
        })
    }

    pub fn get_current_account_export_manifest(
        &self,
    ) -> OmniResult<Option<AccountExportManifestV1>> {
        self.storage.get_current_account_export_manifest()
    }

    pub fn restore_account_from_export_manifest(
        &self,
        manifest: &AccountExportManifestV1,
        mnemonic: impl AsRef<str>,
    ) -> OmniResult<LocalAccountSnapshot> {
        let snapshot = self.storage.restore_local_account_from_export_manifest(
            manifest,
            mnemonic.as_ref(),
            current_unix_ms(),
        )?;
        self.storage
            .ensure_local_crypto_foundation(current_unix_ms())?;
        Ok(snapshot)
    }

    pub fn restore_account_from_export_manifest_sanitized(
        &self,
        manifest: &AccountExportManifestV1,
        mnemonic: impl AsRef<str>,
    ) -> OmniResult<SanitizedLocalAccountSnapshot> {
        self.restore_account_from_export_manifest(manifest, mnemonic)
            .map(sanitize_local_account_snapshot)
    }

    pub fn restore_account_from_mnemonic(
        &self,
        display_name: impl AsRef<str>,
        mnemonic: impl AsRef<str>,
        device_label: impl Into<String>,
    ) -> OmniResult<LocalAccountSnapshot> {
        let device_label = device_label.into();
        let snapshot = self.storage.restore_local_account_from_mnemonic(
            display_name.as_ref(),
            mnemonic.as_ref(),
            &device_label,
            current_unix_ms(),
        ).map_err(|error| self.map_restore_mnemonic_error(error))?;
        self.storage
            .ensure_local_crypto_foundation(current_unix_ms())?;
        // With no other device of the account left, the profile, contacts and groups come back
        // from the holders of the account passport (docs/decisions/account-recovery-from-friends.md).
        start_account_recovery(&self.storage, current_unix_ms());
        Ok(snapshot)
    }

    pub fn restore_account_from_mnemonic_sanitized(
        &self,
        display_name: impl AsRef<str>,
        mnemonic: impl AsRef<str>,
        device_label: impl Into<String>,
    ) -> OmniResult<SanitizedLocalAccountSnapshot> {
        self.restore_account_from_mnemonic(display_name, mnemonic, device_label)
            .map(sanitize_local_account_snapshot)
    }

    pub fn reopen_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<LocalProfileSummary> {
        let Some(profile) = self.storage.get_local_profile_summary(profile_id)? else {
            return Err(OmniError::InvalidInput("profile was not found".to_string()));
        };
        match profile.state {
            omni_identity::LocalProfileState::SignedIn => {}
            omni_identity::LocalProfileState::SignedOutLocked => {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileLocked,
                    "locked local profile must be unlocked before it can be reopened",
                )
                .into_omni_error());
            }
            omni_identity::LocalProfileState::Deleting => {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleting,
                    "local profile is deleting and cannot be reopened right now",
                )
                .into_omni_error());
            }
            omni_identity::LocalProfileState::Deleted => {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "local profile was removed from this device",
                )
                .into_omni_error());
            }
        }
        let reopened = self.storage.reopen_local_profile(profile_id)?;
        self.current_active_profile_context_required()?;
        self.sync_transport_local_identity()?;
        Ok(reopened)
    }

    pub fn get_local_account(&self) -> OmniResult<Option<LocalAccountSnapshot>> {
        self.storage.get_local_account()
    }

    pub fn get_local_account_sanitized(&self) -> OmniResult<Option<SanitizedLocalAccountSnapshot>> {
        self.get_local_account()
            .map(|snapshot| snapshot.map(sanitize_local_account_snapshot))
    }

    pub fn update_local_public_profile(
        &self,
        display_name: impl AsRef<str>,
        avatar_source_path: Option<&Path>,
        clear_avatar: bool,
    ) -> OmniResult<LocalAccountSnapshot> {
        let _ = self.ensure_runtime_profile_active("update_local_public_profile")?;
        let snapshot = self.storage.update_local_public_profile(
            display_name.as_ref(),
            avatar_source_path,
            clear_avatar,
            current_unix_ms(),
        )?;
        self.sync_transport_local_identity()?;
        if let Some(transport) = clone_active_transport(&self.transport_handle)? {
            // The sync builds encrypted frames, so it takes the session lock like every other sender.
            let _session_guard = lock_app_session_guard(&self.session_lock)?;
            let _ = send_current_public_profile_sync_to_approved_contacts(&self.storage, &transport);
            sync_profile_with_own_devices(&self.storage, &transport);
        }
        // The members of our groups who are not our contacts learn it from our card.
        crate::app_group_member_profiles::broadcast_own_profile(&self.storage);
        self.flush_group_feed();
        Ok(snapshot)
    }

    pub fn update_local_public_profile_sanitized(
        &self,
        display_name: impl AsRef<str>,
        avatar_source_path: Option<&Path>,
        clear_avatar: bool,
    ) -> OmniResult<SanitizedLocalAccountSnapshot> {
        self.update_local_public_profile(display_name, avatar_source_path, clear_avatar)
            .map(sanitize_local_account_snapshot)
    }

    pub fn get_local_device(&self) -> OmniResult<Option<LocalDeviceSnapshot>> {
        self.storage.get_local_device()
    }

    pub fn get_active_profile_id(&self) -> OmniResult<Option<LocalProfileId>> {
        self.storage.get_active_profile_id()
    }

    pub fn select_active_profile(&self, profile_id: &LocalProfileId) -> OmniResult<()> {
        self.storage.set_active_profile_id(Some(profile_id))?;
        self.sync_transport_local_identity()
    }

    pub fn clear_active_profile(&self) -> OmniResult<()> {
        self.storage.set_active_profile_id(None)?;
        self.clear_transport_local_identity();
        Ok(())
    }

    pub fn list_local_profiles(&self) -> OmniResult<SavedProfileList> {
        self.storage.list_local_profiles()
    }

    pub fn get_local_profile_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalProfileSummary>> {
        self.storage.get_local_profile_summary(profile_id)
    }

    pub fn get_profile_gate_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileGateSummary>> {
        self.profile_lifecycle_coordinator()
            .profile_gate_summary(profile_id)
    }

    pub fn get_current_profile_gate_summary(&self) -> OmniResult<Option<ProfileGateSummary>> {
        let profile_id = match self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
        {
            Ok(value) => value,
            Err(error) => {
                if error.code == ProfileLifecycleErrorCode::RuntimeContextUnavailable {
                    return Ok(None);
                }
                if error.code == ProfileLifecycleErrorCode::NoActiveProfile
                    || error.code == ProfileLifecycleErrorCode::ActiveProfileSwitchRequired
                {
                    return Ok(None);
                }
                return Err(ProfileLifecycleDomainError::into_omni_error(error));
            }
        };
        self.profile_lifecycle_coordinator()
            .profile_gate_summary(&profile_id)
    }

    pub fn get_profile_lifecycle_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileLifecycleSummary>> {
        self.profile_lifecycle_coordinator()
            .profile_lifecycle_summary(profile_id)
    }

    pub fn get_current_profile_lifecycle_summary(
        &self,
    ) -> OmniResult<Option<ProfileLifecycleSummary>> {
        let profile_id = match self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
        {
            Ok(value) => value,
            Err(error) => {
                if error.code == ProfileLifecycleErrorCode::RuntimeContextUnavailable {
                    return Ok(None);
                }
                if error.code == ProfileLifecycleErrorCode::NoActiveProfile
                    || error.code == ProfileLifecycleErrorCode::ActiveProfileSwitchRequired
                {
                    return Ok(None);
                }
                return Err(ProfileLifecycleDomainError::into_omni_error(error));
            }
        };
        self.profile_lifecycle_coordinator()
            .profile_lifecycle_summary(&profile_id)
    }

    pub fn get_lifecycle_fast_snapshot(&self) -> OmniResult<LifecycleFastSnapshot> {
        Ok(LifecycleFastSnapshot {
            active_profile_id: self.get_active_profile_id()?,
            local_account: self.get_local_account_sanitized()?,
            local_device: self.get_local_device()?,
            gate_summary: self.get_current_profile_gate_summary()?,
            lifecycle_summary: self.get_current_profile_lifecycle_summary()?,
        })
    }

    pub fn sign_out_profile(&self, profile_id: &LocalProfileId) -> OmniResult<LocalProfileSummary> {
        let preview = self
            .profile_lifecycle_coordinator()
            .prepare_sign_out_profile(profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        if preview.as_ref().is_some_and(|value| value.requires_confirmation) {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::DangerousActionConfirmationRequired,
                "use preview_sign_out_profile + confirm_sign_out_profile for explicit sign-out",
            )
            .into_omni_error());
        }

        self.profile_lifecycle_coordinator()
            .confirm_sign_out_profile(profile_id, None)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn prepare_sign_out_current_profile(&self) -> OmniResult<Option<DangerousActionPreview>> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .prepare_sign_out_profile(&profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn confirm_sign_out_current_profile(
        &self,
        confirmation_token: Option<&str>,
    ) -> OmniResult<LocalProfileSummary> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .confirm_sign_out_profile(&profile_id, confirmation_token)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn confirm_sign_out_profile(
        &self,
        profile_id: &LocalProfileId,
        confirmation_token: Option<&str>,
    ) -> OmniResult<LocalProfileSummary> {
        self.profile_lifecycle_coordinator()
            .confirm_sign_out_profile(profile_id, confirmation_token)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn unlock_profile_with_password(
        &self,
        profile_id: &LocalProfileId,
        password: impl AsRef<str>,
    ) -> OmniResult<LocalProfileSummary> {
        self.profile_lifecycle_coordinator()
            .unlock_profile_with_password(profile_id, password.as_ref())
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn unlock_current_profile_with_password(
        &self,
        password: impl AsRef<str>,
    ) -> OmniResult<LocalProfileSummary> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .unlock_profile_with_password(&profile_id, password.as_ref())
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn unlock_profile_with_mnemonic(
        &self,
        profile_id: &LocalProfileId,
        mnemonic: impl AsRef<str>,
    ) -> OmniResult<LocalProfileSummary> {
        self.profile_lifecycle_coordinator()
            .unlock_profile_with_mnemonic(profile_id, mnemonic.as_ref())
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn unlock_current_profile_with_mnemonic(
        &self,
        mnemonic: impl AsRef<str>,
    ) -> OmniResult<LocalProfileSummary> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .unlock_profile_with_mnemonic(&profile_id, mnemonic.as_ref())
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn set_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        password: impl AsRef<str>,
    ) -> OmniResult<PasswordProtectionSummary> {
        self.storage
            .set_local_profile_password(profile_id, password.as_ref())
    }

    pub fn change_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        current_password: impl AsRef<str>,
        new_password: impl AsRef<str>,
    ) -> OmniResult<PasswordProtectionSummary> {
        self.storage.change_local_profile_password(
            profile_id,
            current_password.as_ref(),
            new_password.as_ref(),
        )
    }

    pub fn remove_local_profile_password(
        &self,
        profile_id: &LocalProfileId,
        current_password: impl AsRef<str>,
    ) -> OmniResult<PasswordProtectionSummary> {
        self.storage
            .remove_local_profile_password(profile_id, current_password.as_ref())
    }

    pub fn get_password_protection_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<PasswordProtectionSummary>> {
        self.storage.get_password_protection_summary(profile_id)
    }

    pub fn reveal_current_profile_recovery_phrase_with_password(
        &self,
        password: impl AsRef<str>,
    ) -> OmniResult<RecoveryPhraseV2> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.storage
            .reveal_profile_recovery_phrase_with_password(&profile_id, password.as_ref())
    }

    pub fn get_local_profile_footprint_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<LocalProfileFootprintSummary>> {
        self.storage.get_local_profile_footprint_summary(profile_id)
    }

    pub fn preview_sign_out_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        self.profile_lifecycle_coordinator()
            .prepare_sign_out_profile(profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn preview_delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        self.profile_lifecycle_coordinator()
            .prepare_delete_local_profile(profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    fn map_restore_mnemonic_error(&self, error: OmniError) -> OmniError {
        let message = error.to_string();
        if message.contains("profile already present on this device") {
            return ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::ProfileAlreadyPresent,
                "profile already present on this device",
            )
            .into_omni_error();
        }

        error
    }

    pub fn prepare_delete_current_profile(&self) -> OmniResult<Option<DangerousActionPreview>> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .prepare_delete_local_profile(&profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn confirm_delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
        confirmation_token: Option<&str>,
    ) -> OmniResult<()> {
        self.profile_lifecycle_coordinator()
            .confirm_delete_local_profile(profile_id, confirmation_token)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn confirm_delete_current_profile(
        &self,
        confirmation_token: Option<&str>,
    ) -> OmniResult<()> {
        let profile_id = self
            .profile_lifecycle_coordinator()
            .current_profile_id_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        self.profile_lifecycle_coordinator()
            .confirm_delete_local_profile(&profile_id, confirmation_token)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub fn preview_revoke_current_device_and_wipe(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<DangerousActionPreview>> {
        self.storage.preview_revoke_current_device_and_wipe(profile_id)
    }

    pub fn delete_local_profile(&self, profile_id: &LocalProfileId) -> OmniResult<()> {
        let preview = self
            .profile_lifecycle_coordinator()
            .prepare_delete_local_profile(profile_id)
            .map_err(ProfileLifecycleDomainError::into_omni_error)?;
        if preview.as_ref().is_some_and(|value| value.requires_confirmation) {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::DangerousActionConfirmationRequired,
                "use preview_delete_local_profile + confirm_delete_local_profile for destructive profile removal",
            )
            .into_omni_error());
        }

        self.profile_lifecycle_coordinator()
            .confirm_delete_local_profile(profile_id, None)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }
}

fn sanitize_local_account_snapshot(snapshot: LocalAccountSnapshot) -> SanitizedLocalAccountSnapshot {
    SanitizedLocalAccountSnapshot {
        descriptor: snapshot.descriptor,
        current_device_id: snapshot.current_device_id,
        restored_from_recovery: snapshot.restored_from_recovery,
        account_representation_version: snapshot.account_representation_version,
        public_summary: snapshot.public_summary,
        local_profile_summary: snapshot.local_profile_summary,
        public_avatar_hash_hex: snapshot.public_avatar_hash_hex,
        public_avatar_mime_type: snapshot.public_avatar_mime_type,
        public_avatar_image_path: snapshot.public_avatar_image_path,
        public_profile_updated_at_unix_ms: snapshot.public_profile_updated_at_unix_ms,
        created_at_unix_ms: snapshot.created_at_unix_ms,
        updated_at_unix_ms: snapshot.updated_at_unix_ms,
    }
}
