fn init_impl(config_json: *const c_char) -> OmniResult<OmniAppHandle> {
    let request = parse_init_request(config_json, "init")?;
    let app = OmniApp::init(request)?;

    Ok(OmniAppHandle { inner: app })
}

fn inspect_database_bootstrap_impl(config_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request = parse_init_request(config_json, "inspect_database_bootstrap")?;
        OmniApp::inspect_database_bootstrap(request)
    })();

    to_result_payload(result)
}

fn database_lock_state_impl(config_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<_> {
        let request = parse_init_request(config_json, "database_lock_state")?;
        let state = match omni_storage::Storage::database_lock_state(&request.db_path)? {
            omni_storage::DatabaseLockState::Openable => "openable",
            omni_storage::DatabaseLockState::NeedsRecoveryPhrase => "needs_recovery_phrase",
        };
        Ok(DatabaseLockStateResponse {
            state: state.to_string(),
        })
    })();

    to_result_payload(result)
}

fn unlock_database_with_mnemonic_impl(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<()> {
        let request = parse_init_request(config_json, "unlock_database_with_mnemonic")?;
        let unlock: UnlockDatabaseWithMnemonicRequest =
            parse_json_request(request_json, "unlock_database_with_mnemonic request")?;
        OmniApp::unlock_database_with_mnemonic(request, &unlock.mnemonic)
    })();

    to_result_payload(result)
}

fn reset_local_data_impl(config_json: *const c_char) -> *mut c_char {
    let result = (|| -> OmniResult<()> {
        let request = parse_init_request(config_json, "reset_local_data")?;
        let db_path = std::path::Path::new(&request.db_path);
        omni_storage::Storage::delete_local_data_at_path(db_path)
    })();

    to_result_payload(result)
}

fn export_database_backup_bundle_impl(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<()> {
        let request = parse_init_request(config_json, "export_database_backup_bundle")?;
        let export_request: ExportCurrentDatabaseBackupRequest =
            parse_json_request(request_json, "export_database_backup_bundle request")?;

        OmniApp::export_database_backup_bundle_at_path(
            request.db_path,
            export_request.destination_path,
        )
    })();

    to_result_payload(result)
}

fn import_database_backup_bundle_impl(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    let result = (|| -> OmniResult<()> {
        let request = parse_init_request(config_json, "import_database_backup_bundle")?;
        let import_request: ImportDatabaseBackupRequest =
            parse_json_request(request_json, "import_database_backup_bundle request")?;

        OmniApp::import_database_backup_bundle(
            request.db_path,
            import_request.backup_db_path,
            import_request.inspection_token,
        )
    })();

    to_result_payload(result)
}

fn ping_impl(handle: *mut OmniAppHandle) -> *mut c_char {
    to_result_payload(require_handle(handle).map(|handle| handle.inner.ping()))
}

