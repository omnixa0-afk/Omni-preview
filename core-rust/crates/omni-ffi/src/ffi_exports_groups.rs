// FFI exports for groups. Thin panic-guarded wrappers over the impls in exports_impl_groups.rs.
// Groups run on MLS — docs/decisions/group-crypto-mls.md.

#[no_mangle]
pub extern "C" fn omni_groups_create(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_create_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_list(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_list_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_list_members(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        groups_list_members_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_send_attachment(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        groups_send_attachment_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_send_text(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        groups_send_text_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_invite(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_invite_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_start_call(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_start_call_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_leave_call(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_leave_call_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_call_snapshot(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_call_snapshot_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_set_call_media_state(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        groups_set_call_media_state_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_publish_media_capabilities(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        groups_publish_media_capabilities_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_list_invites(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_list_invites_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_accept_invite(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_accept_invite_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_decline_invite(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_decline_invite_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_leave(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_leave_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_delete_for_everyone(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_delete_for_everyone_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_set_avatar(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_set_avatar_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_remove_member(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_remove_member_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_rename(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_rename_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// An admin turns the recent history for newcomers on or off.
#[no_mangle]
pub extern "C" fn omni_groups_set_newcomer_history(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_set_newcomer_history_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_set_admin(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_set_admin_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_list_messages(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_list_messages_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_call_inbox(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_call_inbox_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_join_call(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_join_call_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Our unanswered invitations for a group (only the inviter knows them).
#[no_mangle]
pub extern "C" fn omni_groups_list_sent_invites(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_list_sent_invites_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Withdraw an invitation we sent (`{group_id, dialog_key}`).
#[no_mangle]
pub extern "C" fn omni_groups_cancel_invite(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_cancel_invite_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_groups_dismiss_call(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_dismiss_call_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
