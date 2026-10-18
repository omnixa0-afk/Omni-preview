fn groups_create_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: CreateGroupRequest = parse_json_request(request_json, "create_group request")?;
        require_handle(handle)?.inner.create_group(&request.name)
    })();
    to_result_payload(result)
}

fn groups_list_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.list_group_summaries()))
}

fn groups_list_members_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "group members request")?;
        require_handle(handle)?
            .inner
            .group_members_view(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_send_text_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SendGroupTextRequest =
            parse_json_request(request_json, "send_group_text request")?;
        require_handle(handle)?
            .inner
            .send_group_text(&request.group_id, &request.body)
    })();
    to_result_payload(result)
}

fn groups_send_attachment_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SendGroupAttachmentRequest =
            parse_json_request(request_json, "send_group_attachment request")?;
        require_handle(handle)?.inner.send_group_attachment(
            &request.group_id,
            &request.body,
            &request.source_file_path,
            request.display_file_name.as_deref(),
            request.as_photo,
        )
    })();
    to_result_payload(result)
}

fn groups_invite_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: InviteToGroupRequest =
            parse_json_request(request_json, "invite_to_group request")?;
        let mut dialog_keys = request.dialog_keys;
        if !request.dialog_key.is_empty() {
            dialog_keys.push(request.dialog_key);
        }
        require_handle(handle)?
            .inner
            .invite_contacts_to_group(&request.group_id, &dialog_keys)
    })();
    to_result_payload(result)
}

fn groups_start_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "start_group_call request")?;
        require_handle(handle)?.inner.start_group_call(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_leave_call_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "leave_group_call request")?;
        require_handle(handle)?.inner.leave_group_call(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_call_snapshot_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "group_call_snapshot request")?;
        require_handle(handle)?
            .inner
            .group_call_snapshot(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_set_call_media_state_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetGroupCallMediaStateRequest =
            parse_json_request(request_json, "set_group_call_media_state request")?;
        require_handle(handle)?.inner.set_group_call_media_state(
            &request.group_id,
            request.muted,
            request.camera_off,
            request.screen_sharing,
        )
    })();
    to_result_payload(result)
}

fn groups_publish_media_capabilities_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: PublishRealtimeMediaCapabilitiesRequest = parse_json_request(
            request_json,
            "publish_group_media_capabilities request",
        )?;
        require_handle(handle)?
            .inner
            .publish_group_media_capabilities(request.capabilities)
    })();
    to_result_payload(result)
}

fn groups_list_invites_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| {
        let app = &handle.inner;
        app.list_group_invites()
    }))
}

fn groups_accept_invite_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_accept_invite request")?;
        let app = &require_handle(handle)?.inner;
        app.accept_group_invite(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_decline_invite_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_decline_invite request")?;
        let app = &require_handle(handle)?.inner;
        app.decline_group_invite(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_leave_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_leave request")?;
        let app = &require_handle(handle)?.inner;
        app.leave_group(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_delete_for_everyone_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_delete_for_everyone request")?;
        let app = &require_handle(handle)?.inner;
        app.delete_group_for_everyone(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_remove_member_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupMemberRequest = parse_json_request(request_json, "omni_groups_remove_member request")?;
        let app = &require_handle(handle)?.inner;
        app.remove_group_member(&request.group_id, &request.member_pub)
    })();
    to_result_payload(result)
}

fn groups_set_avatar_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetGroupAvatarRequest = parse_json_request(request_json, "omni_groups_set_avatar request")?;
        let app = &require_handle(handle)?.inner;
        app.set_group_avatar(&request.group_id, request.source_path.as_deref())
    })();
    to_result_payload(result)
}

fn groups_rename_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: RenameGroupRequest = parse_json_request(request_json, "omni_groups_rename request")?;
        let app = &require_handle(handle)?.inner;
        app.rename_group(&request.group_id, &request.name)
    })();
    to_result_payload(result)
}

fn groups_set_admin_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetGroupAdminRequest = parse_json_request(request_json, "omni_groups_set_admin request")?;
        let app = &require_handle(handle)?.inner;
        app.set_group_admin(&request.group_id, &request.member_pub, request.admin)
    })();
    to_result_payload(result)
}

fn groups_set_newcomer_history_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetGroupNewcomerHistoryRequest =
            parse_json_request(request_json, "omni_groups_set_newcomer_history request")?;
        let app = &require_handle(handle)?.inner;
        app.set_group_newcomer_history(&request.group_id, request.on)
    })();
    to_result_payload(result)
}

fn groups_list_messages_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_list_messages request")?;
        let app = &require_handle(handle)?.inner;
        app.list_group_messages(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_call_inbox_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| {
        let app = &handle.inner;
        app.group_call_inbox()
    }))
}

fn groups_join_call_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_join_call request")?;
        let app = &require_handle(handle)?.inner;
        app.join_group_call(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_list_sent_invites_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_list_sent_invites request")?;
        require_handle(handle)?.inner.list_group_sent_invites(&request.group_id)
    })();
    to_result_payload(result)
}

fn groups_cancel_invite_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: InviteToGroupRequest = parse_json_request(request_json, "omni_groups_cancel_invite request")?;
        require_handle(handle)?
            .inner
            .cancel_group_invite(&request.group_id, &request.dialog_key)
    })();
    to_result_payload(result)
}

fn groups_dismiss_call_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_groups_dismiss_call request")?;
        let app = &require_handle(handle)?.inner;
        app.dismiss_group_call(&request.group_id)
    })();
    to_result_payload(result)
}
