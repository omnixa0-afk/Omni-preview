/// The shell's injector: `context` is the shell's, `json` a `ScreenControlDelivery`.
pub type OmniScreenControlSink = unsafe extern "C" fn(context: *mut std::ffi::c_void, json: *const u8, json_len: usize);

fn screen_control_set_sink_impl(
    handle: *mut OmniAppHandle,
    sink: Option<OmniScreenControlSink>,
    context: *mut std::ffi::c_void,
) -> OmniResult<()> {
    let sink = sink.map(|sink| {
        // Carried as an integer so the closure is Send; the shell keeps it alive until it unsets.
        let context = context as usize;
        std::sync::Arc::new(move |delivery: &omni_app::ScreenControlDelivery| {
            if let Ok(json) = serde_json::to_vec(delivery) {
                // SAFETY: the shell keeps `context` valid until it removes this sink.
                unsafe { sink(context as *mut std::ffi::c_void, json.as_ptr(), json.len()) }
            }
        }) as omni_app::ScreenControlSink
    });
    require_handle(handle)?.inner.set_screen_control_sink(sink);
    Ok(())
}

fn screen_control_send_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ScreenControlSendRequest = parse_json_request(request_json, "screen_control_send request")?;
        require_handle(handle)?.inner.send_screen_control(
            &request.dialog_key,
            request.sharer.as_deref(),
            request.pointer.map(|[x, y]| (x, y)),
            &request.events,
        )
    })();
    to_result_payload(result)
}

fn calls_set_screen_control_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetCallScreenControlRequest =
            parse_json_request(request_json, "set_call_screen_control request")?;
        let app = &require_handle(handle)?.inner;
        match request.sharer_state {
            Some(state) => app.report_call_screen_control_state(&request.call_id, state),
            None => app.set_call_screen_control(&request.call_id, request.granted),
        }
    })();
    to_result_payload(result)
}

fn calls_get_screen_control_state_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.call_screen_control_state()))
}

fn groups_set_call_screen_control_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetGroupCallScreenControlRequest =
            parse_json_request(request_json, "set_group_call_screen_control request")?;
        let grant = (request.everyone || !request.members.is_empty()).then_some(omni_protocol::ScreenControlGrantV1 {
            everyone: request.everyone,
            members: request.members,
        });
        require_handle(handle)?.inner.set_group_call_screen_control(&request.group_id, grant)
    })();
    to_result_payload(result)
}
