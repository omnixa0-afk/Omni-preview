fn discoverability_get_omni_id_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.get_my_omni_id()))
}

fn discoverability_get_mode_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(
        require_handle(handle).map(|handle| handle.inner.discoverability_mode().as_db_value()),
    )
}

fn discoverability_set_mode_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetDiscoverabilityRequest =
            parse_json_request(request_json, "set_discoverability request")?;
        require_handle(handle)?
            .inner
            .set_discoverability_mode(omni_app::DiscoverabilityMode::from_db_value(&request.mode))
    })();
    to_result_payload(result)
}

fn discoverability_find_by_omni_id_impl(
    handle: *mut OmniAppHandle,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: FindByOmniIdRequest =
            parse_json_request(request_json, "find_by_omni_id request")?;
        require_handle(handle)?
            .inner
            .find_contact_by_omni_id(&request.omni_id, request.petname)
    })();
    to_result_payload(result)
}

fn discoverability_get_name_findable_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.name_findable()))
}

fn discoverability_set_name_findable_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetNameFindableRequest = parse_json_request(request_json, "set_name_findable request")?;
        require_handle(handle)?.inner.set_name_findable(request.on)
    })();
    to_result_payload(result)
}

fn discoverability_search_by_name_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SearchByNameRequest = parse_json_request(request_json, "search_by_name request")?;
        require_handle(handle)?.inner.search_people_by_name(&request.query)
    })();
    to_result_payload(result)
}

fn relay_directory_get_role_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.public_relay_role()))
}

fn relay_directory_set_role_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: SetPublicRelayRoleRequest = parse_json_request(request_json, "set_public_relay_role request")?;
        require_handle(handle)?.inner.set_public_relay_role(request.on, &request.name)
    })();
    to_result_payload(result)
}

fn relay_directory_search_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.search_public_relays()))
}

fn pinned_relay_get_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.pinned_relay_state()))
}

fn pinned_relay_set_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: omni_app::PinnedRelayV1 = parse_json_request(request_json, "pin_relay request")?;
        require_handle(handle)?.inner.pin_relay(request)
    })();
    to_result_payload(result)
}

fn pinned_relay_clear_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).and_then(|handle| handle.inner.unpin_relay()))
}
