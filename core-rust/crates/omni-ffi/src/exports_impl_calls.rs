fn calls_decline_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: DeclineCallRequest =
            parse_json_request(request_json, "decline_call request")?;
        require_handle(handle)?
            .inner
            .decline_call(request.call_id, request.reason)
    })();

    to_result_payload(result)
}

fn calls_hang_up_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: CallIdRequest =
            parse_json_request(request_json, "hang_up_call request")?;
        require_handle(handle)?
            .inner
            .hang_up_call(request.call_id)
    })();

    to_result_payload(result)
}

fn calls_place_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RealtimeMediaDialogRequest =
            parse_json_request(request_json, "place_call request")?;
        require_handle(handle)?.inner.place_call(request.dialog_key)
    })();

    to_result_payload(result)
}

fn calls_accept_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: CallIdRequest = parse_json_request(request_json, "accept_call request")?;
        require_handle(handle)?.inner.accept_call(request.call_id)
    })();

    to_result_payload(result)
}

fn calls_set_local_mic_muted_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetLocalMicMutedRequest =
            parse_json_request(request_json, "set_local_mic_muted request")?;
        require_handle(handle)?
            .inner
            .set_local_mic_muted(request.call_id, request.is_muted)
    })();

    to_result_payload(result)
}

fn calls_get_inbox_snapshot_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.get_call_inbox_snapshot()),
    )
}

fn calls_publish_realtime_media_capabilities_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: PublishRealtimeMediaCapabilitiesRequest = parse_json_request(
            request_json,
            "publish_realtime_media_capabilities request",
        )?;
        require_handle(handle)?
            .inner
            .publish_realtime_media_capabilities(request.capabilities)
    })();

    to_result_payload(result)
}

fn calls_get_realtime_media_negotiation_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RealtimeMediaDialogRequest =
            parse_json_request(request_json, "get_realtime_media_negotiation request")?;
        require_handle(handle)?
            .inner
            .get_realtime_media_negotiation(request.dialog_key)
    })();

    to_result_payload(result)
}

fn calls_clear_realtime_media_negotiation_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<()> {
        let request: RealtimeMediaDialogRequest =
            parse_json_request(request_json, "clear_realtime_media_negotiation request")?;
        require_handle(handle)?
            .inner
            .clear_realtime_media_negotiation(request.dialog_key)
    })();

    to_result_payload(result)
}

fn calls_request_remote_control_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ContactIdRequest =
            parse_json_request(request_json, "request_remote_control request")?;
        require_handle(handle)?
            .inner
            .request_remote_control(&request.contact_id)
    })();

    to_result_payload(result)
}
