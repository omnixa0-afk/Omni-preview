// FFI exports for discoverability / Omni-ID (global identity discovery, Model A MA1).

#[no_mangle]
pub extern "C" fn omni_discoverability_get_omni_id(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| discoverability_get_omni_id_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_discoverability_get_mode(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| discoverability_get_mode_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_discoverability_set_mode(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        discoverability_set_mode_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_discoverability_find_by_omni_id(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        discoverability_find_by_omni_id_impl(handle, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

// Find by name (docs/decisions/find-by-name.md): the switch and the search.

#[no_mangle]
pub extern "C" fn omni_discoverability_get_name_findable(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| discoverability_get_name_findable_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_discoverability_set_name_findable(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| discoverability_set_name_findable_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_discoverability_search_by_name(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| discoverability_search_by_name_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

// Public relay directory (docs/decisions/public-relay-directory.md): the operator's switch and the search.

#[no_mangle]
pub extern "C" fn omni_relay_directory_get_role(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| relay_directory_get_role_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_relay_directory_set_role(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| relay_directory_set_role_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_relay_directory_search(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| relay_directory_search_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

// Pinned relay (public-relay-directory.md, stage 3): «Сидеть через него».

#[no_mangle]
pub extern "C" fn omni_pinned_relay_get(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| pinned_relay_get_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_pinned_relay_set(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| pinned_relay_set_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_pinned_relay_clear(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| pinned_relay_clear_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
