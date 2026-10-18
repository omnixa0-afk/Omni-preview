fn identity_get_current_account_export_manifest_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle)
            .and_then(|handle| handle.inner.get_current_account_export_manifest()),
    )
}

fn identity_export_current_database_backup_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ExportCurrentDatabaseBackupRequest =
            parse_json_request(request_json, "export_current_database_backup request")?;
        require_handle(handle)?
            .inner
            .export_current_database_backup(request.destination_path)
    })();

    to_result_payload(result)
}

fn identity_export_current_database_backup_bundle_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ExportCurrentDatabaseBackupRequest =
            parse_json_request(request_json, "export_current_database_backup_bundle request")?;
        require_handle(handle)?
            .inner
            .export_current_database_backup_bundle(request.destination_path)
    })();

    to_result_payload(result)
}

fn identity_inspect_account_backup_manifest_impl(request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: InspectAccountBackupManifestRequest =
            parse_json_request(request_json, "inspect_account_backup_manifest request")?;
        OmniApp::inspect_account_backup_manifest(request.backup_db_path, request.mnemonic)
    })();

    to_result_payload(result)
}

fn identity_create_account_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: CreateAccountRequest =
            parse_json_request(request_json, "create_account request")?;
        require_handle(handle)?
            .inner
            .create_account_response(request.display_name, request.device_label)
    })();

    to_result_payload(result)
}

fn identity_restore_account_from_export_manifest_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RestoreAccountFromExportManifestRequest =
            parse_json_request(request_json, "restore_account_from_export_manifest request")?;
        require_handle(handle)?
            .inner
            .restore_account_from_export_manifest_sanitized(&request.manifest, request.mnemonic)
    })();

    to_result_payload(result)
}

fn identity_restore_account_from_input_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RestoreAccountFromInputRequest =
            parse_json_request(request_json, "restore_account_from_input request")?;
        require_handle(handle)?
            .inner
            .restore_account_from_mnemonic_sanitized(
                request.display_name,
                request.mnemonic,
                request.device_label,
            )
    })();

    to_result_payload(result)
}

fn identity_inspect_recovery_input_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: InspectRecoveryInputRequest =
            parse_json_request(request_json, "inspect_recovery_input request")?;
        require_handle(handle)?.inner.inspect_recovery_input(
            request.mnemonic,
            request.display_name.as_deref(),
            request.device_label.as_deref(),
        )
    })();

    to_result_payload(result)
}

fn identity_get_local_account_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_local_account_sanitized()),
    )
}

fn identity_update_local_public_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: UpdateLocalPublicProfileRequest =
            parse_json_request(request_json, "update_local_public_profile request")?;
        require_handle(handle)?.inner.update_local_public_profile_sanitized(
            request.display_name.as_str(),
            request.avatar_source_path.as_deref().map(std::path::Path::new),
            request.clear_avatar,
        )
    })();

    to_result_payload(result)
}

fn identity_get_local_device_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.get_local_device()))
}

fn identity_get_local_prekey_bundle_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_local_prekey_bundle()),
    )
}

fn identity_store_remote_prekey_bundle_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: PrekeyBundleV1 =
            parse_json_request(request_json, "store_remote_prekey_bundle request")?;
        require_handle(handle)?
            .inner
            .store_remote_prekey_bundle(&request)
    })();

    to_result_payload(result)
}

fn identity_get_active_profile_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.get_active_profile_id()))
}

fn identity_select_active_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "select_active_profile request")?;
        require_handle(handle)?
            .inner
            .select_active_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_reopen_local_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "reopen_local_profile request")?;
        require_handle(handle)?.inner.reopen_local_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_clear_active_profile_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.clear_active_profile()))
}

fn identity_list_local_profiles_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.list_local_profiles()))
}

fn identity_get_local_profile_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "get_local_profile_summary request")?;
        require_handle(handle)?
            .inner
            .get_local_profile_summary(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_get_profile_gate_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "get_profile_gate_summary request")?;
        require_handle(handle)?
            .inner
            .get_profile_gate_summary(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_get_current_profile_gate_summary_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_current_profile_gate_summary()),
    )
}

fn identity_get_profile_lifecycle_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "get_profile_lifecycle_summary request")?;
        require_handle(handle)?
            .inner
            .get_profile_lifecycle_summary(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_get_current_profile_lifecycle_summary_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle)
            .and_then(|handle| handle.inner.get_current_profile_lifecycle_summary()),
    )
}

fn identity_get_lifecycle_fast_snapshot_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_lifecycle_fast_snapshot()),
    )
}

fn identity_sign_out_profile_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "sign_out_profile request")?;
        require_handle(handle)?
            .inner
            .sign_out_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_prepare_sign_out_current_profile_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.prepare_sign_out_current_profile()),
    )
}

fn identity_confirm_sign_out_current_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ConfirmCurrentProfileActionRequest =
            parse_json_request(request_json, "confirm_sign_out_current_profile request")?;
        require_handle(handle)?
            .inner
            .confirm_sign_out_current_profile(request.confirmation_token.as_deref())
    })();

    to_result_payload(result)
}

fn identity_confirm_sign_out_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ConfirmSignOutProfileRequest =
            parse_json_request(request_json, "confirm_sign_out_profile request")?;
        require_handle(handle)?
            .inner
            .confirm_sign_out_profile(&request.profile_id, request.confirmation_token.as_deref())
    })();

    to_result_payload(result)
}

fn identity_unlock_profile_with_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: UnlockProfileWithPasswordRequest =
            parse_json_request(request_json, "unlock_profile_with_password request")?;
        require_handle(handle)?.inner.unlock_profile_with_password(
            &request.profile_id,
            request.password,
        )
    })();

    to_result_payload(result)
}

fn identity_unlock_current_profile_with_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: UnlockCurrentProfileWithPasswordRequest =
            parse_json_request(request_json, "unlock_current_profile_with_password request")?;
        require_handle(handle)?
            .inner
            .unlock_current_profile_with_password(request.password)
    })();

    to_result_payload(result)
}

fn identity_reveal_current_profile_recovery_phrase_with_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RevealCurrentProfileRecoveryPhraseWithPasswordRequest = parse_json_request(
            request_json,
            "reveal_current_profile_recovery_phrase_with_password request",
        )?;
        require_handle(handle)?
            .inner
            .reveal_current_profile_recovery_phrase_with_password(request.password)
    })();

    to_result_payload(result)
}

fn identity_unlock_profile_with_mnemonic_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: UnlockProfileWithMnemonicRequest =
            parse_json_request(request_json, "unlock_profile_with_mnemonic request")?;
        require_handle(handle)?.inner.unlock_profile_with_mnemonic(
            &request.profile_id,
            request.mnemonic,
        )
    })();

    to_result_payload(result)
}

fn identity_unlock_current_profile_with_mnemonic_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: UnlockCurrentProfileWithMnemonicRequest =
            parse_json_request(request_json, "unlock_current_profile_with_mnemonic request")?;
        require_handle(handle)?
            .inner
            .unlock_current_profile_with_mnemonic(request.mnemonic)
    })();

    to_result_payload(result)
}

fn identity_set_local_profile_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetLocalProfilePasswordRequest =
            parse_json_request(request_json, "set_local_profile_password request")?;
        require_handle(handle)?
            .inner
            .set_local_profile_password(&request.profile_id, request.password)
    })();

    to_result_payload(result)
}

fn identity_change_local_profile_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ChangeLocalProfilePasswordRequest =
            parse_json_request(request_json, "change_local_profile_password request")?;
        require_handle(handle)?.inner.change_local_profile_password(
            &request.profile_id,
            request.current_password,
            request.new_password,
        )
    })();

    to_result_payload(result)
}

fn identity_remove_local_profile_password_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RemoveLocalProfilePasswordRequest =
            parse_json_request(request_json, "remove_local_profile_password request")?;
        require_handle(handle)?.inner.remove_local_profile_password(
            &request.profile_id,
            request.current_password,
        )
    })();

    to_result_payload(result)
}

fn identity_get_password_protection_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "get_password_protection_summary request")?;
        require_handle(handle)?
            .inner
            .get_password_protection_summary(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_get_local_profile_footprint_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "get_local_profile_footprint_summary request")?;
        require_handle(handle)?
            .inner
            .get_local_profile_footprint_summary(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_preview_sign_out_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "preview_sign_out_profile request")?;
        require_handle(handle)?
            .inner
            .preview_sign_out_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_preview_delete_local_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "preview_delete_local_profile request")?;
        require_handle(handle)?
            .inner
            .preview_delete_local_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_prepare_delete_current_profile_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.prepare_delete_current_profile()),
    )
}

fn identity_preview_revoke_current_device_and_wipe_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "preview_revoke_current_device_and_wipe request")?;
        require_handle(handle)?
            .inner
            .preview_revoke_current_device_and_wipe(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_delete_local_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: LocalProfileRequest =
            parse_json_request(request_json, "delete_local_profile request")?;
        require_handle(handle)?
            .inner
            .delete_local_profile(&request.profile_id)
    })();

    to_result_payload(result)
}

fn identity_confirm_delete_local_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ConfirmDeleteLocalProfileRequest =
            parse_json_request(request_json, "confirm_delete_local_profile request")?;
        require_handle(handle)?.inner.confirm_delete_local_profile(
            &request.profile_id,
            request.confirmation_token.as_deref(),
        )
    })();

    to_result_payload(result)
}

fn identity_confirm_delete_current_profile_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ConfirmCurrentProfileActionRequest =
            parse_json_request(request_json, "confirm_delete_current_profile request")?;
        require_handle(handle)?
            .inner
            .confirm_delete_current_profile(request.confirmation_token.as_deref())
    })();

    to_result_payload(result)
}

fn contacts_list_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.list_contacts()))
}

fn contacts_get_refresh_summary_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_contacts_refresh_summary()),
    )
}

fn contacts_get_screen_snapshot_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_contacts_screen_snapshot()),
    )
}

fn contacts_add_by_identifier_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: AddContactByIdentifierRequest =
            parse_json_request(request_json, "add_contact_by_identifier request")?;
        require_handle(handle)?.inner.add_contact_by_identifier(
            request.account_id,
            request.petname,
            request.public_display_name,
        )
    })();

    to_result_payload(result)
}

fn contacts_request_by_identifier_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RequestContactByIdentifierRequest =
            parse_json_request(request_json, "request_contact_by_identifier request")?;
        require_handle(handle)?.inner.request_contact_by_identifier(
            request.account_id,
            request.petname,
            request.public_display_name,
        )
    })();

    to_result_payload(result)
}

fn contacts_add_from_payload_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: AddContactFromPayloadRequest =
            parse_json_request(request_json, "add_contact_from_payload request")?;
        require_handle(handle)?
            .inner
            .add_contact_from_payload(&request.payload, request.petname)
    })();

    to_result_payload(result)
}

fn contacts_rename_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RenameContactRequest =
            parse_json_request(request_json, "rename_contact request")?;
        require_handle(handle)?
            .inner
            .rename_contact(&request.contact_id, request.petname)
    })();

    to_result_payload(result)
}

fn contacts_block_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest = parse_json_request(request_json, "block_contact request")?;
        require_handle(handle)?
            .inner
            .block_contact(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_approve_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest = parse_json_request(request_json, "approve_contact request")?;
        require_handle(handle)?
            .inner
            .approve_contact(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_set_friend_relay_consent_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetContactFriendRelayConsentRequest =
            parse_json_request(request_json, "set_contact_friend_relay_consent request")?;
        require_handle(handle)?
            .inner
            .set_contact_friend_relay_consent(&request.contact_id, request.consent)
    })();

    to_result_payload(result)
}

fn contacts_get_remote_access_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest = parse_json_request(request_json, "get_contact_remote_access request")?;
        require_handle(handle)?.inner.get_contact_remote_access(&request.contact_id)
    })();
    to_result_payload(result)
}

fn contacts_list_remote_access_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.list_contact_remote_access()))
}

fn contacts_set_remote_access_granted_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetContactRemoteAccessGrantedRequest =
            parse_json_request(request_json, "set_contact_remote_access_granted request")?;
        require_handle(handle)?
            .inner
            .set_contact_remote_access_granted(&request.contact_id, request.mode)
    })();
    to_result_payload(result)
}

fn account_devices_list_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.list_own_devices()))
}

fn account_devices_acknowledge_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.acknowledge_own_devices()))
}

fn account_move_working_key_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MoveAccountKeyRequest = parse_json_request(request_json, "move_account_key request")?;
        require_handle(handle)?.inner.move_account_to_new_working_key(
            &request.keep_device_ids,
            request.phrase.as_deref(),
            request.password.as_deref(),
        )
    })();
    to_result_payload(result)
}

fn account_devices_hide_revoked_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: HideRevokedOwnDevicesRequest =
            parse_json_request(request_json, "hide_revoked_own_devices request")?;
        require_handle(handle)?.inner.hide_revoked_own_devices(request.device_id.as_deref())
    })();
    to_result_payload(result)
}

fn account_device_revoke_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RevokeOwnDeviceRequest = parse_json_request(request_json, "revoke_own_device request")?;
        require_handle(handle)?
            .inner
            .revoke_own_device(&request.device_id, request.password.as_deref())
    })();
    to_result_payload(result)
}

fn contacts_decline_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest = parse_json_request(request_json, "decline_contact request")?;
        require_handle(handle)?
            .inner
            .decline_contact(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_forget_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest = parse_json_request(request_json, "forget_contact request")?;
        require_handle(handle)?
            .inner
            .forget_contact(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_forget_and_reset_peer_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "forget_contact_and_reset_peer request")?;
        require_handle(handle)?
            .inner
            .forget_contact_and_reset_peer(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_get_card_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "get_contact_card request")?;
        require_handle(handle)?
            .inner
            .get_contact_card(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_get_card_revision_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "get_contact_card_revision request")?;
        require_handle(handle)?
            .inner
            .get_contact_card_revision(&request.contact_id)
    })();

    to_result_payload(result)
}

fn identity_get_my_verification_payload_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_my_verification_payload()),
    )
}

fn contacts_get_verification_state_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "get_contact_verification_state request")?;
        require_handle(handle)?
            .inner
            .get_contact_verification_state(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_confirm_verification_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ConfirmContactVerificationRequest =
            parse_json_request(request_json, "confirm_contact_verification request")?;
        require_handle(handle)?.inner.confirm_contact_verification(
            &request.contact_id,
            &request.payload,
            request.method,
        )
    })();

    to_result_payload(result)
}

fn chat_get_header_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_get_header_summary request")?;
        require_handle(handle)?
            .inner
            .get_chat_header_contact_summary(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_get_screen_snapshot_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_get_screen_snapshot request")?;
        require_handle(handle)?
            .inner
            .get_chat_header_screen_snapshot(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_open_session_lease_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_open_session_lease request")?;
        require_handle(handle)?
            .inner
            .open_chat_session_lease(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_mark_read_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest = parse_json_request(request_json, "chat_mark_read request")?;
        require_handle(handle)?.inner.mark_chat_read(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_end_session_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_end_session request")?;
        require_handle(handle)?
            .inner
            .end_chat_session(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_list_thread_projections_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.list_chat_thread_projections()),
    )
}

fn chat_get_thread_list_refresh_summary_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_chat_thread_list_refresh_summary()),
    )
}

fn chat_get_security_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_get_security_summary request")?;
        require_handle(handle)?
            .inner
            .get_chat_security_summary(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_get_message_refresh_summary_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogKeyRequest =
            parse_json_request(request_json, "chat_get_message_refresh_summary request")?;
        require_handle(handle)?
            .inner
            .get_dialog_message_refresh_summary(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_get_message_delta_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DialogDeltaRequest =
            parse_json_request(request_json, "chat_get_message_delta request")?;
        require_handle(handle)?
            .inner
            .get_dialog_message_delta(request.dialog_key, &request.cursor)
    })();

    to_result_payload(result)
}

fn chat_get_message_security_details_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MessageIdRequest =
            parse_json_request(request_json, "chat_get_message_security_details request")?;
        require_handle(handle)?
            .inner
            .get_message_security_details(&request.message_id)
    })();

    to_result_payload(result)
}

fn chat_get_message_security_details_batch_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MessageIdBatchRequest =
            parse_json_request(request_json, "chat_get_message_security_details_batch request")?;
        require_handle(handle)?
            .inner
            .get_message_security_details_batch(&request.message_ids)
    })();

    to_result_payload(result)
}

fn contacts_start_chat_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "contacts_start_chat request")?;
        require_handle(handle)?
            .inner
            .start_chat_from_contact(&request.contact_id)
    })();

    to_result_payload(result)
}

fn contacts_quick_add_from_dialog_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: QuickAddContactFromDialogRequest =
            parse_json_request(request_json, "contacts_quick_add_from_dialog request")?;
        require_handle(handle)?.inner.quick_add_contact_from_dialog(
            request.dialog_key,
            request.petname,
            request.public_display_name,
        )
    })();

    to_result_payload(result)
}

fn contacts_attach_dialog_to_contact_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: AttachDialogToContactRequest =
            parse_json_request(request_json, "attach_dialog_to_contact request")?;
        require_handle(handle)?.inner.attach_dialog_to_contact(
            &request.contact_id,
            request.dialog_key,
        )
    })();

    to_result_payload(result)
}
