#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleCommandKind {
    PrepareSignOut,
    ConfirmSignOut,
    UnlockWithPassword,
    UnlockWithMnemonic,
    PrepareDeleteLocalProfile,
    ConfirmDeleteLocalProfile,
}

struct ProfileLifecycleCoordinator<'a> {
    app: &'a OmniApp,
}

#[derive(Debug, Clone)]
struct ActiveProfileRuntimeContext {
    account: LocalAccountSnapshot,
    device: LocalDeviceSnapshot,
}

impl OmniApp {
    fn profile_lifecycle_coordinator(&self) -> ProfileLifecycleCoordinator<'_> {
        ProfileLifecycleCoordinator { app: self }
    }

    fn ensure_runtime_profile_active(
        &self,
        operation_name: &str,
    ) -> OmniResult<ProfileLifecycleSummary> {
        self.profile_lifecycle_coordinator()
            .ensure_runtime_active(operation_name)
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }

    pub(crate) fn current_active_profile_context_required(
        &self,
    ) -> OmniResult<ActiveProfileRuntimeContext> {
        self.profile_lifecycle_coordinator()
            .active_profile_context_required()
            .map_err(ProfileLifecycleDomainError::into_omni_error)
    }
}

impl<'a> ProfileLifecycleCoordinator<'a> {
    fn missing_active_profile_error(
        &self,
    ) -> Result<ProfileLifecycleDomainError, ProfileLifecycleDomainError> {
        let profiles = self.app.storage.list_local_profiles().map_err(|error| {
            ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                error.to_string(),
            )
        })?;
        let (code, message) = if profiles.profiles.is_empty() {
            (
                ProfileLifecycleErrorCode::NoActiveProfile,
                "no active profile is selected",
            )
        } else {
            (
                ProfileLifecycleErrorCode::ActiveProfileSwitchRequired,
                "select an active profile to continue",
            )
        };
        Ok(ProfileLifecycleDomainError::new(code, message))
    }

    fn current_profile_summary(
        &self,
    ) -> Result<Option<LocalProfileSummary>, ProfileLifecycleDomainError> {
        let active_profile_id = self.app.storage.get_active_profile_id().map_err(|error| {
            ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                error.to_string(),
            )
        })?;
        if let Some(profile_id) = active_profile_id {
            let profile = self
                .app
                .storage
                .get_local_profile_summary(&profile_id)
                .map_err(|error| {
                    ProfileLifecycleDomainError::new(
                        ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                        error.to_string(),
                    )
                })?
                .ok_or_else(|| {
                    ProfileLifecycleDomainError::new(
                        ProfileLifecycleErrorCode::ProfileDeleted,
                        "active profile id does not resolve to a saved profile",
                    )
                })?;
            return Ok(Some(profile));
        }
        let profiles = self.app.storage.list_local_profiles().map_err(|error| {
            ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                error.to_string(),
            )
        })?;
        if profiles.profiles.is_empty() {
            let deleted = self.app.storage.was_local_profile_deleted().map_err(|error| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                    error.to_string(),
                )
            })?;
            if deleted {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "active profile context is unavailable after local profile deletion",
                ));
            }
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::NoActiveProfile,
                "no active profile is selected",
            ));
        }
        Err(ProfileLifecycleDomainError::new(
            ProfileLifecycleErrorCode::ActiveProfileSwitchRequired,
            "select an active profile to continue",
        ))
    }

    fn current_profile_summary_required(
        &self,
    ) -> Result<LocalProfileSummary, ProfileLifecycleDomainError> {
        match self.current_profile_summary()? {
            Some(profile) => Ok(profile),
            None => Err(self.missing_active_profile_error()?),
        }
    }

    fn active_profile_context_required(
        &self,
    ) -> Result<ActiveProfileRuntimeContext, ProfileLifecycleDomainError> {
        let profile = self.current_profile_summary_required()?;
        let account = self
            .app
            .storage
            .get_local_account_by_profile_id(&profile.profile_id)
            .map_err(|error| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                    error.to_string(),
                )
            })?
            .ok_or_else(|| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "active profile is no longer available on this device",
                )
            })?;
        let device = self
            .app
            .storage
            .get_local_device_by_profile_id(&profile.profile_id)
            .map_err(|error| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                    error.to_string(),
                )
            })?
            .ok_or_else(|| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                    "active profile does not have a current local device",
                )
            })?;

        Ok(ActiveProfileRuntimeContext {
            account,
            device,
        })
    }

    fn ensure_runtime_active(
        &self,
        operation_name: &str,
    ) -> Result<ProfileLifecycleSummary, ProfileLifecycleDomainError> {
        let profile = self.current_profile_summary_required()?;
        let summary = omni_identity::build_profile_lifecycle_summary(&profile);
        match summary.state {
            LocalProfileState::SignedIn => Ok(summary),
            LocalProfileState::SignedOutLocked => Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::ProfileLocked,
                format!("operation '{operation_name}' is blocked while profile is locked"),
            )),
            LocalProfileState::Deleting => Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::ProfileDeleting,
                format!("operation '{operation_name}' is blocked while profile is deleting"),
            )),
            LocalProfileState::Deleted => Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::ProfileDeleted,
                format!("operation '{operation_name}' is blocked because profile is deleted"),
            )),
        }
    }

    fn get_profile_summary_required(
        &self,
        profile_id: &LocalProfileId,
    ) -> Result<LocalProfileSummary, ProfileLifecycleDomainError> {
        let profile = self
            .app
            .storage
            .get_local_profile_summary(profile_id)
            .map_err(|error| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                    error.to_string(),
                )
            })?;
        profile.ok_or_else(|| {
            ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::ProfileDeleted,
                "profile was not found on this device",
            )
        })
    }

    fn current_profile_id_required(&self) -> Result<LocalProfileId, ProfileLifecycleDomainError> {
        let profile = self.current_profile_summary_required()?;
        Ok(profile.profile_id)
    }

    fn validate_transition(
        &self,
        current_state: LocalProfileState,
        next_state: LocalProfileState,
    ) -> Result<(), ProfileLifecycleDomainError> {
        let allowed = matches!(
            (current_state, next_state),
            (LocalProfileState::SignedIn, LocalProfileState::SignedOutLocked)
                | (LocalProfileState::SignedOutLocked, LocalProfileState::SignedIn)
                | (LocalProfileState::SignedIn, LocalProfileState::Deleting)
                | (LocalProfileState::SignedOutLocked, LocalProfileState::Deleting)
                | (LocalProfileState::Deleting, LocalProfileState::Deleted)
        );
        if allowed {
            return Ok(());
        }

        Err(ProfileLifecycleDomainError::new(
            ProfileLifecycleErrorCode::LifecycleTransitionNotAllowed,
            format!(
                "transition is not allowed: {current_state:?} -> {next_state:?}"
            ),
        ))
    }

    fn validate_command_preconditions(
        &self,
        state: LocalProfileState,
        command: LifecycleCommandKind,
    ) -> Result<(), ProfileLifecycleDomainError> {
        match state {
            LocalProfileState::Deleted => {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "profile is already deleted",
                ));
            }
            LocalProfileState::Deleting => {
                return Err(ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleting,
                    "profile is currently deleting",
                ));
            }
            _ => {}
        }

        match command {
            LifecycleCommandKind::PrepareSignOut => {
                if state == LocalProfileState::SignedOutLocked {
                    return Err(ProfileLifecycleDomainError::new(
                        ProfileLifecycleErrorCode::ProfileLocked,
                        "profile is already locked",
                    ));
                }
                self.validate_transition(state, LocalProfileState::SignedOutLocked)
            }
            LifecycleCommandKind::ConfirmSignOut => {
                if state == LocalProfileState::SignedOutLocked {
                    return Err(ProfileLifecycleDomainError::new(
                        ProfileLifecycleErrorCode::ProfileLocked,
                        "profile is already locked",
                    ));
                }
                self.validate_transition(state, LocalProfileState::SignedOutLocked)
            }
            LifecycleCommandKind::UnlockWithPassword
            | LifecycleCommandKind::UnlockWithMnemonic => {
                if state == LocalProfileState::SignedIn {
                    return Err(ProfileLifecycleDomainError::new(
                        ProfileLifecycleErrorCode::LifecycleTransitionNotAllowed,
                        "profile is already unlocked",
                    ));
                }
                self.validate_transition(state, LocalProfileState::SignedIn)
            }
            LifecycleCommandKind::PrepareDeleteLocalProfile
            | LifecycleCommandKind::ConfirmDeleteLocalProfile => {
                self.validate_transition(state, LocalProfileState::Deleting)
            }
        }
    }

    fn stop_runtime_context(&self) -> Result<(), ProfileLifecycleDomainError> {
        self.app.clear_transport_local_identity();
        let _ = clear_runtime_chat_sessions_for_active_profile(&self.app.storage);
        let db_path = self.app.storage.db_path().to_string_lossy().to_string();
        clear_session_reachability_for_profile(&db_path);
        clear_outgoing_envelopes_for_profile(&db_path);
        clear_session_resync_for_profile(&db_path);
        clear_call_runtime_for_profile(&db_path);
        crate::app_group_calls::clear_group_calls_for_profile(&db_path);
        crate::app_group_attachments::clear_group_attachment_pulls_for_db(&db_path);
        crate::clear_own_device_not_here_for_db(&db_path);
        crate::app_attachment_preparation::stop_attachment_preparations_for_profile(&db_path);
        crate::app_screen_control::clear_screen_control_for_profile(&db_path);
        crate::app_nat_class::clear_nat_class_for_profile(&db_path);
        crate::app_peer_door::clear_peer_doors_for_profile(&db_path);
        clear_realtime_media_negotiation_for_profile(&db_path);
        clear_verification_challenges_for_profile(&db_path);
        clear_public_profile_syncs_for_profile(&db_path);
        clear_attachment_buffers_for_profile(&db_path);
        clear_pending_contact_rate_limits_for_profile(&db_path);
        clear_core_events_for_profile(&db_path);
        clear_rendezvous_runtime_for_profile(&db_path);
        crate::app_name_discovery::clear_name_discovery_for_profile(&db_path);
        crate::app_relay_directory::clear_relay_directory_for_profile(&db_path);
        crate::app_pinned_relay::clear_pinned_relay_for_profile(&db_path);
        clear_account_mailbox_for_profile(&db_path);
        clear_account_passport_state_for_profile(&db_path);
        clear_history_sync_state_for_profile(&db_path);
        clear_revocation_notices_for_profile(&db_path);
        clear_connection_traces(&db_path);
        Ok(())
    }

    fn map_unlock_password_error(&self, error: OmniError) -> ProfileLifecycleDomainError {
        let message = error.to_string();
        if message.contains("password does not unlock")
            || message.contains("password does not unlock profile master key")
        {
            return ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::UnlockFailedInvalidPassword,
                "password does not unlock this profile",
            );
        }

        ProfileLifecycleDomainError::new(
            ProfileLifecycleErrorCode::RuntimeContextUnavailable,
            message,
        )
    }

    fn map_unlock_mnemonic_error(&self, error: OmniError) -> ProfileLifecycleDomainError {
        match error {
            OmniError::InvalidInput(_) => ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::UnlockFailedInvalidMnemonic,
                "mnemonic phrase does not unlock this profile",
            ),
            other => ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                other.to_string(),
            ),
        }
    }

    fn map_storage_error(&self, error: OmniError) -> ProfileLifecycleDomainError {
        ProfileLifecycleDomainError::new(
            ProfileLifecycleErrorCode::RuntimeContextUnavailable,
            error.to_string(),
        )
    }

    fn ensure_confirmation_token(
        &self,
        preview: &DangerousActionPreview,
        confirmation_token: Option<&str>,
    ) -> Result<(), ProfileLifecycleDomainError> {
        if !preview.requires_confirmation {
            return Ok(());
        }

        let Some(expected) = preview.confirmation_token.as_deref() else {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::DangerousActionConfirmationRequired,
                "dangerous action confirmation token is required",
            ));
        };
        let Some(actual) = confirmation_token.map(|value| value.trim()).filter(|value| !value.is_empty()) else {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::DangerousActionConfirmationRequired,
                format!("confirmation token '{expected}' is required"),
            ));
        };
        if actual != expected {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::DangerousActionConfirmationRequired,
                format!("invalid confirmation token, expected '{expected}'"),
            ));
        }

        Ok(())
    }

    fn profile_gate_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileGateSummary>> {
        let profile = self.app.storage.get_local_profile_summary(profile_id)?;
        Ok(profile.map(|value| {
            omni_identity::build_profile_gate_summary(&value.profile_id, value.state)
        }))
    }

    fn profile_lifecycle_summary(
        &self,
        profile_id: &LocalProfileId,
    ) -> OmniResult<Option<ProfileLifecycleSummary>> {
        let profile = self.app.storage.get_local_profile_summary(profile_id)?;
        Ok(profile
            .as_ref()
            .map(omni_identity::build_profile_lifecycle_summary))
    }

    fn prepare_sign_out_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> Result<Option<DangerousActionPreview>, ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(profile.state, LifecycleCommandKind::PrepareSignOut)?;
        self.app
            .storage
            .preview_sign_out_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))
    }

    fn confirm_sign_out_profile(
        &self,
        profile_id: &LocalProfileId,
        confirmation_token: Option<&str>,
    ) -> Result<LocalProfileSummary, ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(profile.state, LifecycleCommandKind::ConfirmSignOut)?;
        let preview = self
            .app
            .storage
            .preview_sign_out_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))?
            .ok_or_else(|| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "profile was not found before sign-out confirmation",
                )
            })?;
        self.ensure_confirmation_token(&preview, confirmation_token)?;
        let active_profile_id = self
            .app
            .storage
            .get_active_profile_id()
            .map_err(|error| self.map_storage_error(error))?;
        if active_profile_id.as_ref() == Some(profile_id) {
            self.stop_runtime_context()?;
        }
        self.app
            .storage
            .sign_out_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))
    }

    fn unlock_profile_with_password(
        &self,
        profile_id: &LocalProfileId,
        password: &str,
    ) -> Result<LocalProfileSummary, ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(profile.state, LifecycleCommandKind::UnlockWithPassword)?;
        let summary = self
            .app
            .storage
            .unlock_profile_with_password(profile_id, password)
            .map_err(|error| self.map_unlock_password_error(error))?;
        self.app
            .current_active_profile_context_required()
            .map_err(|error| self.map_storage_error(error))?;
        self.app
            .sync_transport_local_identity()
            .map_err(|error| self.map_storage_error(error))?;
        Ok(summary)
    }

    fn unlock_profile_with_mnemonic(
        &self,
        profile_id: &LocalProfileId,
        mnemonic: &str,
    ) -> Result<LocalProfileSummary, ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(profile.state, LifecycleCommandKind::UnlockWithMnemonic)?;
        let summary = self
            .app
            .storage
            .unlock_profile_with_mnemonic(profile_id, mnemonic)
            .map_err(|error| self.map_unlock_mnemonic_error(error))?;
        self.app
            .current_active_profile_context_required()
            .map_err(|error| self.map_storage_error(error))?;
        self.app
            .sync_transport_local_identity()
            .map_err(|error| self.map_storage_error(error))?;
        Ok(summary)
    }

    fn prepare_delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
    ) -> Result<Option<DangerousActionPreview>, ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(
            profile.state,
            LifecycleCommandKind::PrepareDeleteLocalProfile,
        )?;
        self.app
            .storage
            .preview_delete_local_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))
    }

    fn confirm_delete_local_profile(
        &self,
        profile_id: &LocalProfileId,
        confirmation_token: Option<&str>,
    ) -> Result<(), ProfileLifecycleDomainError> {
        let profile = self.get_profile_summary_required(profile_id)?;
        self.validate_command_preconditions(
            profile.state,
            LifecycleCommandKind::ConfirmDeleteLocalProfile,
        )?;
        let preview = self
            .app
            .storage
            .preview_delete_local_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))?
            .ok_or_else(|| {
                ProfileLifecycleDomainError::new(
                    ProfileLifecycleErrorCode::ProfileDeleted,
                    "profile was not found before delete confirmation",
                )
            })?;
        self.ensure_confirmation_token(&preview, confirmation_token)?;
        let active_profile_id = self
            .app
            .storage
            .get_active_profile_id()
            .map_err(|error| self.map_storage_error(error))?;
        if active_profile_id.as_ref() == Some(profile_id) {
            // While the profile is still active (the MLS engine needs its identity): forget its groups.
            crate::app_groups::forget_groups_mls(&self.app.storage);
            self.stop_runtime_context()?;
        }
        self.app
            .storage
            .delete_local_profile(profile_id)
            .map_err(|error| self.map_storage_error(error))?;
        if active_profile_id.as_ref() == Some(profile_id) {
            self.app
                .storage
                .set_active_profile_id(None)
                .map_err(|error| self.map_storage_error(error))?;
        }
        if self
            .app
            .storage
            .get_local_profile_summary(profile_id)
            .map_err(|error| self.map_storage_error(error))?
            .is_some()
        {
            return Err(ProfileLifecycleDomainError::new(
                ProfileLifecycleErrorCode::RuntimeContextUnavailable,
                "profile still exists after delete confirmation",
            ));
        }
        self.validate_transition(LocalProfileState::Deleting, LocalProfileState::Deleted)?;
        self.wipe_device_traces_if_no_profiles_remain()?;
        Ok(())
    }

    /// "Удалить с устройства" deletes the profile's ROWS. That is right while other profiles share
    /// the db — but when the deleted one was the LAST, a pile of device-level files stays behind that
    /// belongs to no profile and that nothing else ever cleans: the mesh transport identity, the DHT
    /// peer cache, the settings sidecars, attachments. The identity is the one that matters — without
    /// this, the next profile the user creates comes up as the very same mesh node the peers already
    /// know, so "delete" would not actually break the link to the deleted identity.
    ///
    /// The db file itself stays (it is open, and the app still needs it to offer onboarding) — by now
    /// an empty container. Best-effort by design: a locked file must not turn a completed delete into
    /// an error the user can only read as "delete failed".
    fn wipe_device_traces_if_no_profiles_remain(&self) -> Result<(), ProfileLifecycleDomainError> {
        let remaining = self
            .app
            .storage
            .list_local_profiles()
            .map_err(|error| self.map_storage_error(error))?;
        if !remaining.profiles.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.app.storage.wipe_mls_state() {
            omni_app_trace_log("profile/delete", &format!("MLS state wipe failed: {error}"));
        }
        let db_path = self.app.storage.db_path().to_path_buf();
        if let Err(error) = omni_storage::Storage::delete_device_traces_keeping_open_database(&db_path)
        {
            omni_app_trace_log(
                "profile/delete",
                &format!("device-trace wipe after the last profile left files behind: {error}"),
            );
        }
        Ok(())
    }
}
