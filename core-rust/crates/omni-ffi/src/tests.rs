#[cfg(test)]
mod tests {
    use std::ffi::{CStr, CString};
    
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use omni_app::AppInitRequest;
    use omni_identity::{ContactSnapshot, VerificationMethod};
    use omni_protocol::{
        contact_payload_from_my_payload,
        AccountPublicDescriptorV1, ContactVerificationPayloadV1, DevicePublicDescriptorV1,
        ImportableContactPayloadV1, PrekeyBundleV1, CONTACT_VERIFICATION_PAYLOAD_VERSION_V1,
        IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1, PUBLIC_DESCRIPTOR_VERSION_V1,
    };
    use omni_types::{dialog_key_from_instance_id, TransportState};

    use super::{
        omni_app_free, omni_app_init, omni_app_ping, omni_daemon_start,
        omni_app_database_lock_state, omni_app_unlock_database_with_mnemonic,
        omni_chat_list_messages, omni_chat_send_text,
        omni_chat_get_message_delta, omni_chat_get_message_refresh_summary,
        omni_chat_get_message_security_details, omni_chat_get_security_summary,
        omni_configure_platform_secret_wrap_key_hex,
        omni_contacts_add_by_identifier, omni_contacts_add_from_payload,
        omni_contacts_get_card_revision,
        omni_contacts_request_by_identifier,
        omni_contacts_attach_dialog_to_contact,
        omni_contacts_confirm_verification, omni_contacts_get_card,
        omni_contacts_get_verification_state, omni_contacts_list, omni_identity_create_account,
        omni_identity_get_active_profile,
        omni_identity_get_current_profile_gate_summary,
        omni_identity_get_current_profile_lifecycle_summary,
        omni_identity_get_local_account, omni_identity_get_local_device,
        omni_identity_get_local_prekey_bundle, omni_identity_store_remote_prekey_bundle,
        omni_identity_get_local_profile_footprint_summary, omni_identity_list_local_profiles,
        omni_identity_get_my_verification_payload, omni_queue_diagnostics, omni_queue_snapshot,
        omni_identity_reveal_current_profile_recovery_phrase_with_password,
        omni_identity_prepare_sign_out_current_profile,
        omni_identity_confirm_sign_out_current_profile,
        omni_identity_confirm_sign_out_profile,
        omni_identity_preview_sign_out_profile,
        omni_identity_inspect_recovery_input,
        omni_identity_preview_delete_local_profile,
        omni_identity_set_local_profile_password,
        omni_identity_restore_account_from_input,
        omni_identity_clear_active_profile,
        omni_identity_reopen_local_profile,
        omni_identity_select_active_profile,
        omni_realtime_media_packet_sink, omni_realtime_media_set_inbound_sink,
        omni_realtime_media_packet_target_counters, omni_realtime_media_packet_target_create,
        omni_realtime_media_packet_target_destroy,
        omni_string_free,
        AddContactByIdentifierRequest, AddContactFromPayloadRequest,
        RequestContactByIdentifierRequest,
        ConfirmContactVerificationRequest, ContactIdRequest, CreateAccountRequest,
        DialogKeyRequest, MessageIdRequest,
        RevealCurrentProfileRecoveryPhraseWithPasswordRequest, SetLocalProfilePasswordRequest,
    };

    fn create_account_response_account(value: &serde_json::Value) -> &serde_json::Value {
        &value["account"]
    }

    fn create_account_response_profile_id(value: &serde_json::Value) -> Option<String> {
        create_account_response_account(value)["local_profile_summary"]["profile_id"]
            .as_str()
            .map(ToOwned::to_owned)
    }

    fn create_account_response_mnemonic(value: &serde_json::Value) -> Option<String> {
        value["initial_recovery_phrase_v2"]["mnemonic"]
            .as_str()
            .map(ToOwned::to_owned)
    }

    #[test]
    fn ffi_init_and_ping_work() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-ffi-tests")
            .join(format!("{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");

        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let response_ptr = omni_app_ping(handle);
        assert!(!response_ptr.is_null());

        let response = unsafe { CStr::from_ptr(response_ptr) }
            .to_str()
            .expect("response should be valid utf-8")
            .to_string();

        assert!(response.contains("pong from rust core"));
        assert!(db_path.exists());

        omni_string_free(response_ptr);
        omni_app_free(handle);
    }

    #[test]
    fn a_data_folder_from_another_machine_reports_it_needs_the_phrase() {
        let take = |ptr: *mut std::ffi::c_char| -> serde_json::Value {
            assert!(!ptr.is_null());
            let text = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap().to_string();
            omni_string_free(ptr);
            serde_json::from_str(&text).unwrap()
        };
        let config = |path: &std::path::Path| {
            CString::new(format!(r#"{{"db_path":"{}"}}"#, escape_json_string(&path.to_string_lossy()))).unwrap()
        };
        let dir = std::env::temp_dir()
            .join("omni-stage-0-ffi-tests")
            .join(format!("lock-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let here = dir.join("here.db");
        let state = |path: &std::path::Path| take(omni_app_database_lock_state(config(path).as_ptr()));

        assert_eq!(state(&here)["state"], "openable", "no file yet opens as a new one");
        drop(omni_storage::Storage::open(&here).unwrap());
        assert_eq!(state(&here)["state"], "openable");

        // The same folder on another machine: its key file has no DPAPI wrap this account can open.
        let there = dir.join("there.db");
        std::fs::copy(&here, &there).unwrap();
        let mut keys: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("here.db.keys")).unwrap()).unwrap();
        keys["dpapi"] = serde_json::Value::Null;
        std::fs::write(dir.join("there.db.keys"), serde_json::to_vec(&keys).unwrap()).unwrap();
        assert_eq!(state(&there)["state"], "needs_recovery_phrase");

        let request = CString::new(r#"{"mnemonic":""}"#).unwrap();
        let refused = take(omni_app_unlock_database_with_mnemonic(config(&there).as_ptr(), request.as_ptr()));
        assert_eq!(refused["ok"], false);
        assert_eq!(state(&there)["state"], "needs_recovery_phrase", "a refused phrase changes nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ffi_embedded_daemon_serves_the_handles_core() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let db_path = std::env::temp_dir()
            .join("omni-embedded-daemon-ffi-tests")
            .join(format!("{}.db", unique_suffix()));
        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring = CString::new(config_json).expect("config json no null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        // Start the embedded daemon on a localhost TCP endpoint against THIS handle's core.
        let endpoint = CString::new("127.0.0.1:47811").unwrap();
        let started_ptr = omni_daemon_start(handle, endpoint.as_ptr());
        let started = unsafe { CStr::from_ptr(started_ptr) }.to_str().unwrap().to_string();
        omni_string_free(started_ptr);
        assert!(started.contains("\"ok\":true"), "start should succeed: {started}");

        // One listener per process: a second start only points it at the handle again.
        let again_ptr = omni_daemon_start(handle, endpoint.as_ptr());
        let again = unsafe { CStr::from_ptr(again_ptr) }.to_str().unwrap().to_string();
        omni_string_free(again_ptr);
        assert!(again.contains("\"ok\":true"), "second start keeps the one daemon: {again}");

        // Connect a plain client and drive the core over the line protocol. Retry until the
        // daemon thread has bound the listener.
        let mut stream = None;
        for _ in 0..40 {
            if let Ok(s) = TcpStream::connect("127.0.0.1:47811") {
                stream = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut writer = stream.expect("embedded daemon should accept a connection");
        let mut reader = BufReader::new(writer.try_clone().unwrap());

        writeln!(writer, "create-profile DaemonTest Dev").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "OK", "create-profile over the embedded daemon");

        writeln!(writer, "account-id").unwrap();
        writer.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        let daemon_account = line.trim().strip_prefix("OK ").unwrap_or("").to_string();
        assert!(daemon_account.starts_with("acct-"), "account-id over daemon: {line}");

        // The profile the daemon created is visible through a DIRECT FFI call on the SAME
        // handle — proving the embedded daemon and the shell's FFI share one core.
        let ffi_account_ptr = omni_identity_get_local_account(handle);
        let ffi_account = unsafe { CStr::from_ptr(ffi_account_ptr) }.to_str().unwrap().to_string();
        omni_string_free(ffi_account_ptr);
        assert!(
            ffi_account.contains(&daemon_account),
            "FFI must see the daemon-created account {daemon_account}: {ffi_account}"
        );

        // The shell frees its core while the daemon runs — importing a backup does exactly that
        // (docs/bugs/the-daemon-touched-a-freed-core.md; this test used to leak the handle to dodge
        // it). The daemon keeps polling and answering, and touches nothing freed.
        omni_app_free(handle);
        std::thread::sleep(Duration::from_millis(600));
        writeln!(writer, "account-id").unwrap();
        writer.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("restarting"), "no core, no answer from a freed one: {line}");

        // The core opened again: the daemon serves the new one.
        let second_db = db_path.with_file_name(format!("{}-second.db", unique_suffix()));
        let second_config = CString::new(format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&second_db.to_string_lossy())
        ))
        .unwrap();
        let second = omni_app_init(second_config.as_ptr());
        assert!(!second.is_null());
        let restarted_ptr = omni_daemon_start(second, endpoint.as_ptr());
        omni_string_free(restarted_ptr);
        writeln!(writer, "account-id").unwrap();
        writer.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(!line.contains(&daemon_account), "the new core is a different one: {line}");
        assert!(!line.contains("restarting"), "the daemon has a core again: {line}");

        writeln!(writer, "quit").unwrap();
        let _ = writer.flush();
        omni_app_free(second);
        std::thread::sleep(Duration::from_millis(600));
    }

    #[test]
    fn ffi_can_configure_platform_secret_wrap_key_hex() {
        let key = CString::new("0909090909090909090909090909090909090909090909090909090909090909")
            .expect("key should not contain null bytes");

        let error_ptr = omni_configure_platform_secret_wrap_key_hex(key.as_ptr());
        assert!(error_ptr.is_null(), "valid platform key should not produce ffi error");
    }

    /// The JSON the shell speaks for remote screen control: every event kind parses into the core's
    /// events, a delivery serialises to the shape the shell reads. Negative half: an unknown event
    /// kind or button is refused, and a send with no app is an error, not a crash.
    #[test]
    fn ffi_screen_control_json_contract_holds_both_ways() {
        use omni_protocol::{ScreenInputEvent, ScreenMouseButton};
        let request: crate::ScreenControlSendRequest = serde_json::from_str(
            r#"{"dialog_key":"group:g","sharer":"QUJD","pointer":[1,65535],"events":[
                {"kind":"button","button":"x2","down":true},
                {"kind":"wheel","dx":0,"dy":-120},
                {"kind":"key","vk":65,"scan":30,"extended":true,"down":false}]}"#,
        )
        .expect("the shell's request parses");
        assert_eq!(request.pointer, Some([1, 65535]));
        assert_eq!(
            request.events,
            vec![
                ScreenInputEvent::Button { button: ScreenMouseButton::X2, down: true },
                ScreenInputEvent::Wheel { dx: 0, dy: -120 },
                ScreenInputEvent::Key { vk: 65, scan: 30, extended: true, down: false },
            ]
        );
        let delivery = omni_app::ScreenControlDelivery {
            dialog_key: "group:g".into(),
            viewer: "QUJD".into(),
            pointer: Some((7, 8)),
            events: vec![ScreenInputEvent::Button { button: ScreenMouseButton::Left, down: true }],
            release: false,
        };
        assert_eq!(
            serde_json::to_string(&delivery).unwrap(),
            r#"{"dialog_key":"group:g","viewer":"QUJD","pointer":[7,8],"events":[{"kind":"button","button":"left","down":true}],"release":false}"#
        );

        // Negative half.
        assert!(serde_json::from_str::<crate::ScreenControlSendRequest>(
            r#"{"dialog_key":"d","events":[{"kind":"teleport"}]}"#
        )
        .is_err());
        assert!(serde_json::from_str::<crate::ScreenControlSendRequest>(
            r#"{"dialog_key":"d","events":[{"kind":"button","button":"thumb","down":true}]}"#
        )
        .is_err());
        let request = CString::new(r#"{"dialog_key":"d","events":[]}"#).unwrap();
        let payload = crate::omni_screen_control_send(std::ptr::null_mut(), request.as_ptr());
        let text = unsafe { std::ffi::CStr::from_ptr(payload) }.to_string_lossy().into_owned();
        omni_string_free(payload);
        assert!(text.contains(r#""ok":false"#), "no app is an error envelope: {text}");
    }

    #[test]
    fn ffi_realtime_media_binary_pump_rejects_invalid_inputs_and_drains_empty_batch() {
        let db_path = std::env::temp_dir()
            .join("omni-realtime-media-ffi-tests")
            .join(format!("{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");

        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let dialog_key = CString::new(dialog_key_from_instance_id("ffi-media-empty")).unwrap();
        let dialog_key_bytes = dialog_key.as_bytes();
        let payload = [1_u8; 8];

        // A 1:1 dialog with no approved contact gets no send target at all.
        let mut refused_target = u64::MAX;
        let error_ptr = omni_realtime_media_packet_target_create(
            handle,
            dialog_key_bytes.as_ptr(),
            dialog_key_bytes.len(),
            &mut refused_target,
        );
        assert!(!error_ptr.is_null(), "an unapproved 1:1 dialog must be refused a send target");
        assert_eq!(refused_target, 0);
        omni_string_free(error_ptr);

        // A group dialog does (its gate is membership + a live call, per packet). With no call
        // running the core drops the packet quietly — accepted, not refused.
        let group_dialog = b"group:ffi-media";
        let mut target = 0_u64;
        let error_ptr = omni_realtime_media_packet_target_create(
            handle,
            group_dialog.as_ptr(),
            group_dialog.len(),
            &mut target,
        );
        assert!(error_ptr.is_null(), "a group dialog resolves a send target");
        assert_ne!(target, 0);
        let context = target as usize as *mut std::ffi::c_void;
        omni_realtime_media_packet_sink(context, payload.as_ptr(), payload.len());
        // Negative half: an empty packet is refused, an unknown context is ignored without a crash.
        omni_realtime_media_packet_sink(context, payload.as_ptr(), 0);
        omni_realtime_media_packet_sink(std::ptr::null_mut(), payload.as_ptr(), payload.len());
        let (mut accepted, mut refused, mut test_dropped) = (0_u64, 0_u64, 0_u64);
        omni_realtime_media_packet_target_counters(
            target,
            &mut accepted,
            &mut refused,
            &mut test_dropped,
        );
        assert_eq!((accepted, refused, test_dropped), (1, 1, 0));
        omni_realtime_media_packet_target_destroy(target);
        // After destroy the same context is a dead ID: a late packet from an engine object that
        // outlived its call is ignored, not sent and not a crash.
        omni_realtime_media_packet_sink(context, payload.as_ptr(), payload.len());
        let mut after = u64::MAX;
        omni_realtime_media_packet_target_counters(
            target,
            &mut after,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(after, u64::MAX, "a destroyed target reports nothing");

        // Inbound: a sink wires and unwires for the dialog; a null dialog key is refused.
        unsafe extern "C" fn ignore_packet(_: *mut std::ffi::c_void, _: *const u8, _: usize) {}
        let wired = omni_realtime_media_set_inbound_sink(
            handle,
            dialog_key_bytes.as_ptr(),
            dialog_key_bytes.len(),
            Some(ignore_packet),
            std::ptr::null_mut(),
        );
        assert!(wired.is_null(), "a sink wires for the dialog");
        let unwired = omni_realtime_media_set_inbound_sink(
            handle,
            dialog_key_bytes.as_ptr(),
            dialog_key_bytes.len(),
            None,
            std::ptr::null_mut(),
        );
        assert!(unwired.is_null(), "a null sink unwires");
        let refused = omni_realtime_media_set_inbound_sink(
            handle,
            std::ptr::null(),
            0,
            None,
            std::ptr::null_mut(),
        );
        assert!(!refused.is_null(), "a missing dialog key is refused");
        omni_string_free(refused);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_send_text_disconnected_still_persists_queued_message_history() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-ffi-tests")
            .join(format!("{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");

        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());
        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "FFI Local".to_string(),
                device_label: "FFI Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let contact_request = CString::new(
            serde_json::to_string(&AddContactByIdentifierRequest {
                account_id: "acct-ffi-remote".to_string(),
                petname: "FFI Remote".to_string(),
                public_display_name: Some("FFI Remote".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let contact_ptr = omni_contacts_add_by_identifier(handle, contact_request.as_ptr());
        let contact_json = unsafe { CStr::from_ptr(contact_ptr) }
            .to_str()
            .expect("contact response should be valid utf-8")
            .to_string();
        let contact: ContactSnapshot = serde_json::from_str(&contact_json).unwrap();
        omni_string_free(contact_ptr);

        let dialog_key = dialog_key_from_instance_id("ffi-stage1-remote");
        let attach_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id,
                "dialog_key": dialog_key.clone(),
            })
            .to_string(),
        )
        .unwrap();
        let attach_ptr = omni_contacts_attach_dialog_to_contact(handle, attach_request.as_ptr());
        assert!(!attach_ptr.is_null());
        omni_string_free(attach_ptr);

        let send_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key.clone(),
                "body": "hello from ffi",
            })
            .to_string(),
        )
        .expect("send request should not contain null bytes");

        let send_response_ptr = omni_chat_send_text(handle, send_request.as_ptr());
        assert!(!send_response_ptr.is_null());
        let send_response = unsafe { CStr::from_ptr(send_response_ptr) }
            .to_str()
            .expect("send response should be valid utf-8")
            .to_string();
        assert!(send_response.contains("hello from ffi"));
        assert!(send_response.contains("\"queued\""));
        omni_string_free(send_response_ptr);

        let list_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key,
            })
            .to_string(),
        )
        .expect("list request should be valid");
        let list_response_ptr = omni_chat_list_messages(handle, list_request.as_ptr());
        assert!(!list_response_ptr.is_null());

        let list_response = unsafe { CStr::from_ptr(list_response_ptr) }
            .to_str()
            .expect("list response should be valid utf-8")
            .to_string();

        assert!(list_response.contains("hello from ffi"));
        assert!(list_response.contains("\"queued\""));

        omni_string_free(list_response_ptr);
        omni_app_free(handle);
    }

    #[test]
    fn ffi_queue_endpoints_expose_stage3_snapshots() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-ffi-tests")
            .join(format!("{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());
        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "FFI Local".to_string(),
                device_label: "FFI Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let contact_request = CString::new(
            serde_json::to_string(&AddContactByIdentifierRequest {
                account_id: "acct-ffi-queued".to_string(),
                petname: "FFI Queue".to_string(),
                public_display_name: Some("FFI Queue".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let contact_ptr = omni_contacts_add_by_identifier(handle, contact_request.as_ptr());
        let contact_json = unsafe { CStr::from_ptr(contact_ptr) }
            .to_str()
            .expect("contact response should be valid utf-8")
            .to_string();
        let contact: ContactSnapshot = serde_json::from_str(&contact_json).unwrap();
        omni_string_free(contact_ptr);

        let dialog_key = dialog_key_from_instance_id("ffi-stage3-queued");
        let attach_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id,
                "dialog_key": dialog_key.clone(),
            })
            .to_string(),
        )
        .unwrap();
        let attach_ptr = omni_contacts_attach_dialog_to_contact(handle, attach_request.as_ptr());
        assert!(!attach_ptr.is_null());
        omni_string_free(attach_ptr);

        let send_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key,
                "body": "hello queued from ffi",
            })
            .to_string(),
        )
        .expect("send request should not contain null bytes");
        let send_response_ptr = omni_chat_send_text(handle, send_request.as_ptr());
        omni_string_free(send_response_ptr);

        let snapshot_ptr = omni_queue_snapshot(handle);
        assert!(!snapshot_ptr.is_null());
        let snapshot = unsafe { CStr::from_ptr(snapshot_ptr) }
            .to_str()
            .expect("queue snapshot should be valid utf-8")
            .to_string();
        assert!(
            snapshot.contains("\"queued_count\": 1") || snapshot.contains("\"queued_count\":1")
        );
        assert!(
            snapshot.contains("hello queued from ffi")
                || snapshot.contains("\"status\": \"queued\"")
                || snapshot.contains("\"status\":\"queued\"")
        );
        omni_string_free(snapshot_ptr);

        let diagnostics_ptr = omni_queue_diagnostics(handle);
        assert!(!diagnostics_ptr.is_null());
        let diagnostics = unsafe { CStr::from_ptr(diagnostics_ptr) }
            .to_str()
            .expect("queue diagnostics should be valid utf-8")
            .to_string();
        assert!(
            diagnostics.contains("\"queue_length\": 1")
                || diagnostics.contains("\"queue_length\":1")
        );
        assert!(
            diagnostics.contains("\"queued_count\": 1")
                || diagnostics.contains("\"queued_count\":1")
        );
        omni_string_free(diagnostics_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_identity_endpoints_expose_stage4_snapshots() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("identity-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let request_json = serde_json::to_string(&CreateAccountRequest {
            display_name: "Alice".to_string(),
            device_label: "Office PC".to_string(),
        })
        .unwrap();
        let request = CString::new(request_json).unwrap();
        let created_ptr = omni_identity_create_account(handle, request.as_ptr());
        let created = unsafe { CStr::from_ptr(created_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            created.contains("\"display_name\": \"Alice\"")
                || created.contains("\"display_name\":\"Alice\"")
        );
        omni_string_free(created_ptr);

        let account_ptr = omni_identity_get_local_account(handle);
        let account = unsafe { CStr::from_ptr(account_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            account.contains("\"display_name\": \"Alice\"")
                || account.contains("\"display_name\":\"Alice\"")
        );
        omni_string_free(account_ptr);

        let device_ptr = omni_identity_get_local_device(handle);
        let device = unsafe { CStr::from_ptr(device_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            device.contains("\"device_label\": \"Office PC\"")
                || device.contains("\"device_label\":\"Office PC\"")
        );
        omni_string_free(device_ptr);

        let payload_ptr = omni_identity_get_my_verification_payload(handle);
        let payload = unsafe { CStr::from_ptr(payload_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(payload.contains("\"account_id\""));
        assert!(payload.contains("\"fingerprint\""));
        omni_string_free(payload_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_create_account_is_sanitized_and_reveal_requires_password() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("sanitized-reveal-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Reveal Test".to_string(),
                device_label: "Reveal Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, request.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(created_ptr);

        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        assert!(created_value.get("initial_recovery_phrase_v2").is_some());
        assert!(create_account_response_account(&created_value)
            .get("recovery_phrase_v2")
            .is_none());

        let profile_id = create_account_response_profile_id(&created_value).unwrap();
        let set_password_request = CString::new(
            serde_json::to_string(&SetLocalProfilePasswordRequest {
                profile_id: omni_identity::LocalProfileId(profile_id.clone()),
                password: "local-secret".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let set_password_ptr =
            omni_identity_set_local_profile_password(handle, set_password_request.as_ptr());
        omni_string_free(set_password_ptr);

        let account_ptr = omni_identity_get_local_account(handle);
        let account_json = unsafe { CStr::from_ptr(account_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(account_ptr);
        let account_value: serde_json::Value = serde_json::from_str(&account_json).unwrap();
        assert!(account_value.get("recovery_phrase_v2").is_none());

        let reveal_request = CString::new(
            serde_json::to_string(&RevealCurrentProfileRecoveryPhraseWithPasswordRequest {
                password: "local-secret".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let reveal_ptr = omni_identity_reveal_current_profile_recovery_phrase_with_password(
            handle,
            reveal_request.as_ptr(),
        );
        let reveal_json = unsafe { CStr::from_ptr(reveal_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(reveal_ptr);
        let reveal_value: serde_json::Value = serde_json::from_str(&reveal_json).unwrap();
        assert_eq!(reveal_value["version"].as_u64(), Some(2));
        assert!(reveal_value["mnemonic"].as_str().is_some());

        omni_app_free(handle);
    }

    #[test]
    fn ffi_restore_account_from_input_supports_mnemonic_v2() {
        let source_db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("source-mnemonic-{}.db", unique_suffix()));
        let target_db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("target-mnemonic-{}.db", unique_suffix()));

        let source_config = CString::new(format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&source_db_path.to_string_lossy())
        ))
        .unwrap();
        let target_config = CString::new(format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&target_db_path.to_string_lossy())
        ))
        .unwrap();

        let source_handle = omni_app_init(source_config.as_ptr());
        let target_handle = omni_app_init(target_config.as_ptr());
        assert!(!source_handle.is_null());
        assert!(!target_handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Mnemonic Source".to_string(),
                device_label: "Source Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(source_handle, create_request.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(created_ptr);

        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let mnemonic = create_account_response_mnemonic(&created_value).unwrap();

        let restore_request = CString::new(
            serde_json::json!({
                "display_name": "Mnemonic Source",
                "mnemonic": mnemonic,
                "device_label": "Target Device",
            })
            .to_string(),
        )
        .unwrap();
        let restored_ptr =
            omni_identity_restore_account_from_input(target_handle, restore_request.as_ptr());
        let restored_json = unsafe { CStr::from_ptr(restored_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(restored_ptr);

        assert!(restored_json.contains("\"account_representation_version\":2"));
        assert!(restored_json.contains("\"restored_from_recovery\":true"));

        omni_app_free(target_handle);
        omni_app_free(source_handle);
    }

    #[test]
    fn ffi_recovery_preview_and_reopen_contracts_are_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-ffi-tests")
            .join(format!("recovery-preview-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Mnemonic Source".to_string(),
                device_label: "Source Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(created_ptr);
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let profile_id = create_account_response_profile_id(&created_value).unwrap();
        let mnemonic = create_account_response_mnemonic(&created_value).unwrap();

        let preview_request = CString::new(
            serde_json::json!({
                "mnemonic": mnemonic,
                "display_name": "Mnemonic Source",
                "device_label": "Source Device"
            })
            .to_string(),
        )
        .unwrap();
        let preview_ptr = omni_identity_inspect_recovery_input(handle, preview_request.as_ptr());
        let preview_json = unsafe { CStr::from_ptr(preview_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(preview_ptr);
        let preview_value: serde_json::Value = serde_json::from_str(&preview_json).unwrap();
        assert_eq!(preview_value["flow_kind"].as_str(), Some("reopen_existing_local_profile"));
        assert_eq!(
            preview_value["mnemonic_usage"].as_str(),
            Some("root_basis_for_reopen")
        );
        assert_eq!(
            preview_value["conflict"].as_str(),
            Some("existing_profile_already_present")
        );
        assert_eq!(preview_value["can_reopen"].as_bool(), Some(true));

        let reopen_request = CString::new(
            serde_json::json!({ "profile_id": profile_id }).to_string(),
        )
        .unwrap();
        let reopen_ptr = omni_identity_reopen_local_profile(handle, reopen_request.as_ptr());
        let reopen_json = unsafe { CStr::from_ptr(reopen_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(reopen_ptr);
        let reopen_value: serde_json::Value = serde_json::from_str(&reopen_json).unwrap();
        assert_eq!(reopen_value["profile_id"].as_str(), Some(profile_id.as_str()));

        let active_ptr = omni_identity_get_active_profile(handle);
        let active_json = unsafe { CStr::from_ptr(active_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(active_ptr);
        let active_value: serde_json::Value = serde_json::from_str(&active_json).unwrap();
        assert_eq!(active_value.as_str(), Some(profile_id.as_str()));

        omni_app_free(handle);
    }

    #[test]
    fn ffi_current_profile_lifecycle_contract_endpoints_are_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-ffi-tests")
            .join(format!("lifecycle-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Lifecycle User".to_string(),
                device_label: "Lifecycle Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let gate_ptr = omni_identity_get_current_profile_gate_summary(handle);
        let gate_json = unsafe { CStr::from_ptr(gate_ptr) }.to_str().unwrap().to_string();
        let gate_value: serde_json::Value = serde_json::from_str(&gate_json).unwrap();
        assert_eq!(gate_value["state"].as_str(), Some("signed_in"));
        assert_eq!(gate_value["is_unlocked"].as_bool(), Some(true));
        omni_string_free(gate_ptr);

        let lifecycle_ptr = omni_identity_get_current_profile_lifecycle_summary(handle);
        let lifecycle_json = unsafe { CStr::from_ptr(lifecycle_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let lifecycle_value: serde_json::Value = serde_json::from_str(&lifecycle_json).unwrap();
        assert_eq!(lifecycle_value["state"].as_str(), Some("signed_in"));
        assert_eq!(lifecycle_value["messaging_allowed"].as_bool(), Some(true));
        omni_string_free(lifecycle_ptr);

        let preview_ptr = omni_identity_prepare_sign_out_current_profile(handle);
        let preview_json = unsafe { CStr::from_ptr(preview_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let preview_value: serde_json::Value = serde_json::from_str(&preview_json).unwrap();
        assert_eq!(preview_value["action"].as_str(), Some("sign_out_profile"));
        let token = preview_value["confirmation_token"]
            .as_str()
            .map(|value| value.to_string());
        omni_string_free(preview_ptr);

        let confirm_request = CString::new(
            serde_json::json!({ "confirmation_token": token }).to_string(),
        )
        .unwrap();
        let confirmed_ptr =
            omni_identity_confirm_sign_out_current_profile(handle, confirm_request.as_ptr());
        let confirmed_json = unsafe { CStr::from_ptr(confirmed_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let confirmed_value: serde_json::Value = serde_json::from_str(&confirmed_json).unwrap();
        assert_eq!(confirmed_value["state"].as_str(), Some("signed_out_locked"));
        omni_string_free(confirmed_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_explicit_sign_out_profile_contract_is_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-ffi-tests")
            .join(format!("explicit-signout-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "FFI Local".to_string(),
                device_label: "FFI Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        let created_json = unsafe { CStr::from_ptr(created_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let created_value: serde_json::Value = serde_json::from_str(&created_json).unwrap();
        let profile_id = create_account_response_profile_id(&created_value)
            .expect("profile id should exist");
        omni_string_free(created_ptr);

        let preview_request = CString::new(
            serde_json::json!({ "profile_id": profile_id }).to_string(),
        )
        .unwrap();
        let preview_ptr =
            omni_identity_preview_sign_out_profile(handle, preview_request.as_ptr());
        let preview_json = unsafe { CStr::from_ptr(preview_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let preview_value: serde_json::Value = serde_json::from_str(&preview_json).unwrap();
        assert_eq!(preview_value["action"].as_str(), Some("sign_out_profile"));
        let token = preview_value["confirmation_token"]
            .as_str()
            .map(|value| value.to_string());
        omni_string_free(preview_ptr);

        let confirm_request = CString::new(
            serde_json::json!({
                "profile_id": profile_id,
                "confirmation_token": token,
            })
            .to_string(),
        )
        .unwrap();
        let confirmed_ptr =
            omni_identity_confirm_sign_out_profile(handle, confirm_request.as_ptr());
        let confirmed_json = unsafe { CStr::from_ptr(confirmed_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let confirmed_value: serde_json::Value = serde_json::from_str(&confirmed_json).unwrap();
        assert_eq!(confirmed_value["state"].as_str(), Some("signed_out_locked"));
        omni_string_free(confirmed_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_active_profile_selection_contract_is_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-ffi-tests")
            .join(format!("active-profile-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let alice_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Alice".to_string(),
                device_label: "Alice Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let alice_ptr = omni_identity_create_account(handle, alice_request.as_ptr());
        let alice_json = unsafe { CStr::from_ptr(alice_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(alice_ptr);
        let alice_value: serde_json::Value = serde_json::from_str(&alice_json).unwrap();
        let alice_profile_id = create_account_response_profile_id(&alice_value).unwrap();

        let bob_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Bob".to_string(),
                device_label: "Bob Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let bob_ptr = omni_identity_create_account(handle, bob_request.as_ptr());
        let bob_json = unsafe { CStr::from_ptr(bob_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(bob_ptr);
        let bob_value: serde_json::Value = serde_json::from_str(&bob_json).unwrap();
        let bob_profile_id = create_account_response_profile_id(&bob_value).unwrap();

        let active_ptr = omni_identity_get_active_profile(handle);
        let active_json = unsafe { CStr::from_ptr(active_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(active_ptr);
        let active_value: serde_json::Value = serde_json::from_str(&active_json).unwrap();
        assert_eq!(active_value.as_str(), Some(bob_profile_id.as_str()));

        let select_request = CString::new(
            serde_json::json!({ "profile_id": alice_profile_id }).to_string(),
        )
        .unwrap();
        let select_ptr = omni_identity_select_active_profile(handle, select_request.as_ptr());
        assert!(!select_ptr.is_null());
        omni_string_free(select_ptr);

        let active_ptr = omni_identity_get_active_profile(handle);
        let active_json = unsafe { CStr::from_ptr(active_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(active_ptr);
        let active_value: serde_json::Value = serde_json::from_str(&active_json).unwrap();
        assert_eq!(active_value.as_str(), Some(alice_profile_id.as_str()));

        let account_ptr = omni_identity_get_local_account(handle);
        let account_json = unsafe { CStr::from_ptr(account_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(account_ptr);
        let account_value: serde_json::Value = serde_json::from_str(&account_json).unwrap();
        assert_eq!(
            account_value["local_profile_summary"]["profile_id"].as_str(),
            Some(alice_profile_id.as_str())
        );

        omni_app_free(handle);
    }

    #[test]
    fn ffi_final_stage13_profile_registry_and_footprint_contract_is_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-ffi-tests")
            .join(format!("final-stage13-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let alice_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Alice".to_string(),
                device_label: "Alice Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let alice_ptr = omni_identity_create_account(handle, alice_request.as_ptr());
        let alice_json = unsafe { CStr::from_ptr(alice_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(alice_ptr);
        let alice_value: serde_json::Value = serde_json::from_str(&alice_json).unwrap();
        let alice_profile_id = create_account_response_profile_id(&alice_value).unwrap();

        let bob_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Bob".to_string(),
                device_label: "Bob Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let bob_ptr = omni_identity_create_account(handle, bob_request.as_ptr());
        let bob_json = unsafe { CStr::from_ptr(bob_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(bob_ptr);
        let bob_value: serde_json::Value = serde_json::from_str(&bob_json).unwrap();
        let bob_profile_id = create_account_response_profile_id(&bob_value).unwrap();

        let list_ptr = omni_identity_list_local_profiles(handle);
        let list_json = unsafe { CStr::from_ptr(list_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(list_ptr);
        let list_value: serde_json::Value = serde_json::from_str(&list_json).unwrap();
        let profiles = list_value["profiles"]
            .as_array()
            .expect("profile list should contain profiles");
        assert_eq!(profiles.len(), 2);
        assert!(profiles
            .iter()
            .any(|profile| profile["profile_id"].as_str() == Some(alice_profile_id.as_str())));
        assert!(profiles
            .iter()
            .any(|profile| profile["profile_id"].as_str() == Some(bob_profile_id.as_str())));

        let select_request = CString::new(
            serde_json::json!({ "profile_id": alice_profile_id }).to_string(),
        )
        .unwrap();
        let select_ptr = omni_identity_select_active_profile(handle, select_request.as_ptr());
        assert!(!select_ptr.is_null());
        omni_string_free(select_ptr);

        let active_ptr = omni_identity_get_active_profile(handle);
        let active_json = unsafe { CStr::from_ptr(active_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(active_ptr);
        let active_value: serde_json::Value = serde_json::from_str(&active_json).unwrap();
        assert_eq!(active_value.as_str(), Some(alice_profile_id.as_str()));

        let contact_request = CString::new(
            serde_json::json!({
                "account_id": "acct-stage13-friend",
                "petname": "Stage13 Friend",
                "public_display_name": "Stage13 Friend"
            })
            .to_string(),
        )
        .unwrap();
        let contact_ptr = omni_contacts_add_by_identifier(handle, contact_request.as_ptr());
        let contact_json = unsafe { CStr::from_ptr(contact_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let contact: ContactSnapshot = serde_json::from_str(&contact_json).unwrap();
        omni_string_free(contact_ptr);

        let dialog_key = dialog_key_from_instance_id("acct-stage13-friend");
        let attach_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id,
                "dialog_key": dialog_key.clone(),
            })
            .to_string(),
        )
        .unwrap();
        let attach_ptr = omni_contacts_attach_dialog_to_contact(handle, attach_request.as_ptr());
        assert!(!attach_ptr.is_null());
        omni_string_free(attach_ptr);

        let send_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key,
                "body": "stage13 footprint message",
            })
            .to_string(),
        )
        .unwrap();
        let send_ptr = omni_chat_send_text(handle, send_request.as_ptr());
        omni_string_free(send_ptr);

        let footprint_request = CString::new(
            serde_json::json!({ "profile_id": alice_profile_id }).to_string(),
        )
        .unwrap();
        let footprint_ptr =
            omni_identity_get_local_profile_footprint_summary(handle, footprint_request.as_ptr());
        let footprint_json = unsafe { CStr::from_ptr(footprint_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(footprint_ptr);
        let footprint_value: serde_json::Value = serde_json::from_str(&footprint_json).unwrap();
        assert_eq!(
            footprint_value["profile_id"].as_str(),
            Some(alice_profile_id.as_str())
        );
        assert_eq!(footprint_value["contacts_count"].as_u64(), Some(1));
        assert_eq!(footprint_value["messages_count"].as_u64(), Some(1));

        let clear_ptr = omni_identity_clear_active_profile(handle);
        assert!(!clear_ptr.is_null());
        omni_string_free(clear_ptr);

        let active_ptr = omni_identity_get_active_profile(handle);
        let active_json = unsafe { CStr::from_ptr(active_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(active_ptr);
        let active_value: serde_json::Value = serde_json::from_str(&active_json).unwrap();
        assert!(active_value.is_null());

        let preview_request = CString::new(
            serde_json::json!({ "profile_id": bob_profile_id }).to_string(),
        )
        .unwrap();
        let preview_ptr = omni_identity_preview_delete_local_profile(handle, preview_request.as_ptr());
        let preview_json = unsafe { CStr::from_ptr(preview_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        omni_string_free(preview_ptr);
        let preview_value: serde_json::Value = serde_json::from_str(&preview_json).unwrap();
        assert_eq!(
            preview_value["profile_id"].as_str(),
            Some(bob_profile_id.as_str())
        );
        assert_eq!(preview_value["action"].as_str(), Some("delete_local_profile"));
        assert!(
            preview_value["summary"]
                .as_str()
                .unwrap_or_default()
                .contains("this local profile")
        );
        // Bob is not the last profile (Alice remains), so the preview must say the device-level
        // state stays — it is shared, and only the last profile out takes it along. The summary
        // deliberately no longer claims "only this local profile": for the LAST profile it is not
        // true, the device's mesh identity goes with it.
        let consequences = preview_value["consequences"]
            .as_array()
            .expect("consequences")
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            consequences.contains("Other local profiles stay intact"),
            "non-last-profile delete must promise the shared device state survives: {consequences}"
        );

        omni_app_free(handle);
    }

    #[test]
    fn ffi_contact_endpoints_support_identifier_and_verification_flows() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("contacts-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Local".to_string(),
                device_label: "Local PC".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let add_request = CString::new(
            serde_json::to_string(&AddContactByIdentifierRequest {
                account_id: "acct-remote".to_string(),
                petname: "Алиса".to_string(),
                public_display_name: Some("Alice".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let added_ptr = omni_contacts_add_by_identifier(handle, add_request.as_ptr());
        let added_json = unsafe { CStr::from_ptr(added_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        let contact: ContactSnapshot = serde_json::from_str(&added_json).unwrap();
        omni_string_free(added_ptr);

        let list_ptr = omni_contacts_list(handle);
        let list_json = unsafe { CStr::from_ptr(list_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(list_json.contains("acct-remote"));
        omni_string_free(list_ptr);

        let verification_payload = ContactVerificationPayloadV1 {
            version: CONTACT_VERIFICATION_PAYLOAD_VERSION_V1,
            account: AccountPublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                account_id: "acct-remote".to_string(),
                display_name: "Alice".to_string(),
            },
            devices: vec![DevicePublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                device_id: "dev-remote-1".to_string(),
                account_id: "acct-remote".to_string(),
                device_label: "Alice Laptop".to_string(),
            }],
            fingerprint: "fp-remote".to_string(),
            safety_number: "1111 2222 3333".to_string(),
            exported_at_unix_ms: 100,
        };

        let confirm_request = CString::new(
            serde_json::to_string(&ConfirmContactVerificationRequest {
                contact_id: contact.contact_id.clone(),
                payload: verification_payload,
                method: VerificationMethod::ManualCompare,
            })
            .unwrap(),
        )
        .unwrap();
        let confirmed_ptr = omni_contacts_confirm_verification(handle, confirm_request.as_ptr());
        let confirmed_json = unsafe { CStr::from_ptr(confirmed_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(confirmed_json.contains("\"verified\""));
        omni_string_free(confirmed_ptr);

        let card_request = CString::new(
            serde_json::to_string(&ContactIdRequest {
                contact_id: contact.contact_id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let card_ptr = omni_contacts_get_card(handle, card_request.as_ptr());
        let card_json = unsafe { CStr::from_ptr(card_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(card_json.contains("\"verified\""));
        omni_string_free(card_ptr);

        let state_ptr = omni_contacts_get_verification_state(handle, card_request.as_ptr());
        let state_json = unsafe { CStr::from_ptr(state_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(state_json.contains("\"verified\""));
        omni_string_free(state_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_can_add_contact_from_importable_payload() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("payload-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());
        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "FFI Local".to_string(),
                device_label: "FFI Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let payload = ImportableContactPayloadV1 {
            version: IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1,
            account: AccountPublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                account_id: "acct-import".to_string(),
                display_name: "Imported Alice".to_string(),
            },
            devices: vec![DevicePublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                device_id: "dev-import-1".to_string(),
                account_id: "acct-import".to_string(),
                device_label: "Imported Laptop".to_string(),
            }],
            fingerprint: "fp-import".to_string(),
            safety_number: "9999 8888 7777".to_string(),
            exported_at_unix_ms: 100,
        };

        let add_request = CString::new(
            serde_json::to_string(&AddContactFromPayloadRequest {
                payload,
                petname: "Импорт".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let added_ptr = omni_contacts_add_from_payload(handle, add_request.as_ptr());
        let added_json = unsafe { CStr::from_ptr(added_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(added_json.contains("acct-import"));
        assert!(added_json.contains("Импорт"));
        omni_string_free(added_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_can_request_contact_by_identifier() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-ffi-tests")
            .join(format!("request-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());
        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "FFI Local".to_string(),
                device_label: "FFI Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        omni_string_free(created_ptr);

        let request = CString::new(
            serde_json::to_string(&RequestContactByIdentifierRequest {
                account_id: "acct-request".to_string(),
                petname: "Запрос".to_string(),
                public_display_name: Some("Remote Display".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let requested_ptr = omni_contacts_request_by_identifier(handle, request.as_ptr());
        let requested_json = unsafe { CStr::from_ptr(requested_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(requested_json.contains("acct-request"));
        assert!(requested_json.contains("pending"));
        omni_string_free(requested_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_stage5_security_projection_endpoints_expose_typed_summaries_without_raw_secrets() {
        let local_db = std::env::temp_dir()
            .join("omni-stage-5-ffi-tests")
            .join(format!("local-{}.db", unique_suffix()));
        let remote_db = std::env::temp_dir()
            .join("omni-stage-5-ffi-tests")
            .join(format!("remote-{}.db", unique_suffix()));
        std::fs::create_dir_all(local_db.parent().unwrap()).unwrap();

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&local_db.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let local = unsafe { &mut *handle };
        local
            .inner
            .create_account("Local", "Local PC")
            .expect("local account should be created");

        let remote = omni_app::OmniApp::init(AppInitRequest {
            db_path: remote_db.to_string_lossy().to_string(),
        })
        .expect("remote app should initialize");
        let remote_account = remote
            .create_account("Remote", "Remote Laptop")
            .expect("remote account should be created");
        let bundle = remote
            .get_local_prekey_bundle()
            .expect("remote bundle should exist");
        let remote_verification_payload = contact_payload_from_my_payload(
            &remote
                .export_my_verification_payload()
                .expect("remote verification payload should exist"),
        )
        .expect("remote verification payload should be importable");

        local
            .inner
            .store_remote_prekey_bundle(&bundle)
            .expect("remote bundle should be stored");
        let contact = local
            .inner
            .add_contact_by_identifier(
                remote_account.descriptor.account_id.0.clone(),
                "Боб".to_string(),
                Some("Remote".to_string()),
            )
            .expect("contact should be added");
        let verification_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id.clone(),
                "payload": remote_verification_payload,
                "method": VerificationMethod::ChallengeResponse,
            })
            .to_string(),
        )
        .unwrap();
        let verified_ptr = omni_contacts_confirm_verification(handle, verification_request.as_ptr());
        assert!(!verified_ptr.is_null());
        omni_string_free(verified_ptr);
        // The two cores link over the mesh (loopback, dialed at the remote's address).
        local.inner.start_mesh().expect("local mesh starts");
        remote.start_mesh().expect("remote mesh starts");
        let remote_hex = remote.mesh_node_address_hex().unwrap().expect("remote mesh is up");
        let remote_addr = remote.local_transport_endpoint().expect("remote mesh address");
        wait_for_condition(Duration::from_secs(20), || {
            let _ = local.inner.mesh_dial_at(&remote_hex, &remote_addr);
            matches!(
                local.inner.transport_snapshot().state,
                TransportState::Connected | TransportState::ListeningAndConnected
            )
        });

        let connected_dialog_key = local
            .inner
            .transport_snapshot()
            .dialog_key
            .expect("connected session should expose dialog key");
        let attach_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id.0,
                "dialog_key": connected_dialog_key.clone(),
            })
            .to_string(),
        )
        .unwrap();
        let attach_ptr = omni_contacts_attach_dialog_to_contact(handle, attach_request.as_ptr());
        assert!(!attach_ptr.is_null());
        omni_string_free(attach_ptr);

        let sent = local
            .inner
            .send_text(connected_dialog_key.as_str(), "secret over ffi")
            .expect("encrypted message should be sent");

        let summary_request = CString::new(
            serde_json::to_string(&DialogKeyRequest {
                dialog_key: connected_dialog_key.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let summary_ptr = omni_chat_get_security_summary(handle, summary_request.as_ptr());
        let summary_json = unsafe { CStr::from_ptr(summary_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(summary_json.contains("\"uses_encrypted_transport\":true"));
        assert!(summary_json.contains("\"warning\""));
        assert!(!summary_json.contains("private_key"));
        assert!(!summary_json.contains("blob_hex"));
        assert!(!summary_json.contains("protected_session_key"));
        omni_string_free(summary_ptr);

        let details_request = CString::new(
            serde_json::to_string(&MessageIdRequest {
                message_id: sent.id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        wait_for_condition(Duration::from_secs(3), || {
            let details_ptr = omni_chat_get_message_security_details(handle, details_request.as_ptr());
            let details_json = unsafe { CStr::from_ptr(details_ptr) }
                .to_str()
                .unwrap()
                .to_string();
            omni_string_free(details_ptr);
            details_json.contains(&sent.id)
        });
        let details_ptr = omni_chat_get_message_security_details(handle, details_request.as_ptr());
        let details_json = unsafe { CStr::from_ptr(details_ptr) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(details_json.contains(&sent.id));
        assert!(details_json.contains("\"has_encrypted_payload\":false"));
        assert!(details_json.contains("\"encryption_state\""));
        assert!(!details_json.contains("private_key"));
        assert!(!details_json.contains("blob_hex"));
        assert!(!details_json.contains("protected_session_key"));
        omni_string_free(details_ptr);

        omni_app_free(handle);
    }

    #[test]
    fn ffi_prekey_bundle_export_and_import_roundtrip_is_wired() {
        let local_db = std::env::temp_dir()
            .join("omni-stage-5-ffi-tests")
            .join(format!("local-prekey-{}.db", unique_suffix()));
        let remote_db = std::env::temp_dir()
            .join("omni-stage-5-ffi-tests")
            .join(format!("remote-prekey-{}.db", unique_suffix()));
        std::fs::create_dir_all(local_db.parent().unwrap()).unwrap();

        let local_config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&local_db.to_string_lossy())
        );
        let remote_config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&remote_db.to_string_lossy())
        );
        let local_config_cstring =
            CString::new(local_config_json).expect("local config json should not contain null bytes");
        let remote_config_cstring =
            CString::new(remote_config_json).expect("remote config json should not contain null bytes");
        let local_handle = omni_app_init(local_config_cstring.as_ptr());
        let remote_handle = omni_app_init(remote_config_cstring.as_ptr());
        assert!(!local_handle.is_null());
        assert!(!remote_handle.is_null());

        let local = unsafe { &mut *local_handle };
        let remote = unsafe { &mut *remote_handle };
        local
            .inner
            .create_account("Local", "Local PC")
            .expect("local account should be created");
        remote
            .inner
            .create_account("Remote", "Remote Laptop")
            .expect("remote account should be created");

        let bundle_ptr = omni_identity_get_local_prekey_bundle(remote_handle);
        let bundle_json = unsafe { CStr::from_ptr(bundle_ptr) }
            .to_str()
            .expect("bundle json should be valid utf-8")
            .to_string();
        omni_string_free(bundle_ptr);

        let bundle: PrekeyBundleV1 = serde_json::from_str(&bundle_json)
            .expect("bundle json should deserialize into a protocol bundle");
        assert!(!bundle.account.account_id.is_empty());
        assert!(!bundle.device.device_id.is_empty());

        let bundle_request = CString::new(bundle_json).expect("bundle json should not contain null bytes");
        let stored_ptr = omni_identity_store_remote_prekey_bundle(local_handle, bundle_request.as_ptr());
        let stored_json = unsafe { CStr::from_ptr(stored_ptr) }
            .to_str()
            .expect("stored response should be valid utf-8")
            .to_string();
        omni_string_free(stored_ptr);

        let stored_value: serde_json::Value = serde_json::from_str(&stored_json)
            .expect("stored response should be valid json");
        assert!(stored_value.is_null(), "store_remote_prekey_bundle should return null on success");

        omni_app_free(local_handle);
        omni_app_free(remote_handle);
    }

    #[test]
    fn ffi_chat_delta_and_detail_revision_endpoints_are_wired() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-11-ffi-tests")
            .join(format!("delta-{}.db", unique_suffix()));

        let config_json = format!(
            r#"{{"db_path":"{}"}}"#,
            escape_json_string(&db_path.to_string_lossy())
        );
        let config_cstring =
            CString::new(config_json).expect("config json should not contain null bytes");
        let handle = omni_app_init(config_cstring.as_ptr());
        assert!(!handle.is_null());

        let create_request = CString::new(
            serde_json::to_string(&CreateAccountRequest {
                display_name: "Delta Local".to_string(),
                device_label: "Delta Device".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let created_ptr = omni_identity_create_account(handle, create_request.as_ptr());
        assert!(!created_ptr.is_null());
        omni_string_free(created_ptr);
        let contact_request = CString::new(
            serde_json::to_string(&AddContactByIdentifierRequest {
                account_id: "acct-ffi-delta".to_string(),
                petname: "Delta Remote".to_string(),
                public_display_name: Some("Delta Remote".to_string()),
            })
            .unwrap(),
        )
        .unwrap();
        let contact_ptr = omni_contacts_add_by_identifier(handle, contact_request.as_ptr());
        let contact_json = unsafe { CStr::from_ptr(contact_ptr) }
            .to_str()
            .expect("contact response should be valid utf-8")
            .to_string();
        let contact: ContactSnapshot =
            serde_json::from_str(&contact_json).expect("contact snapshot should parse");
        omni_string_free(contact_ptr);

        let contact_revision_request = CString::new(
            serde_json::to_string(&ContactIdRequest {
                contact_id: contact.contact_id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let contact_revision_ptr =
            omni_contacts_get_card_revision(handle, contact_revision_request.as_ptr());
        let contact_revision_json = unsafe { CStr::from_ptr(contact_revision_ptr) }
            .to_str()
            .expect("contact revision response should be valid utf-8")
            .to_string();
        let contact_revision_value: serde_json::Value =
            serde_json::from_str(&contact_revision_json).expect("contact revision should parse");
        omni_string_free(contact_revision_ptr);
        assert_eq!(
            contact_revision_value["entity_id"].as_str(),
            Some(contact.contact_id.0.as_str())
        );
        assert!(contact_revision_value["revision"].as_i64().is_some());

        let dialog_key = dialog_key_from_instance_id("ffi-delta");
        let attach_request = CString::new(
            serde_json::json!({
                "contact_id": contact.contact_id,
                "dialog_key": dialog_key.clone(),
            })
            .to_string(),
        )
        .unwrap();
        let attach_ptr = omni_contacts_attach_dialog_to_contact(handle, attach_request.as_ptr());
        assert!(!attach_ptr.is_null());
        omni_string_free(attach_ptr);

        let first_send_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key.clone(),
                "body": "first ffi delta message",
            })
            .to_string(),
        )
        .unwrap();
        let first_send_ptr = omni_chat_send_text(handle, first_send_request.as_ptr());
        assert!(!first_send_ptr.is_null());
        omni_string_free(first_send_ptr);

        let summary_request = CString::new(
            serde_json::to_string(&DialogKeyRequest {
                dialog_key: dialog_key.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let summary_ptr = omni_chat_get_message_refresh_summary(handle, summary_request.as_ptr());
        let summary_json = unsafe { CStr::from_ptr(summary_ptr) }
            .to_str()
            .expect("message refresh summary should be valid utf-8")
            .to_string();
        let summary_value: serde_json::Value =
            serde_json::from_str(&summary_json).expect("message refresh summary should parse");
        omni_string_free(summary_ptr);

        let second_send_request = CString::new(
            serde_json::json!({
                "peer_addr": dialog_key.clone(),
                "body": "second ffi delta message",
            })
            .to_string(),
        )
        .unwrap();
        let second_send_ptr = omni_chat_send_text(handle, second_send_request.as_ptr());
        assert!(!second_send_ptr.is_null());
        omni_string_free(second_send_ptr);

        let delta_request = CString::new(
            serde_json::json!({
                "dialog_key": dialog_key,
                "cursor": summary_value,
            })
            .to_string(),
        )
        .unwrap();
        let delta_ptr = omni_chat_get_message_delta(handle, delta_request.as_ptr());
        let delta_json = unsafe { CStr::from_ptr(delta_ptr) }
            .to_str()
            .expect("message delta response should be valid utf-8")
            .to_string();
        let delta_value: serde_json::Value =
            serde_json::from_str(&delta_json).expect("message delta should parse");
        omni_string_free(delta_ptr);
        assert_eq!(delta_value["requires_full_reload"].as_bool(), Some(false));
        assert!(delta_value["changed_messages"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["body"].as_str() == Some("second ffi delta message")
            })));
        assert!(delta_value["cursor"]["latest_message_id"].as_str().is_some());

        omni_app_free(handle);
    }

    fn unique_suffix() -> u128 {
        // Every test base here is named through this: sweep what earlier runs left in temp first.
        omni_storage::sweep_stale_test_temp();
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos()
    }

    fn wait_for_condition(timeout: Duration, predicate: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }

        panic!("condition was not satisfied within {:?}", timeout);
    }

    fn escape_json_string(value: &str) -> String {
        value.replace('\\', "\\\\")
    }

    /// A shell trace field never splits a record or shifts its columns; null pointers are a no-op.
    #[test]
    fn shell_trace_lines_stay_one_record_and_null_is_a_no_op() {
        let (category, message) =
            crate::shell_trace_fields("media/x", "boom: IOException\r\n   at Foo()\tline 3");
        assert_eq!(category, "media/x");
        assert!(!message.contains(['\r', '\n', '\t']), "{message:?}");
        assert_eq!(message, "boom: IOException     at Foo() line 3");
        // Negative half: nothing to write, nothing written — and no panic across the boundary.
        crate::omni_trace_write(std::ptr::null(), std::ptr::null());
        let category = CString::new("c").unwrap();
        crate::omni_trace_write(category.as_ptr(), std::ptr::null());
    }
}
