fn workspace_add_repo_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceAddRepoRequest = parse_json_request(request_json, "omni_workspace_add_repo request")?;
        let app = &require_handle(handle)?.inner;
        match (request.files, request.work_path.as_deref()) {
            (true, Some(folder)) => app.workspace_add_files_repo(&request.group_id, &request.repo, folder),
            (true, None) => Err(OmniError::InvalidInput("a files repository needs its folder".into())),
            (false, work_path) => app.workspace_add_repo(&request.group_id, &request.repo, work_path),
        }
    })();
    to_result_payload(result)
}

fn workspace_repos_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_workspace_repos request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_repos(&request.group_id)
    })();
    to_result_payload(result)
}

fn workspace_branches_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceRepoRequest = parse_json_request(request_json, "omni_workspace_branches request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_member_branches(&request.group_id, &request.repo)
    })();
    to_result_payload(result)
}

fn workspace_main_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceRepoRequest = parse_json_request(request_json, "omni_workspace_main request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_main(&request.group_id, &request.repo)
    })();
    to_result_payload(result)
}

fn workspace_set_main_rule_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceMainRuleRequest = parse_json_request(request_json, "omni_workspace_set_main_rule request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_set_main_rule(&request.group_id, &request.repo, request.needs_approval)
    })();
    to_result_payload(result)
}

fn workspace_approve_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceApproveRequest = parse_json_request(request_json, "omni_workspace_approve request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_approve(&request.group_id, &request.repo, &request.oid)
    })();
    to_result_payload(result)
}

fn workspace_view_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: GroupIdRequest = parse_json_request(request_json, "omni_workspace_view request")?;
        // The database under the serializer, git without it: the shell's other calls do not wait for git.
        let snapshot = require_handle(handle)?.inner.workspace_view_snapshot(&request.group_id)?;
        snapshot.render()
    })();
    to_result_payload(result)
}

fn workspace_pull_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceRepoRequest = parse_json_request(request_json, "omni_workspace_pull request")?;
        // The folder under the serializer, git without it (as the screen and «Сохранить версию»).
        require_handle(handle)?.inner.workspace_folder(&request.group_id, &request.repo)?.pull()
    })();
    to_result_payload(result)
}

fn workspace_publish_branch_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceBranchRequest = parse_json_request(request_json, "omni_workspace_publish_branch request")?;
        let folder = require_handle(handle)?.inner.workspace_folder(&request.group_id, &request.repo)?;
        folder.push_branch(&request.repo, &request.branch)?;
        require_handle(handle)?.inner.workspace_publish(&request.group_id, &request.repo)
    })();
    to_result_payload(result)
}

fn workspace_take_repo_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceTakeRepoRequest = parse_json_request(request_json, "omni_workspace_take_repo request")?;
        let app = &require_handle(handle)?.inner;
        app.workspace_take_repo(&request.group_id, &request.repo, &request.folder)
    })();
    to_result_payload(result)
}

fn workspace_send_to_my_devices_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceBranchRequest = parse_json_request(request_json, "omni_workspace_send_to_my_devices request")?;
        let folder = require_handle(handle)?.inner.workspace_folder(&request.group_id, &request.repo)?;
        folder.send_to_my_devices(&request.repo, &request.branch)
    })();
    to_result_payload(result)
}

fn workspace_take_from_device_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceDeviceBranchRequest = parse_json_request(request_json, "omni_workspace_take_from_device request")?;
        let folder = require_handle(handle)?.inner.workspace_folder(&request.group_id, &request.repo)?;
        folder.take_from_device(&request.repo, &request.node, &request.branch)
    })();
    to_result_payload(result)
}

fn workspace_save_version_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request: WorkspaceSaveVersionRequest = parse_json_request(request_json, "omni_workspace_save_version request")?;
        // The database under the serializer, git without it: the shell's other calls do not wait for
        // the commit of a folder of models.
        let save = require_handle(handle)?.inner.workspace_save_version_prepare(&request.group_id, &request.repo, &request.message)?;
        let Some(oid) = save.save()? else { return Ok(None) };
        require_handle(handle)?.inner.workspace_save_version_publish(&request.group_id, &request.repo, &oid)?;
        Ok(Some(oid))
    })();
    to_result_payload(result)
}

fn workspace_files_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let command: omni_app::WorkspaceFilesCommand = parse_json_request(request_json, "omni_workspace_files request")?;
        let job = require_handle(handle)?.inner.prepare_workspace_files_command(command)?;
        job.run()
    })();
    to_result_payload(result)
}

fn workspace_agents_impl(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let command: omni_app::WorkspaceAgentsCommand = parse_json_request(request_json, "omni_workspace_agents request")?;
        require_handle(handle)?.inner.workspace_agents_command(command)
    })();
    to_result_payload(result)
}
