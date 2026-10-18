fn transport_snapshot_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.transport_snapshot()))
}

fn transport_discovery_refresh_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle)
            .and_then(|handle| handle.inner.transport_discovery_refresh_snapshot()),
    )
}

fn chat_send_text_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SendTextRequest = parse_json_request(request_json, "send_text request")?;
        require_handle(handle)?
            .inner
            .send_text(request.dialog_key, request.body)
    })();

    to_result_payload(result)
}

fn chat_send_attachment_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SendAttachmentRequest =
            parse_json_request(request_json, "send_attachment request")?;
        require_handle(handle)?.inner.send_attachment_request(request)
    })();

    to_result_payload(result)
}

fn chat_request_attachment_download_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: AttachmentDownloadRequest =
            parse_json_request(request_json, "request_attachment_download request")?;
        require_handle(handle)?
            .inner
            .request_attachment_download(request.message_id, request.save_path)
    })();

    to_result_payload(result)
}

fn chat_revoke_attachment_access_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MessageIdRequest =
            parse_json_request(request_json, "revoke_attachment_access request")?;
        require_handle(handle)?
            .inner
            .revoke_attachment_access(request.message_id)
    })();

    to_result_payload(result)
}

fn chat_list_messages_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ListMessagesRequest =
            parse_json_request(request_json, "list_messages request")?;
        require_handle(handle)?
            .inner
            .list_messages(request.dialog_key)
    })();

    to_result_payload(result)
}

fn chat_list_messages_page_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: ListMessagesPageRequest =
            parse_json_request(request_json, "list_messages_page request")?;
        require_handle(handle)?.inner.list_messages_page(request)
    })();

    to_result_payload(result)
}

fn queue_snapshot_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).and_then(|handle| handle.inner.outbound_queue_snapshot()),
    )
}

fn queue_diagnostics_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.queue_diagnostics()))
}

fn media_read_file_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MediaFileRequest = parse_json_request(request_json, "media_read_file request")?;
        require_handle(handle)?.inner.read_media_file(request.path)
    })();

    to_result_payload(result)
}

fn media_open_copy_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: MediaFileRequest = parse_json_request(request_json, "media_open_copy request")?;
        require_handle(handle)?
            .inner
            .open_media_file_copy(request.path, request.file_name)
    })();

    to_result_payload(result)
}
