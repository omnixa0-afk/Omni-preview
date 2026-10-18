#[no_mangle]
pub extern "C" fn omni_transport_snapshot(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| transport_snapshot_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_transport_discovery_refresh(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| transport_discovery_refresh_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_send_text(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_send_text_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_send_attachment(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_send_attachment_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_request_attachment_download(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_request_attachment_download_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_revoke_attachment_access(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_revoke_attachment_access_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_list_messages(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_list_messages_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_chat_list_messages_page(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        chat_list_messages_page_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_queue_snapshot(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| queue_snapshot_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_queue_diagnostics(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| queue_diagnostics_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// The plaintext of a media file the shell shows (`{"path"}` → `{"bytes_b64"}`): Omni's storage is
/// encrypted at rest, so the shell never opens such a path itself.
#[no_mangle]
pub extern "C" fn omni_media_read_file(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| media_read_file_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// A path an outside program can open a media file at (`{"path","file_name"}` → `{"path"}`): a plain
/// copy under `<data>\tmp\open\` for a file in Omni's storage, cleared at the next start.
#[no_mangle]
pub extern "C" fn omni_media_open_copy(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| media_open_copy_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
