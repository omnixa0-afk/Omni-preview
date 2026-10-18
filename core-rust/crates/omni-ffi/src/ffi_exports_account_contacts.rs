#[no_mangle]
pub extern "C" fn omni_identity_get_current_account_export_manifest(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_current_account_export_manifest_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_export_current_database_backup(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_export_current_database_backup_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_export_current_database_backup_bundle(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_export_current_database_backup_bundle_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_inspect_account_backup_manifest(
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_inspect_account_backup_manifest_impl(request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_create_account(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_create_account_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_restore_account_from_export_manifest(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_restore_account_from_export_manifest_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_restore_account_from_input(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_restore_account_from_input_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_inspect_recovery_input(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_inspect_recovery_input_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_local_account(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_get_local_account_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_update_local_public_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_update_local_public_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_local_device(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_get_local_device_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_local_prekey_bundle(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_get_local_prekey_bundle_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_store_remote_prekey_bundle(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_store_remote_prekey_bundle_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_active_profile(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_get_active_profile_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_list_local_profiles(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_list_local_profiles_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_select_active_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_select_active_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_reopen_local_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_reopen_local_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_clear_active_profile(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| identity_clear_active_profile_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_local_profile_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_local_profile_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_profile_gate_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_profile_gate_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_current_profile_gate_summary(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_current_profile_gate_summary_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_profile_lifecycle_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_profile_lifecycle_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_current_profile_lifecycle_summary(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_current_profile_lifecycle_summary_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_lifecycle_fast_snapshot(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_lifecycle_fast_snapshot_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_sign_out_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_sign_out_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_prepare_sign_out_current_profile(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_prepare_sign_out_current_profile_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_confirm_sign_out_current_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_confirm_sign_out_current_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_confirm_sign_out_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_confirm_sign_out_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_unlock_profile_with_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_unlock_profile_with_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_unlock_current_profile_with_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_unlock_current_profile_with_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_reveal_current_profile_recovery_phrase_with_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_reveal_current_profile_recovery_phrase_with_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_unlock_profile_with_mnemonic(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_unlock_profile_with_mnemonic_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_unlock_current_profile_with_mnemonic(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_unlock_current_profile_with_mnemonic_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_set_local_profile_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_set_local_profile_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_change_local_profile_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_change_local_profile_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_remove_local_profile_password(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_remove_local_profile_password_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_password_protection_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_password_protection_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_local_profile_footprint_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_local_profile_footprint_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_preview_sign_out_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_preview_sign_out_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_preview_delete_local_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_preview_delete_local_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_prepare_delete_current_profile(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_prepare_delete_current_profile_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_preview_revoke_current_device_and_wipe(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_preview_revoke_current_device_and_wipe_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_delete_local_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_delete_local_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_confirm_delete_local_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_confirm_delete_local_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_confirm_delete_current_profile(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_confirm_delete_current_profile_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_list(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_list_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_get_refresh_summary(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_get_refresh_summary_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_get_screen_snapshot(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_get_screen_snapshot_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_add_by_identifier(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_add_by_identifier_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_request_by_identifier(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_request_by_identifier_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_add_from_payload(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_add_from_payload_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_rename(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_rename_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_block(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_block_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_approve(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_approve_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Friend-proxy (Axis 2): set/revoke the per-contact carriage consent (persisted; mirrored into
/// the running mesh + re-derives the advertised role). See friend-proxy-by-consent.md (Phase C).
#[no_mangle]
pub extern "C" fn omni_contacts_set_friend_relay_consent(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_set_friend_relay_consent_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Remote access (remote-access-for-contacts.md): read one contact's record — what we let them do
/// and what they told us they let us do.
#[no_mangle]
pub extern "C" fn omni_contacts_get_remote_access(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_get_remote_access_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Remote access: every contact with any grant, either direction (the Settings list).
#[no_mangle]
pub extern "C" fn omni_contacts_list_remote_access(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_list_remote_access_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Remote access: let this contact control our computer (`mode` set) or stop (`mode` null). Only a
/// verified contact may be granted; revoking is always allowed.
#[no_mangle]
pub extern "C" fn omni_contacts_set_remote_access_granted(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| contacts_set_remote_access_granted_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Our own account's devices, this one first (the Settings «Устройства» list).
#[no_mangle]
pub extern "C" fn omni_account_devices_list(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| account_devices_list_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// "The account key was stolen": move the account to a new working key, keeping the listed devices.
#[no_mangle]
pub extern "C" fn omni_account_move_working_key(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| account_move_working_key_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The owner has seen the account's devices: none is "new" any more.
#[no_mangle]
pub extern "C" fn omni_account_devices_acknowledge(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| account_devices_acknowledge_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Take a revoked device of ours (or, with no `device_id`, all of them) out of the Settings list;
/// the revocation itself stays. Returns how many were hidden.
#[no_mangle]
pub extern "C" fn omni_account_devices_hide_revoked(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| account_devices_hide_revoked_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Take one of our own devices back: it is dropped from every route, our other devices and our
/// contacts are told, and it erases its profile when it learns.
#[no_mangle]
pub extern "C" fn omni_account_device_revoke(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| account_device_revoke_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_decline(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_decline_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_forget(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_forget_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_forget_and_reset_peer(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_forget_and_reset_peer_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_get_card(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_get_card_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_get_card_revision(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_get_card_revision_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_identity_get_my_verification_payload(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        identity_get_my_verification_payload_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_get_verification_state(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_get_verification_state_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_confirm_verification(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_confirm_verification_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_header_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_header_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_screen_snapshot(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_screen_snapshot_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_open_session_lease(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_open_session_lease_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The chat is on screen: its lines so far are read (unread counters).
#[no_mangle]
pub extern "C" fn omni_chat_mark_read(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_mark_read_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_end_session(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_end_session_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_list_thread_projections(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| chat_list_thread_projections_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_thread_list_refresh_summary(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_thread_list_refresh_summary_impl(handle)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_security_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_security_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_message_refresh_summary(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_message_refresh_summary_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_message_delta(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_message_delta_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_message_security_details(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_message_security_details_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_get_message_security_details_batch(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_get_message_security_details_batch_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_start_chat(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_start_chat_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_quick_add_from_dialog(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_quick_add_from_dialog_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_contacts_attach_dialog_to_contact(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        contacts_attach_dialog_to_contact_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
