// Remote screen control (docs/decisions/remote-screen-control.md): the shell grants, sends its input
// as a viewer, and injects what the core verified as a sharer.

/// Install the shell's injector: `sink(context, json, len)` is called once per accepted input batch,
/// on the mesh pump thread, with a `ScreenControlDelivery` as JSON (`dialog_key`, `viewer`, `pointer`
/// `[x, y]` or null, `events`, `release`). A null `sink` removes it. Returns null or an error
/// envelope.
#[no_mangle]
pub extern "C" fn omni_screen_control_set_sink(
    handle: *mut OmniAppHandle,
    sink: Option<OmniScreenControlSink>,
    context: *mut std::ffi::c_void,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| screen_control_set_sink_impl(handle, sink, context))) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal("panic crossed FFI boundary".to_string())),
    }
}

/// Send our input to the screen we control. Request: `{dialog_key, sharer?, pointer?: [x, y],
/// events: [...]}`; `sharer` (a member_pub) only in a group call. Also the keep-alive: the shell calls
/// it at least every 100 ms while controlling, with no events.
#[no_mangle]
pub extern "C" fn omni_screen_control_send(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| screen_control_send_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Let the peer of the current 1:1 call control the screen we share (`{call_id, granted}`).
#[no_mangle]
pub extern "C" fn omni_calls_set_screen_control(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| calls_set_screen_control_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Screen control in the current 1:1 call: `{i_grant, peer_grants_me}`.
#[no_mangle]
pub extern "C" fn omni_calls_get_screen_control_state(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| calls_get_screen_control_state_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Who may control the screen we share in a group call: `{group_id, everyone, members}`; nobody when
/// `everyone` is false and `members` is empty.
#[no_mangle]
pub extern "C" fn omni_groups_set_call_screen_control(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| groups_set_call_screen_control_impl(handle, request_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}
