#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    
    use omni_identity::{
        AccountId, AddContactInput, ContactId,
        ContactObservationSnapshot, ContactRelationshipState, ContactRequestDirection,
        ContactSnapshot, DeviceId, LocalAccountSnapshot, LocalDeviceSnapshot, LocalProfileState,
        DangerousActionKind, ProfileLifecycleErrorCode, RecoveryConflictCode, RecoveryFlowKind,
        RecoveryMnemonicUsage, TrustState, VerificationMethod,
        ACCOUNT_REPRESENTATION_VERSION_V2,
    };
    use omni_messaging::{DialogDeltaCursor, SecurityPayloadKind, SecurityWarningKind};
    use omni_protocol::{
        AccountPublicDescriptorV1, ContactVerificationPayloadV1,
        DevicePublicDescriptorV1, ImportableContactPayloadV1, PrekeyBundleV1,
        CONTACT_VERIFICATION_PAYLOAD_VERSION_V1, IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1,
        PUBLIC_DESCRIPTOR_VERSION_V1,
    };
    use base64::Engine as _;
    use crate::{current_call_runtime, set_current_call_runtime};
    use omni_storage::Storage;
    use omni_transport::ReceivedChatContext;
    use omni_types::{
        dialog_key_from_instance_id, CallDirection, CallInboxSnapshot, CallNetworkQuality,
        CallOutcome, CallState, CallSummary,
        ChatDirection,
        ListMessagesPageRequest, MessageStatus, OmniError, QueueFailureKind, RealtimeMediaCapabilitiesV1,
        RealtimeMediaNegotiationStatus,
        RealtimeMediaSelectionV1, RealtimeVideoCodec, RealtimeVideoCodecCapabilityV1,
        RealtimeVideoProfileCapabilityV1, RealtimeVideoSelectionV1,
        RealtimeVideoPixelFormat,
        WirePayloadV1,
    };

    use super::{
        accept_incoming_text, attachments_directory_for_db_path, backup_bundle_directory,
        buffer_attachment_chunk_for_reorder, clear_attachment_reorder_buffer,
        choose_attachment_chunk_size_bytes, choose_local_device_name, current_unix_ms,
        call_link_health, call_link_transition, derive_call_network_quality, CallLinkHealth,
        CallLinkTransition, CALL_PEER_SILENCE_MS, CALL_RECONNECT_GRACE_MS,
        handle_session_activated, handle_session_closed,
        persist_incoming_contact_request_signal, persist_relay_set_update,
        record_pending_contact_creation_under_rate_limit,
        clear_pending_contact_rate_limits_for_profile,
        resolve_transport_contact_for_receive_context,
        lock_app_session_guard, session_lock_poison_recovery_count,
        runtime_chat_session_auto_reconnect_paused,
        select_realtime_media_configuration, with_capabilities_id,
        set_runtime_chat_session_confirmed_keepalive_at,
        clear_session_resync_for_profile, note_session_counter_gap, note_session_payload_accepted,
        SESSION_REBUILD_BUDGET, SESSION_REBUILD_BUDGET_WINDOW_MS,
        SESSION_RESYNC_GAP_THRESHOLD, SESSION_RESYNC_MIN_INTERVAL_MS,
        note_session_auth_failure, SESSION_RESYNC_AUTH_FAILURE_THRESHOLD,
        SESSION_RESYNC_FRESH_SESSION_GRACE_MS,
        cached_outgoing_envelope, clear_outgoing_envelopes_for_profile, forget_outgoing_envelope,
        outgoing_envelopes_in_flight_for_session,
        remember_outgoing_envelope, OUTGOING_ENVELOPE_CACHE_CAP,
        clear_session_reachability_for_profile, forget_session_reachability_for_remote_account,
        note_session_probe_answered, note_session_probe_sent, session_acknowledgement,
        note_session_outbound_progress_for_dialog,
        SESSION_PROBE_EXPIRY_MS, SESSION_PROBE_TRACKING_LIMIT,
        allow_session_recovery_attempt, session_acknowledgement_age_for_dialog,
        session_acknowledgement_age_for_remote_account, session_outbound_leg_is_proven_dead,
        session_outbound_leg_is_proven_dead_for_remote_account,
        assess_session_delivery, link_session_to_dialog, session_key_for_dialog,
        SessionAcknowledgement, SessionDeliveryEvidence, SessionDeliveryVerdict,
        SESSION_ACK_DEAD_MS, SESSION_ACK_FRESH_MS,
        ActiveCallRuntime, prekey_bundle_v1_from_raw,
        transport_mode_sidecar_path, AppInitRequest,
        transport_accept_session_bootstrap, OmniApp,
        clear_realtime_media_negotiation_for_dialog,
        dialog_has_active_media_negotiation, push_incoming_realtime_media_datagram,
        RealtimeMediaInboundSinks,
        ATTACHMENT_REORDER_BUFFER_MAX_BYTES,
    };

struct Stage5Peer {
    db_path: String,
    app: OmniApp,
    account: LocalAccountSnapshot,
    device: LocalDeviceSnapshot,
    bundle: PrekeyBundleV1,
    instance_id: String,
    dialog_key: String,
}

fn create_app_with_active_profile(
    db_path: String,
    display_name: &str,
    device_label: &str,
) -> OmniApp {
    let app = OmniApp::init(AppInitRequest { db_path }).unwrap();
    let account = app
        .create_account(display_name, device_label)
        .expect("account should be created");
    if let Some(profile_summary) = account.local_profile_summary.as_ref() {
        app.select_active_profile(&profile_summary.profile_id)
            .expect("active profile should be selectable");
    }
    app
}

#[test]
fn identity_beacon_prekey_bundle_serializes_as_wire_v1() {
    // Regression (found in the Model A cross-network live test): MA2's identity-beacon
    // publish serialized the RAW `omni_crypto::PrekeyBundle` (byte-array fields), but the
    // find-by-ID side (`find_contact_by_omni_id`) deserializes `PrekeyBundleV1` (hex
    // strings) → "invalid type: sequence, expected a string". Both sides must go through
    // `prekey_bundle_v1_from_raw`. The MA2/MA3 unit tests missed this because they hand-built
    // beacons with `prekey_bundle_json: "{}"` and never serialized a real bundle.
    let app = create_app_with_active_profile(
        isolated_test_db_path("omni-identity-beacon-wire", "prekey"),
        "Wire Bundle Alice",
        "Wire Laptop",
    );
    let now = current_unix_ms();
    let raw = app
        .storage
        .build_local_prekey_bundle(now)
        .expect("raw prekey bundle");
    let device_label = app
        .storage
        .get_local_device()
        .expect("local device query")
        .expect("local device present")
        .device_label;

    // What publish_identity_beacon now serializes MUST parse as the wire type the finder reads.
    let wire = prekey_bundle_v1_from_raw(&raw, device_label);
    let beacon_json = serde_json::to_string(&wire).expect("serialize wire bundle");
    let parsed: PrekeyBundleV1 = serde_json::from_str(&beacon_json)
        .expect("identity-beacon prekey bundle must parse as PrekeyBundleV1");
    assert_eq!(parsed, wire);
    // It agrees with the canonical GUI/manual producer (same one converter now).
    assert_eq!(
        app.get_local_prekey_bundle().expect("gui bundle").account.account_id,
        wire.account.account_id
    );
    // Guard the footgun: the RAW bundle must NOT masquerade as the wire V1 (byte arrays ≠ strings).
    let raw_json = serde_json::to_string(&raw).expect("serialize raw bundle");
    assert!(
        serde_json::from_str::<PrekeyBundleV1>(&raw_json).is_err(),
        "raw PrekeyBundle must not deserialize as wire PrekeyBundleV1"
    );
}

#[test]
fn orphaned_call_session_self_heals_instead_of_resurrecting_unkillable() {
    // Regression (live bug report): a process killed/crashed mid-call never runs
    // `end_runtime_call`, so its `call_sessions` row is left non-`ended` forever. Because
    // `CALL_RUNTIME_REGISTRY` is in-memory (per-process), the NEXT process for this profile has
    // an empty live registry — but `get_call_inbox_snapshot`'s storage fallback used to pick that
    // stale row as "the current call" on every future poll, rendering it as an apparently-live,
    // ever-growing-duration call the user could NEVER end: hang_up_call/decline_call only match
    // against the (empty) live registry, so both always failed with a generic "invalid input"
    // (the C# translation layer collapses either InvalidInput reason to the same message,
    // masking which one fired — see MapRustErrorToRussian's `_ => "Некорректные входные данные."`
    // fallback for the `calls_hang_up_call` operation, which isn't special-cased).
    let app = create_app_with_active_profile(
        isolated_test_db_path("omni-orphaned-call", "phantom"),
        "Alice",
        "Laptop",
    );

    // No live call was ever placed in THIS process (registry empty for this db_path) — simulate
    // a stale non-ended row left by a killed prior process, writing storage directly the way
    // `build_call_runtime_for_dialog` + `upsert_call_session` would have, but without ever
    // populating `CALL_RUNTIME_REGISTRY`.
    let now = current_unix_ms();
    let phantom = CallSummary {
        call_id: "call-out-phantom".to_string(),
        dialog_key: "dm:acct-phantom-peer".to_string(),
        contact_id: None,
        direction: CallDirection::Outgoing,
        state: CallState::Active,
        outcome: None,
        decline_reason: None,
        participants: Vec::new(),
        local_mic_muted: false,
        network_quality: None,
        started_at_unix_ms: now - 30 * 60_000,
        ringing_started_at_unix_ms: None,
        connected_at_unix_ms: Some(now - 30 * 60_000),
        ended_at_unix_ms: None,
        reconnect_deadline_unix_ms: None,
        updated_at_unix_ms: now - 30 * 60_000,
        remote_control: None,
    };
    app.storage
        .upsert_call_session(&phantom)
        .expect("seed the orphaned call row");

    // Before the fix this would return the phantom as `current_call` (Active, 30 min "old").
    let inbox: CallInboxSnapshot = app
        .get_call_inbox_snapshot()
        .expect("inbox snapshot self-heals rather than erroring");
    assert!(
        inbox.current_call.is_none(),
        "an orphaned row from a dead process must not resurrect as a live call: {:?}",
        inbox.current_call
    );

    // The row itself is finalized (not just hidden) — any OTHER read path (chat header's
    // get_latest_call_for_dialog, call history) now honestly sees it as ended/failed too.
    let finalized = app
        .storage
        .get_call_summary("call-out-phantom")
        .expect("read back the finalized row")
        .expect("row still exists, just finalized");
    assert_eq!(finalized.state, CallState::Ended);
    assert_eq!(finalized.outcome, Some(CallOutcome::Failed));

    // hang_up_call now correctly reports "nothing to hang up" instead of a confusing mismatch —
    // there is genuinely no call, live or phantom, left to act on.
    let hang_up_err = app
        .hang_up_call("call-out-phantom")
        .expect_err("no active call remains to hang up");
    assert!(matches!(hang_up_err, OmniError::InvalidInput(_)));
}

#[test]
fn a_call_that_could_not_be_accepted_stays_ringing_instead_of_going_active() {
    // Живой двухмашинный прогон 2026-08-29, три раза подряд: принимающий брал трубку, уходил в
    // `active` и запускал захват, а согласие в это время ложилось в durable «доставлю позже» —
    // живого пути к вызывающему не было. Вызывающий отваливался по таймауту, принимающий
    // оставался в звонке с пустотой И ОТКРЫТЫМ МИКРОФОНОМ.
    //
    // Причина была не в транспорте: `accept_call` писал `CallState::Active` в хранилище и в
    // живой реестр ДО отправки, поэтому судьба звонка вообще не зависела от того, уехало ли
    // согласие. Тест держит именно этот инвариант: не доставили — звонок остался звонящим.
    //
    // Без правки падает: звонок оказывается Active при том же самом отказе отправки.
    let db_path = isolated_test_db_path("omni-accept-delivery", "ringing");
    let app = create_app_with_active_profile(db_path.clone(), "Callee", "Laptop");

    // Входящий звонок, который УЖЕ звонит. Собеседник намеренно без сессии: отправка согласия
    // провалится — ровно то состояние, в котором согласию некуда ехать.
    let now = current_unix_ms();
    let summary = CallSummary {
        call_id: "call-in-unreachable".to_string(),
        dialog_key: "dm:acct-unreachable-peer".to_string(),
        contact_id: None,
        direction: CallDirection::Incoming,
        state: CallState::Ringing,
        outcome: None,
        decline_reason: None,
        participants: Vec::new(),
        local_mic_muted: false,
        network_quality: None,
        started_at_unix_ms: now,
        ringing_started_at_unix_ms: Some(now),
        connected_at_unix_ms: None,
        ended_at_unix_ms: None,
        reconnect_deadline_unix_ms: None,
        updated_at_unix_ms: now,
        remote_control: None,
    };
    app.storage.upsert_call_session(&summary).expect("seed the ringing call row");
    set_current_call_runtime(
        &db_path,
        Some(ActiveCallRuntime {
            summary: summary.clone(),
            remote_account_id: AccountId("acct-unreachable-peer".to_string()),
            remote_device_id: DeviceId("dev-unreachable-peer".to_string()),
            invite_expires_at_unix_ms: now + 60_000,
            invited_devices: Vec::new(),
        }),
    )
    .expect("seed the live call runtime");

    let error = app
        .accept_call("call-in-unreachable")
        .expect_err("acceptance that cannot be delivered must not succeed");
    assert!(
        matches!(error, OmniError::Transport(_)),
        "the failure has to be a transport one, not a state one: {error:?}"
    );

    // ГЛАВНОЕ: звонок не стал активным ни в живом реестре, ни в хранилище.
    let live = current_call_runtime(&db_path)
        .expect("live runtime readable")
        .expect("the call is still here — it was not ended, only not accepted");
    assert_eq!(
        live.summary.state,
        CallState::Ringing,
        "a call whose acceptance never left must stay ringing, not go active with the mic open"
    );
    assert!(
        live.summary.connected_at_unix_ms.is_none(),
        "nothing connected, so there is no connect time to record"
    );
    let stored = app
        .storage
        .get_call_summary("call-in-unreachable")
        .expect("read back the row")
        .expect("row exists");
    assert_eq!(stored.state, CallState::Ringing);
}

#[test]
fn a_dial_already_in_flight_is_not_reissued_every_tick() {
    // Дозвон до одобренных контактов ушёл с цикла обслуживания в фон (замер показал такт до 97 с
    // при бюджете 3 с, и на 100% это были синхронные дозвоны по 30 с). Но «в фон» без учёта уже
    // идущих попыток — это лавина: такт три секунды, дозвон до тридцати, и на ОДИН недостижимый
    // контакт пришлось бы десять параллельных попыток, каждая со своим соединением.
    //
    // Тест держит ровно этот инвариант — не больше и не меньше.
    let in_flight: std::sync::Arc<Mutex<std::collections::HashSet<crate::MeshNodeAddress>>> =
        std::sync::Arc::new(Mutex::new(std::collections::HashSet::new()));
    let node = crate::MeshNodeAddress::from_bytes([7u8; 20]);
    let other = crate::MeshNodeAddress::from_bytes([9u8; 20]);

    assert!(crate::claim_dial_slot(&in_flight, node), "первая попытка занимает место");
    assert!(
        !crate::claim_dial_slot(&in_flight, node),
        "пока попытка идёт, вторая не начинается — иначе такт плодит дозвоны поверх ещё живого"
    );
    assert!(
        crate::claim_dial_slot(&in_flight, other),
        "занятость ОДНОГО узла не мешает дозваниваться до других — ради этого всё и делалось"
    );

    crate::release_dial_slot(&in_flight, &node);
    assert!(
        crate::claim_dial_slot(&in_flight, node),
        "после завершения попытки место снова свободно"
    );
}

#[test]
fn a_peer_that_does_not_know_about_the_call_ends_it_but_only_when_it_is_safe() {
    // Потерянное управляющее сообщение оставляло принявшего в звонке, которого нет, С ОТКРЫТЫМ
    // МИКРОФОНОМ (живой прогон 2026-08-30). Сверка через presence это закрывает — но порвать ЖИВОЙ
    // звонок здесь легче, чем починить фантомный, поэтому каждое запрещающее условие проверяется
    // отдельно, а не «по построению».
    use omni_types::PresenceCallStateV1;

    let dialog = "dm:acct-peer";
    let now = 1_000_000_i64;
    let call = |state: CallState, connected_at: Option<i64>, dialog_key: &str| CallSummary {
        call_id: "call-1".to_string(),
        dialog_key: dialog_key.to_string(),
        contact_id: None,
        direction: CallDirection::Incoming,
        state,
        outcome: None,
        decline_reason: None,
        participants: Vec::new(),
        local_mic_muted: false,
        network_quality: None,
        started_at_unix_ms: now - 60_000,
        ringing_started_at_unix_ms: None,
        connected_at_unix_ms: connected_at,
        ended_at_unix_ms: None,
        reconnect_deadline_unix_ms: None,
        updated_at_unix_ms: now,
        remote_control: None,
    };
    let says_none = PresenceCallStateV1 { active_call_id: None };
    let says_other = PresenceCallStateV1 { active_call_id: Some("call-2".to_string()) };
    let says_same = PresenceCallStateV1 { active_call_id: Some("call-1".to_string()) };
    let long_ago = Some(now - 60_000);

    // РАДИ ЧЕГО ВСЁ: собеседник о звонке не знает, а звонок давно активен.
    assert!(crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, long_ago, dialog)), dialog, Some(&says_none), now));
    assert!(crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, long_ago, dialog)), dialog, Some(&says_other), now));

    // Собеседник назвал ТОТ ЖЕ звонок — всё в порядке, не трогаем.
    assert!(!crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, long_ago, dialog)), dialog, Some(&says_same), now));

    // СТАРАЯ СБОРКА не сказала ничего. Отсутствие поля — это «не знаю», а не «звонка нет»:
    // иначе новая сторона рвала бы живые звонки со старой по каждому пингу.
    assert!(!crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, long_ago, dialog)), dialog, None, now));

    // Гонка установления: звонок активен ВТОРУЮ секунду, собеседник ещё мог не узнать.
    assert!(!crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, Some(now - 2_000), dialog)), dialog, Some(&says_none), now));

    // Звонок ещё звонит — быть неизвестным собеседнику ему пока положено.
    assert!(!crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Ringing, None, dialog)), dialog, Some(&says_none), now));

    // ПИНГ ОТ ДРУГОГО КОНТАКТА не смеет закрывать этот звонок.
    assert!(!crate::should_end_call_after_peer_presence(
        Some(&call(CallState::Active, long_ago, dialog)), "dm:acct-somebody-else",
        Some(&says_none), now));

    // Звонка нет вовсе — закрывать нечего.
    assert!(!crate::should_end_call_after_peer_presence(None, dialog, Some(&says_none), now));
}

fn shutdown_stage5_peer(peer: &Stage5Peer) {
    peer.app.stop_mesh();
}

fn confirm_sign_out_profile(app: &OmniApp, profile_id: &omni_identity::LocalProfileId) {
    let preview = app
        .preview_sign_out_profile(profile_id)
        .unwrap()
        .expect("sign-out preview should exist");
    let token = preview
        .confirmation_token
        .as_deref()
        .expect("confirmation token should exist");
    app.confirm_sign_out_profile(profile_id, Some(token)).unwrap();
}

    /// The 1:1 consent leg without the 1:1 transport (covered by its own tests): `admin` invited
    /// `newcomer` (as `newcomer_dialog`), the newcomer accepted with a real KeyPackage, the admin adds
    /// it and the newcomer joins from the real MLS Welcome.
    fn group_join_for_real(admin: &OmniApp, newcomer: &OmniApp, group_id: &str, newcomer_dialog: &str) {
        admin.storage.record_group_sent_invite(group_id, newcomer_dialog, 1).unwrap();
        newcomer
            .storage
            .upsert_group_invite(&omni_storage::GroupInviteRecord {
                group_id: group_id.to_string(),
                name: "group".into(),
                inviter_name: "admin".into(),
                inviter_dialog_key: "dialog:admin".into(),
                status: "accepted".into(),
                received_at_unix_ms: 1,
            })
            .unwrap();
        let kp = super::app_groups::own_key_package(&newcomer.storage).unwrap();
        let accept = omni_protocol::GroupInviteAcceptV2 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            group_id: group_id.to_string(),
            key_package_b64: base64::engine::general_purpose::STANDARD.encode(kp),
        };
        // While the Welcome is on its way the invitation stays visible as "joining"...
        let joining = newcomer.list_group_invites().unwrap();
        assert!(joining.iter().any(|i| i.group_id == group_id && i.status == "accepted"));
        let welcome = super::app_groups::add_invitee(&admin.storage, newcomer_dialog, &accept)
            .unwrap()
            .expect("an invited newcomer is added");
        super::app_groups::join_from_welcome(&newcomer.storage, &newcomer.transport, &welcome).unwrap();
        // ...and is gone once we are in the group.
        assert!(newcomer.list_group_invites().unwrap().iter().all(|i| i.group_id != group_id));
    }

    /// Deliver what `from` queued for its group feeds to each of `to` (the mesh's job in production).
    fn deliver_group_feed(from: &OmniApp, to: &[&OmniApp]) {
        for (row, group_id, mls) in from.storage.list_group_feed_outbox(1_000).unwrap() {
            let item = omni_protocol::GroupFeedItemV2 {
                version: omni_protocol::GROUP_WIRE_VERSION_V2,
                group_id,
                mls_b64: base64::engine::general_purpose::STANDARD.encode(mls),
            };
            let json = serde_json::to_string(&item).unwrap();
            for app in to {
                super::app_groups::ingest_group_feed_item(&app.storage, &app.transport, &json).unwrap();
            }
            from.storage.delete_group_feed_outbox(row).unwrap();
        }
        super::app_groups::finish_deleted_groups(&from.storage, None);
    }

    fn member_pub_of(app: &OmniApp, group_id: &str, name: &str) -> String {
        app.group_members_view(group_id)
            .unwrap()
            .into_iter()
            .find(|m| m.display_name == name)
            .unwrap_or_else(|| panic!("{name} is a member"))
            .member_pub
    }

    /// Two ends of one in-memory pipe, shaped as the mesh hands a bulk stream over.
    fn bulk_pair() -> (omni_mesh_runtime::BulkStream, omni_mesh_runtime::BulkStream) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_recv, a_send) = tokio::io::split(a);
        let (b_recv, b_send) = tokio::io::split(b);
        (
            omni_mesh_runtime::BulkStream { send: Box::new(a_send), recv: Box::new(a_recv) },
            omni_mesh_runtime::BulkStream { send: Box::new(b_send), recv: Box::new(b_recv) },
        )
    }

    /// Serve one group pull from `holder_db` to `asker` on an in-memory stream; the asking half and
    /// the serving thread.
    fn serve_one_group_pull(
        holder_db: String,
        asker: omni_mesh_runtime::MeshNodeAddress,
    ) -> (omni_mesh_runtime::BulkStream, std::thread::JoinHandle<()>) {
        let (asking, holding) = bulk_pair();
        let server = std::thread::spawn(move || {
            if let super::app_group_attachments::InboundBulk::GroupPull(request, bulk) =
                super::app_group_attachments::classify_inbound_bulk(holding).unwrap()
            {
                super::app_group_attachments::serve_group_pull(&holder_db, asker, request, bulk, &|_| true);
            }
        });
        (asking, server)
    }

    /// A group file outlives its author's absence: Bob, who downloaded Alice's photo, hands it on to
    /// Carol while Alice is away — the same file, its hash checked. Until he has it whole he has
    /// nothing to give; a copy he changed afterwards is not served.
    #[test]
    fn a_member_who_downloaded_a_group_file_hands_it_on() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-seed", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-seed", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-seed", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Фото").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        group_join_for_real(&alice, &carol, &gid, "dialog:carol");
        deliver_group_feed(&alice, &[&bob, &carol]);
        let source = std::env::temp_dir().join("omni-groups-seed").join(format!("seed-{}.png", unique_suffix()));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        image::RgbImage::from_fn(640, 480, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]))
            .save(&source)
            .unwrap();
        alice.send_group_attachment(&gid, "", &source.to_string_lossy(), None, true).unwrap();
        deliver_group_feed(&alice, &[&bob, &carol]);
        let message_id = bob
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .find_map(|m| m.attachment.map(|a| a.message_id))
            .unwrap();
        let node_of = |app: &OmniApp, name: &str| {
            app.storage
                .list_group_members(&gid)
                .unwrap()
                .into_iter()
                .find(|m| m.display_name == name)
                .and_then(|m| super::app_groups::node_from_hex(&m.node_hint_hex))
                .unwrap()
        };
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();

        // Bob has nothing to give yet.
        let (asking, server) = serve_one_group_pull(db(&bob), node_of(&bob, "Carol"));
        assert!(super::app_group_attachments::pull_over(&db(&carol), asking, &gid, &message_id).is_err());
        server.join().unwrap();

        // Bob gets it from Alice — and from then on holds it for the others.
        let (asking, server) = serve_one_group_pull(db(&alice), node_of(&alice, "Bob"));
        super::app_group_attachments::pull_over(&db(&bob), asking, &gid, &message_id).unwrap();
        server.join().unwrap();
        let at_bob = bob.storage.get_attachment_runtime_info(&message_id).unwrap().unwrap();
        assert!(at_bob.attachment.local_serving_enabled, "a downloaded group file is served on");

        // Alice is away: Carol gets it from Bob.
        let (asking, server) = serve_one_group_pull(db(&bob), node_of(&bob, "Carol"));
        super::app_group_attachments::pull_over(&db(&carol), asking, &gid, &message_id).unwrap();
        server.join().unwrap();
        let at_carol = carol.storage.get_attachment_runtime_info(&message_id).unwrap().unwrap();
        assert_eq!(at_carol.attachment.state, omni_types::AttachmentTransferState::Downloaded);
        let saved_at_bob = at_bob.attachment.local_saved_path.clone().unwrap();
        let saved_at_carol = at_carol.attachment.local_saved_path.clone().unwrap();
        let (saved_at_bob, saved_at_carol) = (std::path::Path::new(&saved_at_bob), std::path::Path::new(&saved_at_carol));
        assert_eq!(bob.storage.read_media_file(saved_at_bob).unwrap(), carol.storage.read_media_file(saved_at_carol).unwrap());

        // Bob's copy changes: he stops handing it on.
        let mut changed = bob.storage.read_media_file(saved_at_bob).unwrap();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        std::thread::sleep(std::time::Duration::from_millis(20));
        bob.storage.write_media_file(saved_at_bob, &changed).unwrap();
        assert!(bob.storage.attachment_serving_blob_path(&message_id).is_err(), "a changed copy is not served");

    }

    /// A bulk stream's receiving half that counts what it reads and, given a limit, ends there — a
    /// holder that goes away mid-stream.
    struct CountedRead {
        inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        count: Arc<std::sync::atomic::AtomicU64>,
        cut_after: Option<u64>,
    }

    impl tokio::io::AsyncRead for CountedRead {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            use std::sync::atomic::Ordering;
            let read = self.count.load(Ordering::SeqCst);
            let room = match self.cut_after {
                Some(cut) if read >= cut => return std::task::Poll::Ready(Ok(())),
                Some(cut) => ((cut - read) as usize).min(buf.remaining()),
                None => buf.remaining(),
            };
            let mut limited = buf.take(room);
            let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, &mut limited);
            let filled = limited.filled().len();
            if let std::task::Poll::Ready(Ok(())) = polled {
                // SAFETY: `limited` wrote `filled` initialised bytes into `buf`'s unfilled part.
                unsafe { buf.assume_init(filled) };
                buf.advance(filled);
                self.count.fetch_add(filled as u64, Ordering::SeqCst);
            }
            polled
        }
    }

    /// Group members as in-memory holders for `pull_from_holders`: each node opens a stream served
    /// from that member's profile (to the asker as that member sees it), counted.
    #[derive(Clone)]
    struct MemoryHolders {
        /// node → (holder's db, the asker's node in the holder's view, bytes read from it, cut).
        holders: Arc<Mutex<std::collections::HashMap<omni_mesh_runtime::MeshNodeAddress, MemoryHolder>>>,
    }

    #[derive(Clone)]
    struct MemoryHolder {
        db: String,
        asker: omni_mesh_runtime::MeshNodeAddress,
        read: Arc<std::sync::atomic::AtomicU64>,
        cut_after: Option<u64>,
        gone: bool,
    }

    impl MemoryHolders {
        fn new() -> Self {
            Self { holders: Arc::new(Mutex::new(std::collections::HashMap::new())) }
        }

        fn add(&self, node: omni_mesh_runtime::MeshNodeAddress, db: String, asker: omni_mesh_runtime::MeshNodeAddress) -> Arc<std::sync::atomic::AtomicU64> {
            let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
            self.holders.lock().unwrap().insert(node, MemoryHolder { db, asker, read: Arc::clone(&read), cut_after: None, gone: false });
            read
        }

        fn update(&self, node: omni_mesh_runtime::MeshNodeAddress, change: impl FnOnce(&mut MemoryHolder)) {
            change(self.holders.lock().unwrap().get_mut(&node).unwrap());
        }

        fn open(&self, node: omni_mesh_runtime::MeshNodeAddress) -> omni_types::OmniResult<omni_mesh_runtime::BulkStream> {
            let holder = self.holders.lock().unwrap().get(&node).cloned().ok_or_else(|| OmniError::Transport("no such holder".into()))?;
            if holder.gone {
                return Err(OmniError::Transport("the holder is gone".into()));
            }
            let (asking, holding) = bulk_pair();
            std::thread::spawn(move || {
                if let Ok(super::app_group_attachments::InboundBulk::GroupPull(request, bulk)) =
                    super::app_group_attachments::classify_inbound_bulk(holding)
                {
                    super::app_group_attachments::serve_group_pull(&holder.db, holder.asker, request, bulk, &|_| true);
                }
            });
            Ok(omni_mesh_runtime::BulkStream {
                send: asking.send,
                recv: Box::new(CountedRead { inner: asking.recv, count: holder.read, cut_after: holder.cut_after }),
            })
        }
    }

    /// Alice and `members` in one group; Alice sends a file of `size` bytes (every byte names its
    /// place) and the offer reaches the others. Alice, the members, the group, the message, the bytes.
    fn group_with_a_file(tag: &str, members: &[&str], size: usize) -> (OmniApp, Vec<OmniApp>, String, String, Vec<u8>) {
        let alice = create_app_with_active_profile(isolated_test_db_path(tag, "alice"), "Alice", "Laptop");
        let gid = alice.create_group("Файлы").unwrap().group_id;
        let mut joined: Vec<OmniApp> = Vec::new();
        for name in members {
            let member = create_app_with_active_profile(isolated_test_db_path(tag, &name.to_lowercase()), name, "Laptop");
            group_join_for_real(&alice, &member, &gid, &format!("dialog:{}", name.to_lowercase()));
            joined.push(member);
            deliver_group_feed(&alice, &joined.iter().collect::<Vec<_>>());
        }
        let source = std::env::temp_dir().join(tag).join(format!("file-{}.bin", unique_suffix()));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let bytes: Vec<u8> = (0..size).map(|at| (at % 251) as u8 ^ (at / block) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        alice.send_group_attachment(&gid, "сборка", &source.to_string_lossy(), None, false).unwrap();
        let message_id = alice
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .find_map(|m| m.attachment.map(|a| a.message_id))
            .unwrap();
        wait_for_condition(Duration::from_secs(60), || {
            deliver_group_feed(&alice, &joined.iter().collect::<Vec<_>>());
            joined.iter().all(|app| app.storage.get_attachment_runtime_info(&message_id).ok().flatten().is_some())
        });
        (alice, joined, gid, message_id, bytes)
    }

    /// Alice, Bob and Carol in one group, with Alice's file offered to both.
    fn group_of_three_with_a_file(tag: &str, size: usize) -> (OmniApp, OmniApp, OmniApp, String, String, Vec<u8>) {
        let (alice, mut members, gid, message_id, bytes) = group_with_a_file(tag, &["Bob", "Carol"], size);
        let carol = members.pop().unwrap();
        let bob = members.pop().unwrap();
        (alice, bob, carol, gid, message_id, bytes)
    }

    fn group_node_of(app: &OmniApp, gid: &str, name: &str) -> omni_mesh_runtime::MeshNodeAddress {
        app.storage
            .list_group_members(gid)
            .unwrap()
            .into_iter()
            .find(|m| m.display_name == name)
            .and_then(|m| super::app_groups::node_from_hex(&m.node_hint_hex))
            .unwrap_or_else(|| panic!("{name} has a node"))
    }

    fn ask_group_file(app: &OmniApp, message_id: &str, tag: &str) -> PathBuf {
        let save = std::env::temp_dir().join(tag).join(format!("saved-{}.bin", unique_suffix()));
        app.request_attachment_download(message_id, save.to_string_lossy().as_ref())
            .expect("a member asks for a group file");
        save
    }

    /// A group download left `downloading` with no pull at it (a late ask's block landed after its
    /// pull gave up; or a restart) is pulled again by the tick, not left stuck (live 2026-10-16).
    #[test]
    fn a_group_download_under_way_with_no_pull_at_it_is_pulled_again() {
        let tag = "omni-groups-orphan";
        let (_alice, mut members, gid, message_id, _bytes) = group_with_a_file(tag, &["Bob"], 1000);
        let bob = members.pop().unwrap();
        ask_group_file(&bob, &message_id, tag);
        bob.storage.mark_attachment_downloading(&message_id, 0, current_unix_ms()).unwrap();
        let group = bob.storage.list_groups().unwrap().into_iter().find(|g| g.group_id == gid).unwrap();
        let picked = super::app_group_attachments::group_downloads_to_pull(&bob.storage, &group).unwrap();
        assert!(picked.iter().any(|item| item.attachment.message_id == message_id), "{picked:?}");
    }

    /// docs/decisions/attachment-content-tree.md, stage 3: Bob has the first two blocks of Alice's
    /// file, Carol asks both — and gets those blocks from Bob and the rest from Alice at once. Bob
    /// hands on what he has before he has it all, and only to a member.
    #[test]
    fn a_group_file_comes_from_several_holders_at_once_one_holding_part_of_it() {
        let tag = "omni-groups-multi";
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let (alice, bob, carol, gid, message_id, bytes) = group_of_three_with_a_file(tag, 4 * block + 1000);
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();
        let (alice_node, bob_node) = (group_node_of(&carol, &gid, "Alice"), group_node_of(&carol, &gid, "Bob"));

        // Bob gets blocks 0 and 1 from Alice (her first answer), then she is out of reach.
        let for_bob = MemoryHolders::new();
        for_bob.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Bob"));
        ask_group_file(&bob, &message_id, tag);
        let first_two = |storage: &Storage| {
            storage.attachment_received_blocks(&message_id).is_ok_and(|blocks| blocks.have[..2] == [true, true])
        };
        // (Asked again if her answer fails under load — the mesh tick does the same.)
        for _ in 0..3 {
            let pulled = super::app_group_attachments::pull_from_holders(
                &db(&bob),
                &gid,
                &message_id,
                |storage| if first_two(storage) { Vec::new() } else { vec![alice_node] },
                {
                    let for_bob = for_bob.clone();
                    move |node| for_bob.open(node)
                },
            );
            assert!(pulled.is_err(), "Bob has part of the file only");
            if first_two(&bob.storage) {
                break;
            }
        }
        let bob_has = bob.storage.attachment_received_blocks(&message_id).unwrap().have;
        assert_eq!(bob_has, vec![true, true, false, false, false], "Bob holds blocks 0 and 1");

        // A non-member asking Bob for what he has gets nothing.
        let stranger = MemoryHolders::new();
        stranger.add(bob_node, db(&bob), omni_mesh_runtime::MeshNodeAddress::from_bytes([9; 20]));
        let saved_at_carol = ask_group_file(&carol, &message_id, tag);
        let refused = super::app_group_attachments::pull_from_holders(&db(&carol), &gid, &message_id, |_| vec![bob_node], {
            let stranger = stranger.clone();
            move |node| stranger.open(node)
        });
        assert!(refused.is_err(), "a stranger gets nothing from a member");

        // Carol asks Bob and Alice: Bob's blocks come from Bob, the rest from Alice.
        let for_carol = MemoryHolders::new();
        let from_bob = for_carol.add(bob_node, db(&bob), group_node_of(&bob, &gid, "Carol"));
        let from_alice = for_carol.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Carol"));
        super::app_group_attachments::pull_from_holders(&db(&carol), &gid, &message_id, |_| vec![bob_node, alice_node], {
            let for_carol = for_carol.clone();
            move |node| for_carol.open(node)
        })
        .expect("the file comes whole");
        let at_carol = carol.storage.get_attachment_runtime_info(&message_id).unwrap().unwrap();
        assert_eq!(at_carol.attachment.state, omni_types::AttachmentTransferState::Downloaded);
        assert_eq!(fs::read(&saved_at_carol).unwrap(), bytes);
        let (bob_sent, alice_sent) = (
            from_bob.load(std::sync::atomic::Ordering::SeqCst),
            from_alice.load(std::sync::atomic::Ordering::SeqCst),
        );
        assert!(bob_sent >= 2 * block as u64, "Bob's two blocks came from Bob: {bob_sent} bytes");
        assert!(alice_sent >= 2 * block as u64, "the rest came from Alice: {alice_sent} bytes");
        assert!(bob_sent + alice_sent < (bytes.len() + 2 * block) as u64, "no block came twice over: {bob_sent} + {alice_sent}");
    }

    /// The author goes away in the middle of a block: what she sent is kept, and the rest comes from
    /// a member who has the file.
    #[test]
    fn a_group_file_comes_whole_when_its_author_leaves_mid_pull() {
        let tag = "omni-groups-author-leaves";
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let (alice, bob, carol, gid, message_id, bytes) = group_of_three_with_a_file(tag, 3 * block + 77);
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();
        let (alice_node, bob_node) = (group_node_of(&carol, &gid, "Alice"), group_node_of(&carol, &gid, "Bob"));

        // Bob has it whole.
        let for_bob = MemoryHolders::new();
        for_bob.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Bob"));
        ask_group_file(&bob, &message_id, tag);
        super::app_group_attachments::pull_from_holders(&db(&bob), &gid, &message_id, |_| vec![alice_node], {
            let for_bob = for_bob.clone();
            move |node| for_bob.open(node)
        })
        .unwrap();

        // Carol asks Alice first; Alice's stream ends a block and a half in, and she is gone.
        let for_carol = MemoryHolders::new();
        for_carol.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Carol"));
        let from_bob = for_carol.add(bob_node, db(&bob), group_node_of(&bob, &gid, "Carol"));
        for_carol.update(alice_node, |alice| alice.cut_after = Some((block + block / 2) as u64));
        let saved = ask_group_file(&carol, &message_id, tag);
        let gone = for_carol.clone();
        super::app_group_attachments::pull_from_holders(&db(&carol), &gid, &message_id, |_| vec![alice_node, bob_node], move |node| {
            let opened = gone.open(node);
            if node == alice_node {
                gone.update(alice_node, |alice| alice.gone = true);
            }
            opened
        })
        .expect("the rest comes from Bob");
        assert_eq!(fs::read(&saved).unwrap(), bytes);
        assert!(from_bob.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    /// A holder that sends a block that is not the file's (a member with the key can seal anything)
    /// is caught at its first chunk; the file comes whole from another holder.
    #[test]
    fn a_wrong_block_from_one_holder_is_refused_and_the_file_comes_whole_from_another() {
        let tag = "omni-groups-wrong-block";
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let (alice, _bob, carol, gid, message_id, bytes) = group_of_three_with_a_file(tag, 2 * block + 5);
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();
        let alice_node = group_node_of(&carol, &gid, "Alice");
        let mallory = omni_mesh_runtime::MeshNodeAddress::from_bytes([66; 20]);
        let tree = alice.storage.attachment_content_tree(&message_id).unwrap().unwrap();
        let saved = ask_group_file(&carol, &message_id, tag);
        let carol_db = db(&carol);
        let material = crate::attachment_content_crypto_material(&carol.storage.get_attachment_runtime_info(&message_id).unwrap().unwrap()).unwrap();

        let honest = MemoryHolders::new();
        honest.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Carol"));
        let mallory_asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let open = {
            let (honest, mallory_asked, tree, material) = (honest.clone(), Arc::clone(&mallory_asked), tree.clone(), material.clone());
            let real_leaves: Vec<omni_crypto::ContentHash> =
                bytes[..block].chunks(omni_crypto::CONTENT_TREE_LEAF_BYTES as usize).map(omni_crypto::content_leaf_hash).collect();
            move |node: omni_mesh_runtime::MeshNodeAddress| -> omni_types::OmniResult<omni_mesh_runtime::BulkStream> {
                if node != mallory {
                    return honest.open(node);
                }
                mallory_asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (asking, holding) = bulk_pair();
                let (tree, material, leaves) = (tree.clone(), material.clone(), real_leaves.clone());
                std::thread::spawn(move || {
                    let Ok(super::app_group_attachments::InboundBulk::GroupPull(request, bulk)) =
                        super::app_group_attachments::classify_inbound_bulk(holding)
                    else {
                        return;
                    };
                    // A note like any holder's ("I have it all"), then a block that is not the file's.
                    let mut send = bulk.send;
                    let note = br#"{"elsewhere":[]}"#;
                    let mut head = omni_protocol::GROUP_ATTACHMENT_NOTE_MAGIC.to_vec();
                    head.extend_from_slice(&(note.len() as u32).to_le_bytes());
                    head.extend_from_slice(note);
                    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let _ = runtime.block_on(async {
                        use tokio::io::AsyncWriteExt as _;
                        send.write_all(&head).await
                    });
                    let mut stream = omni_transport::AttachmentTransferStream::from_halves(send, bulk.recv).unwrap();
                    let at = current_unix_ms();
                    let start = crate::serialize_attachment_transfer_payload(&crate::AttachmentTransferStartPayloadV2 {
                        total_size_bytes: tree.size as i64,
                        content_id_hex: hex::encode(tree.id),
                        chunk_size_bytes: omni_crypto::CONTENT_TREE_LEAF_BYTES as u32,
                        block_layer: tree.blocks.clone(),
                    })
                    .unwrap();
                    let sealed = crate::seal_attachment_transfer_payload(&material, &request.transfer_id, "start", crate::ATTACHMENT_TRANSFER_COUNTER_START, at, &start).unwrap();
                    let _ = stream.write_frame(&omni_transport::AttachmentTransferFrameV2::Start(omni_transport::AttachmentTransferStartV2 {
                        transfer_id: request.transfer_id.clone(),
                        payload: sealed,
                        started_at_unix_ms: at,
                    }));
                    // Block 0's true proof, with a leaf of junk.
                    let proof = crate::AttachmentBlockProofV2 { leaves, path: tree.block_proof(0).unwrap() };
                    let junk = vec![0xEE_u8; omni_crypto::CONTENT_TREE_LEAF_BYTES as usize];
                    let chunk = crate::serialize_attachment_transfer_chunk_payload(0, &junk, Some(&proof)).unwrap();
                    let sealed = crate::seal_attachment_transfer_payload(&material, &request.transfer_id, "chunk", 1, at, &chunk).unwrap();
                    let _ = stream.write_frame(&omni_transport::AttachmentTransferFrameV2::Chunk(omni_transport::AttachmentTransferChunkV2 {
                        transfer_id: request.transfer_id.clone(),
                        chunk_index: 0,
                        payload: sealed,
                        sent_at_unix_ms: at,
                    }));
                });
                Ok(asking)
            }
        };
        super::app_group_attachments::pull_from_holders(&carol_db, &gid, &message_id, |_| vec![mallory, alice_node], open)
            .expect("the file comes whole from Alice");
        assert!(mallory_asked.load(std::sync::atomic::Ordering::SeqCst) >= 1, "Mallory was asked first");
        assert_eq!(fs::read(&saved).unwrap(), bytes);
    }

    /// docs/decisions/attachment-content-tree.md, stage 4: three members fetch Alice's file at once.
    /// Alice seeds — a block she just gave one member she names to the next asker instead of sending
    /// it again — and the members trade the rest, so her upload stays near one file, not three.
    #[test]
    fn a_seeding_author_sends_each_block_about_once_while_members_trade_the_rest() {
        let tag = "omni-groups-seed4";
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let names = ["Bob", "Carol", "Dave"];
        let (alice, members, gid, message_id, bytes) = group_with_a_file(tag, &names, 5 * block + 1000);
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();
        let mut pulls = Vec::new();
        let mut from_alice = Vec::new();
        for (index, asker) in members.iter().enumerate() {
            let holders = MemoryHolders::new();
            let mut order = Vec::new();
            let alice_node = group_node_of(asker, &gid, "Alice");
            from_alice.push(holders.add(alice_node, db(&alice), group_node_of(&alice, &gid, names[index])));
            order.push(alice_node);
            for (other_index, other) in members.iter().enumerate().filter(|(other_index, _)| *other_index != index) {
                let node = group_node_of(asker, &gid, names[other_index]);
                holders.add(node, db(other), group_node_of(other, &gid, names[index]));
                order.push(node);
            }
            let saved = ask_group_file(asker, &message_id, tag);
            let (asker_db, gid, message_id) = (db(asker), gid.clone(), message_id.clone());
            pulls.push((
                saved,
                std::thread::spawn(move || {
                    super::app_group_attachments::pull_from_holders(&asker_db, &gid, &message_id, move |_| order.clone(), move |node| {
                        holders.open(node)
                    })
                }),
            ));
        }
        for (saved, pull) in pulls {
            pull.join().unwrap().expect("each member gets the file");
            assert_eq!(fs::read(&saved).unwrap(), bytes);
        }
        let alice_sent: u64 = from_alice.iter().map(|read| read.load(std::sync::atomic::Ordering::SeqCst)).sum();
        let file = bytes.len() as u64;
        // Without seeding each member would take the whole file from Alice: three files. Seeding
        // makes it one (measured alone: each block once); a member's answer that fails under load
        // makes the next asker insist with Alice for that block (up to ~1.6 files measured with
        // five such tests at once). Two files at most is what tells seeding from no seeding.
        assert!(
            alice_sent <= file * 2,
            "Alice sent {alice_sent} bytes for a {file}-byte file to three members (at most two files)"
        );
    }

    /// Stage 4: Alice names Bob for the blocks she gave him, but Bob is gone — Carol is not left
    /// waiting on the name: she asks Alice again, insisting, and gets them.
    #[test]
    fn a_member_named_for_blocks_who_is_gone_is_not_waited_for() {
        let tag = "omni-groups-named-gone";
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        let (alice, bob, carol, gid, message_id, bytes) = group_of_three_with_a_file(tag, 3 * block + 7);
        let db = |app: &OmniApp| app.storage.db_path().to_string_lossy().into_owned();
        let alice_node = group_node_of(&carol, &gid, "Alice");
        let bob_node = group_node_of(&carol, &gid, "Bob");

        // Bob takes blocks 0 and 1 from Alice — she remembers giving them to him.
        let for_bob = MemoryHolders::new();
        for_bob.add(group_node_of(&bob, &gid, "Alice"), db(&alice), group_node_of(&alice, &gid, "Bob"));
        ask_group_file(&bob, &message_id, tag);
        let first_two = |storage: &Storage| {
            storage.attachment_received_blocks(&message_id).is_ok_and(|blocks| blocks.have[..2] == [true, true])
        };
        let bob_alice = group_node_of(&bob, &gid, "Alice");
        let _ = super::app_group_attachments::pull_from_holders(
            &db(&bob),
            &gid,
            &message_id,
            |storage| if first_two(storage) { Vec::new() } else { vec![bob_alice] },
            {
                let for_bob = for_bob.clone();
                move |node| for_bob.open(node)
            },
        );
        assert!(first_two(&bob.storage));

        // Bob is gone; Carol asks Alice (who names Bob) and Bob (who does not answer).
        let for_carol = MemoryHolders::new();
        let from_alice = for_carol.add(alice_node, db(&alice), group_node_of(&alice, &gid, "Carol"));
        for_carol.add(bob_node, db(&bob), group_node_of(&bob, &gid, "Carol"));
        for_carol.update(bob_node, |bob| bob.gone = true);
        let saved = ask_group_file(&carol, &message_id, tag);
        let started = Instant::now();
        let pull = {
            let (carol_db, gid, message_id, for_carol) = (db(&carol), gid.clone(), message_id.clone(), for_carol.clone());
            std::thread::spawn(move || {
                super::app_group_attachments::pull_from_holders(&carol_db, &gid, &message_id, move |_| vec![alice_node, bob_node], move |node| {
                    for_carol.open(node)
                })
            })
        };
        // Alice's blocks start coming at once, not after the name goes stale.
        while from_alice.load(std::sync::atomic::Ordering::SeqCst) < 1 << 20 && !pull.is_finished() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let first_bytes = started.elapsed();
        pull.join().unwrap().expect("the file comes whole from Alice");
        assert_eq!(fs::read(&saved).unwrap(), bytes);
        assert!(from_alice.load(std::sync::atomic::Ordering::SeqCst) >= bytes.len() as u64, "all of it from Alice");
        assert!(
            first_bytes < Duration::from_millis(crate::app_download_scheduler::HINT_TRUST_MS as u64 / 3),
            "not waited out the name: Alice's first MiB after {first_bytes:?}"
        );
    }

    /// A big file into a group (C2): the line is in the author's thread at once, «Подготовка», and its
    /// offer reaches the members only once the preparation has read the tree — under that tree.
    #[test]
    fn a_big_group_file_is_shown_at_once_and_offered_once_prepared() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-prepare", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-prepare", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Файлы").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        let source = std::env::temp_dir().join("omni-groups-prepare").join(format!("big-{}.bin", unique_suffix()));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let bytes: Vec<u8> = (0..super::app_attachment_preparation::PREPARE_INLINE_MAX_BYTES as usize + 4099)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        fs::write(&source, &bytes).unwrap();

        alice.send_group_attachment(&gid, "сборка", &source.to_string_lossy(), None, false).unwrap();
        let line = alice
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .find(|m| m.attachment.is_some())
            .expect("the line is in the thread at once");
        let message_id = line.attachment.unwrap().message_id;

        // The offer is sent by the preparation, right after it marks the file available.
        wait_for_condition(Duration::from_secs(30), || {
            deliver_group_feed(&alice, &[&bob]);
            bob.storage.get_attachment_runtime_info(&message_id).ok().flatten().is_some()
        });
        let at_bob = bob
            .storage
            .get_attachment_runtime_info(&message_id)
            .unwrap()
            .expect("the offer reached the member once prepared");
        let at_alice = alice.storage.get_attachment_runtime_info(&message_id).unwrap().unwrap();
        assert_eq!(at_bob.attachment.content_id_hex, at_alice.attachment.content_id_hex);
        assert_eq!(at_bob.attachment.file_size_bytes, bytes.len() as i64);
        assert_eq!(at_bob.attachment.state, omni_types::AttachmentTransferState::Offered);
    }

    /// A photo in a group: the offer rides the group, the photo asks for itself, and the author
    /// hands it over a bulk stream to a member — and to nobody else.
    #[test]
    fn a_group_photo_is_offered_through_the_group_and_pulled_from_its_author_by_members_only() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-files", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-files", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Фото").expect("create").group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);

        let source = std::env::temp_dir()
            .join("omni-groups-files")
            .join(format!("group-photo-{}.png", unique_suffix()));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        image::RgbImage::from_fn(1200, 800, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 40]))
            .save(&source)
            .unwrap();
        alice
            .send_group_attachment(&gid, "закат", &source.to_string_lossy(), None, true)
            .unwrap();
        deliver_group_feed(&alice, &[&bob]);

        let line = bob
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .find(|m| m.kind == "attachment")
            .expect("the offer reached bob");
        assert_eq!((line.sender_name.as_str(), line.body.as_str()), ("Alice", "закат"));
        let offered = line.attachment.expect("an attachment line carries its file");
        assert!(offered.presentation.is_some(), "it stays a photo at bob's");
        assert_eq!(offered.state, omni_types::AttachmentTransferState::Requested, "a photo asks for itself");
        let at_alice = alice.list_group_messages(&gid).unwrap();
        assert!(at_alice.iter().any(|m| m.is_mine && m.attachment.is_some()));

        let alice_db = alice.storage.db_path().to_string_lossy().into_owned();
        let bob_db = bob.storage.db_path().to_string_lossy().into_owned();
        let bob_node = alice
            .storage
            .list_group_members(&gid)
            .unwrap()
            .into_iter()
            .find(|m| m.display_name == "Bob")
            .and_then(|m| super::app_groups::node_from_hex(&m.node_hint_hex))
            .expect("bob has a node in alice's view");

        // A node that is no member is refused: the stream just ends, bob's file stays unfetched.
        let stranger = omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20]);
        let (asker, holder) = bulk_pair();
        let serve_db = alice_db.clone();
        let server = std::thread::spawn(move || {
            match super::app_group_attachments::classify_inbound_bulk(holder).unwrap() {
                super::app_group_attachments::InboundBulk::GroupPull(request, bulk) => {
                    super::app_group_attachments::serve_group_pull(&serve_db, stranger, request, bulk, &|_| true)
                }
                super::app_group_attachments::InboundBulk::Transfer(_) => panic!("a pull reads as a pull"),
            }
        });
        let refused = super::app_group_attachments::pull_over(&bob_db, asker, &gid, &offered.message_id);
        server.join().unwrap();
        assert!(refused.is_err(), "a stranger gets nothing");

        let (asker, holder) = bulk_pair();
        let serve_db = alice_db.clone();
        let server = std::thread::spawn(move || {
            if let super::app_group_attachments::InboundBulk::GroupPull(request, bulk) =
                super::app_group_attachments::classify_inbound_bulk(holder).unwrap()
            {
                super::app_group_attachments::serve_group_pull(&serve_db, bob_node, request, bulk, &|_| true);
            }
        });
        super::app_group_attachments::pull_over(&bob_db, asker, &gid, &offered.message_id).unwrap();
        server.join().unwrap();

        let fetched = bob
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .find(|m| m.message_id == offered.message_id)
            .and_then(|m| m.attachment)
            .unwrap();
        assert_eq!(fetched.state, omni_types::AttachmentTransferState::Downloaded);
        let saved = fetched.local_saved_path.expect("saved into the photo folder");
        // The photo folder is Omni's own storage: encrypted at rest, read through the core.
        assert_eq!(
            image::ImageReader::new(std::io::Cursor::new(bob.storage.read_media_file(std::path::Path::new(&saved)).unwrap()))
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (1200, 800)
        );

        // In the chat list: the caption, or «Фото» for a photo without one — not an empty line.
        assert_eq!(bob.list_group_summaries().unwrap()[0].last_message_preview, "Alice: закат");
        alice.send_group_attachment(&gid, "", &source.to_string_lossy(), None, true).unwrap();
        deliver_group_feed(&alice, &[&bob]);
        assert_eq!(alice.list_group_summaries().unwrap()[0].last_message_preview, "Вы: Фото");
        assert_eq!(bob.list_group_summaries().unwrap()[0].last_message_preview, "Alice: Фото");
    }

    /// The whole people-to-people path on MLS: three real profiles join through Invite → Accept →
    /// Commit + Welcome, talk with authors kept apart from the text, and a leaver is removed by the
    /// admin for everyone — after which they read nothing new. No synthesized keys anywhere.
    #[test]
    fn a_group_of_three_joins_talks_with_authors_and_a_leaver_is_removed() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Друзья").expect("create").group_id;

        // Negative half first: an accept from someone never invited does not get in.
        let stranger_accept = omni_protocol::GroupInviteAcceptV2 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            group_id: gid.clone(),
            key_package_b64: base64::engine::general_purpose::STANDARD
                .encode(super::app_groups::own_key_package(&bob.storage).unwrap()),
        };
        assert!(super::app_groups::add_invitee(&alice.storage, "dialog:stranger", &stranger_accept)
            .unwrap()
            .is_none());

        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        group_join_for_real(&alice, &carol, &gid, "dialog:carol");
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert_eq!(bob.group_members_view(&gid).unwrap().len(), 3, "bob sees carol arrive");
        let bob_summary = bob.list_group_summaries().unwrap();
        assert_eq!(bob_summary[0].name, "Друзья");
        assert_eq!(bob_summary[0].my_role, "member");

        bob.send_group_text(&gid, "привет всем").unwrap();
        deliver_group_feed(&bob, &[&alice, &carol]);
        let at_alice = alice.list_group_messages(&gid).unwrap();
        let hi = at_alice.iter().find(|m| m.body == "привет всем").expect("bob's message reached alice");
        assert_eq!((hi.sender_name.as_str(), hi.is_mine, hi.kind.as_str()), ("Bob", false, "text"));
        assert!(carol.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "привет всем"));
        assert!(bob.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "привет всем" && m.is_mine));
        // The group feed delivers it — the 1:1 retry queue must not hold it (it would wait for a
        // 1:1 receipt forever and show "retrying").
        assert!(bob.storage.outbound_queue_snapshot().unwrap().items.is_empty(), "a group line is not 1:1-queued");
        assert_eq!(bob.list_group_summaries().unwrap()[0].last_message_preview, "Вы: привет всем");
        assert_eq!(alice.list_group_summaries().unwrap()[0].last_message_preview, "Bob: привет всем");

        // Carol leaves; the admin's removal reaches everyone. Her chat goes at once — the farewell
        // waiting for a mesh keeps only the row it needs, and that goes once it is out.
        carol.leave_group(&gid).unwrap();
        assert!(carol.list_group_summaries().unwrap().is_empty(), "a left group is not a dead chat");
        let group_dialog = omni_storage::group_dialog_key(&gid);
        assert!(carol.storage.list_messages_for_dialog(&group_dialog).unwrap().is_empty());
        deliver_group_feed(&carol, &[&alice, &bob]);
        assert!(carol.storage.get_group(&gid).unwrap().is_none(), "nothing of it is left once the farewell is out");
        deliver_group_feed(&alice, &[&bob, &carol]);
        let bob_members = bob.group_members_view(&gid).unwrap();
        assert_eq!(bob_members.len(), 2, "{bob_members:?}");
        assert!(bob
            .list_group_messages(&gid)
            .unwrap()
            .iter()
            .any(|m| m.kind == "system" && m.body.contains("Carol")));

        // Negative half: what is said after she left does not reach Carol.
        alice.send_group_text(&gid, "без Кэрол").unwrap();
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert!(bob.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "без Кэрол"));
        assert!(carol.storage.get_group(&gid).unwrap().is_none(), "the group does not come back");
        assert!(carol.storage.list_messages_for_dialog(&group_dialog).unwrap().is_empty());
        assert!(bob.list_group_summaries().unwrap()[0].last_message_preview.contains("Alice: без Кэрол"));
    }

    /// A newcomer sees the recent history (owner decision 2026-10-11, on by default): Carol, added
    /// after Alice and Bob talked, gets their lines — each signed by its author, so Alice, who hands
    /// them over, could not have changed Bob's words. Bob, already in, gets nothing twice. Once an
    /// admin turns the switch off, Dave joins to an empty thread.
    #[test]
    fn a_newcomer_gets_the_recent_history_signed_by_its_authors() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-history", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-history", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-history", "carol"), "Carol", "Laptop");
        let dave = create_app_with_active_profile(isolated_test_db_path("omni-groups-history", "dave"), "Dave", "Laptop");
        let gid = alice.create_group("История").expect("create").group_id;
        assert!(alice.list_group_summaries().unwrap()[0].newcomer_history, "on by default");
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        alice.send_group_text(&gid, "первое").unwrap();
        deliver_group_feed(&alice, &[&bob]);
        bob.send_group_text(&gid, "второе").unwrap();
        deliver_group_feed(&bob, &[&alice]);

        group_join_for_real(&alice, &carol, &gid, "dialog:carol");
        deliver_group_feed(&alice, &[&bob, &carol]);
        let at_carol: Vec<(String, String)> = carol
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .filter(|m| m.kind == "text")
            .map(|m| (m.sender_name, m.body))
            .collect();
        assert_eq!(
            at_carol,
            vec![("Alice".to_string(), "первое".to_string()), ("Bob".to_string(), "второе".to_string())],
            "{at_carol:?}"
        );
        let texts_at_bob = bob.list_group_messages(&gid).unwrap().into_iter().filter(|m| m.kind == "text").count();
        assert_eq!(texts_at_bob, 2, "a member already in gets nothing twice");
        // Carol can hand the same lines on: she kept them with their authors' signatures.
        assert_eq!(carol.storage.list_recent_group_signed_lines(&gid, 100).unwrap().len(), 2);

        // A packet with one line changed: the signature of its author does not hold.
        let group = carol.storage.get_group(&gid).unwrap().unwrap();
        let mut packet: omni_protocol::GroupHistoryPacketV1 = omni_protocol::GroupHistoryPacketV1 {
            lines: carol
                .storage
                .list_recent_group_signed_lines(&gid, 100)
                .unwrap()
                .into_iter()
                .map(|line| omni_protocol::GroupHistoryLineV1 {
                    message_id: format!("{}-copy", line.message_id),
                    kind: omni_protocol::GroupAppKindV1::Text,
                    sender_pub: line.sender_pub,
                    sender_name: line.sender_name,
                    body: line.body,
                    authored_at_unix_ms: line.authored_at_unix_ms,
                    signature_b64: line.signature_b64,
                })
                .collect(),
        };
        packet.lines.truncate(1);
        let forged = omni_protocol::GroupAppPayloadV1 {
            message_id: "g-forged".into(),
            kind: omni_protocol::GroupAppKindV1::History,
            body: serde_json::to_string(&packet).unwrap(),
            authored_at_unix_ms: 1,
            signature_b64: None,
        };
        assert_eq!(super::app_group_history::receive_history(&carol.storage, &gid, &group, &forged).unwrap(), 0, "a renamed line breaks its signature");

        // The admin turns it off: Dave sees nothing from before.
        alice.set_group_newcomer_history(&gid, false).unwrap();
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert!(!bob.list_group_summaries().unwrap()[0].newcomer_history, "every member sees the switch");
        assert!(bob.set_group_newcomer_history(&gid, true).is_err(), "only an admin changes it");
        group_join_for_real(&alice, &dave, &gid, "dialog:dave");
        deliver_group_feed(&alice, &[&bob, &carol, &dave]);
        assert!(dave.list_group_messages(&gid).unwrap().iter().all(|m| m.kind != "text"));
    }

    /// A device restored from the phrase comes back into the account's group: Bob, a member, adds
    /// it because its key is a certified device of Alice's account and Alice is already in. The new
    /// device gets the Welcome and the recent history. Nobody else talks their way in that way: not
    /// an account that is not in the group, not a key Bob does not hold certified for the account,
    /// not a KeyPackage that names another account.
    #[test]
    fn a_device_restored_from_the_phrase_is_added_back_to_the_accounts_group() {
        use crate::app_group_rejoin::{add_account_device, device_join_request};
        let alice_db = isolated_test_db_path("omni-groups-rejoin", "alice");
        let alice = OmniApp::init(AppInitRequest { db_path: alice_db }).unwrap();
        let alice_account = alice.create_account("Alice", "Laptop").unwrap();
        let mnemonic = alice_account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();
        let account_id = alice_account.descriptor.account_id.clone();
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-rejoin", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-rejoin", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Семья").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        alice.send_group_text(&gid, "от старого ноутбука").unwrap();
        deliver_group_feed(&alice, &[&bob]);
        bob.send_group_text(&gid, "от Боба").unwrap();
        deliver_group_feed(&bob, &[&alice]);

        // The laptop is gone; the phrase brings the account to a new one.
        let restored = OmniApp::init(AppInitRequest {
            db_path: isolated_test_db_path("omni-groups-rejoin", "alice-restored"),
        })
        .unwrap();
        let account = restored.restore_account_from_mnemonic("Alice", &mnemonic, "New Laptop").unwrap();
        if let Some(summary) = account.local_profile_summary.as_ref() {
            restored.select_active_profile(&summary.profile_id).unwrap();
        }
        assert_eq!(account.descriptor.account_id, account_id);
        let old_pub = member_pub_of(&bob, &gid, "Alice");
        let (_, new_card) = super::app_groups::local_mls_identity(&restored.storage).unwrap();
        assert_ne!(old_pub, new_card.member_pub, "a new device, a new leaf");
        let known = vec![old_pub.clone(), new_card.member_pub.clone()];
        let now = current_unix_ms();
        let request = device_join_request(&restored.storage, &gid, "Семья", "dialog:bob", "Bob", now).unwrap();

        // Negative half first.
        let (_, carol_card) = super::app_groups::local_mls_identity(&carol.storage).unwrap();
        let carol_account = omni_identity::AccountId(carol_card.account_id.clone());
        let carol_request = device_join_request(&carol.storage, &gid, "Семья", "dialog:bob", "Bob", now).unwrap();
        assert!(add_account_device(&bob.storage, "dialog:carol", &carol_account, std::slice::from_ref(&carol_card.member_pub), &carol_request)
            .unwrap()
            .is_none(), "an account that is not in the group");
        assert!(add_account_device(&bob.storage, "dialog:alice-new", &account_id, std::slice::from_ref(&old_pub), &request)
            .unwrap()
            .is_none(), "a key not certified for the account");
        assert!(add_account_device(&bob.storage, "dialog:alice-new", &carol_account, &known, &request)
            .unwrap()
            .is_none(), "a KeyPackage of another account");
        assert_eq!(bob.group_members_view(&gid).unwrap().len(), 2);

        // Bob adds the new device; it joins with the Welcome and the recent history.
        let welcome = add_account_device(&bob.storage, "dialog:alice-new", &account_id, &known, &request)
            .unwrap()
            .expect("a new device of a member's account is added");
        super::app_groups::join_from_welcome(&restored.storage, &restored.transport, &welcome).unwrap();
        deliver_group_feed(&bob, &[&alice, &restored]);
        let summaries = restored.list_group_summaries().unwrap();
        assert_eq!((summaries.len(), summaries[0].name.as_str()), (1, "Семья"));
        // Three MLS members, two people: Alice's two devices are one row, "me" on both of hers.
        let people = bob.group_members_view(&gid).unwrap();
        assert_eq!(people.len(), 2, "{people:?}");
        assert_eq!(bob.list_group_summaries().unwrap()[0].active_member_count, 2);
        let at_restored = restored.group_members_view(&gid).unwrap();
        assert!(at_restored.iter().any(|p| p.is_me && p.account_id == account_id.0), "{at_restored:?}");
        assert_eq!(alice.group_members_view(&gid).unwrap().len(), 2, "the old device sees itself and Bob");
        assert_eq!(
            bob.storage.list_group_members(&gid).unwrap().iter().filter(|m| m.status == "member").count(),
            3,
            "each device is its own leaf"
        );
        let lines: Vec<String> = restored
            .list_group_messages(&gid)
            .unwrap()
            .into_iter()
            .filter(|m| m.kind == "text")
            .map(|m| m.body)
            .collect();
        assert_eq!(lines, vec!["от старого ноутбука".to_string(), "от Боба".to_string()], "{lines:?}");

        // It talks in the group; and asking again changes nothing.
        restored.send_group_text(&gid, "с нового").unwrap();
        deliver_group_feed(&restored, &[&alice, &bob]);
        assert!(bob.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "с нового"));
        let again = device_join_request(&restored.storage, &gid, "Семья", "dialog:bob", "Bob", now).unwrap();
        assert!(add_account_device(&bob.storage, "dialog:alice-new", &account_id, &known, &again).unwrap().is_none());

        // Alice is the admin: her new device becomes one with her, and leaves with her when removed.
        assert!(bob.group_members_view(&gid).unwrap().iter().any(|p| p.account_id == account_id.0 && p.role == "admin"));
        let bob_pub = member_pub_of(&alice, &gid, "Bob");
        alice.set_group_admin(&gid, &bob_pub, true).unwrap();
        deliver_group_feed(&alice, &[&bob, &restored]);
        bob.remove_group_member(&gid, &old_pub).unwrap();
        deliver_group_feed(&bob, &[&alice, &restored]);
        let left = bob.storage.list_group_members(&gid).unwrap();
        assert!(
            left.iter().filter(|m| m.account_id == account_id.0).all(|m| m.status != "member"),
            "removing a person removes every device of theirs: {left:?}"
        );
    }

    /// A device restored by the phrase types a name but has no photo yet: that is a placeholder, not an
    /// edit. Live 2026-10-13 it was stamped "now", so it beat the photo set earlier on the laptop: our
    /// other device took the photo off, and so did every group card.
    #[test]
    fn a_device_restored_by_the_phrase_does_not_wipe_the_accounts_photo() {
        let alice = OmniApp::init(AppInitRequest { db_path: isolated_test_db_path("omni-restore-profile", "alice") }).unwrap();
        let created = alice.create_account("Alice", "Laptop").unwrap();
        let mnemonic = created.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();
        let avatar_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("restore-profile-avatar-{}.png", unique_suffix()));
        fs::create_dir_all(avatar_path.parent().unwrap()).unwrap();
        fs::write(&avatar_path, tiny_png_bytes()).unwrap();
        let laptop = alice.update_local_public_profile("Alice", Some(&avatar_path), false).unwrap();
        thread::sleep(Duration::from_millis(5));

        let desktop = OmniApp::init(AppInitRequest { db_path: isolated_test_db_path("omni-restore-profile", "desktop") }).unwrap();
        let account = desktop.restore_account_from_mnemonic("Alice", &mnemonic, "Desktop").unwrap();
        if let Some(summary) = account.local_profile_summary.as_ref() {
            desktop.select_active_profile(&summary.profile_id).unwrap();
        }
        let restored = desktop.storage.get_local_public_profile_state().unwrap();
        assert_eq!(restored.avatar_bytes, None);

        // What the restored device would hand the laptop: older than the laptop's photo.
        let wiped = alice
            .storage
            .adopt_public_profile_from_own_device(Some(&restored.display_name), None, None, None, restored.updated_at_unix_ms)
            .unwrap();
        assert!(!wiped, "the restored placeholder does not replace the account's profile");
        assert_eq!(alice.storage.get_local_public_profile_state().unwrap().avatar_hash_hex, laptop.public_avatar_hash_hex);

        // The other way the laptop's photo comes in.
        let photo = alice.storage.get_local_public_profile_state().unwrap();
        assert!(desktop
            .storage
            .adopt_public_profile_from_own_device(
                Some(&photo.display_name),
                photo.avatar_hash_hex.as_deref(),
                photo.avatar_mime_type.as_deref(),
                photo.avatar_bytes.as_deref(),
                photo.updated_at_unix_ms,
            )
            .unwrap());

        // A group card from the restored device before that does not take the photo off either.
        let laptop_card = super::app_group_member_profiles::own_member_profile(&alice.storage).unwrap();
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-restore-profile", "bob"), "Bob", "Laptop");
        let account_id = created.descriptor.account_id.0.clone();
        bob.storage
            .store_group_member_profile(&account_id, laptop_card.version, "Alice", Some(&[0xFF, 0xD8, 0xFF, 0xD9]), current_unix_ms())
            .unwrap();
        assert!(!bob
            .storage
            .store_group_member_profile(&account_id, restored.updated_at_unix_ms, "Alice", None, current_unix_ms())
            .unwrap());
        assert!(bob.storage.get_group_member_profile(&account_id).unwrap().unwrap().avatar_path.is_some());
    }

    /// A second live device of the account comes into the account's groups (stage 4): the member
    /// device adds it by the account's own device list — no contact in between — and one ask per
    /// group in ten minutes is all the second device sends, however many of ours offer.
    #[test]
    fn a_second_device_of_the_account_is_added_to_its_groups_by_our_member_device() {
        use crate::app_group_rejoin::{account_device_pubs, add_account_device, answer_device_offer, device_join_request};
        let alice = OmniApp::init(AppInitRequest { db_path: isolated_test_db_path("omni-groups-stage4", "alice") }).unwrap();
        let created = alice.create_account("Alice", "Laptop").unwrap();
        let mnemonic = created.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();
        let own = created.descriptor.account_id.clone();
        let desktop = OmniApp::init(AppInitRequest { db_path: isolated_test_db_path("omni-groups-stage4", "desktop") }).unwrap();
        let account = desktop.restore_account_from_mnemonic("Alice", &mnemonic, "Desktop").unwrap();
        if let Some(summary) = account.local_profile_summary.as_ref() {
            desktop.select_active_profile(&summary.profile_id).unwrap();
        }
        // The two devices know each other (they met over the account's topic in production).
        let now = current_unix_ms();
        let desktop_chain = desktop.storage.get_local_device_certificate_chain().unwrap().unwrap();
        alice.storage.record_own_device(&desktop_chain, "Desktop", "dialog:desktop", now).unwrap();
        let laptop_chain = alice.storage.get_local_device_certificate_chain().unwrap().unwrap();
        desktop.storage.record_own_device(&laptop_chain, "Laptop", "dialog:laptop", now).unwrap();
        assert!(super::app_groups::own_device_dialog(&alice.storage, "dialog:desktop").unwrap());
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-stage4", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Дом").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        alice.send_group_text(&gid, "до второго устройства").unwrap();
        deliver_group_feed(&alice, &[&bob]);

        let known = account_device_pubs(&alice.storage, &own).unwrap();
        let (_, desktop_card) = super::app_groups::local_mls_identity(&desktop.storage).unwrap();
        assert!(known.contains(&desktop_card.member_pub), "our device list certifies the desktop");
        let request = device_join_request(&desktop.storage, &gid, "Дом", "dialog:laptop", "ваше устройство", now).unwrap();
        // A second offer while that ask is on its way is not answered (no second ask, nothing sent).
        let offer = omni_protocol::GroupInviteV2 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            group_id: gid.clone(),
            name: "Дом".into(),
            inviter_name: "Alice".into(),
            renew: false,
            authored_at_unix_ms: now,
        };
        answer_device_offer(&desktop.storage, &desktop.transport, "dialog:laptop", &offer).unwrap();

        let welcome = add_account_device(&alice.storage, "dialog:desktop", &own, &known, &request)
            .unwrap()
            .expect("our member device adds our other device");
        super::app_groups::join_from_welcome(&desktop.storage, &desktop.transport, &welcome).unwrap();
        deliver_group_feed(&alice, &[&bob, &desktop]);
        assert!(desktop
            .list_group_messages(&gid)
            .unwrap()
            .iter()
            .any(|m| m.body == "до второго устройства"));
        desktop.send_group_text(&gid, "с десктопа").unwrap();
        deliver_group_feed(&desktop, &[&alice, &bob]);
        let at_bob = bob.list_group_messages(&gid).unwrap();
        let line = at_bob.iter().find(|m| m.body == "с десктопа").expect("bob reads the desktop");
        assert_eq!(line.sender_name, "Alice");
        assert_eq!(bob.group_members_view(&gid).unwrap().len(), 2, "still two people");
        assert!(
            !at_bob.iter().any(|m| m.body == "Alice в группе"),
            "another device of someone already here is no «в группе» line: {:?}",
            at_bob.iter().map(|m| &m.body).collect::<Vec<_>>()
        );
        // At the laptop the desktop's line is ours.
        assert!(alice.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "с десктопа" && m.is_mine));
    }

    /// A member who is not our contact: its name and photo come from its own card in the group
    /// (docs/decisions/group-member-profile.md). The newcomer sends its card and asks for the others';
    /// a renamed member with a new photo reaches everyone; a card for another account is ignored.
    #[test]
    fn group_members_who_are_not_contacts_show_their_own_name_and_photo() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-profile", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-profile", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Карточки").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        // Bob's card and his ask reach Alice; she answers with hers.
        deliver_group_feed(&bob, &[&alice]);
        deliver_group_feed(&alice, &[&bob]);
        let alice_at_bob = || bob.group_members_view(&gid).unwrap().into_iter().find(|m| !m.is_me).unwrap();
        assert_eq!(alice_at_bob().display_name, "Alice");
        assert!(alice_at_bob().avatar_path.is_none(), "no photo yet");

        let source = std::env::temp_dir().join("omni-groups-profile").join(format!("me-{}.png", unique_suffix()));
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        image::RgbImage::from_fn(600, 600, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 120]))
            .save(&source)
            .unwrap();
        alice.update_local_public_profile("Алиса Новая", Some(&source), false).unwrap();
        deliver_group_feed(&alice, &[&bob]);
        let seen = alice_at_bob();
        assert_eq!(seen.display_name, "Алиса Новая");
        let photo = seen.avatar_path.expect("the card brought a photo");
        // Profile media is encrypted at rest: read through the core.
        assert!(omni_crypto::is_file_at_rest(std::path::Path::new(&photo)));
        let bytes = bob.storage.read_media_file(std::path::Path::new(&photo)).unwrap();
        assert!(bytes.len() <= omni_protocol::GROUP_MEMBER_AVATAR_MAX_BYTES);
        assert!(bytes.starts_with(&[0xFF, 0xD8]), "a JPEG");

        // A card claiming Alice's account, sent by Bob, changes nothing at Alice's or anyone's.
        let (_, bob_card) = super::app_groups::local_mls_identity(&bob.storage).unwrap();
        let alice_account = alice.storage.get_local_account().unwrap().unwrap().descriptor.account_id.0;
        let forged = omni_protocol::GroupMemberProfileV1 {
            account_id: alice_account.clone(),
            version: i64::MAX,
            display_name: "Не Алиса".into(),
            avatar_jpeg_b64: None,
        };
        let changed = super::app_group_member_profiles::receive_profile(&alice.storage, &bob_card, &serde_json::to_string(&forged).unwrap()).unwrap();
        assert!(!changed);
        assert_eq!(bob.storage.get_group_member_profile(&alice_account).unwrap().unwrap().display_name, "Алиса Новая");
    }

    /// The group splits for longer than one epoch — Alice alone, Bob and Carol together — and both
    /// sides rename it and talk. When they meet, the losing side asks to be put back, a member of the
    /// winning side puts it back, and everyone ends on one branch: one name, one epoch, every line of
    /// both sides in every chat. Whichever side wins.
    #[test]
    fn a_group_split_for_epochs_comes_back_together_with_both_sides_lines() {
        for round in 0..3 {
            let tag = format!("omni-groups-branches-{round}");
            let alice = create_app_with_active_profile(isolated_test_db_path(&tag, "alice"), "Alice", "Laptop");
            let bob = create_app_with_active_profile(isolated_test_db_path(&tag, "bob"), "Bob", "Laptop");
            let carol = create_app_with_active_profile(isolated_test_db_path(&tag, "carol"), "Carol", "Laptop");
            let gid = alice.create_group("Друзья").expect("create").group_id;
            group_join_for_real(&alice, &bob, &gid, "dialog:bob");
            deliver_group_feed(&alice, &[&bob]);
            group_join_for_real(&alice, &carol, &gid, "dialog:carol");
            deliver_group_feed(&alice, &[&bob, &carol]);
            let carol_pub = member_pub_of(&alice, &gid, "Carol");
            alice.set_group_admin(&gid, &carol_pub, true).unwrap();
            deliver_group_feed(&alice, &[&bob, &carol]);

            // Apart. The feed keeps every frame (the mesh's durable store): what Bob and Carol say
            // to each other reaches Alice too once they meet — kept here and handed over then.
            let mut kept: Vec<String> = Vec::new();
            let pass = |from: &OmniApp, to: &[&OmniApp], kept: &mut Vec<String>| {
                for (row, group_id, mls) in from.storage.list_group_feed_outbox(1_000).unwrap() {
                    let item = serde_json::to_string(&omni_protocol::GroupFeedItemV2 {
                        version: omni_protocol::GROUP_WIRE_VERSION_V2,
                        group_id,
                        mls_b64: base64::engine::general_purpose::STANDARD.encode(mls),
                    })
                    .unwrap();
                    for app in to {
                        super::app_groups::ingest_group_feed_item(&app.storage, &app.transport, &item).unwrap();
                    }
                    from.storage.delete_group_feed_outbox(row).unwrap();
                    kept.push(item);
                }
            };
            alice.rename_group(&gid, "Икс-1").unwrap();
            alice.send_group_text(&gid, "Алиса одна").unwrap();
            alice.rename_group(&gid, "Икс-2").unwrap();
            let mut alice_kept = Vec::new();
            pass(&alice, &[], &mut alice_kept);
            carol.rename_group(&gid, "Игрек-1").unwrap();
            pass(&carol, &[&bob], &mut kept);
            bob.send_group_text(&gid, "Боб с Кэрол").unwrap();
            pass(&bob, &[&carol], &mut kept);
            carol.rename_group(&gid, "Игрек-2").unwrap();
            pass(&carol, &[&bob], &mut kept);
            for item in &kept {
                super::app_groups::ingest_group_feed_item(&alice.storage, &alice.transport, item).unwrap();
            }
            for item in &alice_kept {
                for app in [&bob, &carol] {
                    super::app_groups::ingest_group_feed_item(&app.storage, &app.transport, item).unwrap();
                }
            }

            // Together again: everyone gets everything, the branch tick runs, until nothing moves.
            let apps = [&alice, &bob, &carol];
            let mut clock = current_unix_ms();
            for _ in 0..14 {
                clock += 6 * 60 * 1000;
                for app in apps {
                    super::app_group_branches::group_branch_tick(&app.storage, clock);
                }
                for (i, from) in apps.iter().enumerate() {
                    let to: Vec<&OmniApp> = apps.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| *a).collect();
                    deliver_group_feed(from, &to);
                }
            }

            let summaries: Vec<crate::GroupSummary> = apps.iter().map(|app| app.list_group_summaries().unwrap().remove(0)).collect();
            let names: Vec<&str> = summaries.iter().map(|s| s.name.as_str()).collect();
            assert!(names.iter().all(|name| *name == names[0]), "one name: {names:?}");
            assert!(names[0] == "Икс-2" || names[0] == "Игрек-2", "{names:?}");
            let epochs: Vec<u64> = summaries.iter().map(|s| s.epoch).collect();
            assert!(epochs.iter().all(|epoch| *epoch == epochs[0]), "one epoch: {epochs:?}");
            assert!(summaries.iter().all(|s| s.member_count == 3), "{summaries:?}");
            for app in apps {
                let texts: Vec<String> =
                    app.list_group_messages(&gid).unwrap().into_iter().filter(|m| m.kind == "text").map(|m| m.body).collect();
                assert!(texts.contains(&"Алиса одна".to_string()) && texts.contains(&"Боб с Кэрол".to_string()), "{texts:?}");
                assert!(app.storage.list_group_branch_states().unwrap().is_empty(), "nothing left pending");
            }
            // And the group works: a line from each reaches the others.
            for (i, from) in apps.iter().enumerate() {
                from.send_group_text(&gid, &format!("после встречи {i}")).unwrap();
                let to: Vec<&OmniApp> = apps.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, a)| *a).collect();
                deliver_group_feed(from, &to);
            }
            for app in apps {
                let texts: Vec<String> = app.list_group_messages(&gid).unwrap().into_iter().map(|m| m.body).collect();
                for i in 0..3 {
                    assert!(texts.contains(&format!("после встречи {i}")), "{texts:?}");
                }
            }
        }
    }

    /// An admin deletes the group for everyone: it goes from the admin's device, the members see who
    /// deleted it and can no longer write; then deleting the chat there leaves nothing. A member
    /// may not delete it for everyone, and a forged "deleted" from a member is ignored.
    #[test]
    fn an_admin_deletes_the_group_for_everyone() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-dissolve", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-dissolve", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-dissolve", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Поход").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        group_join_for_real(&alice, &carol, &gid, "dialog:carol");
        deliver_group_feed(&alice, &[&bob, &carol]);

        // Negative half: Bob is no admin.
        assert!(bob.delete_group_for_everyone(&gid).is_err());
        assert_eq!(alice.list_group_summaries().unwrap()[0].my_status, "member");

        // A forged "deleted" from a member changes nothing.
        let forged = omni_protocol::GroupAppPayloadV1 {
            message_id: "g-forged".into(),
            kind: omni_protocol::GroupAppKindV1::Dissolve,
            body: String::new(),
            authored_at_unix_ms: 1,
            signature_b64: None,
        };
        super::app_groups::send_app_payload(&bob.storage, &gid, &forged).unwrap();
        deliver_group_feed(&bob, &[&alice, &carol]);
        assert_eq!(carol.list_group_summaries().unwrap()[0].my_status, "member");

        // The last admin may not just walk out of a group with others in it.
        assert!(alice.leave_group(&gid).is_err());

        alice.delete_group_for_everyone(&gid).unwrap();
        assert!(alice.list_group_summaries().unwrap().is_empty());
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert!(alice.storage.get_group(&gid).unwrap().is_none());

        let at_bob = &bob.list_group_summaries().unwrap()[0];
        assert_eq!(at_bob.my_status, "dissolved");
        assert!(bob.list_group_invites().unwrap().is_empty(), "no invitation card for a group that is gone");
        assert!(bob
            .list_group_messages(&gid)
            .unwrap()
            .iter()
            .any(|m| m.kind == "system" && m.body == "Группа удалена (Alice)"));
        assert!(bob.send_group_text(&gid, "алло?").is_err(), "nobody writes into a deleted group");

        bob.leave_group(&gid).unwrap();
        assert!(bob.list_group_summaries().unwrap().is_empty());
        assert!(bob.storage.get_group(&gid).unwrap().is_none(), "nothing to say on the way out of a dead group");
        assert_eq!(carol.list_group_summaries().unwrap()[0].my_status, "dissolved");
    }

    /// A group whose rows went (an old build's migration dropped `groups`/`group_members`) comes back
    /// from its MLS state with its name, its members, and a feed that counts on; a group left here
    /// took its chat and stays gone.
    #[test]
    fn a_group_whose_rows_were_dropped_comes_back_from_its_mls_state() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-restore", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-restore", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Дача").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        alice.send_group_text(&gid, "до").unwrap();
        let left = alice.create_group("Ушёл").unwrap().group_id;
        alice.leave_group(&left).unwrap();

        // What the migration did: these two tables, nothing else (the outbox, the MLS state stay).
        omni_storage::Storage::open_keyed_connection(alice.storage.db_path())
            .unwrap()
            .execute_batch("DELETE FROM group_members; DELETE FROM groups;")
            .unwrap();
        assert!(alice.list_group_summaries().unwrap().is_empty());

        assert_eq!(super::app_groups::restore_groups_without_rows(&alice.storage), 1);
        let back = alice.list_group_summaries().unwrap();
        assert_eq!(back.len(), 1, "the left group stays gone");
        assert_eq!(back[0].name, "Дача");
        let members = alice.group_members_view(&gid).unwrap();
        assert_eq!(members.iter().filter(|m| m.status == "member").count(), 2);
        assert!(members.iter().any(|m| m.is_me && m.role == "admin"));
        assert!(alice.storage.next_group_feed_seq(&gid).unwrap() > 1, "the feed counts on past what it carried");
        assert_eq!(super::app_groups::restore_groups_without_rows(&alice.storage), 0, "once");

        alice.send_group_text(&gid, "после").unwrap();
        deliver_group_feed(&alice, &[&bob]);
        assert!(bob.list_group_messages(&gid).unwrap().iter().any(|m| m.body == "после"));
    }

    /// The group's picture: an admin sets it, every member gets the file and a line about it, a
    /// newcomer gets it from the Welcome; a member may not set it; clearing it reaches everyone.
    #[test]
    fn an_admin_sets_the_group_picture_for_everyone() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-avatar", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-avatar", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-avatar", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Фото").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        assert!(bob.list_group_summaries().unwrap()[0].avatar_path.is_none());

        let picture = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("group-avatar-{}.png", unique_suffix()));
        fs::create_dir_all(picture.parent().unwrap()).unwrap();
        fs::write(&picture, tiny_png_bytes()).unwrap();
        let picture = picture.to_string_lossy().to_string();

        // Negative half: Bob is no admin.
        assert!(bob.set_group_avatar(&gid, Some(&picture)).is_err());

        alice.set_group_avatar(&gid, Some(&picture)).unwrap();
        let at_alice = alice.list_group_summaries().unwrap()[0].avatar_path.clone().expect("alice has it");
        deliver_group_feed(&alice, &[&bob]);
        let at_bob = bob.list_group_summaries().unwrap()[0].avatar_path.clone().expect("bob got it");
        assert_eq!(
            alice.storage.read_media_file(std::path::Path::new(&at_alice)).unwrap(),
            bob.storage.read_media_file(std::path::Path::new(&at_bob)).unwrap(),
            "the same picture"
        );
        assert!(bob
            .list_group_messages(&gid)
            .unwrap()
            .iter()
            .any(|m| m.kind == "system" && m.body == "У группы появилась аватарка"));

        // A newcomer gets it from the Welcome, with no line about a change.
        group_join_for_real(&alice, &carol, &gid, "dialog:carol");
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert!(carol.list_group_summaries().unwrap()[0].avatar_path.is_some(), "carol got it");

        alice.set_group_avatar(&gid, None).unwrap();
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert!(bob.list_group_summaries().unwrap()[0].avatar_path.is_none());
        assert!(carol.list_group_summaries().unwrap()[0].avatar_path.is_none());
        assert!(bob
            .list_group_messages(&gid)
            .unwrap()
            .iter()
            .any(|m| m.kind == "system" && m.body == "Аватарка группы убрана"));
    }

    /// Any member adds; only an admin removes — a member's removal is refused before it leaves.
    #[test]
    fn a_member_may_add_but_not_remove() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls-roles", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls-roles", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-groups-mls-roles", "carol"), "Carol", "Laptop");
        let gid = alice.create_group("Команда").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        // Bob (member) adds Carol.
        group_join_for_real(&bob, &carol, &gid, "dialog:carol");
        deliver_group_feed(&bob, &[&alice, &carol]);
        assert_eq!(alice.group_members_view(&gid).unwrap().len(), 3);
        // Negative half: Bob may not remove Alice, nor rename.
        let alice_pub = member_pub_of(&bob, &gid, "Alice");
        assert!(bob.remove_group_member(&gid, &alice_pub).is_err());
        assert!(bob.rename_group(&gid, "Bob's").is_err());
        // The admin renames; everyone sees it.
        alice.rename_group(&gid, "Семья").unwrap();
        deliver_group_feed(&alice, &[&bob, &carol]);
        assert_eq!(carol.list_group_summaries().unwrap()[0].name, "Семья");
    }

    #[test]
    fn group_call_media_routes_through_group_path_not_1to1_approval() {
        // Proves the native-media wiring: a `group:<id>` dialog on the realtime
        // media send/drain path routes into the peer-SFU group path instead of the
        // 1:1 approval gate (which would reject a group dialog — it has no contact).
        let app = create_app_with_active_profile(
            isolated_test_db_path("omni-group-call-wiring", "route"),
            "Alice",
            "Laptop",
        );
        let summary = app.create_group("Squad").expect("create group");
        let group_dialog = summary.dialog_key.clone();

        // Start a call: full mesh at 1 participant, we are it.
        let snapshot = app.start_group_call(&summary.group_id).expect("start call");
        assert_eq!(snapshot.topology, "full_mesh");
        assert_eq!(snapshot.participant_count, 1);
        assert!(app.group_call_snapshot(&summary.group_id).unwrap().is_some());

        // Sending media on the group dialog must NOT hit the 1:1 approval gate
        // (which errors for a contactless dialog) — it routes into the group path
        // and, with no co-members/mesh, is a no-op Ok.
        let group_route = app
            .realtime_media_route(&group_dialog)
            .expect("a group dialog resolves without the 1:1 approval gate");
        app.send_realtime_media_packet(&group_route, &[0x80, 0x60, 1, 2, 3])
            .expect("group media send routes through the group path");

        // Wiring the group dialog's receive side must NOT hit the 1:1 gate either.
        app.set_realtime_media_inbound_sink(&group_dialog, Some(Arc::new(|_: &[u8]| {})))
            .expect("group media sink does not hit the 1:1 approval gate");
        app.set_realtime_media_inbound_sink(&group_dialog, None)
            .expect("group media sink unwires");

        // A group receiver's RTCP target (`#fb:<sender tag>`) is a group route too: no 1:1 gate,
        // and with no such sender in the call it is a no-op. Negative half: a malformed tag is not
        // a feedback target and falls to the 1:1 gate, which rejects it.
        let feedback_key = format!("{group_dialog}#fb:0102030405060708");
        let feedback_route = app.realtime_media_route(&feedback_key).expect("feedback target resolves");
        app.send_realtime_media_packet(&feedback_route, &[0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7])
            .expect("feedback to an unknown sender is dropped quietly");
        assert!(app.realtime_media_route(format!("{group_dialog}#fb:zz")).is_err());

        // Contrast: a 1:1 dialog with no approved contact is rejected by the gate.
        assert!(app.realtime_media_route("dm:nobody").is_err());

        // Leaving clears the call state.
        app.leave_group_call(&summary.group_id).expect("leave call");
        assert!(app.group_call_snapshot(&summary.group_id).unwrap().is_none());
    }

    /// A call rings, it does not join: Bob hears Alice's Invite, sees it ringing, and is NOT in the call
    /// until he joins. Then mute/camera badges reach only the sender's tile (the sender is the
    /// MLS-authenticated leaf — a member cannot spoof another's badge).
    #[test]
    fn group_call_rings_joins_by_consent_and_badges_only_sender() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-call-consent", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-call-consent", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);
        // Bob's card and his ask for theirs (group-member-profile.md) go out on joining — before the call.
        deliver_group_feed(&bob, &[&alice]);
        deliver_group_feed(&alice, &[&bob]);
        let bob_pub = member_pub_of(&alice, &gid, "Bob");

        let snapshot = alice.start_group_call(&gid).expect("start call");
        assert_eq!(snapshot.participant_count, 1, "only the starter is in a fresh call");
        deliver_group_feed(&alice, &[&bob]);

        // Negative half: hearing the Invite does not put Bob in the call, and he sends nothing.
        assert!(bob.group_call_snapshot(&gid).unwrap().is_none(), "an Invite must not join Bob");
        assert!(bob.storage.list_group_feed_outbox(10).unwrap().is_empty(), "Bob announces nothing");
        let inbox = bob.group_call_inbox().unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].ringing && !inbox[0].joined);
        assert_eq!(inbox[0].started_by_name, "Alice");
        assert_eq!(inbox[0].participant_count, 1);

        // Bob answers. Alice learns him from the Join and replies with her presence.
        let joined = bob.join_group_call(&gid).expect("join");
        assert_eq!(joined.call_id, snapshot.call_id);
        assert_eq!(joined.participant_count, 2);
        deliver_group_feed(&bob, &[&alice]);
        deliver_group_feed(&alice, &[&bob]);
        let snap = alice.group_call_snapshot(&gid).unwrap().unwrap();
        assert!(snap.members.iter().any(|m| m.member_pub == bob_pub), "Alice sees Bob in the call");
        assert!(bob.group_call_inbox().unwrap()[0].joined);

        // Our own broadcast badges our tile; Bob is untouched.
        alice.set_group_call_media_state(&gid, true, false, true).expect("announce own state");
        let snap = alice.group_call_snapshot(&gid).unwrap().unwrap();
        let me = snap.members.iter().find(|m| m.is_self).unwrap();
        assert!(me.is_muted && !me.is_camera_off && me.is_screen_sharing);
        let bob_tile = snap.members.iter().find(|m| m.member_pub == bob_pub).unwrap();
        assert!(!bob_tile.is_muted && bob_tile.is_camera_off && !bob_tile.is_screen_sharing);

        // Bob announces muted + camera on; it reaches Alice over the group feed.
        bob.set_group_call_media_state(&gid, true, false, false).expect("bob announces");
        deliver_group_feed(&bob, &[&alice]);
        let snap = alice.group_call_snapshot(&gid).unwrap().unwrap();
        let bob_tile = snap.members.iter().find(|m| m.member_pub == bob_pub).unwrap();
        assert!(bob_tile.is_muted && !bob_tile.is_camera_off, "Bob's badge reflects HIS broadcast");
        let me = snap.members.iter().find(|m| m.is_self).unwrap();
        assert!(me.is_muted && !me.is_camera_off, "our own state unchanged by Bob's control");

        // Bob leaves: Alice drops him; Bob still sees the call (quietly) and can come back.
        bob.leave_group_call(&gid).unwrap();
        deliver_group_feed(&bob, &[&alice]);
        assert_eq!(alice.group_call_snapshot(&gid).unwrap().unwrap().participant_count, 1);
        let inbox = bob.group_call_inbox().unwrap();
        assert!(!inbox[0].joined && !inbox[0].ringing, "a left call is listed without ringing");
        // Alice leaves too: the call is over for Bob.
        alice.leave_group_call(&gid).unwrap();
        deliver_group_feed(&alice, &[&bob]);
        assert!(bob.group_call_inbox().unwrap().is_empty());
    }

    fn group_control_from(dialog_key: &str, control: &str, payload_json: String) -> (ReceivedChatContext, omni_types::GroupControlV1) {
        (
            ReceivedChatContext {
                peer_id: "127.0.0.1:0".to_string(),
                dialog_key: dialog_key.to_string(),
                remote_instance_id: "inst-test".to_string(),
                remote_prekey_bundle: None,
                initiated_locally: false,
            },
            omni_types::GroupControlV1 { control: control.to_string(), payload_json, sent_at_unix_ms: 1 },
        )
    }

    fn system_lines(app: &OmniApp, gid: &str) -> Vec<String> {
        app.list_group_messages(gid).unwrap().into_iter().filter(|m| m.kind == "system").map(|m| m.body).collect()
    }

    /// The inviter sees whom they invited and can take it back; a late accept then adds nobody.
    #[test]
    fn a_sent_invitation_is_listed_until_cancelled_and_a_late_accept_adds_nobody() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-sent", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-sent", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        alice.storage.record_group_sent_invite(&gid, "dialog:bob", 5).unwrap();

        let sent = alice.list_group_sent_invites(&gid).unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].dialog_key, "dialog:bob");
        // The member view now carries the account, so a picker can say "already in the group".
        assert!(alice.group_members_view(&gid).unwrap().iter().all(|m| !m.account_id.is_empty()));

        alice.cancel_group_invite(&gid, "dialog:bob").unwrap();
        assert!(alice.list_group_sent_invites(&gid).unwrap().is_empty());
        assert!(system_lines(&alice, &gid).iter().any(|l| l.starts_with("Приглашение отменено")));
        // Negative halves: nothing left to cancel, and Bob's accept no longer gets him in.
        assert!(alice.cancel_group_invite(&gid, "dialog:bob").is_err());
        let accept = omni_protocol::GroupInviteAcceptV2 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            group_id: gid.clone(),
            key_package_b64: base64::engine::general_purpose::STANDARD
                .encode(super::app_groups::own_key_package(&bob.storage).unwrap()),
        };
        assert!(super::app_groups::add_invitee(&alice.storage, "dialog:bob", &accept).unwrap().is_none());
    }

    /// "invite_decline" both ways: from the invitee it closes our sent invitation (with a line in
    /// the thread); from the inviter it withdraws the offer we hold — but only the inviter may.
    #[test]
    fn a_decline_closes_the_sent_invite_and_only_the_inviter_can_withdraw_an_offer() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-decline", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-decline", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        let off = serde_json::to_string(&omni_protocol::GroupInviteDeclineV2 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            group_id: gid.clone(),
        })
        .unwrap();

        // Bob declines Alice's invitation.
        alice.storage.record_group_sent_invite(&gid, "dialog:bob", 5).unwrap();
        let (ctx, control) = group_control_from("dialog:bob", "invite_decline", off.clone());
        super::app_groups::handle_incoming_group_control(&alice.storage, &alice.transport, &ctx, &control).unwrap();
        assert!(alice.list_group_sent_invites(&gid).unwrap().is_empty());
        assert!(system_lines(&alice, &gid).iter().any(|l| l.starts_with("Приглашение отклонено")));

        // Bob holds an offer from Alice. A stranger cannot withdraw it...
        bob.storage
            .upsert_group_invite(&omni_storage::GroupInviteRecord {
                group_id: gid.clone(),
                name: "Squad".into(),
                inviter_name: "Alice".into(),
                inviter_dialog_key: "dialog:alice".into(),
                status: "pending".into(),
                received_at_unix_ms: 1,
            })
            .unwrap();
        let (ctx, control) = group_control_from("dialog:mallory", "invite_decline", off.clone());
        super::app_groups::handle_incoming_group_control(&bob.storage, &bob.transport, &ctx, &control).unwrap();
        assert_eq!(bob.list_group_invites().unwrap().len(), 1, "only the inviter withdraws");
        // ...Alice can.
        let (ctx, control) = group_control_from("dialog:alice", "invite_decline", off);
        super::app_groups::handle_incoming_group_control(&bob.storage, &bob.transport, &ctx, &control).unwrap();
        assert!(bob.list_group_invites().unwrap().is_empty(), "the withdrawn offer is gone");
    }

    /// An admin promotes a member: everyone sees it in the thread and the new admin can now remove;
    /// demoted again, the right is gone. Rights are decided by the core, not by the shell's menu.
    #[test]
    fn promoting_and_demoting_an_admin_moves_the_right_and_shows_in_the_thread() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-roles", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-roles", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-group-roles", "carol"), "Carol", "Laptop");
        let dave = create_app_with_active_profile(isolated_test_db_path("omni-group-roles", "dave"), "Dave", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        for (who, dialog) in [(&bob, "dialog:bob"), (&carol, "dialog:carol"), (&dave, "dialog:dave")] {
            group_join_for_real(&alice, who, &gid, dialog);
            deliver_group_feed(&alice, &[&bob, &carol, &dave]);
        }
        let bob_pub = member_pub_of(&alice, &gid, "Bob");
        let carol_pub = member_pub_of(&alice, &gid, "Carol");
        let dave_pub = member_pub_of(&alice, &gid, "Dave");
        let thread = |app: &OmniApp| -> Vec<String> {
            app.list_group_messages(&gid).unwrap().into_iter().filter(|m| m.kind == "system").map(|m| m.body).collect()
        };

        // Before: Bob is a member and may not remove.
        assert!(bob.remove_group_member(&gid, &carol_pub).is_err());

        alice.set_group_admin(&gid, &bob_pub, true).unwrap();
        deliver_group_feed(&alice, &[&bob, &carol, &dave]);
        assert!(thread(&carol).iter().any(|l| l == "Bob — теперь админ"), "everyone sees the promotion");
        let bob_view = bob.group_members_view(&gid).unwrap();
        assert_eq!(bob_view.iter().find(|m| m.is_me).unwrap().role, "admin");

        // Now he may remove Carol, and it reaches the others.
        bob.remove_group_member(&gid, &carol_pub).unwrap();
        deliver_group_feed(&bob, &[&alice, &dave]);
        assert!(alice.group_members_view(&gid).unwrap().iter().all(|m| m.member_pub != carol_pub));

        // Demoted: the right is gone again.
        alice.set_group_admin(&gid, &bob_pub, false).unwrap();
        deliver_group_feed(&alice, &[&bob, &dave]);
        assert!(thread(&dave).iter().any(|l| l == "Bob больше не админ"));
        assert!(bob.remove_group_member(&gid, &dave_pub).is_err(), "a demoted member may not remove");
    }

    /// Removing someone from the GROUP mid-call rotates the call secret (sent in the new epoch), so
    /// the removed member can neither decrypt nor forge the call's media; the rest share the new
    /// key. Removing a member who is not in the call changes nothing.
    #[test]
    fn removing_a_call_participant_from_the_group_rekeys_the_call() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-call-rekey", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-call-rekey", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-group-call-rekey", "carol"), "Carol", "Laptop");
        let dave = create_app_with_active_profile(isolated_test_db_path("omni-group-call-rekey", "dave"), "Dave", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        for (who, dialog) in [(&bob, "dialog:bob"), (&carol, "dialog:carol"), (&dave, "dialog:dave")] {
            group_join_for_real(&alice, who, &gid, dialog);
            deliver_group_feed(&alice, &[&bob, &carol, &dave]);
        }
        let everyone = [&alice, &bob, &carol, &dave];
        let exchange = || {
            for _ in 0..3 {
                for from in everyone {
                    let others: Vec<&OmniApp> = everyone.iter().copied().filter(|o| !std::ptr::eq(*o, from)).collect();
                    deliver_group_feed(from, &others);
                }
            }
        };
        let secret = |app: &OmniApp| {
            super::app_group_calls::call_secret_for_test(&app.storage.db_path().to_string_lossy(), &gid)
        };

        // Alice, Bob and Carol are in the call; Dave is a member but stays out of it.
        alice.start_group_call(&gid).unwrap();
        exchange();
        bob.join_group_call(&gid).unwrap();
        carol.join_group_call(&gid).unwrap();
        exchange();
        let original = secret(&alice).expect("alice in the call");
        assert_eq!(secret(&bob).as_ref(), Some(&original));
        assert_eq!(secret(&carol).as_ref(), Some(&original));

        // Negative half: removing Dave (not in the call) leaves the key alone.
        alice.remove_group_member(&gid, &member_pub_of(&alice, &gid, "Dave")).unwrap();
        exchange();
        assert_eq!(secret(&alice).as_ref(), Some(&original), "a non-participant's removal does not rekey");
        assert_eq!(secret(&bob).as_ref(), Some(&original));

        // Removing Carol mid-call rotates the key for those who stay.
        let carol_pub = member_pub_of(&alice, &gid, "Carol");
        alice.remove_group_member(&gid, &carol_pub).unwrap();
        exchange();
        let fresh = secret(&alice).expect("alice still in the call");
        assert_ne!(fresh, original, "the call was rekeyed");
        assert_eq!(secret(&bob).as_ref(), Some(&fresh), "bob got the new key");
        assert_ne!(secret(&carol).as_ref(), Some(&fresh), "the removed member never learns it");
        let snap = alice.group_call_snapshot(&gid).unwrap().unwrap();
        assert!(snap.members.iter().all(|m| m.member_pub != carol_pub), "carol is out of the call");
        assert_eq!(snap.participant_count, 2);
    }

    /// Declining stops the ring but keeps the call listed; pressing "call" while a call is on joins
    /// it instead of starting a second one.
    #[test]
    fn group_call_dismiss_stops_ringing_and_call_button_joins_the_running_call() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-call-dismiss", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-call-dismiss", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);

        let call_id = alice.start_group_call(&gid).unwrap().call_id;
        deliver_group_feed(&alice, &[&bob]);
        bob.dismiss_group_call(&gid).unwrap();
        let inbox = bob.group_call_inbox().unwrap();
        assert!(!inbox[0].ringing, "a declined call stops ringing");
        assert!(bob.group_call_snapshot(&gid).unwrap().is_none());

        let snap = bob.start_group_call(&gid).unwrap();
        assert_eq!(snap.call_id, call_id, "the call button joins the running call");
    }

    /// Two members press "call" at once: both end up in ONE call — the earlier call id.
    #[test]
    fn group_call_concurrent_starts_fold_into_the_earlier_call() {
        let alice = create_app_with_active_profile(isolated_test_db_path("omni-group-call-race", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-call-race", "bob"), "Bob", "Laptop");
        let gid = alice.create_group("Squad").unwrap().group_id;
        group_join_for_real(&alice, &bob, &gid, "dialog:bob");
        deliver_group_feed(&alice, &[&bob]);

        let a = alice.start_group_call(&gid).unwrap().call_id;
        let b = bob.start_group_call(&gid).unwrap().call_id;
        assert_ne!(a, b, "two separate calls before they hear each other");
        for _ in 0..3 {
            deliver_group_feed(&alice, &[&bob]);
            deliver_group_feed(&bob, &[&alice]);
        }
        let winner = std::cmp::min(a, b);
        let sa = alice.group_call_snapshot(&gid).unwrap().unwrap();
        let sb = bob.group_call_snapshot(&gid).unwrap().unwrap();
        assert_eq!(sa.call_id, winner);
        assert_eq!(sb.call_id, winner);
        assert_eq!(sa.participant_count, 2, "Alice sees both in the call");
        assert_eq!(sb.participant_count, 2, "Bob sees both in the call");
    }

    /// Phase 4a (per-pair-video-codec-selection.md §9): a group sender sends everybody the one codec
    /// they all decode. H.264 while a watcher's passport is unknown; AV1 once everybody says they
    /// decode it; back to H.264 at once when somebody new, about whom nothing is known, joins.
    #[test]
    fn a_group_sender_sends_the_one_codec_every_watcher_decodes() {
        let app = create_app_with_active_profile(isolated_test_db_path("omni-group-codec", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-codec", "bob"), "Bob", "Laptop");
        let carol = create_app_with_active_profile(isolated_test_db_path("omni-group-codec", "carol"), "Carol", "Laptop");
        let group_id = app.create_group("Codecs").unwrap().group_id;
        group_join_for_real(&app, &bob, &group_id, "dialog:bob");
        group_join_for_real(&app, &carol, &group_id, "dialog:carol");
        let bob_pub = member_pub_of(&app, &group_id, "Bob");
        let carol_pub = member_pub_of(&app, &group_id, "Carol");
        let call_id = app.start_group_call(&group_id).expect("start call").call_id;
        let dialog_key = format!("group:{group_id}");
        let all_three = || {
            vec![
                test_realtime_video_codec(RealtimeVideoCodec::Av1, 45, true),
                test_realtime_video_codec(RealtimeVideoCodec::H265, 103, true),
                test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true),
            ]
        };
        let frame = |kind| omni_protocol::GroupCallControlV1 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            call_id: call_id.clone(),
            control: kind,
            call_secret_b64: None,
            muted: Some(false),
            camera_off: Some(false),
            screen_sharing: Some(false),
            screen_control: None,
            at_unix_ms: 2_400,
        };
        let sends = || {
            app.get_realtime_media_negotiation(&dialog_key)
                .unwrap()
                .send_video
                .map(|stream| stream.codec)
        };

        super::app_group_calls::handle_group_call_control(
            &app.storage,
            &group_id,
            &bob_pub,
            &frame(omni_protocol::GroupCallControlKindV1::Join),
        )
        .unwrap();
        app.publish_group_media_capabilities(test_realtime_media_capabilities(
            &dialog_key,
            all_three(),
            (1280, 720, 30),
            (1280, 720, 30),
        ))
        .unwrap();
        assert_eq!(sends(), Some(RealtimeVideoCodec::H264), "Bob's passport is not in yet");

        // Bob's passport comes on the call's media, not the group feed: only the call hears it. It
        // fits one datagram with all three codecs in it.
        let bob_decodes = test_realtime_media_capabilities("group:x", all_three(), (1280, 720, 30), (1280, 720, 30));
        let wire = super::app_group_calls::encode_group_passport(&bob_decodes).expect("a passport fits one datagram");
        assert!(wire.starts_with(super::app_group_calls::GROUP_PASSPORT_MAGIC));
        let read = super::app_group_calls::decode_group_passport(&wire).flatten().expect("reads back");
        assert_eq!(read.codecs, bob_decodes.codecs);
        let db_path = app.storage.db_path().to_string_lossy().to_string();
        assert!(super::app_group_calls::note_member_video_capabilities(&db_path, &group_id, &bob_pub, bob_decodes.clone()));
        crate::reselect_group_media(&db_path, &dialog_key);
        assert_eq!(sends(), Some(RealtimeVideoCodec::Av1), "everybody decodes AV1");
        assert!(
            !super::app_group_calls::note_member_video_capabilities(&db_path, &group_id, &bob_pub, bob_decodes),
            "the same passport again is no news"
        );

        super::app_group_calls::handle_group_call_control(
            &app.storage,
            &group_id,
            &carol_pub,
            &frame(omni_protocol::GroupCallControlKindV1::Join),
        )
        .unwrap();
        assert_eq!(sends(), Some(RealtimeVideoCodec::H264), "Carol may decode nothing else");
    }

    /// Presence for a joiner: a member in the call answers a Join with its full state (even the
    /// audio-first default) — durable GC may have compacted earlier frames, so the joiner learns who
    /// is in the call from these replies. A MediaState does not trigger a reply (no ping-pong).
    #[test]
    fn group_call_join_is_answered_with_presence_and_media_state_is_not() {
        let app = create_app_with_active_profile(isolated_test_db_path("omni-group-call-presence", "alice"), "Alice", "Laptop");
        let bob = create_app_with_active_profile(isolated_test_db_path("omni-group-call-presence", "bob"), "Bob", "Laptop");
        let group_id = app.create_group("Squad").unwrap().group_id;
        group_join_for_real(&app, &bob, &group_id, "dialog:bob");
        let bob_pub = member_pub_of(&app, &group_id, "Bob");
        let call_id = app.start_group_call(&group_id).expect("start call").call_id;

        let frame = |kind| omni_protocol::GroupCallControlV1 {
            version: omni_protocol::GROUP_WIRE_VERSION_V2,
            call_id: call_id.clone(),
            control: kind,
            call_secret_b64: None,
            muted: Some(false),
            camera_off: Some(true),
            screen_sharing: Some(false),
            screen_control: None,
            at_unix_ms: 2_400,
        };
        let emits = super::app_group_calls::handle_group_call_control(
            &app.storage,
            &group_id,
            &bob_pub,
            &frame(omni_protocol::GroupCallControlKindV1::Join),
        )
        .expect("handle join");
        assert_eq!(emits.len(), 1, "a Join is answered with our presence");
        assert_eq!(emits[0].control, omni_protocol::GroupCallControlKindV1::MediaState);
        assert_eq!(emits[0].camera_off, Some(true));

        let emits = super::app_group_calls::handle_group_call_control(
            &app.storage,
            &group_id,
            &bob_pub,
            &frame(omni_protocol::GroupCallControlKindV1::MediaState),
        )
        .expect("handle media state");
        assert!(emits.is_empty(), "a MediaState is not answered");
        app.leave_group_call(&group_id).ok();
    }

    /// The periodic durable snapshot skips an unchanged set (order does not matter) and writes a
    /// changed one.
    #[test]
    fn durable_snapshot_marks_ignore_order_and_see_a_new_entry() {
        let a = (vec![1u8; 16], 1i64, vec![9u8]);
        let b = (vec![2u8; 16], 5i64, vec![8u8]);
        let marks = super::durable_rows_marks;
        assert_eq!(marks(&[a.clone(), b.clone()]), marks(&[b.clone(), a.clone()]));
        let c = (vec![2u8; 16], 6i64, vec![7u8]);
        assert_ne!(marks(&[a.clone(), b.clone()]), marks(&[a, b, c]));
    }

    #[test]
    fn init_creates_database_and_ping_works() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .expect("app should initialize");

        let response = app.ping();

        assert!(db_path.exists());
        assert!(response.ok);
        assert_eq!(response.message, "pong from rust core");
        assert!(!app
            .storage
            .get_or_create_local_instance_id()
            .expect("instance id should exist")
            .is_empty());
    }

    #[test]
    fn init_defaults_to_quic_transport_mode() {
        let app = OmniApp::init(AppInitRequest {
            db_path: isolated_test_db_path("omni-stage-0-app-tests", "default-quic"),
        })
        .expect("app should initialize");

        assert_eq!(
            app.transport_snapshot().transport_mode,
            omni_types::TransportMode::Quic
        );
    }

    #[test]
    fn bootstrap_invite_persists_and_reloads_from_storage() {
        // docs/bugs/relay-less-first-contact-validate-before-advertise.md: an outstanding bootstrap
        // invite is persisted (nonce, not secret) and re-armed after a restart — register writes it to
        // storage, and reload_persisted_bootstrap_invites re-derives the secret + repopulates the
        // in-memory bootstrap-invite + pairing-target sets.
        let app = create_app_with_active_profile(
            isolated_test_db_path("omni-bootstrap-reload-tests", "reload"),
            "Inviter",
            "dev",
        );
        let nonce = [0x33u8; 32];
        let nonce_hex = hex::encode(nonce);
        let node = omni_mesh_runtime::MeshNodeAddress::from_bytes([0x9Au8; 20]);
        let secret = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce).unwrap();
        app.register_bootstrap_invite(&nonce_hex, secret, Some(node), current_unix_ms() + 60_000);

        // Simulate a process restart: the in-memory sets are gone, but storage keeps the row.
        app.mesh_bootstrap_invites.lock().unwrap().clear();
        app.mesh_pairing_targets.lock().unwrap().clear();

        app.reload_persisted_bootstrap_invites();
        let invites = app.mesh_bootstrap_invites.lock().unwrap();
        let entry = invites.get(&nonce_hex).expect("invite reloaded from storage");
        assert_eq!(entry.inviter_node, Some(node), "inviter node round-trips");
        assert_eq!(entry.secret, secret, "secret re-derived from the persisted nonce");
        drop(invites);
        assert!(
            app.mesh_pairing_targets.lock().unwrap().contains(&node),
            "importer dial target re-armed"
        );
    }

    #[test]
    fn bundle_backup_export_and_import_preserve_sidecars_and_attachments() {
        let base_dir = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("bundle-roundtrip-{}", unique_suffix()));
        let source_dir = base_dir.join("source");
        let backup_dir = base_dir.join("backup");
        let target_dir = base_dir.join("target");
        let source_db_path = source_dir.join("omni.db");
        let backup_db_path = backup_dir.join("bundle-backup.db");
        let target_db_path = target_dir.join("omni.db");
        let source_attachment_path = source_dir.join("bundle-attachment.txt");

        fs::create_dir_all(
            source_attachment_path
                .parent()
                .expect("attachment path should have parent"),
        )
        .expect("attachment source directory should exist");
        fs::create_dir_all(&backup_dir).expect("backup directory should exist");
        fs::create_dir_all(&target_dir).expect("target directory should exist");
        fs::write(&source_attachment_path, "bundle attachment payload")
            .expect("attachment source file should be written");

        let app = OmniApp::init(AppInitRequest {
            db_path: source_db_path.to_string_lossy().into_owned(),
        })
        .expect("source app should initialize");
        let account = app
            .create_account("Bundle Alice", "Bundle Laptop")
            .expect("account should be created");
        let mnemonic = account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();

        app.storage
            .prepare_outgoing_attachment(
                "bundle-message-1",
                &dialog_key_from_instance_id("bundle-inst"),
                source_attachment_path
                    .to_str()
                    .expect("attachment source path should be utf-8"),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_000,
            )
            .expect("attachment should be prepared");

        fs::write(transport_mode_sidecar_path(&source_db_path.to_string_lossy()), "{\"transport_mode\":\"quic\"}")
            .expect("transport mode sidecar should be written");
        // A profile from before the pre-mesh discovery was removed still has its sidecars.
        fs::write(source_db_path.with_file_name("node-mode.json"), "{\"node_mode\":\"observe\"}")
            .expect("legacy node mode sidecar should be written");
        fs::write(
            source_db_path.with_file_name("discovery-policy.json"),
            "{\"policy\":{}}",
        )
        .expect("legacy discovery policy sidecar should be written");

        let source_attachments_dir =
            attachments_directory_for_db_path(&source_db_path).expect("attachments dir");
        let attachment_entries = fs::read_dir(source_attachments_dir.join("blobs"))
            .expect("blob directory should exist")
            .collect::<Result<Vec<_>, _>>()
            .expect("blob entries should load");
        assert_eq!(attachment_entries.len(), 1);
        let source_blob_name = attachment_entries[0].file_name();

        app.export_current_database_backup_bundle(&backup_db_path)
            .expect("bundle export should succeed");
        let inspection = OmniApp::inspect_account_backup_manifest(
            backup_db_path.to_string_lossy(),
            &mnemonic,
        )
        .expect("backup inspection should succeed");

        OmniApp::import_database_backup_bundle(
            target_db_path.to_string_lossy(),
            &backup_db_path,
            &inspection.inspection_token,
        )
        .expect("bundle import should succeed");

        let imported_manifest =
            Storage::inspect_account_backup_manifest(&target_db_path, &mnemonic)
                .expect("imported backup should validate");
        assert_eq!(imported_manifest.account_id, account.descriptor.account_id);

        assert_eq!(
            fs::read(transport_mode_sidecar_path(&target_db_path.to_string_lossy()))
                .expect("transport mode sidecar should import"),
            b"{\"transport_mode\":\"quic\"}"
        );
        // ...but they configured nothing any more, so the bundle does not carry them across.
        assert!(!target_db_path.with_file_name("node-mode.json").exists());
        assert!(!target_db_path.with_file_name("discovery-policy.json").exists());
        assert!(
            attachments_directory_for_db_path(&target_db_path)
                .expect("target attachments dir")
                .join("blobs")
                .join(source_blob_name)
                .exists(),
            "attachment blob should import"
        );
    }

    #[test]
    fn an_encrypted_backup_file_opens_only_whole_and_only_with_the_accounts_phrase() {
        use crate::{opened_backups, take_opened_backup};
        let base_dir = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("encrypted-backup-{}", unique_suffix()));
        let source_db_path = base_dir.join("source").join("omni.db");
        let other_db_path = base_dir.join("other").join("omni.db");
        let backup_path = base_dir.join("backup").join("copy.omnibackup");
        let target_db_path = base_dir.join("target").join("omni.db");
        let attachment_path = base_dir.join("source").join("note.txt");
        for dir in ["source", "other", "backup", "target"] {
            fs::create_dir_all(base_dir.join(dir)).unwrap();
        }
        fs::write(&attachment_path, "a private attachment line").unwrap();

        let app = OmniApp::init(AppInitRequest { db_path: source_db_path.to_string_lossy().into_owned() }).unwrap();
        let account = app.create_account("Backup Alice Secretname", "Backup Laptop").unwrap();
        let mnemonic = account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();
        app.storage
            .prepare_outgoing_attachment(
                "backup-message-1",
                &dialog_key_from_instance_id("backup-inst"),
                attachment_path.to_str().unwrap(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                1_000,
            )
            .unwrap();
        let other = OmniApp::init(AppInitRequest { db_path: other_db_path.to_string_lossy().into_owned() }).unwrap();
        let other_mnemonic = other
            .create_account("Backup Bob", "Bob Laptop")
            .unwrap()
            .recovery_phrase_v2
            .unwrap()
            .mnemonic;

        assert_eq!(app.get_current_account_export_manifest().unwrap().unwrap().last_backup_at_unix_ms, None);
        app.export_current_database_backup_bundle(&backup_path).unwrap();
        assert!(app.get_current_account_export_manifest().unwrap().unwrap().last_backup_at_unix_ms.is_some());

        // One file, nothing in the clear: no database header, no name, no plain bundle beside it.
        let bytes = fs::read(&backup_path).unwrap();
        assert!(bytes.starts_with(omni_crypto::BACKUP_MAGIC));
        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
        assert!(!contains(b"SQLite format 3"));
        assert!(!contains(b"Secretname"));
        assert!(!backup_bundle_directory(&backup_path).exists());
        assert!(!backup_path.with_extension("part").exists());

        // Another account's phrase does not open it.
        let error = OmniApp::inspect_account_backup_manifest(backup_path.to_string_lossy(), &other_mnemonic)
            .unwrap_err()
            .to_string();
        assert!(error.contains("another account"), "{error}");

        // A changed byte, or a file cut short, does not open.
        let tampered = base_dir.join("backup").join("tampered.omnibackup");
        let mut changed = bytes.clone();
        let middle = changed.len() / 2;
        changed[middle] ^= 0x40;
        fs::write(&tampered, &changed).unwrap();
        assert!(OmniApp::inspect_account_backup_manifest(tampered.to_string_lossy(), &mnemonic).is_err());
        let cut = base_dir.join("backup").join("cut.omnibackup");
        fs::write(&cut, &bytes[..bytes.len() - 20]).unwrap();
        assert!(OmniApp::inspect_account_backup_manifest(cut.to_string_lossy(), &mnemonic).is_err());

        // An import that skipped the check with the phrase is refused.
        let token = OmniApp::inspect_account_backup_manifest(backup_path.to_string_lossy(), &mnemonic)
            .unwrap()
            .inspection_token;
        let _ = take_opened_backup(&backup_path).expect("the inspection opened the file");
        assert!(OmniApp::import_database_backup_bundle(target_db_path.to_string_lossy(), &backup_path, &token).is_err());

        // The phrase opens it, and the restore brings the account and its files.
        let inspection = OmniApp::inspect_account_backup_manifest(backup_path.to_string_lossy(), &mnemonic).unwrap();
        assert_eq!(inspection.manifest.account_id, account.descriptor.account_id);
        let staged = opened_backups().lock().unwrap().values().cloned().collect::<Vec<_>>();
        OmniApp::import_database_backup_bundle(target_db_path.to_string_lossy(), &backup_path, &inspection.inspection_token)
            .unwrap();
        let imported = Storage::inspect_account_backup_manifest(&target_db_path, &mnemonic).unwrap();
        assert_eq!(imported.account_id, account.descriptor.account_id);
        let blobs = attachments_directory_for_db_path(&target_db_path).unwrap().join("blobs");
        assert_eq!(fs::read_dir(blobs).unwrap().count(), 1, "the attachment comes back");
        // The opened copy is gone once restored.
        for db in staged {
            assert!(!db.parent().unwrap().exists());
        }
    }

    #[test]
    fn a_backup_file_cannot_name_a_path_outside_itself() {
        use crate::backup_entry_path;
        let staging = Path::new("staging");
        assert!(backup_entry_path(staging, "omni.db").is_ok());
        assert!(backup_entry_path(staging, "omni.db.bundle/attachments/blobs/x").is_ok());
        for bad in [
            "../omni.db",
            "omni.db.bundle/../../evil",
            "omni.db.bundle//x",
            "omni.db.bundle/a\\..\\b",
            "omni.db.bundle/C:/x",
            "/etc/passwd",
            "other.txt",
        ] {
            assert!(backup_entry_path(staging, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn bundle_backup_export_rejects_current_database_path() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("bundle-export-same-path-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .expect("app should initialize");
        app.create_account("Alice", "Office PC")
            .expect("account should be created");

        let error = app
            .export_current_database_backup_bundle(&db_path)
            .expect_err("export should reject current database path");
        assert!(error
            .to_string()
            .contains("backup destination must not match the current database"));
    }

    #[test]
    fn bundle_backup_import_invalid_source_preserves_existing_local_data() {
        let base_dir = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("bundle-import-invalid-{}", unique_suffix()));
        let target_db_path = base_dir.join("target").join("omni.db");
        let invalid_backup_path = base_dir.join("backup").join("not-a-backup.db");

        fs::create_dir_all(
            invalid_backup_path
                .parent()
                .expect("backup path should have a parent"),
        )
        .expect("backup directory should exist");
        fs::write(&invalid_backup_path, "this is not a sqlite backup")
            .expect("invalid backup payload should be written");

        let app = OmniApp::init(AppInitRequest {
            db_path: target_db_path.to_string_lossy().into_owned(),
        })
        .expect("target app should initialize");
        let account = app
            .create_account("Alice", "Office PC")
            .expect("account should be created");

        let error = OmniApp::import_database_backup_bundle(
            target_db_path.to_string_lossy(),
            &invalid_backup_path,
            "invalid-token",
        )
        .expect_err("invalid backup should be rejected");
        assert!(
            !error.to_string().is_empty(),
            "invalid backup should report a useful error"
        );

        let storage = Storage::open(&target_db_path).expect("target storage should still open");
        storage.migrate().expect("migrations should still succeed");
        let account_after_import = storage
            .get_local_account()
            .expect("account should still be readable")
            .expect("existing account should be preserved");
        assert_eq!(account_after_import.descriptor.account_id, account.descriptor.account_id);
    }

    #[test]
    fn bundle_backup_import_rejects_bundle_changes_after_inspection() {
        let base_dir = std::env::temp_dir()
            .join("omni-stage-0-app-tests")
            .join(format!("bundle-import-changed-{}", unique_suffix()));
        let source_db_path = base_dir.join("source").join("omni.db");
        let backup_db_path = base_dir.join("backup").join("bundle-backup.db");
        let target_db_path = base_dir.join("target").join("omni.db");

        fs::create_dir_all(
            backup_db_path
                .parent()
                .expect("backup path should have a parent"),
        )
        .expect("backup directory should exist");
        fs::create_dir_all(
            target_db_path
                .parent()
                .expect("target path should have a parent"),
        )
        .expect("target directory should exist");

        let app = OmniApp::init(AppInitRequest {
            db_path: source_db_path.to_string_lossy().into_owned(),
        })
        .expect("source app should initialize");
        let account = app
            .create_account("Alice", "Office PC")
            .expect("account should be created");
        let mnemonic = account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();

        fs::write(
            transport_mode_sidecar_path(&source_db_path.to_string_lossy()),
            "{\"transport_mode\":\"quic\"}",
        )
        .expect("transport mode sidecar should be written");

        app.export_current_database_backup_bundle(&backup_db_path)
            .expect("bundle export should succeed");
        let inspection = OmniApp::inspect_account_backup_manifest(
            backup_db_path.to_string_lossy(),
            &mnemonic,
        )
        .expect("backup inspection should succeed");

        fs::write(
            backup_bundle_directory(&backup_db_path).join("transport-mode.json"),
            "{\"transport_mode\":\"invalid\"}",
        )
        .expect("bundled sidecar should be modified after inspection");

        let error = OmniApp::import_database_backup_bundle(
            target_db_path.to_string_lossy(),
            &backup_db_path,
            &inspection.inspection_token,
        )
        .expect_err("changed backup should be rejected");
        assert!(
            error.to_string().contains("changed after inspection"),
            "changed backup should fail closed before import"
        );
    }

    #[test]
    fn create_account_baseline_creates_local_account_and_device_snapshots() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .expect("app should initialize");

        let account = app
            .create_account("Alice", "Office PC")
            .expect("account should be created");
        let device = app
            .get_local_device()
            .expect("device snapshot should load")
            .expect("device should exist");

        assert_eq!(account.descriptor.display_name, "Alice");
        assert_eq!(device.device_label, "Office PC");
        assert_eq!(account.current_device_id, device.device_id);
        assert!(!account.restored_from_recovery);
        assert_eq!(
            account.account_representation_version,
            ACCOUNT_REPRESENTATION_VERSION_V2
        );
        let phrase = account.recovery_phrase_v2.as_ref().unwrap();
        assert_eq!(phrase.mnemonic.split_whitespace().count(), 24);
    }

    #[test]
    fn restore_account_from_mnemonic_reuses_account_identity_and_marks_restored_state() {
        let original_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("orig-{}.db", unique_suffix()));
        let restore_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("restore-{}.db", unique_suffix()));

        let original_app = OmniApp::init(AppInitRequest {
            db_path: original_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let original_account = original_app.create_account("Alice", "Office PC").unwrap();
        let original_device = original_app.get_local_device().unwrap().unwrap();

        let restore_app = OmniApp::init(AppInitRequest {
            db_path: restore_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let restored_account = restore_app
            .restore_account_from_mnemonic(
                &original_account.descriptor.display_name,
                &original_account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
                "Laptop",
            )
            .unwrap();
        let restored_device = restore_app.get_local_device().unwrap().unwrap();

        assert_eq!(
            restored_account.descriptor.account_id,
            original_account.descriptor.account_id
        );
        assert_ne!(restored_device.device_id, original_device.device_id);
        assert!(restored_account.restored_from_recovery);
        assert!(restored_device.restored_from_recovery);
    }

    #[test]
    fn restore_account_from_mnemonic_accepts_v2_phrase() {
        let original_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("orig-input-{}.db", unique_suffix()));
        let mnemonic_restore_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("restore-mnemonic-{}.db", unique_suffix()));

        let original_app = OmniApp::init(AppInitRequest {
            db_path: original_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let original_account = original_app.create_account("Alice", "Office PC").unwrap();

        let mnemonic_phrase = original_account.recovery_phrase_v2.as_ref().unwrap().mnemonic.clone();

        let mnemonic_restore_app = OmniApp::init(AppInitRequest {
            db_path: mnemonic_restore_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let restored_from_mnemonic = mnemonic_restore_app
            .restore_account_from_mnemonic(
                &original_account.descriptor.display_name,
                &mnemonic_phrase,
                "Laptop",
            )
            .unwrap();
        assert_eq!(
            restored_from_mnemonic.descriptor.account_id,
            original_account.descriptor.account_id
        );
        assert_eq!(
            restored_from_mnemonic.account_representation_version,
            ACCOUNT_REPRESENTATION_VERSION_V2
        );
    }

    #[test]
    fn inspect_recovery_input_and_reopen_local_profile_are_distinct() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-app-tests")
            .join(format!("recovery-contract-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let preview = app
            .inspect_recovery_input(
                &account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
                Some("Alice"),
                Some("Office PC"),
            )
            .unwrap();
        assert_eq!(preview.flow_kind, RecoveryFlowKind::ReopenExistingLocalProfile);
        assert_eq!(
            preview.mnemonic_usage,
            RecoveryMnemonicUsage::RootBasisForReopen
        );
        assert_eq!(
            preview.conflict,
            Some(RecoveryConflictCode::ExistingProfileAlreadyPresent)
        );
        assert!(!preview.can_unlock);
        assert!(preview.can_reopen);

        let reopened = app.reopen_local_profile(&profile_id).unwrap();
        assert_eq!(reopened.profile_id, profile_id);
        assert_eq!(app.get_active_profile_id().unwrap(), Some(profile_id));
    }

    #[test]
    fn reopen_local_profile_rejects_locked_profiles() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-app-tests")
            .join(format!("reopen-locked-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);

        let error = app.reopen_local_profile(&profile_id).unwrap_err();
        assert!(error
            .to_string()
            .contains(ProfileLifecycleErrorCode::ProfileLocked.as_str()));
    }

    #[test]
    fn restore_account_from_mnemonic_reports_existing_profile_conflict() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-13-app-tests")
            .join(format!("restore-conflict-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();

        let result = app.restore_account_from_mnemonic(
            &account.descriptor.display_name,
            &account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
            "Laptop",
        );
        let error = result.expect_err("restore should reject an existing local profile");
        assert!(
            error.to_string().contains("profile_lifecycle:profile_already_present"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn lifecycle_summaries_reflect_profile_gate_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-summary-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let signed_in = app
            .get_profile_lifecycle_summary(&profile_id)
            .unwrap()
            .expect("summary should exist");
        assert_eq!(signed_in.state, LocalProfileState::SignedIn);
        assert!(signed_in.is_unlocked);
        assert!(signed_in.messaging_allowed);
        assert!(signed_in.transport_allowed);
        assert!(signed_in.device_operations_allowed);
        assert!(signed_in.sync_operations_allowed);
        assert!(!signed_in.requires_unlock);
        assert!(signed_in.destructive_action_allowed);
        assert!(!signed_in.destructive_transition_in_progress);

        confirm_sign_out_profile(&app, &profile_id);
        let locked = app
            .get_profile_lifecycle_summary(&profile_id)
            .unwrap()
            .expect("summary should exist");
        assert_eq!(locked.state, LocalProfileState::SignedOutLocked);
        assert!(!locked.is_unlocked);
        assert!(locked.requires_unlock);
        assert!(!locked.messaging_allowed);
        assert!(!locked.transport_allowed);
        assert!(!locked.device_operations_allowed);
        assert!(!locked.sync_operations_allowed);
        assert!(locked.destructive_action_allowed);
        assert!(!locked.destructive_transition_in_progress);

        let gate = app
            .get_profile_gate_summary(&profile_id)
            .unwrap()
            .expect("gate summary should exist");
        assert_eq!(gate.state, LocalProfileState::SignedOutLocked);
        assert!(!gate.is_unlocked);
        assert!(gate.requires_unlock);
        assert!(!gate.destructive_transition_in_progress);
    }

    #[test]
    fn lifecycle_requires_explicit_active_profile_selection() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("active-profile-selection-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();

        assert_eq!(app.get_active_profile_id().unwrap(), None);
        assert!(app.get_current_profile_gate_summary().unwrap().is_none());
        let no_active_error = app.prepare_sign_out_current_profile().unwrap_err();
        assert!(no_active_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::NoActiveProfile.as_str()));

        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        app.clear_active_profile().unwrap();

        assert_eq!(app.get_active_profile_id().unwrap(), None);
        assert!(app.get_current_profile_lifecycle_summary().unwrap().is_none());
        let switch_required_error = app.prepare_sign_out_current_profile().unwrap_err();
        assert!(switch_required_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::ActiveProfileSwitchRequired.as_str()));

        app.select_active_profile(&profile_id).unwrap();
        assert_eq!(
            app.get_active_profile_id().unwrap(),
            Some(profile_id.clone())
        );
        let lifecycle = app
            .get_current_profile_lifecycle_summary()
            .unwrap()
            .expect("lifecycle summary should exist");
        assert_eq!(lifecycle.profile_id, profile_id);
    }

    #[test]
    fn lifecycle_invalid_transition_returns_typed_error_code() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-transition-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        confirm_sign_out_profile(&app, &profile_id);
        let second_sign_out_error = app.sign_out_profile(&profile_id).unwrap_err();
        assert!(second_sign_out_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::ProfileLocked.as_str()));

        let unlock_while_signed_in_error = {
            let unlocked = app
                .unlock_profile_with_mnemonic(&profile_id, &account.recovery_phrase_v2.as_ref().unwrap().mnemonic)
                .unwrap();
            assert_eq!(unlocked.state, LocalProfileState::SignedIn);
            assert_eq!(app.get_active_profile_id().unwrap(), Some(profile_id.clone()));
            app.unlock_profile_with_mnemonic(&profile_id, &account.recovery_phrase_v2.as_ref().unwrap().mnemonic)
                .unwrap_err()
        };
        assert!(unlock_while_signed_in_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::LifecycleTransitionNotAllowed.as_str()));
    }

    #[test]
    fn lifecycle_unlock_failures_return_typed_error_codes() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-unlock-errors-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        app.set_local_profile_password(&profile_id, "correct-password")
            .unwrap();
        confirm_sign_out_profile(&app, &profile_id);

        let wrong_password_error = app
            .unlock_profile_with_password(&profile_id, "wrong-password")
            .unwrap_err();
        assert!(wrong_password_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::UnlockFailedInvalidPassword.as_str()));

        let wrong_mnemonic_error = app
            .unlock_profile_with_mnemonic(&profile_id, "not a valid recovery phrase")
            .unwrap_err();
        assert!(wrong_mnemonic_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::UnlockFailedInvalidMnemonic.as_str()));

        let unlocked = app
            .unlock_profile_with_password(&profile_id, "correct-password")
            .unwrap();
        assert_eq!(unlocked.state, LocalProfileState::SignedIn);
        assert_eq!(app.get_active_profile_id().unwrap(), Some(profile_id));
    }

    /// Deleting the last profile takes its groups with it: the profile-scoped group rows AND the MLS
    /// state (OpenMLS tables, snapshots, the wrapped at-rest key), which is not profile-scoped and was
    /// left on disk before — a deleted account's group secrets surviving "delete from device".
    #[test]
    fn deleting_the_last_profile_wipes_its_groups_and_mls_state() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-groups-{}.db", unique_suffix()));
        let app = OmniApp::init(AppInitRequest { db_path: db_path.to_string_lossy().into_owned() }).unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account.local_profile_summary.as_ref().unwrap().profile_id.clone();
        app.create_group("Семья").unwrap();
        app.send_group_text(&app.list_group_summaries().unwrap()[0].group_id, "привет").unwrap();

        let count = |sql: &str| -> i64 {
            let conn = omni_storage::Storage::open_keyed_connection(&db_path).unwrap();
            conn.query_row(sql, [], |row| row.get(0)).unwrap()
        };
        // Negative half first: the state is really there before the delete.
        assert_eq!(count("SELECT COUNT(*) FROM groups"), 1);
        assert!(count("SELECT COUNT(*) FROM openmls_group_data") > 0);
        assert_eq!(count("SELECT COUNT(*) FROM mls_storage_key"), 1);

        confirm_sign_out_profile(&app, &profile_id);
        let token = app
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .unwrap()
            .confirmation_token
            .unwrap();
        app.confirm_delete_local_profile(&profile_id, Some(&token)).unwrap();

        assert_eq!(count("SELECT COUNT(*) FROM groups"), 0, "group rows go with the profile");
        assert_eq!(count("SELECT COUNT(*) FROM group_members"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM group_message_meta"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM openmls_group_data"), 0, "MLS group state wiped");
        assert_eq!(count("SELECT COUNT(*) FROM openmls_signature_keys") + count("SELECT COUNT(*) FROM openmls_key_packages"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM mls_storage_key"), 0, "the at-rest key is gone too");
    }

#[test]
fn lifecycle_confirm_delete_requires_token_and_deletes_after_confirm() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-confirm-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);

        let missing_token_error = app
            .confirm_delete_local_profile(&profile_id, None)
            .unwrap_err();
        assert!(missing_token_error.to_string().contains(
            ProfileLifecycleErrorCode::DangerousActionConfirmationRequired.as_str()
        ));

        let preview = app
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .expect("delete preview should exist");
        let token = preview
            .confirmation_token
            .as_deref()
            .expect("confirmation token should exist");
        app.confirm_delete_local_profile(&profile_id, Some(token))
            .unwrap();
        assert!(app.get_local_account().unwrap().is_none());
        assert_eq!(app.get_active_profile_id().unwrap(), None);
    }

    /// Deleting the LAST profile must not leave the device's mesh identity on disk: it belongs to no
    /// profile, so nothing else ever removes it, and the next profile would come up as the very same
    /// node the peers already know — "delete from device" that doesn't break the link to the deleted
    /// identity. The db file itself stays: it is open, and by now it is an empty container.
    #[test]
    fn lifecycle_deleting_the_last_profile_wipes_the_device_mesh_identity() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-traces-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);

        let db_file_name = db_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("db file name")
            .to_string();
        let identity_sidecar =
            db_path.with_file_name(format!("{db_file_name}.transport-identity.key"));
        std::fs::write(&identity_sidecar, b"mesh-identity").expect("identity sidecar");

        let preview = app
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .expect("delete preview should exist");
        let token = preview
            .confirmation_token
            .as_deref()
            .expect("confirmation token should exist");
        app.confirm_delete_local_profile(&profile_id, Some(token))
            .unwrap();

        assert!(
            !identity_sidecar.exists(),
            "the mesh transport identity must not outlive the last local profile"
        );
        assert!(db_path.exists(), "the open database must survive the wipe");
    }

    /// Startup enforces "no profiles ⇒ no device state", which is what cleans up a footprint left by
    /// a build that deleted profiles without wiping traces (or an interrupted delete, or a copied
    /// folder): a stale peer cache, or the pre-mesh transport's identity key an older build created.
    #[test]
    fn init_wipes_device_traces_left_behind_when_no_profile_owns_them() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("startup-orphan-traces-{}.db", unique_suffix()));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db_file_name = db_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("db file name")
            .to_string();
        let legacy_identity = db_path.with_file_name(format!("{db_file_name}.transport-identity.key"));
        let stale_cache = db_path.with_file_name(format!("{db_file_name}.dht-nodes-cache.json"));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(app.list_local_profiles().unwrap().profiles.is_empty());
        drop(app);
        std::fs::write(&legacy_identity, b"identity left by an older build").unwrap();
        std::fs::write(&stale_cache, b"peers").unwrap();

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(!stale_cache.exists(), "an orphaned DHT peer cache must not survive startup");
        assert!(
            !legacy_identity.exists(),
            "an orphaned identity key must not survive startup"
        );

        // With a profile present the device state is in use — startup must leave it alone.
        app.create_account("Alice", "Office PC").unwrap();
        drop(app);
        std::fs::write(&stale_cache, b"live peers").unwrap();
        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(&stale_cache).expect("the cache survives"),
            b"live peers",
            "a live profile's device state must survive startup untouched"
        );
        drop(app);
    }

    #[test]
    fn lifecycle_direct_delete_local_profile_rejects_confirmation_bypass() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-direct-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);

        let error = app.delete_local_profile(&profile_id).unwrap_err();
        assert!(error.to_string().contains(
            ProfileLifecycleErrorCode::DangerousActionConfirmationRequired.as_str()
        ));
        assert!(app.get_local_profile_summary(&profile_id).unwrap().is_some());
    }

    #[test]
    fn lifecycle_direct_sign_out_rejects_confirmation_bypass() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-signout-direct-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let error = app.sign_out_profile(&profile_id).unwrap_err();
        assert!(error.to_string().contains(
            ProfileLifecycleErrorCode::DangerousActionConfirmationRequired.as_str()
        ));
        assert_eq!(
            app.get_profile_lifecycle_summary(&profile_id)
                .unwrap()
                .expect("profile should remain visible")
                .state,
            LocalProfileState::SignedIn
        );
    }

    #[test]
    fn lifecycle_confirm_delete_active_profile_clears_active_context() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-active-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let preview = app
            .preview_delete_local_profile(&profile_id)
            .unwrap()
            .expect("delete preview should exist");
        let token = preview
            .confirmation_token
            .as_deref()
            .expect("confirmation token should exist");

        app.confirm_delete_local_profile(&profile_id, Some(token))
            .unwrap();

        assert_eq!(app.get_active_profile_id().unwrap(), None);
        assert!(app
            .get_local_profile_summary(&profile_id)
            .unwrap()
            .is_none());
        assert!(app.list_local_profiles().unwrap().profiles.is_empty());
    }

    #[test]
    fn lifecycle_confirm_sign_out_profile_keeps_explicit_profile_flow() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-signout-explicit-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        let preview = app
            .preview_sign_out_profile(&profile_id)
            .unwrap()
            .expect("sign-out preview should exist");
        let token = preview
            .confirmation_token
            .as_deref()
            .expect("confirmation token should exist");

        let signed_out = app
            .confirm_sign_out_profile(&profile_id, Some(token))
            .unwrap();

        assert_eq!(signed_out.state, LocalProfileState::SignedOutLocked);
        assert_eq!(app.get_active_profile_id().unwrap(), Some(profile_id));
        assert_eq!(
            app.get_current_profile_lifecycle_summary()
                .unwrap()
                .expect("current lifecycle should exist")
                .state,
            LocalProfileState::SignedOutLocked
        );
    }

    #[test]
    fn lifecycle_delete_non_active_profile_keeps_active_context() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-delete-non-active-{}.db", unique_suffix()));

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let alice = app.create_account("Alice", "Office PC").unwrap();
        let alice_profile_id = alice
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        let bob = app.create_account("Bob", "Laptop").unwrap();
        let bob_profile_id = bob
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();

        app.select_active_profile(&alice_profile_id).unwrap();

        // Device-level state is shared by every profile in this db, so deleting ONE of them must
        // leave it alone — only the last profile out takes it with it.
        let db_file_name = db_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("db file name")
            .to_string();
        let identity_sidecar =
            db_path.with_file_name(format!("{db_file_name}.transport-identity.key"));
        std::fs::write(&identity_sidecar, b"mesh-identity").expect("identity sidecar");

        let preview = app
            .preview_delete_local_profile(&bob_profile_id)
            .unwrap()
            .expect("delete preview should exist");
        let token = preview
            .confirmation_token
            .as_deref()
            .expect("confirmation token should exist");
        app.confirm_delete_local_profile(&bob_profile_id, Some(token))
            .unwrap();

        assert!(
            identity_sidecar.exists(),
            "Alice still uses this device's mesh identity — deleting Bob must not wipe it"
        );
        assert_eq!(
            app.get_active_profile_id().unwrap(),
            Some(alice_profile_id.clone())
        );
        let profiles = app.list_local_profiles().unwrap();
        assert_eq!(profiles.profiles.len(), 1);
        assert_eq!(profiles.profiles[0].profile_id, alice_profile_id);
        assert_eq!(
            app.get_local_account()
                .unwrap()
                .expect("active account should remain"),
            alice
        );
    }

#[test]
fn lifecycle_sign_out_preview_respects_transition_preconditions() {
    let db_path = std::env::temp_dir()
        .join("omni-stage-10-app-tests")
        .join(format!("lifecycle-signout-preview-{}.db", unique_suffix()));

    let app = OmniApp::init(AppInitRequest {
        db_path: db_path.to_string_lossy().into_owned(),
    })
    .unwrap();
    let account = app.create_account("Alice", "Office PC").unwrap();
    let profile_id = account
        .local_profile_summary
        .as_ref()
        .expect("profile summary should exist")
        .profile_id
        .clone();

    let preview = app
        .preview_sign_out_profile(&profile_id)
        .unwrap()
        .expect("sign out preview should exist for signed-in profile");
    assert_eq!(preview.action, DangerousActionKind::SignOutProfile);
    assert_eq!(preview.confirmation_token.as_deref(), Some("SIGN_OUT"));

    confirm_sign_out_profile(&app, &profile_id);
    let locked_preview_error = app.preview_sign_out_profile(&profile_id).unwrap_err();
    assert!(locked_preview_error
        .to_string()
        .contains(ProfileLifecycleErrorCode::ProfileLocked.as_str()));
}

#[test]
fn lifecycle_confirm_sign_out_requires_token() {
    let db_path = std::env::temp_dir()
        .join("omni-stage-10-app-tests")
        .join(format!("lifecycle-signout-confirm-{}.db", unique_suffix()));

    let app = OmniApp::init(AppInitRequest {
        db_path: db_path.to_string_lossy().into_owned(),
    })
    .unwrap();
    app.create_account("Alice", "Office PC").unwrap();
    let missing_token_error = app
        .confirm_sign_out_current_profile(None)
        .unwrap_err();
    assert!(missing_token_error.to_string().contains(
        ProfileLifecycleErrorCode::DangerousActionConfirmationRequired.as_str()
    ));
}

#[test]
fn lifecycle_runtime_gates_block_sensitive_operations_while_locked() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-runtime-gates-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);

        let send_error = app
            .send_text("127.0.0.1:9000", "should be blocked while locked")
            .unwrap_err();
        assert!(send_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::ProfileLocked.as_str()));

        let prekey_error = app.get_local_prekey_bundle().unwrap_err();
        assert!(prekey_error
            .to_string()
            .contains(ProfileLifecycleErrorCode::ProfileLocked.as_str()));
    }

    #[test]
    fn lifecycle_unlock_reopens_runtime_path_for_sensitive_operations() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("lifecycle-runtime-reopen-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let account = app.create_account("Alice", "Office PC").unwrap();
        let profile_id = account
            .local_profile_summary
            .as_ref()
            .expect("profile summary should exist")
            .profile_id
            .clone();
        confirm_sign_out_profile(&app, &profile_id);
        app.unlock_profile_with_mnemonic(&profile_id, &account.recovery_phrase_v2.as_ref().unwrap().mnemonic)
            .unwrap();

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Unlock".to_string(),
                remote_account_id: Some(AccountId("acct-unlock".to_string())),
                public_display_name: Some("Unlock".to_string()),
                remote_instance_id: Some("inst-unlock".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-unlock")),
                initial_observation: None,
            })
            .unwrap();

        let queued = app
            .send_text(
                contact
                    .primary_dialog_key
                    .as_deref()
                    .expect("contact should have a primary dialog"),
                "works after unlock",
            )
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Queued);
    }

    #[test]
    fn stage5_local_prekey_bundle_is_available_after_account_creation() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("bundle-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let account = app.create_account("Alice", "Office PC").unwrap();
        let device = app.get_local_device().unwrap().unwrap();

        let bundle = app.get_local_prekey_bundle().unwrap();

        assert_eq!(bundle.account.account_id, account.descriptor.account_id.0);
        assert_eq!(bundle.device.device_id, device.device_id.0);
        assert!(!bundle.account.identity_public_key.is_empty());
        assert!(!bundle.device.device_public_key.is_empty());
        assert!(!bundle.signed_prekey.public_key.is_empty());
    }

    #[test]
    fn stage5_session_bootstrap_foundation_persists_records() {
        let alice_db_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("alice-{}.db", unique_suffix()));
        let bob_db_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("bob-{}.db", unique_suffix()));

        let alice = OmniApp::init(AppInitRequest {
            db_path: alice_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let bob = OmniApp::init(AppInitRequest {
            db_path: bob_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();

        let alice_account = alice.create_account("Alice", "Alice PC").unwrap();
        let alice_device = alice.get_local_device().unwrap().unwrap();
        let bob_account = bob.create_account("Bob", "Bob PC").unwrap();
        let bob_device = bob.get_local_device().unwrap().unwrap();

        let bob_bundle = bob.get_local_prekey_bundle().unwrap();
        let bootstrap = alice.bootstrap_session_from_prekey_bundle(&bob_bundle).unwrap();
        bob.accept_session_bootstrap(&bootstrap).unwrap();

        let alice_session = alice
            .storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: bob_account.descriptor.account_id.clone(),
                remote_device_id: bob_device.device_id.clone(),
            })
            .unwrap()
            .unwrap();
        let bob_session = bob
            .storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: alice_account.descriptor.account_id.clone(),
                remote_device_id: alice_device.device_id.clone(),
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            alice_session.session_address.remote_account_id,
            bob_account.descriptor.account_id
        );
        assert_eq!(
            bob_session.session_address.remote_account_id,
            alice_account.descriptor.account_id
        );
        assert!(alice
            .storage
            .get_remote_prekey_bundle(&bob_account.descriptor.account_id, &bob_device.device_id)
            .unwrap()
            .is_some());
    }

    #[test]
    fn add_list_rename_block_and_remove_contact_work() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("contact-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Alice".to_string(),
                remote_account_id: None,
                public_display_name: Some("Alice".to_string()),
                remote_instance_id: Some("inst-alice".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-alice")),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some("inst-alice".to_string()),
                    observed_display_name: Some("Alice".to_string()),
                    observed_at_unix_ms: 100,
                }),
            })
            .unwrap();

        assert_eq!(app.list_contacts().unwrap().len(), 1);

        let renamed = app
            .rename_contact(&contact.contact_id, "Alice Work")
            .unwrap();
        assert_eq!(renamed.petname, "Alice Work");

        let blocked = app.block_contact(&contact.contact_id).unwrap();
        assert!(blocked.is_blocked);

        let removed = app.forget_contact(&contact.contact_id).unwrap();
        assert!(removed.removed_at_unix_ms.is_some());
        assert!(app.list_contacts().unwrap().is_empty());
    }

    #[test]
    fn add_contact_by_identifier_baseline_works() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("contact-id-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact_by_identifier("acct-remote", "Alice", Some("Alice".to_string()))
            .unwrap();

        assert_eq!(contact.petname, "Alice");
        assert_eq!(
            contact.remote_account_id,
            Some(omni_identity::AccountId("acct-remote".to_string()))
        );
        assert_eq!(contact.public_display_name.as_deref(), Some("Alice"));
        assert_eq!(
            contact.primary_dialog_key.as_deref(),
            Some(omni_types::dialog_key_from_account_id("acct-remote").as_str())
        );
    }

    #[test]
    fn add_contact_by_identifier_is_idempotent_for_same_account() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("contact-id-dedup-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let first = app
            .add_contact_by_identifier("acct-remote", "Bob", Some("Alice".to_string()))
            .unwrap();
        let second = app
            .add_contact_by_identifier("acct-remote", "Bob Duplicate", Some("Alice Duplicate".to_string()))
            .unwrap();

        assert_eq!(first.contact_id, second.contact_id);
        let contacts = app.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].petname, "Bob");
        assert_eq!(
            contacts[0].remote_account_id,
            Some(omni_identity::AccountId("acct-remote".to_string()))
        );
    }

    #[test]
    fn request_contact_by_identifier_creates_pending_contact() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("contact-request-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .request_contact_by_identifier("acct-request", "Боб", Some("Bob".to_string()))
            .unwrap();

        assert_eq!(contact.petname, "Боб");
        assert_eq!(contact.relationship_state, ContactRelationshipState::Pending);
        assert_eq!(
            contact.pending_request_direction,
            Some(ContactRequestDirection::Outgoing)
        );
        assert_eq!(
            contact.remote_account_id,
            Some(omni_identity::AccountId("acct-request".to_string()))
        );

        let contacts = app.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].relationship_state, ContactRelationshipState::Pending);
        assert_eq!(
            contacts[0].pending_request_direction,
            Some(ContactRequestDirection::Outgoing)
        );
    }

    #[test]
    fn add_contact_from_payload_is_idempotent_for_same_account() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("contact-payload-dedup-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let payload = ImportableContactPayloadV1 {
            version: IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1,
            account: AccountPublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                account_id: "acct-payload".to_string(),
                display_name: "Payload User".to_string(),
            },
            devices: vec![DevicePublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                device_id: "dev-payload-1".to_string(),
                account_id: "acct-payload".to_string(),
                device_label: "Payload Phone".to_string(),
            }],
            fingerprint: "fp-payload".to_string(),
            safety_number: "sn-payload".to_string(),
            exported_at_unix_ms: 1_000,
        };

        let first = app
            .add_contact_from_payload(&payload, "Payload Bob")
            .unwrap();
        let second = app
            .add_contact_from_payload(&payload, "Payload Bob Duplicate")
            .unwrap();

        assert_eq!(first.contact_id, second.contact_id);
        let contacts = app.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].petname, "Payload Bob");
        assert_eq!(
            contacts[0].remote_account_id,
            Some(omni_identity::AccountId("acct-payload".to_string()))
        );
    }

    #[test]
    fn trust_state_transitions_baseline_work() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("trust-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Alice".to_string(),
                remote_account_id: None,
                public_display_name: Some("Alice".to_string()),
                remote_instance_id: Some("inst-alice".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-alice")),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some("inst-alice".to_string()),
                    observed_display_name: Some("Alice".to_string()),
                    observed_at_unix_ms: 100,
                }),
            })
            .unwrap();

        let unverified = app.get_contact_card(&contact.contact_id).unwrap();
        assert_eq!(unverified.trust_state, TrustState::Unverified);

        let verified = app
            .verify_contact(&contact.contact_id, VerificationMethod::ManualCompare)
            .unwrap();
        assert_eq!(verified.trust_state, TrustState::Verified);

        app.record_contact_observation(
            &contact.contact_id,
            &ContactObservationSnapshot {
                identity_key: Some("inst-alice-v2".to_string()),
                observed_display_name: Some("Alice".to_string()),
                observed_at_unix_ms: 200,
            },
        )
        .unwrap();
        let identity_changed = app.get_contact_card(&contact.contact_id).unwrap();
        assert_eq!(identity_changed.trust_state, TrustState::IdentityChanged);
    }

    /// A device of the contact more or less is no reason to re-verify (the account's master key
    /// vouches for it); an identity we no longer know is.
    #[test]
    fn a_contact_stays_verified_across_its_devices_and_asks_again_without_an_identity() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("device-change-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Bob".to_string(),
                remote_account_id: None,
                public_display_name: Some("Bob".to_string()),
                remote_instance_id: Some("inst-bob".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some("inst-bob".to_string()),
                    observed_display_name: Some("Bob".to_string()),
                    observed_at_unix_ms: 100,
                }),
            })
            .unwrap();

        app.verify_contact(&contact.contact_id, VerificationMethod::ManualCompare)
            .unwrap();

        app.record_contact_observation(
            &contact.contact_id,
            &ContactObservationSnapshot {
                identity_key: Some("inst-bob".to_string()),
                observed_display_name: Some("Bob".to_string()),
                observed_at_unix_ms: 200,
            },
        )
        .unwrap();
        assert_eq!(
            app.get_contact_card(&contact.contact_id)
                .unwrap()
                .trust_state,
            TrustState::Verified
        );

        app.record_contact_observation(
            &contact.contact_id,
            &ContactObservationSnapshot {
                identity_key: None,
                observed_display_name: Some("Bob".to_string()),
                observed_at_unix_ms: 300,
            },
        )
        .unwrap();
        assert_eq!(
            app.get_contact_card(&contact.contact_id)
                .unwrap()
                .trust_state,
            TrustState::ReverifyNeeded
        );
    }

    #[test]
    fn contact_storage_survives_restart() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("restart-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Carl".to_string(),
                remote_account_id: None,
                public_display_name: Some("Carl".to_string()),
                remote_instance_id: Some("inst-carl".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-carl")),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some("inst-carl".to_string()),
                    observed_display_name: Some("Carl".to_string()),
                    observed_at_unix_ms: 100,
                }),
            })
            .unwrap();
        app.verify_contact(&contact.contact_id, VerificationMethod::ManualCompare)
            .unwrap();
        drop(app);

        let reopened = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        let contacts = reopened.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].petname, "Carl");
        assert_eq!(
            reopened
                .get_contact_card(&contacts[0].contact_id)
                .unwrap()
                .trust_state,
            TrustState::Verified
        );
    }

    #[test]
    fn add_contact_from_exported_payload_baseline_works() {
        let exporter_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("payload-exporter-{}.db", unique_suffix()));
        let importer_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("payload-importer-{}.db", unique_suffix()));

        let exporter = OmniApp::init(AppInitRequest {
            db_path: exporter_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        exporter.create_account("Alice", "Office PC").unwrap();

        let my_payload = exporter.export_my_verification_payload().unwrap();
        let verification_payload =
            omni_protocol::contact_payload_from_my_payload(&my_payload).unwrap();

        let importer = OmniApp::init(AppInitRequest {
            db_path: importer_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        importer.create_account("Alice", "Office PC").unwrap();
        let importable = importer
            .inspect_remote_verification_payload(&verification_payload)
            .unwrap();
        let contact = importer
            .add_contact_from_payload(&importable, "Alice")
            .unwrap();
        let card = importer.get_contact_card(&contact.contact_id).unwrap();

        assert_eq!(contact.petname, "Alice");
        assert_eq!(
            contact
                .remote_account_id
                .as_ref()
                .map(|value| value.0.as_str()),
            Some(my_payload.account.account_id.as_str())
        );
        assert_eq!(card.trust_state, TrustState::Unverified);
        assert_eq!(
            card.verification.current_identity_key.as_deref(),
            Some(my_payload.account.account_id.as_str())
        );
    }

    #[test]
    fn add_contact_from_exported_payload_can_be_verified_manually() {
        let exporter_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("payload-verify-exporter-{}.db", unique_suffix()));
        let importer_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("payload-verify-importer-{}.db", unique_suffix()));

        let exporter = OmniApp::init(AppInitRequest {
            db_path: exporter_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        exporter.create_account("Alice", "Office PC").unwrap();

        let my_payload = exporter.export_my_verification_payload().unwrap();
        let verification_payload =
            omni_protocol::contact_payload_from_my_payload(&my_payload).unwrap();

        let importer = OmniApp::init(AppInitRequest {
            db_path: importer_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        importer.create_account("Bob", "Desk PC").unwrap();

        let importable = importer
            .inspect_remote_verification_payload(&verification_payload)
            .unwrap();
        let contact = importer
            .add_contact_from_payload(&importable, "Alice-LAN")
            .unwrap();
        let verified = importer
            .confirm_contact_verification(
                &contact.contact_id,
                &verification_payload,
                VerificationMethod::ManualCompare,
            )
            .unwrap();

        assert_eq!(verified.trust_state, TrustState::Verified);
        assert_eq!(
            verified
                .contact
                .remote_account_id
                .as_ref()
                .map(|value| value.0.as_str()),
            Some(my_payload.account.account_id.as_str())
        );
        assert_eq!(
            verified.verification.verified_identity_key.as_deref(),
            Some(my_payload.account.account_id.as_str())
        );
    }

    #[test]
    fn verification_payload_mismatch_is_rejected() {
        let exporter_db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("payload-mismatch-{}.db", unique_suffix()));

        let exporter = OmniApp::init(AppInitRequest {
            db_path: exporter_db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        exporter.create_account("Alice", "Office PC").unwrap();
        let mut payload = omni_protocol::contact_payload_from_my_payload(
            &exporter.export_my_verification_payload().unwrap(),
        )
        .unwrap();
        payload.devices[0].account_id = "acct-other".to_string();

        let error = exporter
            .inspect_remote_verification_payload(&payload)
            .expect_err("mismatched payload should be rejected");
        assert!(error
            .to_string()
            .contains("device descriptor account_id does not match account descriptor"));
    }

    #[test]
    fn send_text_without_connection_stays_queued_until_attempt_starts() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-app-tests")
            .join(format!("{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Remote".to_string(),
                remote_account_id: Some(AccountId("acct-remote".to_string())),
                public_display_name: Some("Remote".to_string()),
                remote_instance_id: Some("inst-remote".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-remote")),
                initial_observation: None,
            })
            .unwrap();

        let queued = app
            .send_text(
                contact
                    .primary_dialog_key
                    .as_deref()
                    .expect("contact should have a primary dialog"),
                "hello while disconnected",
            )
            .expect("send should be accepted into queue");

        let history = app
            .list_messages(
                contact
                    .primary_dialog_key
                    .as_deref()
                    .expect("contact should have a primary dialog"),
            )
            .expect("history should load");
        assert_eq!(
            history.dialog_key,
            dialog_key_from_instance_id("inst-remote")
        );
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].direction, ChatDirection::Outgoing);
        assert_eq!(queued.id, history.messages[0].id);
        assert_eq!(history.messages[0].status, MessageStatus::Queued);
        assert_eq!(history.messages[0].body, "hello while disconnected");

        let first_page = app
            .list_messages_page(ListMessagesPageRequest {
                dialog_key: contact
                    .primary_dialog_key
                    .clone()
                    .expect("contact should have a primary dialog"),
                before_created_at_unix_ms: None,
                before_message_id: None,
                limit: 1,
            })
            .expect("history page should load");
        assert_eq!(
            first_page.dialog_key,
            dialog_key_from_instance_id("inst-remote")
        );
        assert_eq!(first_page.messages.len(), 1);
        assert_eq!(first_page.messages[0].id, queued.id);
        assert!(!first_page.has_older);
        assert_eq!(
            first_page.oldest_loaded_message_id.as_deref(),
            Some(queued.id.as_str())
        );

        let partial_cursor_error = app
            .list_messages_page(ListMessagesPageRequest {
                dialog_key: contact
                    .primary_dialog_key
                    .clone()
                    .expect("contact should have a primary dialog"),
                before_created_at_unix_ms: Some(queued.created_at_unix_ms),
                before_message_id: None,
                limit: 1,
            })
            .expect_err("partial cursor should be rejected");
        assert!(
            partial_cursor_error
                .to_string()
                .contains("before cursor must include both before_created_at_unix_ms and before_message_id")
        );

        let zero_limit_error = app
            .list_messages_page(ListMessagesPageRequest {
                dialog_key: contact
                    .primary_dialog_key
                    .clone()
                    .expect("contact should have a primary dialog"),
                before_created_at_unix_ms: None,
                before_message_id: None,
                limit: 0,
            })
            .expect_err("zero limit should be rejected");
        assert!(
            zero_limit_error
                .to_string()
                .contains("limit must be greater than zero")
        );

        let diagnostics = app
            .queue_diagnostics()
            .expect("queue diagnostics should load");
        assert_eq!(diagnostics.queue_length, 1);
        assert_eq!(diagnostics.queued_count, 1);
        assert_eq!(diagnostics.retrying_count, 0);
    }

    #[test]
    fn send_text_rejects_over_cap_body_up_front_instead_of_dropping_silently() {
        // Regression guard: a body at/over CHAT_BODY_MAX_BYTES used to be
        // accepted by send_text (returned OK, queued, "sent") and then silently
        // rejected by the receiver's validate_chat_text_v1 — the message
        // vanished with no error to the caller. The send path must now fail
        // closed at the same ceiling the receiver enforces, so the over-cap
        // message never enters the queue at all.
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-app-tests")
            .join(format!("{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Remote".to_string(),
                remote_account_id: Some(AccountId("acct-remote".to_string())),
                public_display_name: Some("Remote".to_string()),
                remote_instance_id: Some("inst-remote".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-remote")),
                initial_observation: None,
            })
            .unwrap();
        let dialog_key = contact
            .primary_dialog_key
            .as_deref()
            .expect("contact should have a primary dialog");

        // One byte over the cap: rejected up front with a caller-visible error
        // that points at attachments for large content.
        let over_cap = "b".repeat(omni_protocol::CHAT_BODY_MAX_BYTES + 1);
        let error = app
            .send_text(dialog_key, over_cap)
            .expect_err("over-cap body must be rejected, not silently dropped");
        let message = error.to_string();
        assert!(message.contains("body"), "unexpected error: {message}");
        assert!(message.contains("attachment"), "unexpected error: {message}");

        // The rejected send must not have persisted anything (no silent queue).
        let after_reject = app
            .list_messages(dialog_key)
            .expect("history should load");
        assert!(
            after_reject.messages.is_empty(),
            "over-cap send must not persist a message"
        );

        // Exactly at the cap is still accepted (mirrors the receiver's
        // `len > max` boundary — equal length passes).
        let at_cap = "b".repeat(omni_protocol::CHAT_BODY_MAX_BYTES);
        app.send_text(dialog_key, &at_cap)
            .expect("body at exactly the cap must be accepted");
        let after_accept = app
            .list_messages(dialog_key)
            .expect("history should load");
        assert_eq!(after_accept.messages.len(), 1);
        assert_eq!(after_accept.messages[0].body.len(), omni_protocol::CHAT_BODY_MAX_BYTES);
    }

    #[test]
    fn connected_send_delivers_body_up_to_cap_both_directions() {
        // Companion to the send-side guard: prove CHAT_BODY_MAX_BYTES is the
        // *true* delivery ceiling, so the guard leaves no silent-drop gap below
        // it. A body of exactly the cap, and 16011 bytes (the size a live report
        // claimed silently vanished — it is < cap, so it must actually deliver),
        // both round-trip in *both* directions over the real transport (the mesh). If a
        // sub-cap body ever stops arriving here, there is a second, lower
        // bottleneck on the wire and the send guard must move down to match it.
        let _guard = network_test_guard();
        let server = create_stage5_peer("cap-delivery-server", "Server", "Server PC");
        let client = create_stage5_peer("cap-delivery-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");
        link_stage5_contact(&server.app, &client, "Client");

        link_stage5_peers_over_mesh(&client, &server);

        let wait_incoming_len = |app: &OmniApp, dialog: &str, want_len: usize| {
            wait_for_condition(Duration::from_secs(10), || {
                app.list_messages(dialog)
                    .expect("history should load")
                    .messages
                    .iter()
                    .any(|m| m.direction == ChatDirection::Incoming && m.body.len() == want_len)
            });
        };

        for len in [16011usize, omni_protocol::CHAT_BODY_MAX_BYTES] {
            let body = "b".repeat(len);
            client
                .app
                .send_text(server.dialog_key.as_str(), &body)
                .unwrap_or_else(|e| panic!("client->server send of {len} bytes was rejected: {e}"));
            wait_incoming_len(&server.app, client.dialog_key.as_str(), len);

            server
                .app
                .send_text(client.dialog_key.as_str(), &body)
                .unwrap_or_else(|e| panic!("server->client send of {len} bytes was rejected: {e}"));
            wait_incoming_len(&client.app, server.dialog_key.as_str(), len);
        }
        stop_stage5_meshes(&[&client, &server]);
    }

    #[test]
    fn connected_send_persists_outgoing_and_incoming_messages() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("connected-send-server", "Server", "Server PC");
        let client = create_stage5_peer("connected-send-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");
        link_stage5_contact(&server.app, &client, "Client");

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_text(server.dialog_key.as_str(), "hello over lan")
            .expect("send should succeed");
        assert_eq!(sent.direction, ChatDirection::Outgoing);
        assert_eq!(sent.dialog_key, server.dialog_key);

        wait_for_message_status(
            &client.app,
            sent.dialog_key.as_str(),
            "hello over lan",
            MessageStatus::Delivered,
        );
        let client_history = client
            .app
            .list_messages(sent.dialog_key.clone())
            .expect("client history should load");
        assert_eq!(client_history.messages.len(), 1);
        assert_eq!(
            client_history.messages[0].direction,
            ChatDirection::Outgoing
        );
        assert_eq!(client_history.messages[0].status, MessageStatus::Delivered);
        assert_eq!(client_history.messages[0].body, "hello over lan");

        wait_for_message_count(
            &server.app,
            client.dialog_key.as_str(),
            1,
        );
        let server_history = server
            .app
            .list_messages(client.dialog_key.as_str())
            .expect("server history should load");
        assert_eq!(server_history.messages.len(), 1);
        assert_eq!(
            server_history.messages[0].direction,
            ChatDirection::Incoming
        );
        assert_eq!(server_history.messages[0].status, MessageStatus::Delivered);
        assert_eq!(server_history.messages[0].body, "hello over lan");
        assert_ne!(server_history.messages[0].dialog_key, "127.0.0.1");
    }

    #[test]
    fn connected_send_auto_verifies_mnemonic_ownership_before_delivery() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("auto-verify-server", "Server", "Server PC");
        let client = create_stage5_peer("auto-verify-client", "Client", "Client PC");
        let client_contact =
            link_stage5_contact_without_verification(&client.app, &server, "Server");
        let server_contact =
            link_stage5_contact_without_verification(&server.app, &client, "Client");

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_text(
                server.dialog_key.as_str(),
                "hello after automatic verification",
            )
            .expect("send should queue and then deliver after automatic verification");
        assert_eq!(sent.direction, ChatDirection::Outgoing);
        assert_eq!(sent.dialog_key, server.dialog_key);

        wait_for_contact_verification_state(
            &client.app,
            &client_contact.contact_id,
            TrustState::Verified,
        );
        wait_for_contact_verification_state(
            &server.app,
            &server_contact.contact_id,
            TrustState::Verified,
        );

        wait_for_message_status(
            &client.app,
            sent.dialog_key.as_str(),
            "hello after automatic verification",
            MessageStatus::Delivered,
        );
        let client_history = client
            .app
            .list_messages(sent.dialog_key.clone())
            .expect("client history should load");
        assert!(
            client_history.messages.iter().any(|message| {
                message.body == "hello after automatic verification"
                    && message.status == MessageStatus::Delivered
            })
        );

        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);
        let server_history = server
            .app
            .list_messages(client.dialog_key.as_str())
            .expect("server history should load");
        assert_eq!(server_history.messages.len(), 1);
        assert_eq!(server_history.messages[0].direction, ChatDirection::Incoming);
        assert_eq!(server_history.messages[0].status, MessageStatus::Delivered);
        assert_eq!(
            server_history.messages[0].body,
            "hello after automatic verification"
        );
    }

    #[test]
    fn lan_quick_add_uses_automatically_exchanged_remote_bundle() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("lan-auto-bundle-server", "Server", "Server PC");
        let client = create_stage5_peer("lan-auto-bundle-client", "Client", "Client PC");

        link_stage5_peers_over_mesh(&client, &server);
        // Quick add takes the account from the ACTIVE dialog's verified session: wait until that is the
        // server's. Adding the moment the link was up raced the bundle exchange and the contact came out
        // without an account (a flake of the gate, 2026-10-10).
        let server_account = server.account.descriptor.account_id.0.clone();
        wait_for_condition(Duration::from_secs(30), || {
            client.app.transport.snapshot().dialog_key.as_deref() == Some(server.dialog_key.as_str())
                && client.app.transport.active_remote_account_id().as_deref() == Some(server_account.as_str())
        });

        let contact = client
            .app
            .quick_add_contact_from_dialog(server.dialog_key.as_str(), "Server over LAN", None)
            .expect("quick add should create contact from active dialog");
        assert_eq!(
            contact.remote_account_id.as_ref().map(|value| value.0.as_str()),
            Some(server.account.descriptor.account_id.0.as_str())
        );

        server
            .app
            .store_remote_prekey_bundle(&client.bundle)
            .unwrap();
        let reciprocal = server
            .app
            .add_contact(&AddContactInput {
                petname: "Client".to_string(),
                remote_account_id: Some(client.account.descriptor.account_id.clone()),
                public_display_name: Some(client.account.descriptor.display_name.clone()),
                remote_instance_id: Some(client.instance_id.clone()),
                primary_dialog_key: Some(client.dialog_key.clone()),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some(client.account.descriptor.account_id.0.clone()),
                    observed_display_name: Some(client.account.descriptor.display_name.clone()),
                    observed_at_unix_ms: current_unix_ms(),
                }),
            })
            .unwrap();
        server
            .app
            .verify_contact(&reciprocal.contact_id, VerificationMethod::ChallengeResponse)
            .unwrap();

        let sent = client
            .app
            .send_text(server.dialog_key.as_str(), "hello from auto bundle exchange")
            .expect("send should succeed after automatic bundle exchange");
        assert_eq!(sent.direction, ChatDirection::Outgoing);
        assert!(matches!(
            sent.status,
            MessageStatus::Sent
                | MessageStatus::Queued
                | MessageStatus::Retrying
                | MessageStatus::Delivered
        ));

        wait_for_message_status(
            &client.app,
            sent.dialog_key.as_str(),
            "hello from auto bundle exchange",
            MessageStatus::Delivered,
        );
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);
        let server_history = server
            .app
            .list_messages(client.dialog_key.as_str())
            .expect("server history should load");
        assert_eq!(server_history.messages.len(), 1);
        assert_eq!(server_history.messages[0].direction, ChatDirection::Incoming);
        assert_eq!(server_history.messages[0].status, MessageStatus::Delivered);
        assert_eq!(server_history.messages[0].body, "hello from auto bundle exchange");
    }

    #[test]
    fn accepted_connection_can_reply_without_reverse_connect() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("reply-server", "Server", "Server PC");
        let client = create_stage5_peer("reply-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");
        link_stage5_contact(&server.app, &client, "Client");

        link_stage5_peers_over_mesh(&client, &server);

        client
            .app
            .send_text(server.dialog_key.as_str(), "ping from client")
            .expect("client should send first message");
        wait_for_message_count(
            &server.app,
            client.dialog_key.as_str(),
            1,
        );
        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "ping from client",
            MessageStatus::Delivered,
        );

        let reply = server
            .app
            .send_text(client.dialog_key.as_str(), "reply from server")
            .expect("server should reply over accepted connection");
        assert_eq!(reply.direction, ChatDirection::Outgoing);
        assert!(matches!(reply.status, MessageStatus::Sent | MessageStatus::Delivered));

        wait_for_message_count(&client.app, server.dialog_key.as_str(), 2);
        let client_history = client
            .app
            .list_messages(server.dialog_key.as_str())
            .expect("client history should load");
        assert_eq!(client_history.messages.len(), 2);
        assert_eq!(
            client_history.messages[1].direction,
            ChatDirection::Incoming
        );
        assert_eq!(client_history.messages[1].body, "reply from server");
    }

    #[test]
    fn history_reloads_after_restart_from_same_database() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-1-app-tests")
            .join(format!("restart-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Restart".to_string(),
                remote_account_id: Some(AccountId("acct-restart".to_string())),
                public_display_name: Some("Restart".to_string()),
                remote_instance_id: Some("inst-restart".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-restart")),
                initial_observation: None,
            })
            .unwrap();

        let queued = app
            .send_text(
                contact
                    .primary_dialog_key
                    .as_deref()
                    .expect("contact should have a primary dialog"),
                "persist me",
            )
            .expect("send should queue without active transport connection");
        assert_eq!(queued.status, MessageStatus::Queued);

        drop(app);

        let reopened = OmniApp::init(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .expect("reopened app should initialize");

        let history = reopened
            .list_messages(
                contact
                    .primary_dialog_key
                    .as_deref()
                    .expect("contact should have a primary dialog"),
            )
            .expect("history should load after restart");
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].body, "persist me");
        // Kept and not sent. A queue tick between the send and the restart may already have tried
        // it (no transport) and made it Retrying — seen in a loaded full gate 2026-10-17; either is
        // the state this test is about.
        assert!(
            matches!(history.messages[0].status, MessageStatus::Queued | MessageStatus::Retrying),
            "status={:?}",
            history.messages[0].status
        );
    }

    #[test]
    fn unsupported_legacy_database_is_reported_in_bootstrap_summary() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-app-tests")
            .join(format!("legacy-{}.db", unique_suffix()));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let connection = omni_storage::Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO messages (id, peer_addr, body, status, created_at_unix_ms, direction, updated_at_unix_ms)
                VALUES ('legacy-msg', '127.0.0.1', 'legacy', 'delivered', 10, 'outgoing', 10);
                ",
            )
            .unwrap();
        drop(connection);

        let summary = OmniApp::inspect_database_bootstrap(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            omni_identity::DatabaseBootstrapStateKind::UnsupportedDatabaseFormat
        );
        assert!(summary.reset_required);
    }

    #[test]
    fn queue_recovery_after_restart_and_reconnect_delivers_without_duplicate_ui_rows() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("queue-server", "Server", "Server PC");
        let client = create_stage5_peer("queue-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        let client_db_path = client.db_path.clone();
        let client_dialog_key = client.dialog_key.clone();
        let queued = client
            .app
            .send_text(server.dialog_key.as_str(), "queued before connect")
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Queued);
        drop(client.app);

        let reopened_client = OmniApp::init(AppInitRequest {
            db_path: client_db_path,
        })
        .unwrap();
        link_apps_over_mesh(&reopened_client, &server.app);

        wait_for_message_status(
            &reopened_client,
            server.dialog_key.as_str(),
            "queued before connect",
            MessageStatus::Delivered,
        );
        wait_for_message_count(
            &server.app,
            client_dialog_key.as_str(),
            1,
        );

        let client_history = reopened_client.list_messages(server.dialog_key.as_str()).unwrap();
        assert_eq!(client_history.messages.len(), 1);
        assert_eq!(client_history.messages[0].body, "queued before connect");
        assert_eq!(client_history.messages[0].status, MessageStatus::Delivered);
    }

    #[test]
    fn sent_waiting_for_session_keeps_retrying_not_buried_without_duplicate_history_rows() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("ack-fail-server", "Raw Server", "Raw Server PC");
        let client = create_stage5_peer("ack-fail-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Raw Server");

        // The server is reachable but cannot answer: its link is up, its profile is signed out, so
        // the client's session bootstrap arrives and is never acknowledged.
        link_stage5_peers_over_mesh(&client, &server);
        let server_profile = server
            .account
            .local_profile_summary
            .as_ref()
            .expect("server profile summary")
            .profile_id
            .clone();
        confirm_sign_out_profile(&server.app, &server_profile);

        let sent = client
            .app
            .send_text(server.dialog_key.as_str(), "ack never comes")
            .unwrap();
        assert!(matches!(
            sent.status,
            MessageStatus::Queued | MessageStatus::Retrying | MessageStatus::Sent
        ));

        // The bootstrap is never acknowledged, so the session never establishes — but
        // the message must NOT be buried. It moves to Retrying and stays queued, retried with
        // backoff, until the session can establish (the peer may just be offline). Previously
        // this buried the message as Failed after a fixed timeout — data loss. See
        // docs/decisions/delivery-reliability-model.md.
        wait_for_message_status(
            &client.app,
            server.dialog_key.as_str(),
            "ack never comes",
            MessageStatus::Retrying,
        );

        // Wait well past the OLD fixed bury timeout (test SESSION_BOOTSTRAP_WAIT was 2 s) and
        // confirm it is STILL not Failed, with exactly one history row (no duplicate rows from
        // the repeated retries).
        std::thread::sleep(Duration::from_secs(3));
        let history = client.app.list_messages(server.dialog_key.as_str()).unwrap();
        assert_eq!(history.messages.len(), 1, "no duplicate history rows across retries");
        assert_ne!(
            history.messages[0].status,
            MessageStatus::Failed,
            "a message to a contact must not be buried on a transient session-bootstrap wait"
        );

        let diagnostics = client.app.queue_diagnostics().unwrap();
        assert_eq!(diagnostics.failed_count, 0, "never buried");
        assert_eq!(diagnostics.queue_length, 1, "still pending + retried, not dropped");
        assert_eq!(diagnostics.pending_by_dialog.len(), 1);
        // The reason is still surfaced (so the UI can show "waiting for session"), but as a
        // RETRY reason, not a terminal failure.
        assert_eq!(
            diagnostics.pending_by_dialog[0].last_failure_kind,
            Some(QueueFailureKind::NoActiveSession)
        );
        assert!(diagnostics
            .last_failure_message
            .as_deref()
            .unwrap_or_default()
            .contains("bootstrap acknowledgement"));
    }

    /// A message sent while the peer is offline arrives once it comes back — exactly once, with one
    /// history row on each side. Runs over the mesh, the path the product uses: the old version of
    /// this test reconnected through the pre-mesh QUIC engine and passed only because that engine's
    /// LAN discovery re-introduced the peers (docs/decisions/no-friend-tier.md, part 2). The link is
    /// dialed at Bob's loopback mesh address rather than waiting for multicast, so the test runs on
    /// a host whose multicast is unusable too (a VPN/TUN adapter).
    #[test]
    fn a_message_sent_while_the_peer_is_offline_arrives_once_after_it_returns() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("mesh-retry-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-retry-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");

        let link_alice_to_bob = || {
            let bob_hex = bob.app.mesh_node_address_hex().unwrap().expect("bob mesh is up");
            let bob_addr = bob
                .app
                .mesh
                .lock()
                .unwrap()
                .as_ref()
                .expect("bob mesh is up")
                .quic_addr()
                .to_string();
            wait_for_condition(Duration::from_secs(20), || {
                let _ = alice.app.mesh_dial_at(&bob_hex, &bob_addr);
                alice.app.mesh_has_link(&bob_hex).unwrap_or(false)
            });
        };
        let bob_rows = |body: &str| {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .map(|history| history.messages.iter().filter(|message| message.body == body).count())
                .unwrap_or(0)
        };

        alice.app.start_mesh().expect("alice mesh starts");
        bob.app.start_mesh().expect("bob mesh starts");
        link_alice_to_bob();

        // Bob goes away. Alice learns it the way the product does — the liveness pings stop being
        // answered and the link is pruned (~20 s) — and only then is the message written.
        let bob_hex = bob.app.mesh_node_address_hex().unwrap().expect("bob mesh is up");
        bob.app.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || {
            !alice.app.mesh_has_link(&bob_hex).unwrap_or(true)
        });
        let body = "sent while bob was offline";
        alice.app.send_text(&bob.dialog_key, body).expect("alice sends chat");
        thread::sleep(Duration::from_secs(2));
        assert_eq!(bob_rows(body), 0, "nothing can reach Bob while his mesh is down");

        // Bob is back and linked again: the queued message goes through.
        bob.app.start_mesh().expect("bob mesh restarts");
        link_alice_to_bob();
        wait_for_condition(Duration::from_secs(45), || bob_rows(body) >= 1);
        wait_for_message_status(&alice.app, bob.dialog_key.as_str(), body, MessageStatus::Delivered);

        // Exactly once on both sides: the retries while Bob was away left no duplicate rows.
        thread::sleep(Duration::from_secs(2));
        assert_eq!(bob_rows(body), 1, "Bob holds the message exactly once");
        let alice_rows = alice
            .app
            .list_messages(bob.dialog_key.as_str())
            .unwrap()
            .messages
            .iter()
            .filter(|message| message.body == body)
            .count();
        assert_eq!(alice_rows, 1, "Alice's history has one row for one send");

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    #[test]
    fn unsupported_legacy_stage2_database_is_reported_in_bootstrap_summary() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-3-app-tests")
            .join(format!("coexist-{}.db", unique_suffix()));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        let connection = omni_storage::Storage::open_keyed_connection(&db_path).unwrap();
        connection
            .execute_batch(
                "
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY NOT NULL,
                    peer_addr TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL DEFAULT 'outgoing',
                    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE app_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                INSERT INTO messages (id, peer_addr, body, status, created_at_unix_ms, direction, updated_at_unix_ms)
                VALUES ('legacy-msg', '127.0.0.1', 'legacy body', 'delivered', 10, 'outgoing', 10);
                ",
            )
            .unwrap();
        drop(connection);

        let summary = OmniApp::inspect_database_bootstrap(AppInitRequest {
            db_path: db_path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(!summary.supported);
        assert_eq!(
            summary.state,
            omni_identity::DatabaseBootstrapStateKind::UnsupportedDatabaseFormat
        );
        assert!(summary.reset_required);
    }

    #[test]
    fn receipt_is_sent_only_after_durable_accept() {
        let receiver = create_stage5_peer("receipt-order-receiver", "Receiver", "Receiver PC");
        let sender = create_stage5_peer("receipt-order-sender", "Sender", "Sender PC");
        let bootstrap = sender
            .app
            .bootstrap_session_from_prekey_bundle(&receiver.bundle)
            .unwrap();
        receiver.app.accept_session_bootstrap(&bootstrap).unwrap();

        let receipt_was_sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:50000".to_string(),
            dialog_key: sender.dialog_key.clone(),
            remote_instance_id: sender.instance_id.clone(),
            remote_prekey_bundle: None,
            initiated_locally: false,
        };
        let session = receiver
            .app
            .storage
            .get_latest_session_for_remote_account(&sender.account.descriptor.account_id)
            .unwrap()
            .unwrap();

        accept_incoming_text(
            &receiver.app.storage,
            omni_types::ChatTextV1 {
                message_id: "receipt-msg".to_string(),
                body: "hello".to_string(),
                created_at_unix_ms: 100,
            },
            &receive_context,
            &session,
            omni_messaging::MessageEncryptionState::EncryptedReceived,
            SecurityPayloadKind::Text,
            {
                let receipt_was_sent = Arc::clone(&receipt_was_sent);
                let receiver_db_path = receiver.db_path.clone();
                move |outgoing| {
                    let storage = Storage::open(&receiver_db_path).unwrap();
                    storage.migrate().unwrap();
                    let persisted = storage.get_message("receipt-msg").unwrap();
                    assert_eq!(persisted.body, "hello");
                    assert!(matches!(
                        &outgoing.envelope.payload,
                        WirePayloadV1::EncryptedPayload(_)
                    ));
                    receipt_was_sent.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                }
            },
        )
        .unwrap();

        assert!(receipt_was_sent.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn accepted_inbound_connection_flushes_queued_dialog_messages() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("accepted-flush-server", "Server", "Server PC");
        let client = create_stage5_peer("accepted-flush-client", "Client", "Client PC");
        link_stage5_contact(&server.app, &client, "Client");

        let queued = server
            .app
            .send_text(client.dialog_key.as_str(), "queued before inbound connect")
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Queued);

        link_stage5_peers_over_mesh(&client, &server);

        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "queued before inbound connect",
            MessageStatus::Delivered,
        );

        // The client sees the message in its dialog with the server (keyed by the server instance).
        let client_dialog_key = server.dialog_key.clone();
        wait_for_message_count(&client.app, client_dialog_key.as_str(), 1);
        let client_history = client.app.list_messages(client_dialog_key).unwrap();
        assert_eq!(client_history.messages.len(), 1);
        assert_eq!(
            client_history.messages[0].direction,
            ChatDirection::Incoming
        );
        assert_eq!(
            client_history.messages[0].body,
            "queued before inbound connect"
        );
    }

    #[test]
    fn challenge_response_verification_flushes_queued_dialog_messages() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("challenge-flush-server", "Server", "Server PC");
        let client = create_stage5_peer("challenge-flush-client", "Client", "Client PC");

        let contact = link_stage5_contact(&server.app, &client, "Client");

        let queued = server
            .app
            .send_text(client.dialog_key.as_str(), "queued before challenge response")
            .unwrap();
        assert_eq!(queued.status, MessageStatus::Queued);

        link_stage5_peers_over_mesh(&client, &server);

        wait_for_condition(Duration::from_secs(5), || {
            server
                .app
                .get_contact_card(&contact.contact_id)
                .map(|card| card.trust_state == TrustState::Verified)
                .unwrap_or(false)
        });

        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "queued before challenge response",
            MessageStatus::Delivered,
        );
        wait_for_message_count(&client.app, server.dialog_key.as_str(), 1);
    }

    #[test]
    fn invalid_verification_response_drops_the_session() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("challenge-fail-server", "Server", "Server PC");
        let client = create_stage5_peer("challenge-fail-client", "Client", "Client PC");
        link_stage5_contact(&server.app, &client, "Client");
        link_stage5_contact(&client.app, &server, "Server");

        link_stage5_peers_over_mesh(&client, &server);

        let prime = client
            .app
            .send_text(server.dialog_key.as_str(), "prime verification session")
            .unwrap();
        assert!(matches!(
            prime.status,
            MessageStatus::Sent
                | MessageStatus::Queued
                | MessageStatus::Retrying
                | MessageStatus::Delivered
        ));
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);

        assert!(
            server
                .app
                .storage
                .get_latest_session_for_remote_account(&client.account.descriptor.account_id)
                .unwrap()
                .is_some(),
            "the server holds a session with the client before the bad response"
        );
        let server_context = server
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("server transport is up")
            .received_context_for_dialog_key(client.dialog_key.as_str());
        let challenge_id = format!("challenge-fail-{}", unique_suffix());
        let db_path = server.app.storage.db_path().to_string_lossy().to_string();
        super::register_pending_verification_challenge_for_test(
            &db_path,
            challenge_id.clone(),
            client.dialog_key.clone(),
            server_context.peer_id.clone(),
            Some(client.instance_id.clone()),
            Some(client.account.descriptor.account_id.0.clone()),
            b"bogus-challenge-bytes".to_vec(),
            current_unix_ms() + 60_000,
        );

        let verification_payload_json =
            serde_json::to_string(&client.app.get_my_verification_payload().unwrap()).unwrap();
        let invalid_response = omni_types::VerificationChallengeResponseV1 {
            version: omni_types::OMNI_PROTOCOL_VERSION_V1,
            challenge_id,
            verification_payload_json,
            signature_r: "not-a-valid-signature".to_string(),
            signature_s: "still-not-valid".to_string(),
            responded_at_unix_ms: current_unix_ms(),
        };

        let client_session = client
            .app
            .storage
            .get_latest_session_for_remote_account(&server.account.descriptor.account_id)
            .unwrap()
            .expect("client session should exist before sending the invalid response");
        let outgoing = omni_messaging::build_encrypted_wire_envelope(
            &client_session,
            &omni_types::WirePayloadV1::VerificationChallengeResponse(invalid_response.clone()),
            SecurityPayloadKind::VerificationChallengeResponse,
            current_unix_ms(),
        )
        .unwrap();

        let transport = client
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("client transport should still be available");
        transport
            .send_wire_envelope_for_dialog_key(server.dialog_key.as_str(), &outgoing.envelope)
            .unwrap();

        // A forged challenge response tears the server's session with the sender down.
        wait_for_condition(Duration::from_secs(5), || {
            server
                .app
                .storage
                .get_latest_session_for_remote_account(&client.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_none()
        });
    }

    #[test]
    fn two_node_session_bootstrap_and_encrypted_message_delivery() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("two-node-server", "Server", "Server PC");
        let client = create_stage5_peer("two-node-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_text(server.dialog_key.as_str(), "secret hello")
            .unwrap();
        assert!(matches!(
            sent.status,
            MessageStatus::Sent
                | MessageStatus::Queued
                | MessageStatus::Retrying
                | MessageStatus::Delivered
        ));

        wait_for_message_status(
            &client.app,
            server.dialog_key.as_str(),
            "secret hello",
            MessageStatus::Delivered,
        );
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);

        let server_history = server.app.list_messages(client.dialog_key.as_str()).unwrap();
        assert_eq!(server_history.messages.len(), 1);
        assert_eq!(server_history.messages[0].body, "secret hello");

        let details = client
            .app
            .get_message_security_details(&sent.id)
            .unwrap()
            .unwrap();
        assert!(!details.has_encrypted_payload);
        assert_eq!(
            details.payload_kind,
            Some(omni_messaging::SecurityPayloadKind::Text)
        );
        assert_eq!(
            details.remote_account_id.as_deref(),
            Some(server.account.descriptor.account_id.0.as_str())
        );
    }

    #[test]
    fn realtime_media_negotiation_accepts_remote_local_dialog_keys() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("media-negotiate-server", "Server", "Server PC");
        let client = create_stage5_peer("media-negotiate-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        link_stage5_peers_over_mesh(&client, &server);

        client
            .app
            .send_text(server.dialog_key.as_str(), "prime media negotiation")
            .unwrap();
        wait_for_message_status(
            &client.app,
            server.dialog_key.as_str(),
            "prime media negotiation",
            MessageStatus::Delivered,
        );
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);

        let client_capabilities = test_realtime_media_capabilities(
            server.dialog_key.as_str(),
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 30),
            (1920, 1080, 60),
        );
        let client_id = client_capabilities.capabilities_id.clone();
        let server_capabilities = test_realtime_media_capabilities(
            client.dialog_key.as_str(),
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );

        client
            .app
            .publish_realtime_media_capabilities(client_capabilities)
            .expect("client should publish local media capabilities");
        server
            .app
            .publish_realtime_media_capabilities(server_capabilities)
            .expect("server should publish local media capabilities");

        wait_for_condition(Duration::from_secs(5), || {
            let client_snapshot = client
                .app
                .get_realtime_media_negotiation(server.dialog_key.as_str())
                .expect("client media snapshot should load");
            let server_snapshot = server
                .app
                .get_realtime_media_negotiation(client.dialog_key.as_str())
                .expect("server media snapshot should load");
            client_snapshot.status == RealtimeMediaNegotiationStatus::Negotiated
                && server_snapshot.status == RealtimeMediaNegotiationStatus::Negotiated
        });

        let client_selection = client
            .app
            .get_realtime_media_negotiation(server.dialog_key.as_str())
            .unwrap()
            .selected
            .expect("client should have selected media");
        let server_selection = server
            .app
            .get_realtime_media_negotiation(client.dialog_key.as_str())
            .unwrap()
            .selected
            .expect("server should have selected media");

        assert_eq!(client_selection.dialog_key, server.dialog_key);
        assert_eq!(server_selection.dialog_key, client.dialog_key);
        let client_video = client_selection
            .video
            .expect("both test peers advertise a camera, so video must be negotiated");
        let server_video = server_selection
            .video
            .expect("both test peers advertise a camera, so video must be negotiated");
        // Both sides read the same two directions (the answerer took the offerer's selection).
        for video in [&client_video, &server_video] {
            assert_eq!(video.streams.len(), 2);
            assert!(video.streams.iter().all(|s| s.codec == RealtimeVideoCodec::H264));
        }
        // Each direction in its sender's format: the client sends 1080p, the server 720p.
        for video in [&client_video, &server_video] {
            for stream in &video.streams {
                let expected = if stream.sender_capabilities_id == client_id { (1920, 1080) } else { (1280, 720) };
                assert_eq!((stream.width, stream.height), expected, "{stream:?}");
            }
        }
    }

    /// Phase α-4j: a chat message (and the X3DH bootstrap it triggers) is
    /// delivered between two cores entirely over the mesh — no legacy
    /// listener/connect, no `om3` signaling token. Proves the omni-app mesh
    /// carrier + SessionHello handshake end-to-end. Skips gracefully if the
    /// host lacks a multicast-capable adapter (mirrors omni-network's tests).
    #[test]
    fn two_node_chat_message_delivers_over_mesh() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("mesh-chat-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-chat-bob", "Bob", "Bob PC");
        // Approve each other in-process (contacts + stored prekey bundles).
        link_stage5_contact(&alice.app, &bob, "Bob");

        // Bring up the mesh on both sides — no legacy listener/connect.
        alice.app.start_mesh().expect("alice mesh starts");
        bob.app.start_mesh().expect("bob mesh starts");

        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };

        // Dialed on loopback, not found by multicast: the link is not what this tests, and a host
        // without LAN multicast used to skip the whole test and still count it as passed.
        link_apps_over_mesh(&alice.app, &bob.app);
        let _ = bob_signing_pubkey;

        // Send a chat message. send_text queues it; the queue worker drives the
        // X3DH bootstrap + the encrypted message, all over the mesh Link via the
        // PeerCarrier (no legacy connection exists).
        alice
            .app
            .send_text(&bob.dialog_key, "hello over mesh")
            .expect("alice sends chat");

        // Bob receives + decrypts it over the mesh.
        wait_for_condition(Duration::from_secs(30), || {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history
                        .messages
                        .iter()
                        .any(|message| message.body == "hello over mesh")
                })
                .unwrap_or(false)
        });

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    /// Phase α-4d: the mesh auto-connect loop *alone* establishes the verified
    /// Link and carries a chat message — there is no explicit
    /// `mesh_connect_dialog`. Both cores call `start_mesh` (which spawns the
    /// auto-connect worker); the worker dials each approved contact until its
    /// Link is up, then the normal queue + X3DH path rides it via the
    /// PeerCarrier. This is the live-path enabler: at sign-in the app just
    /// starts the mesh and approved contacts become reachable on their own — no
    /// `om3` token, no manual dial. Distinguishes environment from regression:
    /// if the Link never forms the host lacks a multicast adapter (skip, like
    /// the sibling test); if it forms but delivery fails the assertion panics.
    #[test]
    #[ignore = "needs LAN multicast discovery; run with --ignored on a host that has it"]
    fn mesh_autoconnect_alone_links_peers_and_delivers_chat() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("mesh-auto-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-auto-bob", "Bob", "Bob PC");
        // Approve each other in-process (contacts + stored prekey bundles) —
        // all the auto-connect worker needs to begin dialing.
        link_stage5_contact(&alice.app, &bob, "Bob");

        // Bring up the mesh on both sides. start_mesh spawns the auto-connect
        // worker; crucially we never call mesh_connect_dialog — the Link must
        // form purely from the worker dialing the approved contact.
        alice.app.start_mesh().expect("alice mesh starts");
        bob.app.start_mesh().expect("bob mesh starts");

        // Bob's NodeAddress is derived from the Ed25519 identity *signing* key
        // in his prekey bundle — the same key the auto-connect worker resolves.
        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");

        // Wait for the auto-connect worker to bring the Link up on its own
        // (discovery is async multicast; the worker retries every few seconds).
        // If it never comes up, this host has no multicast adapter — skip,
        // mirroring two_node_chat_message_delivers_over_mesh.
        // Needs LAN multicast discovery (the worker dials only what it discovers): #[ignore]d, run
        // with `--ignored` on a host that has it — there, no link is a failure, not a skip.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !alice.app.mesh.lock().ok().and_then(|guard| guard.as_ref().map(|runtime| runtime.has_link(&bob_node))).unwrap_or(false) {
            assert!(Instant::now() < deadline, "no link by auto-connect alone: no LAN multicast on this host, or a regression");
            thread::sleep(Duration::from_millis(150));
        }

        // The Link is up via auto-connect alone. A chat message now rides it:
        // send_text queues it; the queue worker drives the X3DH bootstrap + the
        // encrypted message over the mesh PeerCarrier. No legacy connection
        // exists and no explicit dial was ever made.
        alice
            .app
            .send_text(&bob.dialog_key, "hello via autoconnect")
            .expect("alice sends chat");

        // Bob receives + decrypts it over the auto-connected mesh Link.
        wait_for_condition(Duration::from_secs(30), || {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history
                        .messages
                        .iter()
                        .any(|message| message.body == "hello via autoconnect")
                })
                .unwrap_or(false)
        });

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    /// Realtime-media packets travel over the mesh Link — the path that carries a live call: the
    /// media engine hands each packet to `send_realtime_media_packet` through the route resolved
    /// once per call. Here there is no legacy connection at all (no listener/connect) — if Bob
    /// drains the datagrams, they can only have come over the mesh.
    ///
    /// Failure points are distinct and debuggable: no Link forming ⇒ the host
    /// lacks a multicast adapter (skip); negotiation never reaching `Negotiated`
    /// ⇒ the mesh wire plane is broken (panic); the Link + negotiation up but no
    /// datagrams drained ⇒ the packet→mesh routing regressed (panic).
    #[test]
    fn mesh_carries_realtime_media_packets() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("mesh-media-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-media-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");

        alice.app.start_mesh().expect("alice mesh starts");
        bob.app.start_mesh().expect("bob mesh starts");

        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");

        // Wait for the auto-connect worker to bring the Link up. The mutual dial
        // populates both directions' dialog→node maps, which the bidirectional
        // negotiation below needs. No multicast adapter ⇒ skip.
        // Dialed on loopback, not found by multicast: the link is not what this tests, and a host
        // without LAN multicast used to skip the whole test and still count it as passed.
        link_apps_over_mesh(&alice.app, &bob.app);
        let _ = &bob_node;

        // Prime the X3DH session over the mesh so capability events can be
        // encrypted (one delivered message establishes the session both ways).
        alice
            .app
            .send_text(&bob.dialog_key, "prime media negotiation")
            .expect("alice sends priming chat");
        wait_for_condition(Duration::from_secs(30), || {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history
                        .messages
                        .iter()
                        .any(|message| message.body == "prime media negotiation")
                })
                .unwrap_or(false)
        });

        // Negotiate media over the mesh wire plane — this arms Bob's inbound
        // media gate (`dialog_has_active_media_negotiation`) for Alice's dialog.
        let alice_caps = test_realtime_media_capabilities(
            bob.dialog_key.as_str(),
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 30),
            (1920, 1080, 60),
        );
        let bob_caps = test_realtime_media_capabilities(
            alice.dialog_key.as_str(),
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );
        alice
            .app
            .publish_realtime_media_capabilities(alice_caps)
            .expect("alice publishes media capabilities");
        bob.app
            .publish_realtime_media_capabilities(bob_caps)
            .expect("bob publishes media capabilities");
        wait_for_condition(Duration::from_secs(15), || {
            let alice_done = alice
                .app
                .get_realtime_media_negotiation(bob.dialog_key.as_str())
                .map(|snapshot| snapshot.status == RealtimeMediaNegotiationStatus::Negotiated)
                .unwrap_or(false);
            let bob_done = bob
                .app
                .get_realtime_media_negotiation(alice.dialog_key.as_str())
                .map(|snapshot| snapshot.status == RealtimeMediaNegotiationStatus::Negotiated)
                .unwrap_or(false);
            alice_done && bob_done
        });

        // Send a batch of distinct datagrams. With no legacy connection, the
        // only way these reach Bob is the mesh Link via the batch path.
        let expected: Vec<Vec<u8>> = vec![
            vec![0x11, 0x22, 0x33],
            vec![0x44, 0x55, 0x66, 0x77],
            vec![0x88, 0x99],
        ];
        // Bob's receive side is wired before anything is sent: inbound media goes straight to the
        // wired sink, and a packet that arrives with none is dropped, not kept for later.
        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let received = Arc::clone(&received);
            bob.app
                .set_realtime_media_inbound_sink(
                    alice.dialog_key.as_str(),
                    Some(Arc::new(move |packet: &[u8]| {
                        received.lock().unwrap().push(packet.to_vec());
                    })),
                )
                .expect("bob wires his receive side");
        }
        let route = alice
            .app
            .realtime_media_route(bob.dialog_key.as_str())
            .expect("alice resolves the media route to bob");
        for payload in &expected {
            alice
                .app
                .send_realtime_media_packet(&route, payload)
                .expect("alice sends a realtime media packet over the mesh");
        }

        // Datagrams are unreliable but loopback-reliable in practice.
        let media_deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < media_deadline && received.lock().unwrap().len() < expected.len() {
            thread::sleep(Duration::from_millis(100));
        }
        let received = received.lock().unwrap().clone();
        assert_eq!(
            received.len(),
            expected.len(),
            "every packet should arrive over the mesh"
        );
        for payload in &expected {
            assert!(
                received.iter().any(|got| got == payload),
                "datagram {payload:?} should have been delivered over the mesh"
            );
        }

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    /// Phase α-4d: the send-side mesh node resolver derives a peer's NodeAddress
    /// from the contact's stored identity, independent of `mesh_dialogs`. This
    /// is the fallback `mesh_node_for_dialog` uses when a call addresses its
    /// peer by a canonical dialog-key the SessionHello never mapped (so media
    /// rides the mesh instead of silently dropping to the legacy transport).
    /// The mesh is deliberately *not* started here — `mesh_dialogs` is empty — so
    /// a `Some` result can only have come from the contact-identity path, making
    /// this deterministic (no multicast, no skip).
    #[test]
    fn mesh_node_for_contact_dialog_resolves_from_contact_identity() {
        let alice = create_stage5_peer("mesh-resolve-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-resolve-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");

        // The node both sides independently derive from Bob's Ed25519 identity
        // signing key (blake3(pubkey)[..20]).
        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");

        // Bob's dialog resolves to his node via the stored contact + prekey
        // bundle — even though the mesh was never started, so nothing populated
        // `mesh_dialogs`. This is exactly the fallback's reason to exist.
        let resolved = alice
            .app
            .mesh_node_for_contact_dialog(bob.dialog_key.as_str())
            .expect("resolve must not error");
        assert_eq!(
            resolved,
            Some(bob_node),
            "an approved contact's dialog must resolve to its mesh node from identity"
        );

        // A dialog with no contact resolves to None so the caller falls back to
        // the legacy transport cleanly rather than misrouting.
        let unknown = alice
            .app
            .mesh_node_for_contact_dialog("dialog:instance:nobody-here")
            .expect("resolve must not error");
        assert_eq!(unknown, None, "an unknown dialog must not resolve to any node");
    }

    /// The rendezvous auto-trigger short-circuits cleanly when there's nothing to do: an
    /// approved contact resolves to a mesh node, but with the mesh never started the
    /// trigger must return `Ok(false)` — no background dial thread, no panic — rather than
    /// attempt a connect. (The positive cross-NAT dial is live-validated, not unit-tested.)
    #[test]
    fn rendezvous_reach_contact_is_noop_without_a_started_mesh() {
        let alice = create_stage5_peer("rdv-trigger-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("rdv-trigger-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let bob_account = omni_identity::AccountId(bob.bundle.account.account_id.clone());

        let kicked = alice
            .app
            .rendezvous_reach_contact_if_no_link(&bob_account)
            .expect("trigger must not error");
        assert!(!kicked, "no started mesh ⇒ nothing to dial onto");
    }

    /// The on-demand rendezvous trigger honours the `DirectOnly` privacy choice: even with
    /// the mesh started and an approved, currently-unlinked contact, a `DirectOnly` profile
    /// must NOT kick a serverless-rendezvous dial (it touches public providers) — matching
    /// how the autoconnect sweep gates itself. The gate is the single core entry point, so no
    /// FFI/shell caller can bypass the "zero third parties" guarantee.
    #[test]
    fn rendezvous_reach_contact_respects_direct_only_privacy() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("rdv-privacy-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("rdv-privacy-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let bob_account = omni_identity::AccountId(bob.bundle.account.account_id.clone());

        alice
            .app
            .set_connection_privacy_mode(crate::ConnectionPrivacyMode::DirectOnly)
            .expect("set direct-only");
        // Mesh IS started (no autoconnect worker) so the only reason a dial would be skipped
        // is the privacy gate, not the not-started short-circuit.
        alice.app.start_mesh_no_autoconnect().expect("start mesh");

        let kicked = alice
            .app
            .rendezvous_reach_contact_if_no_link(&bob_account)
            .expect("trigger must not error");
        assert!(!kicked, "DirectOnly ⇒ on-demand rendezvous must not touch a provider");
    }

    /// The honest connection-state DTO: `NotStarted` + no contacts before the mesh runs; once
    /// started, the node leaves `NotStarted` and every approved contact appears with a link-state.
    /// (The positive direct/relay link-states are exercised by the omni-network unit test; here we
    /// assert the aggregation shape + the not-started short-circuit.)
    #[test]
    fn mesh_connection_state_reports_not_started_then_node_and_contacts() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("conn-state-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("conn-state-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let bob_account = bob.bundle.account.account_id.clone();

        // No mesh → NotStarted, contacts list short-circuited empty.
        let before = alice.app.mesh_connection_state().expect("state must not error");
        assert_eq!(
            before.node.kind,
            crate::NodeConnectivityKind::NotStarted,
            "no mesh ⇒ NotStarted, got {:?}",
            before.node.kind
        );
        assert!(before.contacts.is_empty(), "no mesh ⇒ no contact rows");

        // Mesh up (no autoconnect worker) → node leaves NotStarted, bob shows up with a link-state.
        alice.app.start_mesh_no_autoconnect().expect("start mesh");
        let after = alice.app.mesh_connection_state().expect("state must not error");
        assert_ne!(
            after.node.kind,
            crate::NodeConnectivityKind::NotStarted,
            "mesh started ⇒ not NotStarted, got {:?}",
            after.node.kind
        );
        assert!(
            after.contacts.iter().any(|c| c.account_id == bob_account),
            "approved contact bob must appear in the connection-state DTO"
        );
    }

    /// `is_global_ipv6` accepts only internet-routable global unicast (2000::/3), the class
    /// that connects cross-network with no NAT — and rejects documentation + everything
    /// outside that range (loopback/unspecified/link-local/ULA/multicast).
    #[test]
    fn is_global_ipv6_accepts_only_routable_unicast() {
        use std::net::Ipv6Addr;
        let global = |s: &str| omni_mesh_runtime::is_global_ipv6(s.parse::<Ipv6Addr>().unwrap());
        assert!(global("2606:4700:4700::1111"), "Cloudflare DNS is global");
        assert!(global("2001:4860:4860::8888"), "Google DNS is global");
        assert!(!global("2001:db8::1"), "documentation range excluded");
        // Transition tunnels ride flaky v4 relays — not directly-dialable native v6.
        assert!(!global("2001:0:14c9:d804::1"), "Teredo 2001:0::/32 excluded");
        assert!(!global("2002:c000:0204::1"), "6to4 2002::/16 excluded");
        assert!(!omni_mesh_runtime::is_global_ipv6(Ipv6Addr::LOCALHOST), "::1 not global");
        assert!(!omni_mesh_runtime::is_global_ipv6(Ipv6Addr::UNSPECIFIED), ":: not global");
        assert!(!global("fe80::1"), "link-local not global");
        assert!(!global("fc00::1"), "unique-local not global");
        assert!(!global("fd12:3456::1"), "ULA not global");
        assert!(!global("ff02::1"), "multicast not global");
    }

    /// `detect_global_ipv6` is host-dependent (returns None on an IPv4-only host); it must
    /// never panic, and any address it reports must classify as global unicast.
    #[test]
    fn detect_global_ipv6_returns_global_or_none() {
        if let Some(addr) = super::detect_global_ipv6() {
            assert!(
                omni_mesh_runtime::is_global_ipv6(addr),
                "detected {addr} must be global unicast"
            );
        }
    }

    /// Phase α-4d (pairing-over-mesh): an `om3mesh:` invite carries the
    /// inviter's Ed25519 signing pubkey, and importing it resolves + registers
    /// the inviter's mesh NodeAddress as a pairing target — the whole layer-2
    /// round-trip (generate → encode → parse → derive → register), with no mesh
    /// runtime needed, so it's deterministic. See
    /// docs/decisions/pairing-over-mesh-adoption.md.
    #[test]
    fn mesh_pairing_invite_round_trips_and_registers_target() {
        let alice = create_stage5_peer("mesh-invite-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-invite-bob", "Bob", "Bob PC");

        // The node both sides derive from Bob's Ed25519 identity signing key.
        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");

        let invite = bob
            .app
            .generate_mesh_pairing_invite()
            .expect("bob generates a mesh pairing invite");
        assert!(
            invite.starts_with("om3mesh:"),
            "mesh invite must use the om3mesh: prefix, got {invite}"
        );

        let resolved = alice
            .app
            .register_mesh_pairing_invite(&invite)
            .expect("alice imports bob's mesh invite");
        assert_eq!(
            resolved, bob_node,
            "importing the invite must resolve Bob's mesh node from the carried pubkey"
        );

        // The imported node is now a pairing target the auto-connect worker dials.
        let registered = alice
            .app
            .mesh_pairing_targets
            .lock()
            .map(|set| set.contains(&bob_node))
            .unwrap_or(false);
        assert!(
            registered,
            "the invite's node must be registered as a pairing target"
        );

        // A non-mesh token is rejected cleanly (callers fall back / surface it).
        assert!(
            alice
                .app
                .register_mesh_pairing_invite("om3:not-a-mesh-invite")
                .is_err(),
            "a non-om3mesh token must be rejected"
        );
    }

    /// R7 invite-provider-list (2b): the rendezvous-provider set is a first-class, per-profile registry —
    /// seeded with the cross-protocol defaults, GUI add/remove with scheme validation + dup-reject, and a
    /// friend's pairing invite CONTRIBUTES its providers (source 'invite') so a first contact on a restrictive network
    /// converges on a working shared set. See docs/decisions/global-identity-discovery.md.
    #[test]
    fn rendezvous_providers_add_remove_validate_and_invite_merge() {
        let alice = create_stage5_peer("rv-prov-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("rv-prov-bob", "Bob", "Bob PC");

        // Seeded defaults surface through the app (base-first).
        let seeded = alice.app.list_rendezvous_providers().expect("list");
        assert_eq!(
            seeded.len(),
            omni_rendezvous::DEFAULT_RENDEZVOUS_PROVIDERS.len(),
            "the cross-protocol defaults (ntfy, the Nostr relay pool, Mainline) seeded"
        );
        assert!(seeded.iter().any(|p| p.url == "https://ntfy.sh"));
        assert!(seeded.iter().any(|p| p.url == "dht://mainline"), "bt-dht courier seeded");

        // Manual add validates the scheme and rejects a duplicate.
        alice
            .app
            .add_rendezvous_provider("wss://relay.alice.example")
            .expect("add a valid ws provider");
        assert!(
            alice.app.add_rendezvous_provider("ftp://nope").is_err(),
            "a non http(s)/ws(s) scheme is rejected"
        );
        assert!(
            alice.app.add_rendezvous_provider("https://ntfy.sh").is_err(),
            "a duplicate url is rejected"
        );
        let listed = alice.app.list_rendezvous_providers().unwrap();
        assert!(listed
            .iter()
            .any(|p| p.url == "wss://relay.alice.example" && p.source == "manual"));

        // A seeded default is turned off, not removed (so it can come back); one we added is removable.
        let ntfy_id = listed.iter().find(|p| p.url == "https://ntfy.sh").unwrap().id;
        assert!(alice.app.remove_rendezvous_provider(ntfy_id).is_err(), "a default is not removed");
        alice.app.set_rendezvous_provider_mode(ntfy_id, "off").expect("turn a default off");
        let entries = alice.app.configured_rendezvous_providers();
        assert!(entries.iter().all(|entry| !entry.starts_with("https://ntfy.sh")), "off: not handed to the helper");
        let mom_id = listed.iter().find(|p| p.url == "wss://nostr.mom").unwrap().id;
        alice.app.set_rendezvous_provider_mode(mom_id, "read").expect("only read a courier");
        assert!(alice.app.configured_rendezvous_providers().contains(&"wss://nostr.mom#read".to_string()));
        assert!(alice.app.set_rendezvous_provider_mode(mom_id, "sometimes").is_err());
        let manual_id = listed.iter().find(|p| p.url == "wss://relay.alice.example").unwrap().id;
        alice.app.remove_rendezvous_provider(manual_id).expect("remove one we added");

        // At least one courier has to stay where we post.
        let posting: Vec<i64> = alice
            .app
            .list_rendezvous_providers()
            .unwrap()
            .into_iter()
            .filter(|p| p.mode == "publish_read")
            .map(|p| p.id)
            .collect();
        let (last, rest) = posting.split_last().unwrap();
        for id in rest {
            alice.app.set_rendezvous_provider_mode(*id, "read").unwrap();
        }
        assert!(alice.app.set_rendezvous_provider_mode(*last, "off").is_err(), "the last posting courier stays");
        assert!(alice.app.set_rendezvous_provider_mode(*last, "read").is_err());

        // Invite merge: Bob adds a distinct provider; his invite carries his configured set; importing it
        // adopts Bob's provider tagged 'invite' (and is idempotent on ones Alice already holds).
        bob.app
            .add_rendezvous_provider("wss://relay.bob.example")
            .expect("bob adds a provider");
        let invite = bob.app.generate_mesh_pairing_invite().expect("bob mints invite");
        alice
            .app
            .register_mesh_pairing_invite(&invite)
            .expect("alice imports bob's invite");
        let after = alice.app.list_rendezvous_providers().unwrap();
        let merged = after
            .iter()
            .find(|p| p.url == "wss://relay.bob.example")
            .expect("bob's provider was merged from the invite");
        assert_eq!(merged.source, "invite", "an invite-contributed provider is tagged 'invite'");
    }

    #[test]
    fn connection_privacy_mode_defaults_auto_and_persists_direct_only() {
        // The connection-privacy choice (orthogonal to E2E): Auto allows third-party rendezvous;
        // Direct-only forbids it (LAN + port-map + manual exchange only). Defaults to Auto with no
        // sidecar, persists across a get, and parses its db-value. See connection-privacy-modes.md.
        let alice = create_stage5_peer("privacy-mode-alice", "Alice", "Alice PC");

        assert_eq!(
            alice.app.connection_privacy_mode(),
            crate::ConnectionPrivacyMode::Auto,
            "absent sidecar defaults to Auto"
        );
        assert!(crate::ConnectionPrivacyMode::Auto.uses_rendezvous());

        alice
            .app
            .set_connection_privacy_mode(crate::ConnectionPrivacyMode::DirectOnly)
            .expect("set direct-only");
        assert_eq!(
            alice.app.connection_privacy_mode(),
            crate::ConnectionPrivacyMode::DirectOnly,
            "direct-only persists"
        );
        assert!(
            !crate::ConnectionPrivacyMode::DirectOnly.uses_rendezvous(),
            "direct-only forbids third-party rendezvous"
        );

        alice
            .app
            .set_connection_privacy_mode(crate::ConnectionPrivacyMode::Auto)
            .expect("switch back to auto");
        assert_eq!(alice.app.connection_privacy_mode(), crate::ConnectionPrivacyMode::Auto);

        assert_eq!(
            crate::ConnectionPrivacyMode::from_db_value("direct-only").unwrap(),
            crate::ConnectionPrivacyMode::DirectOnly
        );
        assert!(crate::ConnectionPrivacyMode::from_db_value("nonsense").is_err());

        // RelayOnly (connection-quality-ladder axis B, unified into this one privacy knob): persists,
        // db-round-trips, and — unlike DirectOnly — DOES use rendezvous; it only forbids the direct
        // P2P punch (hides your IP from the peer). Enforcement of the no-punch is R1a Increment 3.
        alice
            .app
            .set_connection_privacy_mode(crate::ConnectionPrivacyMode::RelayOnly)
            .expect("set relay-only");
        assert_eq!(
            alice.app.connection_privacy_mode(),
            crate::ConnectionPrivacyMode::RelayOnly,
            "relay-only persists"
        );
        assert_eq!(
            crate::ConnectionPrivacyMode::from_db_value("relay-only").unwrap(),
            crate::ConnectionPrivacyMode::RelayOnly
        );
        assert!(
            crate::ConnectionPrivacyMode::RelayOnly.uses_rendezvous(),
            "relay-only still leans on rendezvous + relay"
        );
        assert!(
            !crate::ConnectionPrivacyMode::RelayOnly.allows_direct_punch(),
            "relay-only forbids the direct punch"
        );
        assert!(
            crate::ConnectionPrivacyMode::Auto.allows_direct_punch()
                && crate::ConnectionPrivacyMode::DirectOnly.allows_direct_punch(),
            "Auto and DirectOnly both allow the direct punch"
        );
    }

    #[test]
    fn carrier_policy_defaults_auto_and_persists() {
        // R1a axis A: absent sidecar ⇒ Auto (reliability-first base, opportunistic climb); a set
        // persists and round-trips through the sidecar. See connection-quality-ladder.md.
        let bob = create_stage5_peer("carrier-policy-bob", "Bob", "Bob PC");

        assert_eq!(
            bob.app.carrier_policy(),
            omni_types::CarrierPolicy::Auto,
            "absent sidecar defaults to Auto"
        );

        bob.app
            .set_carrier_policy(omni_types::CarrierPolicy::Tcp)
            .expect("set tcp");
        assert_eq!(
            bob.app.carrier_policy(),
            omni_types::CarrierPolicy::Tcp,
            "tcp persists across a get"
        );

        bob.app
            .set_carrier_policy(omni_types::CarrierPolicy::Quic)
            .expect("set quic");
        assert_eq!(bob.app.carrier_policy(), omni_types::CarrierPolicy::Quic);
    }

    #[test]
    fn friend_relay_consent_defaults_off_persists_and_resolves_to_mesh_node() {
        // Friend-proxy Phase C2: per-contact carriage consent is OFF by default, a set/revoke
        // round-trips through storage (the durable source of truth mesh start re-seeds from), and
        // a consented contact resolves to its mesh NodeAddress only once its prekey bundle is
        // stored. See docs/decisions/friend-proxy-by-consent.md.
        let alice = create_stage5_peer("friend-relay-consent-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("friend-relay-consent-bob", "Bob", "Bob PC");

        let contact = alice
            .app
            .add_contact(&AddContactInput {
                petname: "Боб".to_string(),
                remote_account_id: Some(bob.account.descriptor.account_id.clone()),
                public_display_name: None,
                remote_instance_id: Some(bob.instance_id.clone()),
                primary_dialog_key: Some(bob.dialog_key.clone()),
                initial_observation: None,
            })
            .unwrap();
        assert!(
            !contact.friend_relay_consent,
            "carriage consent is OFF by default"
        );

        let granted = alice
            .app
            .set_contact_friend_relay_consent(&contact.contact_id, true)
            .expect("grant consent");
        assert!(granted.friend_relay_consent, "set returns the updated snapshot");
        assert!(
            alice
                .app
                .list_contacts()
                .unwrap()
                .iter()
                .any(|c| c.contact_id == contact.contact_id && c.friend_relay_consent),
            "consent persists into the contact list"
        );

        // Without a stored prekey bundle the consent is persisted but not yet resolvable
        // to a mesh node (nothing to seed; applies on a later start once the bundle exists).
        assert!(
            alice.app.friend_relay_consented_nodes().is_empty(),
            "no stored bundle yet → no resolvable node"
        );

        alice.app.store_remote_prekey_bundle(&bob.bundle).unwrap();
        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");
        assert_eq!(
            alice.app.friend_relay_consented_nodes(),
            vec![bob_node],
            "consented contact resolves to the bundle-derived mesh node"
        );

        let revoked = alice
            .app
            .set_contact_friend_relay_consent(&contact.contact_id, false)
            .expect("revoke consent");
        assert!(!revoked.friend_relay_consent, "revocation flips the snapshot off");
        assert!(
            alice.app.friend_relay_consented_nodes().is_empty(),
            "a revoked consent no longer seeds the mesh"
        );
    }

    #[test]
    fn network_nodes_start_empty_then_add_and_remove() {
        // No base bootstrap node is seeded: the project relay VPS expired (2026-07-24) so its seed was
        // removed; the mechanism stays and a live base node can be re-seeded later. A fresh profile starts
        // with an EMPTY registry and still connects (LAN + invite + rendezvous); the user adds/removes
        // relays by ip:port.
        let alice = create_stage5_peer("network-nodes-alice", "Alice", "Alice PC");

        let seeded = alice.app.list_network_nodes().expect("list");
        assert!(
            seeded.is_empty(),
            "no base node is seeded (VPS expired); the empty bootstrap list still connects"
        );

        // Add a manual relay; reject a bad endpoint and a duplicate.
        let relay_id = alice
            .app
            .add_network_node("203.0.113.7:443", "Друг-релей")
            .expect("add valid relay");
        assert!(alice.app.add_network_node("not-an-endpoint", "x").is_err(), "rejects bad ip:port");
        assert!(
            alice.app.add_network_node("203.0.113.7:443", "dup").is_err(),
            "rejects duplicate endpoint"
        );

        let listed = alice.app.list_network_nodes().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, relay_id);
        assert_eq!(listed[0].source, "manual");

        // Remove it — back to the empty (still-valid) bootstrap list.
        alice.app.remove_network_node(relay_id).expect("remove relay");
        assert!(
            alice.app.list_network_nodes().unwrap().is_empty(),
            "empty bootstrap list is valid"
        );
    }

    #[test]
    fn add_network_node_accepts_pinned_node_at_endpoint_form() {
        // A relay can be added with a PINNED identity ("node_hex@ip:port") so the mesh dials that
        // exact key; a plain "ip:port" stays a hint (no pinned node). See Always-connect step 5.
        let alice = create_stage5_peer("network-nodes-pin-alice", "Alice", "Alice PC");
        let node_hex = "aa".repeat(20); // 40 hex chars = a 20-byte NodeAddress

        let pinned_id = alice
            .app
            .add_network_node(&format!("{node_hex}@203.0.113.9:443"), "Pinned relay")
            .expect("add pinned relay");
        let listed = alice.app.list_network_nodes().unwrap();
        let pinned = listed.iter().find(|n| n.id == pinned_id).expect("pinned present");
        assert_eq!(pinned.endpoint.as_deref(), Some("203.0.113.9:443"));
        assert_eq!(pinned.node_address_hex.as_deref(), Some(node_hex.as_str()));
        assert_ne!(pinned.status, "hint", "a pinned entry has a checkable reachability status");

        let hint_id = alice
            .app
            .add_network_node("198.51.100.4:443", "Hint relay")
            .expect("add hint relay");
        let hint = alice
            .app
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .find(|n| n.id == hint_id)
            .unwrap();
        assert!(hint.node_address_hex.is_none(), "plain ip:port has no pinned identity");
        assert_eq!(hint.status, "hint", "a hint-only entry reads as hint until dialed");

        assert!(
            alice.app.add_network_node("zz@203.0.113.10:443", "bad").is_err(),
            "rejects a non-hex pinned node"
        );
    }

    #[test]
    fn add_network_node_parses_optional_tcp_bridge_suffix() {
        // R1a: a relay may carry its TLS-over-TCP bridge as a "|tcp=<ip:port>" suffix (rung-0
        // carrier). The UDP/QUIC endpoint is the part before the suffix; a malformed bridge is
        // rejected up front. (tcp_endpoint persistence itself is covered by the omni-storage
        // network_nodes round-trip test.) See docs/decisions/connection-quality-ladder.md.
        let alice = create_stage5_peer("network-nodes-tcp-alice", "Alice", "Alice PC");

        let id = alice
            .app
            .add_network_node("203.0.113.7:443|tcp=203.0.113.7:8443", "TCP relay")
            .expect("add relay with a TCP bridge");
        let listed = alice.app.list_network_nodes().unwrap();
        let node = listed.iter().find(|n| n.id == id).expect("present");
        assert_eq!(
            node.endpoint.as_deref(),
            Some("203.0.113.7:443"),
            "the UDP endpoint is the part before |tcp="
        );

        assert!(
            alice
                .app
                .add_network_node("203.0.113.8:443|tcp=not-a-port", "bad bridge")
                .is_err(),
            "rejects a malformed TCP-bridge address"
        );
    }

    /// A beacon whose bytes change is published at once, past its plan: my bundle must stay the same
    /// bytes while my relay set does not change. It was signed with «now» — every pair's beacon went
    /// out on every courier each sweep once a relay was pinned, and ntfy's quota went within the hour
    /// (stand 2026-10-14).
    #[test]
    fn my_relay_bundle_is_the_same_bytes_until_the_set_changes() {
        let alice = create_stage5_peer("bundle-stable-alice", "Alice", "Alice PC");
        alice
            .app
            .pin_relay(crate::PinnedRelayV1 {
                node_id: "ab".repeat(20),
                name: "pc-stand".to_string(),
                udp: "203.0.113.5:51820".to_string(),
                tcp: None,
                always: false,
                keep_address_hidden: false,
            })
            .unwrap();
        let first = alice.app.my_relay_bundle_token().unwrap().expect("the pinned relay makes a bundle");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(alice.app.my_relay_bundle_token().unwrap().as_deref(), Some(first.as_str()));
        alice.app.unpin_relay().unwrap();
        assert_eq!(alice.app.my_relay_bundle_token().unwrap(), None, "no pin, nothing shared — no bundle");
    }

    #[test]
    fn relay_bundle_round_trips_generate_then_import() {
        // R1c: alice exports a SIGNED om3relay: bundle (two relays); bob verifies + imports it, and the
        // relays land tagged with alice's bundle_id + version. See
        // docs/decisions/r1c-rotatable-white-relay.md.
        let alice = create_stage5_peer("relay-bundle-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-bundle-bob", "Bob", "Bob PC");

        let token = alice
            .app
            .generate_relay_bundle(
                1,
                vec![
                    crate::RelayBundleEntryInput {
                        node_address: Some([0x11; 20]),
                        endpoint: Some("203.0.113.1:443".to_string()),
                        tcp_endpoint: Some("203.0.113.1:8443".to_string()),
                        label: "Релей-1".to_string(),
                        ..Default::default()
                    },
                    crate::RelayBundleEntryInput {
                        endpoint: Some("203.0.113.2:443".to_string()),
                        label: "Релей-2".to_string(),
                        ..Default::default()
                    },
                ],
            )
            .expect("alice signs a relay bundle");
        assert!(token.starts_with("om3relay:"), "uses the om3relay: prefix: {token}");

        let imported = bob.app.import_relay_bundle(&token).expect("bob imports the bundle");
        assert_eq!(imported, 2, "both relays imported");

        // The two relays are present in bob's registry, tagged with one shared bundle_id at version 1.
        let rows = bob.app.storage.list_network_nodes().unwrap();
        let tagged: Vec<_> = rows.iter().filter(|n| n.bundle_id.is_some()).collect();
        assert_eq!(tagged.len(), 2);
        let bundle_id = tagged[0].bundle_id.clone().unwrap();
        assert!(tagged.iter().all(|n| n.bundle_id.as_deref() == Some(bundle_id.as_str())));
        assert!(tagged.iter().all(|n| n.bundle_version == Some(1)));
        assert_eq!(bob.app.storage.max_bundle_version(&bundle_id).unwrap(), Some(1));
        // The TCP bridge of relay-1 round-trips through the bundle.
        let relay1 = tagged
            .iter()
            .find(|n| n.endpoint.as_deref() == Some("203.0.113.1:443"))
            .expect("relay-1 present");
        assert_eq!(relay1.tcp_endpoint.as_deref(), Some("203.0.113.1:8443"));
        // So does its node — without it bob's mesh would never dial the relay (a hint only).
        assert_eq!(relay1.node_address.as_deref(), Some([0x11u8; 20].as_slice()));

        // A bundle needs at least one relay, and each relay needs a valid address.
        assert!(alice.app.generate_relay_bundle(1, vec![]).is_err());
        assert!(alice
            .app
            .generate_relay_bundle(
                1,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("not-an-endpoint".to_string()),
                    label: "bad".to_string(),
                    ..Default::default()
                }],
            )
            .is_err());
    }

    #[test]
    fn relay_bundle_rotation_replaces_set_and_ignores_older_or_equal() {
        // R1c rotation: a newer bundle version REPLACES the issuer's prior set; an equal/older version
        // is ignored (returns 0, registry unchanged) — an attacker can't roll a friend back to a burned
        // relay set. "Replace + generous TTL" decision (docs/decisions/r1c-rotatable-white-relay.md).
        let alice = create_stage5_peer("relay-rotate-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-rotate-bob", "Bob", "Bob PC");

        let v1 = alice
            .app
            .generate_relay_bundle(
                1,
                vec![
                    crate::RelayBundleEntryInput {
                        endpoint: Some("203.0.113.1:443".to_string()),
                        label: "old-a".to_string(),
                        ..Default::default()
                    },
                    crate::RelayBundleEntryInput {
                        endpoint: Some("203.0.113.2:443".to_string()),
                        label: "old-b".to_string(),
                        ..Default::default()
                    },
                ],
            )
            .unwrap();
        assert_eq!(bob.app.import_relay_bundle(&v1).unwrap(), 2);

        // Rotate to v2 = a single, different relay; it replaces the v1 pair.
        let v2 = alice
            .app
            .generate_relay_bundle(
                2,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.9:443".to_string()),
                    label: "new".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();
        assert_eq!(bob.app.import_relay_bundle(&v2).unwrap(), 1);

        let rows = bob.app.storage.list_network_nodes().unwrap();
        let tagged: Vec<_> = rows.iter().filter(|n| n.bundle_id.is_some()).collect();
        assert_eq!(tagged.len(), 1, "v2 replaced the v1 set");
        assert_eq!(tagged[0].endpoint.as_deref(), Some("203.0.113.9:443"));
        assert_eq!(tagged[0].bundle_version, Some(2));

        // Re-importing v1 (older) is a no-op; re-importing v2 (equal) is too.
        assert_eq!(bob.app.import_relay_bundle(&v1).unwrap(), 0, "older version ignored");
        assert_eq!(bob.app.import_relay_bundle(&v2).unwrap(), 0, "equal version ignored");
        let after = bob.app.storage.list_network_nodes().unwrap();
        let still: Vec<_> = after.iter().filter(|n| n.bundle_id.is_some()).collect();
        assert_eq!(still.len(), 1, "registry unchanged by the stale re-imports");
        assert_eq!(still[0].endpoint.as_deref(), Some("203.0.113.9:443"));
    }

    #[test]
    fn relay_bundle_import_rejects_tampered_or_garbage() {
        // R1c: the issuer signature covers every field — mutating a signed field (here bundle_version)
        // without re-signing makes import fail, so a forged/poisoned bundle never reaches storage.
        let alice = create_stage5_peer("relay-tamper-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-tamper-bob", "Bob", "Bob PC");

        let token = alice
            .app
            .generate_relay_bundle(
                1,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.1:443".to_string()),
                    label: "r".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();

        // Tamper: bump a signed-over field, re-encode WITHOUT re-signing → signature no longer verifies.
        let mut parsed = crate::parse_relay_bundle(&token).unwrap();
        parsed.bundle_version = 99;
        let tampered = crate::encode_relay_bundle(&parsed).unwrap();
        assert!(
            bob.app.import_relay_bundle(&tampered).is_err(),
            "a mutated bundle fails signature verification"
        );

        // Garbage / wrong-prefix tokens are rejected, not silently accepted.
        assert!(bob.app.import_relay_bundle("om3reach:not-a-relay-bundle").is_err());
        assert!(bob.app.import_relay_bundle("om3relay:###not-base64###").is_err());

        // The valid original still imports — the registry was untouched by the rejected attempts.
        assert_eq!(bob.app.import_relay_bundle(&token).unwrap(), 1);
    }

    #[test]
    fn relay_bundle_propagation_gate_binds_issuer_to_the_propagating_contact() {
        // R1c beacon propagation (increment 3a): `ingest_propagated_relay_bundle` applies a bundle ONLY
        // if its issuer == the contact whose sealed per-pair beacon carried it (first-party-only) — so a
        // paired contact propagates ONLY its own set, never a friend-of-friend's (honours the R1b
        // no-transitive invariant). Same verify + strict-newer rotation as the manual import. See
        // docs/decisions/r1c-rotatable-white-relay.md.
        let alice = create_stage5_peer("relay-prop-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-prop-bob", "Bob", "Bob PC");
        let carol = create_stage5_peer("relay-prop-carol", "Carol", "Carol PC");

        let token = alice
            .app
            .generate_relay_bundle(
                1,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.1:443".to_string()),
                    label: "a".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();

        // Alice's mesh node (= the bundle's issuer) recovered from the signed token.
        let issuer_hex =
            crate::verify_relay_bundle(&crate::parse_relay_bundle(&token).unwrap()).unwrap();
        let alice_node: [u8; 20] = hex::decode(&issuer_hex).unwrap().try_into().unwrap();

        // Attributed to alice (the real issuer) → imported, tagged.
        assert_eq!(bob.app.ingest_propagated_relay_bundle(alice_node, &token).unwrap(), 1);
        let bob_tagged: Vec<_> = bob
            .app
            .storage
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.bundle_id.is_some())
            .collect();
        assert_eq!(bob_tagged.len(), 1);

        // Attributed to the WRONG contact (a node that isn't the issuer) → rejected; never stored.
        let mut wrong_node = alice_node;
        wrong_node[0] ^= 0xff;
        assert!(carol.app.ingest_propagated_relay_bundle(wrong_node, &token).is_err());
        let carol_tagged: Vec<_> = carol
            .app
            .storage
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.bundle_id.is_some())
            .collect();
        assert!(carol_tagged.is_empty(), "a mis-attributed bundle never reaches storage");
    }

    #[test]
    fn my_relay_bundle_publishes_flagged_relays_and_bumps_version_on_change() {
        // R1c inc.3b (issuer side): flagging a relay `shared` makes `my_relay_bundle_token` emit a
        // SIGNED bundle of EXACTLY the flagged relays; changing the flagged set bumps my version so a
        // friend sees a strictly-newer bundle that REPLACES the prior set. Nothing flagged ⇒ no bundle
        // (the presence beacon stays bare, as before). See docs/decisions/r1c-rotatable-white-relay.md.
        let alice = create_stage5_peer("relay-mine-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-mine-bob", "Bob", "Bob PC");

        // Nothing flagged → no bundle to publish.
        assert!(alice.app.my_relay_bundle_token().unwrap().is_none());

        // Two relays; flag only the first as mine.
        let mine = alice
            .app
            .add_network_node("203.0.113.1:443|tcp=203.0.113.1:8443", "Мой релей")
            .unwrap();
        let other = alice.app.add_network_node("198.51.100.9:443", "Чужой релей").unwrap();
        alice.app.set_relay_node_shared(mine, true).unwrap();

        let token = alice
            .app
            .my_relay_bundle_token()
            .unwrap()
            .expect("a bundle once I share a relay");
        assert!(token.starts_with("om3relay:"), "uses the om3relay: prefix: {token}");

        // The published bundle carries EXACTLY my flagged relay (with its TCP bridge), attributed to my
        // node — a friend imports it under the first-party gate.
        let issuer_hex =
            crate::verify_relay_bundle(&crate::parse_relay_bundle(&token).unwrap()).unwrap();
        let alice_node: [u8; 20] = hex::decode(&issuer_hex).unwrap().try_into().unwrap();
        assert_eq!(bob.app.ingest_propagated_relay_bundle(alice_node, &token).unwrap(), 1);
        let tagged: Vec<_> = bob
            .app
            .storage
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.bundle_id.is_some())
            .collect();
        assert_eq!(tagged.len(), 1, "only the flagged relay propagates");
        assert_eq!(tagged[0].endpoint.as_deref(), Some("203.0.113.1:443"));
        assert_eq!(tagged[0].tcp_endpoint.as_deref(), Some("203.0.113.1:8443"));
        let v1 = tagged[0].bundle_version.unwrap();

        // Flag the second relay too → my set changed → version bumps → a strictly-newer bundle that
        // REPLACES the prior set on the friend (now both relays).
        alice.app.set_relay_node_shared(other, true).unwrap();
        let token2 = alice.app.my_relay_bundle_token().unwrap().unwrap();
        let v2 = i64::from(crate::parse_relay_bundle(&token2).unwrap().bundle_version);
        assert!(v2 > v1, "changing my set bumps the version (v2={v2} > v1={v1})");
        assert_eq!(
            bob.app.ingest_propagated_relay_bundle(alice_node, &token2).unwrap(),
            2,
            "the rotated set replaces + grows"
        );

        // Unflag everything → no bundle again (the beacon stops carrying one).
        alice.app.set_relay_node_shared(mine, false).unwrap();
        alice.app.set_relay_node_shared(other, false).unwrap();
        assert!(alice.app.my_relay_bundle_token().unwrap().is_none());
    }

    #[test]
    fn cannot_flag_an_imported_relay_as_mine_no_transitive_reshare() {
        // R1c first-party-only (owner decision #1 + R1b): I can share a relay I added, but NOT one
        // imported from a friend's bundle — flagging an imported relay would re-propagate it under my
        // identity (transitive re-share, forbidden). See docs/decisions/r1c-rotatable-white-relay.md.
        let alice = create_stage5_peer("relay-notransit-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("relay-notransit-bob", "Bob", "Bob PC");

        // Bob imports Alice's signed bundle → the relay lands tagged with Alice's bundle_id.
        let token = alice
            .app
            .generate_relay_bundle(
                1,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.1:443".to_string()),
                    label: "a".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();
        assert_eq!(bob.app.import_relay_bundle(&token).unwrap(), 1);
        let imported = bob
            .app
            .storage
            .list_network_nodes()
            .unwrap()
            .into_iter()
            .find(|n| n.bundle_id.is_some())
            .expect("an imported relay");

        // Bob may NOT flag Alice's imported relay as his own (no transitive re-share); nothing to publish.
        assert!(
            bob.app.set_relay_node_shared(imported.id, true).is_err(),
            "sharing a friend's imported relay is rejected"
        );
        assert!(bob.app.my_relay_bundle_token().unwrap().is_none());

        // But bob's OWN manually-added relay shares fine.
        let mine = bob.app.add_network_node("198.51.100.9:443", "мой релей").unwrap();
        bob.app.set_relay_node_shared(mine, true).unwrap();
        assert!(bob.app.my_relay_bundle_token().unwrap().is_some(), "my own relay publishes");
    }

    #[test]
    fn jittered_rendezvous_sweep_interval_stays_within_bounds() {
        // R7 publish jitter: the sweep cadence is randomized within [INTERVAL, INTERVAL+JITTER] — never
        // outside, so it stays well under the 300 s beacon TTL while breaking the clock-aligned publish
        // fingerprint. Sample many times to exercise the random range.
        let lo = crate::RENDEZVOUS_SWEEP_INTERVAL_SECS;
        let hi = crate::RENDEZVOUS_SWEEP_INTERVAL_SECS + crate::RENDEZVOUS_SWEEP_JITTER_SECS;
        let mut seen_above_floor = false;
        for _ in 0..256 {
            let secs = crate::jittered_rendezvous_sweep_interval_secs();
            assert!((lo..=hi).contains(&secs), "jittered interval {secs} outside [{lo},{hi}]");
            seen_above_floor |= secs > lo;
        }
        assert!(seen_above_floor, "jitter never moved off the floor across 256 samples");
    }

    #[test]
    fn reach_token_round_trips_and_rejects_garbage() {
        // The "код связи" manual reach bundle: encode my node + endpoints into an om3reach: token and
        // parse it back. See docs/decisions/connection-privacy-modes.md.
        let token = crate::MeshReachTokenV1 {
            version: 1,
            node_hex: "ab".repeat(20), // 40 hex chars = a 20-byte node address
            endpoints: vec![
                "203.0.113.7:51820".to_string(),
                "[2001:db8::1]:51820".to_string(),
            ],
            exported_at_unix_ms: 1_700_000_000_000,
        };
        let encoded = crate::encode_reach_token(&token).expect("encode");
        assert!(encoded.starts_with("om3reach:"), "uses the om3reach: prefix: {encoded}");

        let parsed = crate::parse_reach_token(&encoded).expect("parse");
        assert_eq!(parsed.node_hex, "ab".repeat(20));
        assert_eq!(parsed.endpoints.len(), 2);
        assert_eq!(parsed.endpoints[0], "203.0.113.7:51820");

        // Wrong prefix / malformed payload are rejected cleanly.
        assert!(crate::parse_reach_token("om3mesh:whatever").is_err());
        assert!(crate::parse_reach_token("om3reach:!!!not-base64").is_err());
    }

    /// Phase α-4d (pairing-over-mesh) **capstone**: two never-met cores pair
    /// purely over the mesh — no legacy listener, no connect, no `om3` token, no
    /// pre-seeded contacts or bundles. Alice imports Bob's `om3mesh:` invite; the
    /// auto-connect worker dials the pairing target; on Bob's SessionHello Alice's
    /// on_wire handler creates the outgoing-Pending contact + initiator session,
    /// and the existing SessionActivated trigger ships the contact request over
    /// the Link; Bob auto-creates the incoming-Pending contact, approves, and the
    /// approval rides back — both reach `Approved`. Then a chat message delivers
    /// over the freshly-paired mesh session. This is the proof of the live pairing
    /// path (#60). See docs/decisions/pairing-over-mesh-adoption.md.
    ///
    /// The core of mesh durable (store-and-forward) variant (b): a recipient that
    /// holds a paired session for a peer it never linked *this session* must be able
    /// to decrypt that peer's wire envelope from the MINIMAL receive-ctx the durable
    /// path reconstructs (just a dialog_key — no active connection, no prekey bundle).
    /// This isolates the decrypt from the mesh transport: build Alice's encrypted
    /// chat envelope, then inject it into Bob via `received_context_for_dialog_key`
    /// with no active connection, exactly as the never-linked on_wire branch does.
    #[test]
    fn durable_minimal_ctx_decrypts_chat_for_unlinked_contact() {
        let alice = create_stage5_peer("durable-minctx-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("durable-minctx-bob", "Bob", "Bob PC");

        // Establish A↔B contacts + sessions in storage — but NO active transport
        // connection (no listener, no mesh link). Mirrors variant (b): a paired
        // session for a peer never linked this session.
        link_stage5_contact(&alice.app, &bob, "Bob");

        // Bootstrap Alice's initiator session for Bob with no transport and no send
        // — exactly what the queue does lazily on first send, minus the delivery.
        let bob_domain_bundle = super::prekey_bundle_from_protocol(&bob.bundle).unwrap();
        let now = current_unix_ms();
        let (alice_session, bootstrap, _) =
            super::bootstrap_session_for_remote_bundle(&alice.app.storage, &bob_domain_bundle, now)
                .unwrap();

        // A first message is two wire envelopes (mirrors the real send): the X3DH
        // session-bootstrap, then the chat text — both for Bob.
        let bootstrap_envelope = omni_messaging::wrap_session_bootstrap(bootstrap).unwrap();
        let mut chat = omni_messaging::build_encrypted_text_envelope(
            &alice_session,
            &format!("durable-msg-{}", unique_suffix()),
            "ping from the void",
            now,
            now,
        )
        .unwrap();
        alice
            .app
            .storage
            .store_session_record(&mut chat.updated_session)
            .unwrap();

        // Bob's transport has no active connection for Alice, so the ctx is the
        // MINIMAL one (just the dialog_key) — the exact ctx the durable never-linked
        // receive path injects.
        let bob_transport = bob
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("bob transport should be available");
        let ctx = bob_transport.received_context_for_dialog_key(&alice.dialog_key);
        assert_eq!(ctx.dialog_key, alice.dialog_key);
        assert!(
            ctx.remote_instance_id.is_empty() && ctx.remote_prekey_bundle.is_none(),
            "no active connection ⇒ the minimal ctx must be under test",
        );

        // Inject both over the minimal ctx, in order: bootstrap establishes Bob's
        // responder session, the chat then decrypts via that persisted ratchet
        // (resolved from the contact by dialog_key, since the ctx carries no bundle)
        // and lands in the Alice dialog — proving the minimal ctx suffices for the
        // never-linked durable delivery path.
        bob_transport.inject_inbound_wire_envelope(bootstrap_envelope, ctx.clone());
        bob_transport.inject_inbound_wire_envelope(chat.envelope, ctx);
        wait_for_message_count(&bob.app, alice.dialog_key.as_str(), 1);
        let history = bob.app.list_messages(&alice.dialog_key).unwrap();
        assert_eq!(history.messages.len(), 1, "the durable chat must be delivered");
    }

    /// Mutual-pairing wedge (2026-08-06). When both peers register each other's invite at once, the
    /// loser's bootstrap — and every durable-backstop re-delivery of it — reaches the winner AFTER it
    /// is already Established. Rebuilding a responder session from it desyncs the ratchets, and the
    /// chat dies silently and permanently (live: 0 of 8 sends delivered; one-way pairing: 3 of 3).
    /// So an Established session must absorb (ignore) a bootstrap that predates it — while a peer
    /// that genuinely re-bootstraps after a reinstall builds a NEWER one and must still get through.
    /// See docs/bugs/mutual-pairing-wedges-session.md.
    /// A sender that loaded the session before taking the session lock — `approve_contact` waiting
    /// for the session to become established — must not encrypt with that copy: the receive path
    /// may have advanced the ratchet meanwhile, and writing the stale copy back rolls it back for
    /// good (docs/bugs/stale-session-copy-diverges-ratchet.md). Under the lock it re-reads the
    /// session and gets the advanced one. Negative half: a session that is gone or no longer
    /// established yields nothing to encrypt with.
    #[test]
    fn a_session_loaded_before_the_lock_is_reread_not_reused() {
        let alice = create_stage5_peer("stale-copy-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("stale-copy-bob", "Bob", "Bob PC");
        let bob_domain_bundle = super::prekey_bundle_from_protocol(&bob.bundle).unwrap();
        let now = current_unix_ms();
        let (mut session, _, _) =
            super::bootstrap_session_for_remote_bundle(&alice.app.storage, &bob_domain_bundle, now)
                .unwrap();
        session.bootstrap_state = omni_crypto::SessionBootstrapState::Established;
        alice.app.storage.store_session_record(&mut session).unwrap();
        let loaded_earlier = session.clone();

        // Meanwhile another path encrypts and stores: the ratchet moves on.
        let mut other = omni_messaging::build_encrypted_text_envelope(&loaded_earlier, "m-1", "one", now, now)
            .unwrap();
        alice.app.storage.store_session_record(&mut other.updated_session).unwrap();

        // The stale copy would hand out the very same counter again.
        let reused = omni_messaging::build_encrypted_text_envelope(&loaded_earlier, "m-2", "two", now, now)
            .unwrap();
        assert_eq!(reused.updated_session.send_counter, other.updated_session.send_counter);

        let fresh = super::reload_established_session_under_lock(&alice.app.storage, &loaded_earlier)
            .unwrap()
            .expect("the stored, advanced session");
        assert_eq!(fresh.send_counter, other.updated_session.send_counter);
        let next = omni_messaging::build_encrypted_text_envelope(&fresh, "m-2", "two", now, now)
            .unwrap();
        assert_eq!(next.updated_session.send_counter, other.updated_session.send_counter + 1);

        // Negative half.
        let mut pending = fresh.clone();
        pending.bootstrap_state = omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated;
        alice.app.storage.store_session_record(&mut pending).unwrap();
        assert!(super::reload_established_session_under_lock(&alice.app.storage, &loaded_earlier)
            .unwrap()
            .is_none());
        alice
            .app
            .storage
            .delete_session_state_for_remote_account(&bob.account.descriptor.account_id)
            .unwrap();
        assert!(super::reload_established_session_under_lock(&alice.app.storage, &loaded_earlier)
            .unwrap()
            .is_none());
    }

    #[test]
    fn established_session_ignores_a_stale_bootstrap_but_accepts_a_newer_one() {
        let alice = create_stage5_peer("stale-boot-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("stale-boot-bob", "Bob", "Bob PC");

        // Alice holds an initiator session toward bob, established at `established_at`.
        let bob_domain_bundle = super::prekey_bundle_from_protocol(&bob.bundle).unwrap();
        let bootstrap_created_at = current_unix_ms();
        let (mut alice_session, _, _) = super::bootstrap_session_for_remote_bundle(
            &alice.app.storage,
            &bob_domain_bundle,
            bootstrap_created_at,
        )
        .unwrap();
        let established_at = bootstrap_created_at + 500;
        alice_session.bootstrap_state = omni_crypto::SessionBootstrapState::Established;
        alice_session.updated_at_unix_ms = established_at;

        // Bob's own bootstrap from the same collision: created BEFORE alice's session established.
        let alice_domain_bundle = super::prekey_bundle_from_protocol(&alice.bundle).unwrap();
        let (_, mut bob_bootstrap, _) = super::bootstrap_session_for_remote_bundle(
            &bob.app.storage,
            &alice_domain_bundle,
            bootstrap_created_at,
        )
        .unwrap();
        bob_bootstrap.created_at_unix_ms = bootstrap_created_at;

        let (_, local_account, local_device) =
            super::active_profile_context(&alice.app.storage).unwrap();
        // Both timestamps in the comparison come from BOB's clock, so the guard holds no matter how
        // far the two machines' clocks have drifted apart (a live two-host run had 4.3 s of skew).
        let absorb = |message: &super::SessionBootstrapMessageV1, seen_at: Option<i64>| {
            super::existing_session_should_absorb_incoming_bootstrap(
                &local_account.descriptor.account_id,
                &local_device.device_id,
                &alice_session,
                message,
                true, // the contact is Approved by now — the state the old code tripped over
                seen_at,
            )
        };

        assert!(
            absorb(&bob_bootstrap, Some(bootstrap_created_at)),
            "a re-delivered bootstrap we have already seen is the pairing-collision echo and must be \
             ignored, not rebuilt over the working session",
        );

        // A genuine re-bootstrap (peer reinstalled) is newer than anything seen — still accepted.
        let mut reinstall_bootstrap = bob_bootstrap.clone();
        reinstall_bootstrap.created_at_unix_ms = bootstrap_created_at + 60_000;
        assert!(
            !absorb(&reinstall_bootstrap, Some(bootstrap_created_at)),
            "a bootstrap newer than any seen from this peer is a real re-bootstrap and must still be \
             accepted",
        );

        // Alice's own clock is deliberately far behind bob's here: the guard must not depend on it.
        assert!(
            absorb(&bob_bootstrap, Some(bootstrap_created_at)),
            "the decision must not depend on our local clock (established_at={established_at})",
        );
    }

    /// First-contact self-contained bootstrap (relay-less over a lossy link, 2026-07-20). A
    /// `SessionBootstrap` that arrives WITHOUT a preceding `SessionHello` — the durable, never-linked
    /// path a mobile-UDP-drop pairing falls back to — must still create the incoming Pending + the
    /// responder session. The receive-ctx is built from the bootstrap ITSELF
    /// (`received_context_from_bootstrap`; dialog_key from the `initiator_instance_id` it now carries),
    /// then injected into a receiver that has NEVER heard of the initiator and holds no route — exactly
    /// as the on_wire durable branch now does. Before the fix the frame was dropped ("no node_ctx, no
    /// persisted route") and the contact request vanished (the live-test symptom: sender "sent",
    /// receiver got nothing). See docs/decisions/mesh-durable-delivery.md.
    #[test]
    fn first_contact_bootstrap_without_hello_creates_pending_and_session() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("fc-boot-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("fc-boot-bob", "Bob", "Bob PC");
        // Deliberately NO link_stage5_contact: bob has never heard of alice (true first contact).

        // Alice bootstraps an initiator session for bob from his prekey bundle — mirrors find-by-ID,
        // where the bundle came from the identity beacon. The bootstrap carries alice's stable id.
        let bob_domain_bundle = super::prekey_bundle_from_protocol(&bob.bundle).unwrap();
        let now = current_unix_ms();
        let (_alice_session, bootstrap, _) =
            super::bootstrap_session_for_remote_bundle(&alice.app.storage, &bob_domain_bundle, now)
                .unwrap();

        let alice_instance_id = alice.app.storage.get_or_create_local_instance_id().unwrap();
        assert_eq!(
            bootstrap.initiator_instance_id, alice_instance_id,
            "the bootstrap must carry the initiator's stable instance id",
        );

        // Build the ctx from the bootstrap alone — the no-hello, durable-delivery path.
        let ctx = super::received_context_from_bootstrap(&bootstrap)
            .expect("a bootstrap with an instance id yields a ctx");
        assert_eq!(
            ctx.dialog_key,
            omni_types::dialog_key_from_instance_id(&alice_instance_id),
            "the reconstructed dialog_key must match the hello-derived one (no duplicate dialog)",
        );
        assert!(
            ctx.remote_prekey_bundle.is_some(),
            "the reconstructed ctx must carry the initiator account/device for the pending",
        );
        // ...and the device's certificate chain, which proves its account and is now mandatory.
        let chain_json = ctx.remote_prekey_bundle.as_ref().unwrap().device_certificate_chain_json.clone();
        let chain: omni_crypto::DeviceCertificateChainV1 =
            serde_json::from_str(chain_json.as_deref().expect("the bootstrap carries the chain")).unwrap();
        assert_eq!(chain.device.account_id, alice.account.descriptor.account_id);

        // Inject into bob, who has no active connection and no prior contact — exactly the durable
        // never-linked on_wire branch.
        let bootstrap_envelope = omni_messaging::wrap_session_bootstrap(bootstrap).unwrap();
        let bob_transport = bob
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("bob transport should be available");
        bob_transport.inject_inbound_wire_envelope(bootstrap_envelope, ctx);

        // The request arrived: bob now holds an incoming Pending for alice + a responder session.
        let alice_account_id = alice.account.descriptor.account_id.clone();
        wait_for_condition(Duration::from_secs(10), || {
            bob.app
                .list_contacts()
                .ok()
                .map(|contacts| {
                    contacts.iter().any(|contact| {
                        contact.relationship_state == ContactRelationshipState::Pending
                            && contact.remote_account_id.as_ref() == Some(&alice_account_id)
                    })
                })
                .unwrap_or(false)
        });
        let pending = bob
            .app
            .list_contacts()
            .unwrap()
            .into_iter()
            .find(|contact| contact.remote_account_id.as_ref() == Some(&alice_account_id))
            .expect("first-contact bootstrap must create the incoming pending");
        assert_eq!(
            pending.relationship_state,
            ContactRelationshipState::Pending,
            "an incoming first-contact request must land as Pending (consent-gated)",
        );
        // The responder session is written asynchronously by the injected-envelope handler, on a
        // different path than the Pending row above — so poll for it rather than asserting inline
        // (an inline check raced the async write and flaked under load). Same idiom as the pending
        // wait above.
        wait_for_condition(Duration::from_secs(10), || {
            bob.app
                .storage
                .get_latest_session_for_remote_account(&alice_account_id)
                .ok()
                .flatten()
                .is_some()
        });
        assert!(
            bob.app
                .storage
                .get_latest_session_for_remote_account(&alice_account_id)
                .unwrap()
                .is_some(),
            "the responder session must be established from the self-contained bootstrap",
        );
    }

    /// The send-side half of durable variant (b): the offline send path resolves a
    /// recipient's NodeAddress through the in-memory dialog map, which is empty after
    /// a restart. start_mesh must repopulate it from the persisted node↔dialog routes
    /// so a never-online-together send to a peer paired in an earlier run still finds
    /// its NodeAddress. (Multicast-independent: only start_mesh's restore is exercised.)
    #[test]
    fn mesh_start_restores_persisted_dialog_routes_for_offline_send() {
        let _guard = network_test_guard();
        let peer = create_stage5_peer("route-restore", "Peer", "Peer PC");

        // A route persisted by a pairing/link-up in an earlier run.
        let node_raw = [7u8; 20];
        let node = omni_mesh_runtime::MeshNodeAddress::from_bytes(node_raw);
        let dialog_key = "dialog:instance:restored-peer";
        peer.app
            .storage
            .set_mesh_dialog_route(&node_raw, dialog_key)
            .unwrap();

        peer.app.start_mesh().expect("mesh should start");
        let resolved = peer
            .app
            .mesh_dialogs
            .lock()
            .unwrap()
            .node_for_dialog(dialog_key);
        assert!(
            resolved == Some(node),
            "start_mesh must repopulate the in-memory dialog map from persisted routes",
        );
        peer.app.stop_mesh();
    }

    /// Sending to an *offline* approved contact that has a known mesh route hands the
    /// outbound envelope to durable store-and-forward (a held frame), so it can
    /// reconcile to the peer later — the headline never-online-together promise.
    /// `send_text` now attempts delivery for a mesh-routed target even when it is not
    /// the active dialog; with no live link (and no legacy connection) the transport's
    /// last-resort `hold_durable` holds exactly one frame. (This contact has no
    /// established session yet, so the held envelope is the session bootstrap — the
    /// same hold_durable path carries the chat message once the session is
    /// established; that end-to-end leg is the 3-instance live gate's job.)
    #[test]
    fn durable_offline_send_holds_a_store_and_forward_frame() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("durable-rt-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("durable-rt-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");

        // Alice knows Bob's NodeAddress (both derive it from his signing key) and
        // persists the route, so the offline send can resolve it after start_mesh.
        let bob_signing_pubkey: [u8; 32] = hex::decode(&bob.bundle.account.identity_signing_public_key)
            .expect("bob signing pubkey is valid hex")
            .try_into()
            .expect("signing pubkey is 32 bytes");
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");
        alice
            .app
            .storage
            .set_mesh_dialog_route(bob_node.as_bytes(), &bob.dialog_key)
            .unwrap();

        // Alice online, Bob offline.
        alice.app.start_mesh().expect("alice mesh starts");

        // Sending to the offline Bob must hold a durable store-and-forward frame
        // (multicast-independent — it does not need Bob to be reachable).
        alice
            .app
            .send_text(&bob.dialog_key, "held for the void")
            .unwrap();
        wait_for_condition(Duration::from_secs(15), || {
            alice
                .app
                .mesh
                .lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|runtime| !runtime.export_durable_rows().is_empty()))
                .unwrap_or(false)
        });

        // The queue worker also retries a chat that is not the active dialog (it used to wait until
        // that chat became active): once the bootstrap reissue window has passed the bootstrap is
        // re-issued, so a second frame must appear. But never faster than that window -- queue
        // ticks (50 ms here) must not multiply the held frame, so the count stays small.
        let held_frames = || {
            alice
                .app
                .mesh
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .export_durable_rows()
                .len()
        };
        wait_for_condition(Duration::from_secs(10), || held_frames() >= 2);
        let held = held_frames();
        assert!(
            held <= 3,
            "durable frames are paced by the bootstrap reissue window, not per tick (held {held})",
        );

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    /// Environment vs regression (mirrors the sibling mesh tests): if Alice's Link
    /// to Bob never forms, the host lacks a multicast adapter ⇒ skip; if the Link
    /// forms but pairing/approval/chat does not complete, the assertions panic.
    #[test]
    fn two_node_pairing_over_mesh_reaches_approved_and_delivers_chat() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("mesh-pair-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("mesh-pair-bob", "Bob", "Bob PC");
        // Deliberately NO link_stage5_contact: the two are strangers. The only
        // thing Alice learns about Bob is his pubkey, via the offline invite.

        alice.app.start_mesh().expect("alice mesh starts");
        bob.app.start_mesh().expect("bob mesh starts");

        // Bob's NodeAddress — both sides derive it from his Ed25519 signing key.
        let bob_signing_pubkey: [u8; 32] = {
            let bytes = hex::decode(&bob.bundle.account.identity_signing_public_key)
                .expect("bob signing pubkey is valid hex");
            bytes.try_into().expect("signing pubkey is 32 bytes")
        };
        let bob_node = omni_mesh_runtime::node_address_from_ed25519_pubkey(bob_signing_pubkey)
            .expect("bob signing pubkey yields a mesh node address");

        // Alice imports Bob's offline invite → registers him as a pairing target
        // the auto-connect worker dials. No contact exists yet on either side.
        let invite = bob
            .app
            .generate_mesh_pairing_invite()
            .expect("bob generates a mesh pairing invite");
        alice
            .app
            .register_mesh_pairing_invite(&invite)
            .expect("alice imports bob's mesh invite");

        // Wait for the auto-connect worker to bring up the Link to the pairing
        // target on its own. If it never forms, this host has no multicast adapter
        // — skip, mirroring two_node_chat_message_delivers_over_mesh.
        // Dialed on loopback, not found by multicast: the link is not what this tests, and a host
        // without LAN multicast used to skip the whole test and still count it as passed.
        link_apps_over_mesh(&alice.app, &bob.app);
        let _ = &bob_node;

        // The Link is up, so the on_wire pairing handler + contact-request trigger
        // have run over the mesh: Bob must surface an incoming-Pending contact for
        // Alice (a regression if it does not, since the Link exists).
        wait_for_condition(Duration::from_secs(30), || {
            bob.app
                .storage
                .find_contact_by_remote_account_id(&alice.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_some_and(|contact| {
                    contact.relationship_state == ContactRelationshipState::Pending
                        && contact.pending_request_direction
                            == Some(omni_identity::ContactRequestDirection::Incoming)
                })
        });

        // Alice's side is an outgoing-Pending contact for Bob, created by the
        // on_wire pairing handler before the request was sent.
        let alice_contact = alice
            .app
            .storage
            .find_contact_by_remote_account_id(&bob.account.descriptor.account_id)
            .unwrap()
            .expect("alice should have a pending contact for bob");
        assert_eq!(
            alice_contact.relationship_state,
            ContactRelationshipState::Pending,
            "alice's freshly-paired contact starts Pending"
        );
        assert_eq!(
            alice_contact.pending_request_direction,
            Some(omni_identity::ContactRequestDirection::Outgoing),
            "alice initiated the pairing, so her pending request is Outgoing"
        );

        // Bob approves (the one manual step — the user clicking Approve). The
        // approval rides back over the mesh and Alice's outgoing-pending
        // auto-promotes (no approve needed on the initiator side).
        let bob_contact = bob
            .app
            .storage
            .find_contact_by_remote_account_id(&alice.account.descriptor.account_id)
            .unwrap()
            .expect("bob should have an incoming-pending contact for alice");
        bob.app
            .approve_contact(&bob_contact.contact_id)
            .expect("bob approves alice");

        // Both sides reach Approved over the mesh — stranger pairing without any
        // legacy connection is complete.
        wait_for_condition(Duration::from_secs(30), || {
            let alice_approved = alice
                .app
                .storage
                .find_contact_by_remote_account_id(&bob.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_some_and(|contact| {
                    contact.relationship_state == ContactRelationshipState::Approved
                });
            let bob_approved = bob
                .app
                .storage
                .find_contact_by_remote_account_id(&alice.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_some_and(|contact| {
                    contact.relationship_state == ContactRelationshipState::Approved
                });
            alice_approved && bob_approved
        });

        // The paired mesh session carries chat: Alice → Bob over the Link.
        alice
            .app
            .send_text(&bob.dialog_key, "hello after mesh pairing")
            .expect("alice sends chat after pairing");
        wait_for_condition(Duration::from_secs(30), || {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history
                        .messages
                        .iter()
                        .any(|message| message.body == "hello after mesh pairing")
                })
                .unwrap_or(false)
        });

        // The invite is spent: nothing left to re-arm on the next start.
        let leftover = alice
            .app
            .storage
            .list_active_bootstrap_invites(current_unix_ms())
            .unwrap()
            .into_iter()
            .filter(|row| row.inviter_node == Some(*bob_node.as_bytes()))
            .count();
        assert_eq!(leftover, 0, "a completed pairing consumes its persisted invite");

        // Restart Alice's mesh (what a process restart does) and import the same invite again: an
        // already-approved peer must NOT be paired anew — re-pairing re-bootstrapped the session
        // while Bob kept his, and 1:1 traffic died both ways (live 3-node pass, 2026-10-03).
        alice.app.stop_mesh();
        alice.app.start_mesh().expect("alice mesh restarts");
        alice
            .app
            .register_mesh_pairing_invite(&invite)
            .expect("a stale re-import is accepted (and ignored at pairing time)");
        // The restarted mesh listens on a new port: dial it again (multicast would, on a LAN).
        link_apps_over_mesh(&alice.app, &bob.app);
        thread::sleep(Duration::from_secs(3));
        bob.app
            .send_text(&alice.dialog_key, "still one session after restart")
            .expect("bob sends after alice restarted");
        wait_for_condition(Duration::from_secs(30), || {
            alice
                .app
                .list_messages(bob.dialog_key.as_str())
                .ok()
                .is_some_and(|history| {
                    history.messages.iter().any(|m| m.body == "still one session after restart")
                })
        });

        alice.app.stop_mesh();
        bob.app.stop_mesh();
    }

    /// A file that cannot be prepared leaves nothing queued. The message used to be queued before the
    /// file, so a failure here left a plain-text message in the queue — and while a big file was still
    /// being hashed, the queue sent the message as text without the file (live 2026-10-15, 108 MB).
    #[test]
    fn a_file_that_cannot_be_prepared_queues_no_bare_text() {
        let server = create_stage5_peer("attachment-unready-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-unready-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");
        let missing = std::env::temp_dir().join(format!("omni-no-such-file-{}.bin", unique_suffix()));
        client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "the file",
                missing.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .expect_err("a missing file cannot be sent");
        let history = client.app.list_messages(server.dialog_key.as_str()).unwrap();
        assert!(
            history.messages.iter().all(|message| message.body != "the file"),
            "nothing queued: {:?}",
            history.messages.iter().map(|m| &m.body).collect::<Vec<_>>()
        );
    }

    #[test]
    fn two_node_attachment_offer_download_and_local_save_roundtrip() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        let source_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-source-{}.txt", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-downloaded-{}.txt", unique_suffix()));
        fs::create_dir_all(source_path.parent().expect("source parent should exist")).unwrap();
        fs::write(&source_path, "omni attachment end-to-end").unwrap();

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "file for server",
                source_path.to_string_lossy().as_ref(),
                Some("shared-report.txt".to_string()),
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        assert!(matches!(
            sent.status,
            MessageStatus::Sent
                | MessageStatus::Queued
                | MessageStatus::Retrying
                | MessageStatus::Delivered
        ));
        assert!(sent.attachment.is_some());

        wait_for_condition(Duration::from_secs(10), || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history.messages.iter().any(|message| {
                        message.body == "file for server"
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.state == omni_types::AttachmentTransferState::Offered
                            })
                    })
                })
                .unwrap_or(false)
        });

        let offered = server
            .app
            .list_messages(client.dialog_key.as_str())
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.body == "file for server")
            .expect("offered attachment message should exist");
        let offered_attachment = offered
            .attachment
            .as_ref()
            .expect("offered message should carry attachment metadata");
        assert_eq!(offered_attachment.file_name, "shared-report.txt");
        assert_eq!(
            offered_attachment.state,
            omni_types::AttachmentTransferState::Offered
        );
        assert_eq!(
            offered_attachment.content_encryption.as_deref(),
            Some("chacha20_poly1305_v1")
        );

        server
            .app
            .request_attachment_download(
                offered.id.as_str(),
                save_path.to_string_lossy().as_ref(),
            )
            .unwrap();

        wait_for_condition(Duration::from_secs(10), || {
            if !save_path.exists() {
                return false;
            }

            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history.messages.iter().any(|message| {
                        message.id == offered.id
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.local_blob_available
                                    && attachment.state
                                        == omni_types::AttachmentTransferState::Downloaded
                                    && attachment.local_saved_path.as_deref()
                                        == Some(save_path.to_string_lossy().as_ref())
                            })
                    })
                })
                .unwrap_or(false)
        });

        assert_eq!(
            fs::read_to_string(&save_path).unwrap(),
            "omni attachment end-to-end"
        );
        let storage = Storage::open(server.db_path.as_str()).unwrap();
        assert_eq!(
            storage.attachment_local_blob_path(offered.id.as_str()).unwrap(),
            None
        );
    }

    #[test]
    fn attachment_chunk_size_scales_by_file_size() {
        assert_eq!(
            choose_attachment_chunk_size_bytes(
                128 * 1024,
                0,
            ),
            128 * 1024
        );
        assert_eq!(
            choose_attachment_chunk_size_bytes(
                16 * 1024 * 1024,
                0,
            ),
            1024 * 1024
        );
        assert_eq!(
            choose_attachment_chunk_size_bytes(
                128 * 1024 * 1024,
                0,
            ),
            1024 * 1024
        );
    }

    /// Download-progress checkpoints (the progress bar and the block map a resume starts from; a
    /// block closed after the last one is just fetched again) are paced by TIME. The old byte-based rule floored
    /// at 512 KB, which with 1 MB chunks meant a SQLite commit — an fsync — on every single chunk,
    /// and that was the bulk of the receive path's per-chunk cost.
    #[test]
    fn attachment_progress_checkpoints_are_time_paced_and_always_final() {
        let file_size = 159_077_072_i64;
        let started_at = 1_700_000_000_000_i64;

        assert!(
            !super::should_checkpoint_attachment_download_progress(
                file_size,
                8 * 1024 * 1024,
                started_at,
                started_at + 40,
            ),
            "a chunk arriving right after a checkpoint must not trigger another DB write",
        );
        assert!(
            super::should_checkpoint_attachment_download_progress(
                file_size,
                8 * 1024 * 1024,
                started_at,
                started_at + 300,
            ),
            "progress must still be written once the interval has passed",
        );
        assert!(
            super::should_checkpoint_attachment_download_progress(
                file_size,
                file_size,
                started_at,
                started_at + 1,
            ),
            "completion always checkpoints, however recently the last one was written",
        );
    }

    #[test]
    fn reorder_buffer_rejects_chunks_beyond_offered_file_size() {
        let db_path = format!("reorder-db-{}", unique_suffix());
        let transfer_id = format!("reorder-transfer-{}", unique_suffix());
        let result =
            buffer_attachment_chunk_for_reorder(&db_path, &transfer_id, 1024, 900, vec![1_u8; 256]);
        clear_attachment_reorder_buffer(&db_path, &transfer_id)
            .expect("reorder buffer should be cleared");
        assert!(result.is_err());
    }

    #[test]
    fn reorder_buffer_enforces_hard_memory_cap() {
        let db_path = format!("reorder-db-{}", unique_suffix());
        let transfer_id = format!("reorder-transfer-{}", unique_suffix());
        let chunk = vec![7_u8; 1024 * 1024];
        let file_size_bytes = (ATTACHMENT_REORDER_BUFFER_MAX_BYTES as i64) * 2;

        for index in 0..(ATTACHMENT_REORDER_BUFFER_MAX_BYTES / chunk.len()) {
            buffer_attachment_chunk_for_reorder(
                &db_path,
                &transfer_id,
                file_size_bytes,
                (index * chunk.len()) as i64,
                chunk.clone(),
            )
            .expect("chunk should fit within reorder buffer cap");
        }

        let overflow = buffer_attachment_chunk_for_reorder(
            &db_path,
            &transfer_id,
            file_size_bytes,
            ATTACHMENT_REORDER_BUFFER_MAX_BYTES as i64,
            chunk,
        );
        clear_attachment_reorder_buffer(&db_path, &transfer_id)
            .expect("reorder buffer should be cleared");
        assert!(overflow.is_err());
    }

    /// A Start carries the whole block layer: the largest file's fits a stream frame sealed, one
    /// hash more is refused when read.
    #[test]
    fn a_start_carries_the_block_layer_of_the_largest_file_and_no_more() {
        let start = |blocks: usize| crate::AttachmentTransferStartPayloadV2 {
            total_size_bytes: omni_protocol::ATTACHMENT_FILE_SIZE_MAX_BYTES,
            content_id_hex: "ab".repeat(32),
            chunk_size_bytes: omni_crypto::CONTENT_TREE_LEAF_BYTES as u32,
            block_layer: vec![[3; 32]; blocks],
        };
        let largest = crate::serialize_attachment_transfer_payload(&start(crate::ATTACHMENT_BLOCK_LAYER_MAX)).unwrap();
        assert!(largest.len() + 16 <= 4 * 1024 * 1024 + 4096, "sealed, it fits the Start bound: {}", largest.len());
        let back: crate::AttachmentTransferStartPayloadV2 = crate::deserialize_attachment_transfer_payload(&largest).unwrap();
        assert_eq!(back.block_layer.len(), crate::ATTACHMENT_BLOCK_LAYER_MAX);
        let too_long = crate::serialize_attachment_transfer_payload(&start(crate::ATTACHMENT_BLOCK_LAYER_MAX + 1)).unwrap();
        assert!(crate::deserialize_attachment_transfer_payload::<crate::AttachmentTransferStartPayloadV2>(&too_long).is_err());
    }

    /// docs/decisions/attachment-content-tree.md: a chunk is written only as the leaf its block's
    /// proof says it is — not a wrong one, not a misplaced or cut one, not one whose proof never came —
    /// in whatever order the leaves come (stage 3: several sources), a leaf written already is
    /// skipped, and a finished block goes into the map with its hash.
    #[test]
    fn a_chunk_is_written_only_as_the_leaf_its_block_proof_names() {
        let leaf = omni_crypto::CONTENT_TREE_LEAF_BYTES as usize;
        let data: Vec<u8> = (0..2 * leaf + 5).map(|i| (i * 7 + i / 4093) as u8).collect();
        let mut tree = omni_crypto::ContentTreeBuilder::new();
        tree.update(&data);
        let tree = tree.finish();
        let leaves: Vec<omni_crypto::ContentHash> = data.chunks(leaf).map(omni_crypto::content_leaf_hash).collect();
        let path = std::env::temp_dir().join(format!("omni-leaf-check-{}.part", unique_suffix()));
        let file = fs::OpenOptions::new().create(true).read(true).write(true).truncate(true).open(&path).unwrap();
        let mut writer = crate::AttachmentReceiveWriter {
            file: omni_storage::AttachmentPartWriter::Plain(file),
            material: crate::AttachmentContentCryptoMaterial { media_key: None, nonce_seed: None, chunk_size_bytes: leaf as u32 },
            message_id: "m".into(),
            file_size_bytes: data.len() as i64,
            have: vec![false],
            written_leaves: std::collections::BTreeMap::new(),
            progress_bytes: 0,
            checkpointed_at_unix_ms: 0,
            content_id: tree.id,
            block_leaves: std::collections::BTreeMap::new(),
            unsaved_blocks: Vec::new(),
        };
        // No proof for the block yet: refused.
        assert!(crate::write_checked_attachment_chunk(&mut writer, 0, &data[..leaf]).is_err());
        writer.block_leaves.insert(0, leaves.clone());
        // A wrong chunk, a cut one, a misplaced one: refused, nothing written.
        let mut wrong = data[..leaf].to_vec();
        wrong[17] ^= 1;
        assert!(crate::write_checked_attachment_chunk(&mut writer, 0, &wrong).is_err());
        assert!(crate::write_checked_attachment_chunk(&mut writer, 0, &data[..leaf - 1]).is_err());
        assert!(crate::write_checked_attachment_chunk(&mut writer, 1, &data[1..leaf + 1]).is_err());
        assert_eq!((writer.progress_bytes, fs::metadata(&path).unwrap().len()), (0, 0));
        // The right leaves, last first and one twice: written once each; the last one written
        // closes the block with its hash.
        let chunks: Vec<&[u8]> = data.chunks(leaf).collect();
        for index in [2, 0, 0, 1] {
            assert!(!writer.is_complete());
            crate::write_checked_attachment_chunk(&mut writer, (index * leaf) as i64, chunks[index]).unwrap();
        }
        assert!(writer.is_complete());
        assert_eq!(writer.progress_bytes, data.len() as i64);
        assert_eq!(writer.unsaved_blocks, vec![(0, tree.blocks[0])]);
        assert!(writer.block_leaves.is_empty() && writer.written_leaves.is_empty());
        drop(writer);
        assert_eq!(fs::read(&path).unwrap(), data);
        let _ = fs::remove_file(&path);
    }

    /// «Фото» (docs/decisions/media-messages.md §1): the picture goes out re-encoded and capped, the
    /// preview rides in the offer, and the receiver downloads the photo by itself.
    #[test]
    fn two_node_photo_goes_out_reencoded_and_downloads_by_itself() {
        let _guard = network_test_guard();
        let sender = create_stage5_peer("photo-sender", "Sender", "Sender PC");
        let receiver = create_stage5_peer("photo-receiver", "Receiver", "Receiver PC");
        link_stage5_contact(&sender.app, &receiver, "Receiver");

        let source_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("photo-source-{}.png", unique_suffix()));
        fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        image::RgbImage::from_fn(3000, 1500, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]))
            .save(&source_path)
            .unwrap();

        link_stage5_peers_over_mesh(&sender, &receiver);

        let sent = sender
            .app
            .send_attachment_request(omni_types::SendAttachmentRequest {
                dialog_key: receiver.dialog_key.clone(),
                body: String::new(),
                source_file_path: source_path.to_string_lossy().into_owned(),
                display_file_name: None,
                media_protection_mode: omni_types::AttachmentMediaProtectionMode::E2eeMedia,
                as_photo: true,
            })
            .unwrap();
        let sent_attachment = sent.attachment.expect("a photo is an attachment");
        assert!(sent_attachment.file_name.ends_with(".jpg"), "{}", sent_attachment.file_name);
        let sent_presentation = sent_attachment.presentation.expect("sent as a photo");
        assert_eq!((sent_presentation.width, sent_presentation.height), (2560, 1280));
        assert!(sent_presentation.preview_jpeg_b64.is_some());

        wait_for_condition(Duration::from_secs(20), || {
            receiver
                .app
                .list_messages(sender.dialog_key.as_str())
                .ok()
                .is_some_and(|history| {
                    history.messages.iter().any(|message| {
                        message.id == sent.id
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.state == omni_types::AttachmentTransferState::Downloaded
                            })
                    })
                })
        });
        let received = receiver
            .app
            .list_messages(sender.dialog_key.as_str())
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.id == sent.id)
            .unwrap()
            .attachment
            .unwrap();
        let presentation = received.presentation.expect("the receiver kept it a photo");
        assert_eq!((presentation.width, presentation.height), (2560, 1280));
        assert!(presentation.preview_jpeg_b64.is_some());
        let saved = received.local_saved_path.expect("downloaded into the photo folder");
        assert!(omni_crypto::is_file_at_rest(std::path::Path::new(&saved)), "the photo folder is encrypted at rest");
        assert_eq!(
            image::ImageReader::new(std::io::Cursor::new(receiver.app.storage.read_media_file(std::path::Path::new(&saved)).unwrap()))
                .with_guessed_format()
                .unwrap()
                .into_dimensions()
                .unwrap(),
            (2560, 1280)
        );

        sender.app.stop_mesh();
        receiver.app.stop_mesh();
    }

    #[test]
    fn two_node_large_attachment_roundtrip_preserves_bytes() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-large-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-large-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "large-attachment-peer");

        let source_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-large-source-{}.bin", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-large-downloaded-{}.bin", unique_suffix()));
        fs::create_dir_all(source_path.parent().expect("source parent should exist")).unwrap();
        // Wave 5d cleanup (2026-05-18): reduced from 2 MB+137 to
        // 1 MB+137. `choose_attachment_chunk_size_bytes` clamps to
        // 16 KB..1 MB, so 1 MB+137 still exercises the multi-chunk
        // path (2 chunks: 1 MB + 137 bytes) — same coverage as before
        // for "do we reassemble correctly across chunk boundaries",
        // half the cumulative encrypt/decrypt work, and crucially
        // half the wall-clock under the contended full-suite Debug
        // run. The smaller size combined with the longer per-condition
        // budget (60s, see comment below) keeps this test reliable
        // under `--test-threads=<num_cpus>`.
        let payload = vec![0x5A; 1024 * 1024 + 137];
        fs::write(&source_path, &payload).unwrap();

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "large file for server",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        assert!(sent.attachment.is_some());

        // 60 s is generous headroom; the transfer completes in seconds. History (2026-07-24 root-cause):
        // this test used to fail deterministically on slower/contended boxes and was long mis-blamed on
        // "CPU contention / too-short timeout" (payload was shrunk + timeout bumped 12s→60s, to no avail).
        // The real killer was the test-build stale-transfer GC: `ATTACHMENT_TRANSFER_STALE_AFTER_MS`=200 ms
        // marked the still-in-progress download `Failed` ("abandoned by sender") whenever its first chunk
        // took >200 ms — then the `wait_for_condition` just waited out a Failed that never becomes
        // Downloaded. That GC is gone (2026-10-15, attachment-transfer-quotas.md). See docs/architecture/test-patterns.md.
        wait_for_condition(Duration::from_secs(60), || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history.messages.iter().any(|message| {
                        message.body == "large file for server"
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.state == omni_types::AttachmentTransferState::Offered
                            })
                    })
                })
                .unwrap_or(false)
        });

        let offered = server
            .app
            .list_messages(client.dialog_key.as_str())
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.body == "large file for server")
            .expect("offered large attachment message should exist");

        server
            .app
            .request_attachment_download(
                offered.id.as_str(),
                save_path.to_string_lossy().as_ref(),
            )
            .unwrap();

        wait_for_condition(Duration::from_secs(60), || {
            save_path.exists()
                && server
                    .app
                    .list_messages(client.dialog_key.as_str())
                    .ok()
                    .map(|history| {
                        history.messages.iter().any(|message| {
                            message.id == offered.id
                                && message.attachment.as_ref().is_some_and(|attachment| {
                                    attachment.state
                                        == omni_types::AttachmentTransferState::Downloaded
                                })
                        })
                    })
                    .unwrap_or(false)
        });

        assert_eq!(fs::read(&save_path).unwrap(), payload);
    }

    /// docs/decisions/attachment-content-tree.md, stage 3: a download is a map of blocks, not a
    /// prefix. Blocks 0 and 2 of three are here already (from another source, before a break), block
    /// 1 is not: the request resumes at the hole, what is here is counted and kept, and the file
    /// comes out whole.
    #[test]
    fn a_download_with_a_hole_in_its_block_map_fills_the_hole_and_keeps_the_rest() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-hole-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-hole-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "hole-attachment-peer");

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let suffix = unique_suffix();
        let source_path = dir.join(format!("attachment-hole-source-{suffix}.bin"));
        let save_path = dir.join(format!("attachment-hole-downloaded-{suffix}.bin"));
        let block = omni_crypto::CONTENT_TREE_BLOCK_BYTES as usize;
        // Every byte names its place: a block written at the wrong offset cannot pass for the file.
        let payload: Vec<u8> = (0..2 * block + 4096 + 5).map(|at| (at % 251) as u8 ^ (at / block) as u8).collect();
        fs::write(&source_path, &payload).unwrap();
        let mut tree = omni_crypto::ContentTreeBuilder::new();
        tree.update(&payload);
        let tree = tree.finish();
        assert_eq!(tree.blocks.len(), 3);

        link_stage5_peers_over_mesh(&client, &server);
        client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "file with a hole",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        let offered = || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .and_then(|history| history.messages.into_iter().find(|message| message.body == "file with a hole"))
        };
        wait_for_condition(Duration::from_secs(60), || {
            offered().is_some_and(|message| {
                message.attachment.is_some_and(|attachment| attachment.state == omni_types::AttachmentTransferState::Offered)
            })
        });
        let offered = offered().unwrap();

        // Blocks 0 and 2 on disk where the download keeps its file, and in the map.
        let file_name = save_path.file_name().unwrap().to_string_lossy().into_owned();
        let temp_path = dir.join(format!("{file_name}.{}.part", offered.id));
        let mut part = vec![0u8; payload.len()];
        part[..block].copy_from_slice(&payload[..block]);
        part[2 * block..].copy_from_slice(&payload[2 * block..]);
        fs::write(&temp_path, &part).unwrap();
        Storage::open(server.db_path.as_str())
            .unwrap()
            .mark_attachment_blocks_received(&offered.id, &[(0, tree.blocks[0]), (2, tree.blocks[2])])
            .unwrap();

        let requested = server.app.request_attachment_download(offered.id.as_str(), save_path.to_string_lossy().as_ref()).unwrap();
        let here = (block + 4096 + 5) as i64;
        assert!(
            requested.attachment.as_ref().is_some_and(|attachment| attachment.bytes_transferred >= here),
            "what is here counts from the start: {:?}",
            requested.attachment.map(|attachment| attachment.bytes_transferred)
        );
        wait_for_condition(Duration::from_secs(60), || {
            save_path.exists()
                && server.app.list_messages(client.dialog_key.as_str()).ok().is_some_and(|history| {
                    history.messages.iter().any(|message| {
                        message.id == offered.id
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.state == omni_types::AttachmentTransferState::Downloaded
                            })
                    })
                })
        });
        assert_eq!(fs::read(&save_path).unwrap(), payload);
        assert!(!temp_path.exists());
        let _ = fs::remove_file(&source_path);
        let _ = fs::remove_file(&save_path);
    }

    /// docs/decisions/attachment-content-tree.md, «Устойчивость к сети»: a download whose stream died
    /// with its sender is asked for again the moment the sender's session is back — not at the stall
    /// retry (20 s at first, up to 10 minutes after a long break). The receiver stays linked to
    /// someone else meanwhile, so nothing else wakes the download up.
    /// A receiver whose mesh restarted keeps no live context for a sender whose session is up — that
    /// sender sends no new hello. Its file stream must still be taken, by the persisted node↔dialog
    /// route a message takes; dropped, the sender retried and the receiver asked again forever (live
    /// 2026-10-16).
    #[test]
    fn a_file_comes_in_from_a_peer_whose_hello_the_receiver_no_longer_holds() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-noctx-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-noctx-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "noctx-attachment-peer");

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let suffix = unique_suffix();
        let source_path = dir.join(format!("attachment-noctx-source-{suffix}.bin"));
        let save_path = dir.join(format!("attachment-noctx-downloaded-{suffix}.bin"));
        let payload: Vec<u8> = (0..2 * 1024 * 1024 + 7).map(|at| (at % 251) as u8).collect();
        fs::write(&source_path, &payload).unwrap();

        link_stage5_peers_over_mesh(&client, &server);
        client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "file from a peer with no hello held",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        let row = || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .and_then(|history| history.messages.into_iter().find(|message| message.body == "file from a peer with no hello held"))
        };
        wait_for_condition(Duration::from_secs(60), || {
            row().is_some_and(|message| {
                message.attachment.is_some_and(|attachment| attachment.state == omni_types::AttachmentTransferState::Offered)
            })
        });
        // What a mesh restart leaves: the live contexts gone, the persisted routes kept.
        server.app.mesh_node_ctx.lock().unwrap().clear();

        server
            .app
            .request_attachment_download(&row().unwrap().id, save_path.to_string_lossy().as_ref())
            .unwrap();
        let state = || row().and_then(|message| message.attachment).map(|attachment| (attachment.state, attachment.bytes_transferred));
        wait_for_state(
            Duration::from_secs(30),
            || format!("download {:?}", state()),
            || save_path.exists() && state().is_some_and(|(state, _)| state == omni_types::AttachmentTransferState::Downloaded),
        );
        assert_eq!(fs::read(&save_path).unwrap(), payload);
        for peer in [&client, &server] {
            peer.app.stop_mesh();
        }
        let _ = fs::remove_file(&source_path);
        let _ = fs::remove_file(&save_path);
    }

    #[test]
    fn a_download_is_asked_for_again_as_soon_as_its_sender_is_back() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-back-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-back-client", "Client", "Client PC");
        let other = create_stage5_peer("attachment-back-other", "Other", "Other PC");
        link_stage5_contact(&client.app, &server, "back-attachment-peer");
        link_stage5_contact(&other.app, &server, "back-attachment-other");

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let suffix = unique_suffix();
        let source_path = dir.join(format!("attachment-back-source-{suffix}.bin"));
        let save_path = dir.join(format!("attachment-back-downloaded-{suffix}.bin"));
        let payload: Vec<u8> = (0..2 * 1024 * 1024 + 11).map(|at| (at % 253) as u8).collect();
        fs::write(&source_path, &payload).unwrap();

        link_stage5_peers_over_mesh(&client, &server);
        link_stage5_peers_over_mesh(&other, &server);
        client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "file whose sender comes back",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        let row = || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .and_then(|history| history.messages.into_iter().find(|message| message.body == "file whose sender comes back"))
        };
        wait_for_condition(Duration::from_secs(60), || {
            row().is_some_and(|message| {
                message.attachment.is_some_and(|attachment| attachment.state == omni_types::AttachmentTransferState::Offered)
            })
        });
        let offered = row().unwrap();

        // The sender goes away; the receiver learns it the way the product does.
        let client_hex = client.app.mesh_node_address_hex().unwrap().expect("client mesh is up");
        client.app.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || !server.app.mesh_has_link(&client_hex).unwrap_or(true));
        // Its download as a stream that died with it: asked for, under way, nothing coming.
        {
            let storage = Storage::open(server.db_path.as_str()).unwrap();
            let now = current_unix_ms();
            storage.mark_attachment_download_requested(&offered.id, &save_path, "transfer-that-died", now).unwrap();
            storage.mark_attachment_downloading(&offered.id, 0, now).unwrap();
        }

        let back = Instant::now();
        link_stage5_peers_over_mesh(&client, &server);
        let state = || row().and_then(|message| message.attachment).map(|attachment| (attachment.state, attachment.bytes_transferred));
        wait_for_state(
            Duration::from_secs(15),
            || format!("download {:?}, {:?} since the sender is back", state(), back.elapsed()),
            || save_path.exists() && state().is_some_and(|(state, _)| state == omni_types::AttachmentTransferState::Downloaded),
        );
        assert_eq!(fs::read(&save_path).unwrap(), payload);
        for peer in [&client, &server, &other] {
            peer.app.stop_mesh();
        }
        let _ = fs::remove_file(&source_path);
        let _ = fs::remove_file(&save_path);
    }

    /// Sets an environment variable for the duration of a test and puts it back afterwards, panic or
    /// not. Only safe to use while holding `network_test_guard`, which serialises the tests that can
    /// actually read this state.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => std::env::set_var(self.key, previous),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// The multi-stream sender is off by default (`OMNI_ATTACHMENT_STREAMS`), and while it was being
    /// built it broke the transfer twice in ways a single stream cannot expose:
    ///
    /// - every chunk stream opened its own writer for the same output file, so whichever wrote last
    ///   won and the rest of the bytes were lost;
    /// - Complete rode a stream of its own and overtook chunks still in flight, so the receiver was
    ///   told "all done" for a transfer it had not finished — or had not yet heard of at all.
    ///
    /// Both are invisible unless chunks actually interleave across streams, so this is the single
    /// round trip with the fan-out on and enough chunks (5 over 4 streams) that they do. The
    /// assertion that matters is the last one: the bytes on disk, in order, all of them.
    /// An empty file downloads: there is no chunk, so nothing waits for one or makes its `.part`. It
    /// failed every time (QA 2026-10-17) — the wait ran out its 30 s drain, then completion found no
    /// file to check.
    #[test]
    fn two_node_empty_attachment_downloads_at_once() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("attachment-empty-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-empty-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "empty-attachment-peer");
        let source_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-empty-source-{}.txt", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-empty-downloaded-{}.txt", unique_suffix()));
        fs::create_dir_all(source_path.parent().expect("source parent should exist")).unwrap();
        fs::write(&source_path, b"").unwrap();
        link_stage5_peers_over_mesh(&client, &server);

        client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "empty file for server",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        let offered = || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .and_then(|history| {
                    history.messages.into_iter().find(|message| message.body == "empty file for server")
                })
        };
        wait_for_condition(Duration::from_secs(30), || offered().is_some_and(|m| m.attachment.is_some()));
        let id = offered().unwrap().id;
        server
            .app
            .request_attachment_download(id.as_str(), save_path.to_string_lossy().as_ref())
            .unwrap();

        wait_for_state(
            Duration::from_secs(10),
            || format!("{:?}", offered().and_then(|m| m.attachment).map(|a| a.state)),
            || {
                save_path.exists()
                    && offered().and_then(|m| m.attachment).is_some_and(|a| {
                        a.state == omni_types::AttachmentTransferState::Downloaded
                    })
            },
        );
        assert_eq!(fs::read(&save_path).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn two_node_attachment_roundtrip_preserves_bytes_across_several_chunk_streams() {
        let _guard = network_test_guard();
        let _streams = EnvVarGuard::set("OMNI_ATTACHMENT_STREAMS", "4");
        let server = create_stage5_peer("attachment-fanout-server", "Server", "Server PC");
        let client = create_stage5_peer("attachment-fanout-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "fanout-attachment-peer");

        let source_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-fanout-source-{}.bin", unique_suffix()));
        let save_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("attachment-fanout-downloaded-{}.bin", unique_suffix()));
        fs::create_dir_all(source_path.parent().expect("source parent should exist")).unwrap();
        // Chunk size is clamped at 1 MB, so the file size is what buys chunk count: 4 MB + 137 gives
        // five chunks over four streams — every stream carries at least one, and one carries two, so
        // the order chunks arrive in is genuinely not the order they were sent.
        let payload: Vec<u8> = (0..(4 * 1024 * 1024 + 137))
            .map(|index| (index % 251) as u8)
            .collect();
        fs::write(&source_path, &payload).unwrap();

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_attachment(
                server.dialog_key.as_str(),
                "fanned file for server",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();
        assert!(sent.attachment.is_some());

        wait_for_condition(Duration::from_secs(60), || {
            server
                .app
                .list_messages(client.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history.messages.iter().any(|message| {
                        message.body == "fanned file for server"
                            && message.attachment.as_ref().is_some_and(|attachment| {
                                attachment.state == omni_types::AttachmentTransferState::Offered
                            })
                    })
                })
                .unwrap_or(false)
        });

        let offered = server
            .app
            .list_messages(client.dialog_key.as_str())
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.body == "fanned file for server")
            .expect("offered fanned attachment message should exist");

        server
            .app
            .request_attachment_download(offered.id.as_str(), save_path.to_string_lossy().as_ref())
            .unwrap();

        wait_for_condition(Duration::from_secs(60), || {
            save_path.exists()
                && server
                    .app
                    .list_messages(client.dialog_key.as_str())
                    .ok()
                    .map(|history| {
                        history.messages.iter().any(|message| {
                            message.id == offered.id
                                && message.attachment.as_ref().is_some_and(|attachment| {
                                    attachment.state
                                        == omni_types::AttachmentTransferState::Downloaded
                                })
                        })
                    })
                    .unwrap_or(false)
        });

        // Byte-for-byte, not just the length: the writer bug produced a file of the right size with
        // the wrong contents in the middle.
        assert_eq!(fs::read(&save_path).unwrap(), payload);
    }

    #[test]
    fn two_node_public_profile_sync_updates_remote_display_name_and_avatar() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("profile-sync-server", "Server", "Server PC");
        let client = create_stage5_peer("profile-sync-client", "Client", "Client PC");
        let server_contact_for_client = link_stage5_contact(&server.app, &client, "Client peer");
        link_stage5_contact(&client.app, &server, "Server peer");

        let avatar_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("profile-avatar-{}.png", unique_suffix()));
        fs::create_dir_all(avatar_path.parent().expect("avatar parent should exist")).unwrap();
        fs::write(&avatar_path, tiny_png_bytes()).unwrap();

        link_stage5_peers_over_mesh(&client, &server);
        // A contact that has talked before: the profile change rides the established session.
        establish_stage5_session(&client, &server);
        assert_ne!(
            server
                .app
                .get_contact_card(&server_contact_for_client.contact_id)
                .unwrap()
                .contact
                .public_display_name
                .as_deref(),
            Some("Client Renamed"),
            "the server cannot know the new name before the client changes it"
        );

        let updated = client
            .app
            .update_local_public_profile("Client Renamed", Some(&avatar_path), false)
            .unwrap();
        assert_eq!(updated.descriptor.display_name, "Client Renamed");
        assert!(updated.public_avatar_hash_hex.is_some());

        wait_for_condition(Duration::from_secs(10), || {
            let contact = server
                .app
                .get_contact_card(&server_contact_for_client.contact_id)
                .expect("contact card should load");
            contact.contact.public_display_name.as_deref() == Some("Client Renamed")
                && contact.contact.public_avatar_hash_hex == updated.public_avatar_hash_hex
                && contact
                    .contact
                    .public_avatar_image_path
                    .as_ref()
                    .map(|path| PathBuf::from(path).exists())
                    .unwrap_or(false)
        });

        let synced = server
            .app
            .get_contact_card(&server_contact_for_client.contact_id)
            .unwrap();
        assert_eq!(
            synced.contact.public_display_name.as_deref(),
            Some("Client Renamed")
        );
        assert_eq!(
            synced.contact.public_avatar_hash_hex,
            updated.public_avatar_hash_hex
        );
        assert!(synced.contact.public_profile_updated_at_unix_ms.is_some());
        assert!(
            synced.contact
                .public_avatar_image_path
                .as_ref()
                .map(|path| PathBuf::from(path).exists())
                .unwrap_or(false)
        );

        client
            .app
            .update_local_public_profile("Client Final", None, true)
            .unwrap();

        wait_for_condition(Duration::from_secs(10), || {
            let contact = server
                .app
                .get_contact_card(&server_contact_for_client.contact_id)
                .expect("contact card should load");
            contact.contact.public_display_name.as_deref() == Some("Client Final")
                && contact.contact.public_avatar_hash_hex.is_none()
                && contact.contact.public_avatar_image_path.is_none()
        });
    }

    /// A new contact learns my name and avatar when it approves me — the profile rides the session
    /// the pairing made. Formerly "forget, re-add pending, re-approve" over the pre-mesh transport's
    /// active connection; on the mesh the way a contact (re)appears is the pairing, so that is the
    /// path tested. Negative half: before the approval the receiver has only the pairing-time view.
    #[test]
    fn approval_delivers_the_requesters_public_profile_and_avatar() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("approve-profile-server", "Server", "Server PC");
        let client = create_stage5_peer("approve-profile-client", "Client", "Client PC");

        let avatar_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("approve-profile-avatar-{}.png", unique_suffix()));
        fs::create_dir_all(avatar_path.parent().expect("avatar parent should exist")).unwrap();
        fs::write(&avatar_path, tiny_png_bytes()).unwrap();
        let updated = client
            .app
            .update_local_public_profile("Client Approved", Some(&avatar_path), false)
            .unwrap();

        pair_stage5_over_mesh(&client, &server);
        let pending = server
            .app
            .storage
            .find_contact_by_remote_account_id(&client.account.descriptor.account_id)
            .unwrap()
            .expect("the pairing leaves an incoming-pending contact");
        assert!(
            pending.public_avatar_image_path.is_none(),
            "no avatar crosses before the receiver approves"
        );

        let approved = server.app.approve_contact(&pending.contact_id).unwrap();
        assert_eq!(approved.relationship_state, ContactRelationshipState::Approved);
        wait_for_condition(Duration::from_secs(15), || {
            server
                .app
                .get_contact_card(&approved.contact_id)
                .ok()
                .is_some_and(|card| {
                    card.contact.public_display_name.as_deref() == Some("Client Approved")
                        && card.contact.public_avatar_hash_hex == updated.public_avatar_hash_hex
                        && card
                            .contact
                            .public_avatar_image_path
                            .as_ref()
                            .map(|path| PathBuf::from(path).exists())
                            .unwrap_or(false)
                })
        });
    }

    #[test]
    fn session_activation_does_not_create_contact_requests_for_unknown_peers() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("pending-request-server", "Server", "Server PC");
        let client = create_stage5_peer("pending-request-client", "Client", "Client PC");

        link_stage5_peers_over_mesh(&client, &server);
        thread::sleep(Duration::from_millis(700));

        assert!(
            server.app.list_contacts().unwrap().is_empty(),
            "receiver must not treat a bare secure session as a contact request"
        );
        assert!(
            client.app.list_contacts().unwrap().is_empty(),
            "dialer must not receive a reverse contact request from a bare secure session"
        );
    }

    #[test]
    fn encrypted_receipt_handling_updates_delivered_status_correctly() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("receipt-server", "Server", "Server PC");
        let client = create_stage5_peer("receipt-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        link_stage5_peers_over_mesh(&client, &server);

        let sent = client
            .app
            .send_text(server.dialog_key.as_str(), "receipt me")
            .unwrap();
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);
        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "receipt me",
            MessageStatus::Delivered,
        );
        wait_for_condition(Duration::from_secs(10), || {
            client
                .app
                .list_messages(server.dialog_key.as_str())
                .ok()
                .map(|history| {
                    history.messages.iter().any(|message| {
                        message.body == "receipt me"
                            && message.status == MessageStatus::Delivered
                    })
                })
                .unwrap_or(false)
        });

        let history = client.app.list_messages(server.dialog_key.as_str()).unwrap();
        let delivered = history
            .messages
            .iter()
            .find(|message| message.id == sent.id)
            .unwrap();
        assert!(delivered.remote_delivered_at_unix_ms.is_some());
        let client_session = client
            .app
            .storage
            .get_latest_session_for_remote_account(&server.account.descriptor.account_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            client_session.bootstrap_state,
            omni_crypto::SessionBootstrapState::Established
        );

        if let Some(receipt_details) = client
            .app
            .get_message_security_details(&format!("receipt::{}", sent.id))
            .unwrap()
        {
            assert_eq!(
                receipt_details.payload_kind,
                Some(omni_messaging::SecurityPayloadKind::DeliveryReceipt)
            );
            assert_eq!(
            receipt_details.encryption_state,
            omni_messaging::MessageEncryptionState::EncryptedReceived
        );
        }
    }

    /// After the client app restarts, delivery continues both ways. Regression for
    /// docs/bugs/receipt-after-restart-fails-to-decrypt.md: the stopped client never closed its mesh
    /// connections, so the server kept sending the receipt into the dead one until its liveness prune.
    #[test]
    fn restart_restores_session_and_continues_delivery() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("restart-server", "Server", "Server PC");
        let client = create_stage5_peer("restart-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");

        let client_db_path = client.db_path.clone();
        link_stage5_peers_over_mesh(&client, &server);

        client
            .app
            .send_text(server.dialog_key.as_str(), "before restart")
            .unwrap();
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 1);
        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "before restart",
            MessageStatus::Delivered,
        );
        wait_for_message_status(
            &client.app,
            server.dialog_key.as_str(),
            "before restart",
            MessageStatus::Delivered,
        );

        drop(client.app);

        let reopened = OmniApp::init(AppInitRequest {
            db_path: client_db_path,
        })
        .unwrap();
        link_apps_over_mesh(&reopened, &server.app);

        let summary = reopened
            .get_chat_security_summary(server.dialog_key.as_str())
            .unwrap();
        assert!(summary.has_established_session);

        reopened
            .send_text(server.dialog_key.as_str(), "after restart")
            .unwrap();
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 2);
        wait_for_message_status(
            &server.app,
            client.dialog_key.as_str(),
            "after restart",
            MessageStatus::Delivered,
        );
        wait_for_message_status(
            &reopened,
            server.dialog_key.as_str(),
            "after restart",
            MessageStatus::Delivered,
        );
        wait_for_message_count(&server.app, client.dialog_key.as_str(), 2);

        let client_history = reopened.list_messages(server.dialog_key.as_str()).unwrap();
        assert_eq!(client_history.messages.len(), 2);
    }

    #[test]
    fn incoming_verification_challenge_does_not_auto_approve_pending_contact() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("pending-challenge-server", "Bob", "Bob PC");
        let client = create_stage5_peer("pending-challenge-client", "Alice", "Alice PC");

        // Bob holds Alice as an incoming-pending contact with an established session: the state in
        // which a crafted challenge must not be taken as approval.
        pair_stage5_over_mesh(&client, &server);
        let pending = server
            .app
            .storage
            .find_contact_by_remote_account_id(&client.account.descriptor.account_id)
            .unwrap()
            .expect("pending request should exist before the challenge");
        assert_eq!(pending.relationship_state, ContactRelationshipState::Pending);

        let issued_at = current_unix_ms();
        let challenge = omni_types::VerificationChallengeV1 {
            version: omni_types::OMNI_PROTOCOL_VERSION_V1,
            challenge_id: format!("challenge-pending-{}", unique_suffix()),
            nonce_hex: "11".repeat(16),
            issued_at_unix_ms: issued_at,
            expires_at_unix_ms: issued_at + 60_000,
            remote_instance_id: Some(server.instance_id.clone()),
            remote_account_id: Some(server.account.descriptor.account_id.0.clone()),
        };

        // Read, encrypt and write back under the session lock, as the app does: its background
        // senders advance this session too, and a stale write is refused.
        let session_guard = super::lock_app_session_guard(&client.app.session_lock).unwrap();
        let client_session = client
            .app
            .storage
            .get_latest_session_for_remote_account(&server.account.descriptor.account_id)
            .unwrap()
            .expect("client session should exist before sending the challenge");
        let mut outgoing = omni_messaging::build_encrypted_wire_envelope(
            &client_session,
            &WirePayloadV1::VerificationChallenge(challenge),
            SecurityPayloadKind::VerificationChallenge,
            current_unix_ms(),
        )
        .unwrap();
        client
            .app
            .storage
            .store_session_record(&mut outgoing.updated_session)
            .unwrap();
        drop(session_guard);

        let transport = client
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("client transport should still be available");
        transport
            .send_wire_envelope_for_dialog_key(server.dialog_key.as_str(), &outgoing.envelope)
            .unwrap();

        thread::sleep(Duration::from_millis(700));

        let pending_after = server
            .app
            .storage
            .find_contact_by_remote_account_id(&client.account.descriptor.account_id)
            .unwrap()
            .expect("pending contact should still exist after the challenge");
        assert_eq!(
            pending_after.relationship_state,
            ContactRelationshipState::Pending,
            "incoming verification challenge must not auto-approve a pending contact"
        );
    }

    #[test]
    fn pending_contact_attachment_control_events_are_ignored() {
        let _guard = network_test_guard();
        let server = create_stage5_peer("pending-attachment-server", "Bob", "Bob PC");
        let client = create_stage5_peer("pending-attachment-client", "Alice", "Alice PC");

        pair_stage5_over_mesh(&client, &server);
        wait_for_condition(Duration::from_secs(10), || {
            server
                .app
                .get_chat_security_summary(client.dialog_key.as_str())
                .ok()
                .map(|summary| summary.has_established_session)
                .unwrap_or(false)
        });

        let transport = client
            .app
            .transport_handle
            .lock()
            .unwrap()
            .clone()
            .expect("client transport should still be available");
        let mut client_session = client
            .app
            .storage
            .get_latest_session_for_remote_account(&server.account.descriptor.account_id)
            .unwrap()
            .expect("client session should exist before sending attachment controls");
        let message_id = format!("pending-attachment-control-{}", unique_suffix());
        let metadata_ids = [
            format!("attachment-request::{message_id}"),
            format!("attachment-failed::{message_id}"),
            format!("attachment-downloaded::{message_id}"),
            format!("attachment-revoked::{message_id}"),
        ];
        let now = current_unix_ms();
        let controls = [
            (
                omni_types::LogicalEventEnvelopeV1::attachment_download_request(
                    format!("transfer-{message_id}"),
                    message_id.clone(),
                    0,
                    now,
                ),
                SecurityPayloadKind::AttachmentDownloadRequest,
            ),
            (
                omni_types::LogicalEventEnvelopeV1::attachment_transfer_failed(
                    message_id.clone(),
                    "file_not_found",
                    "missing",
                    now + 3,
                ),
                SecurityPayloadKind::AttachmentTransferFailed,
            ),
            (
                omni_types::LogicalEventEnvelopeV1::attachment_downloaded(
                    message_id.clone(),
                    now + 4,
                ),
                SecurityPayloadKind::AttachmentDownloaded,
            ),
            (
                omni_types::LogicalEventEnvelopeV1::attachment_access_revoked(
                    message_id.clone(),
                    now + 5,
                ),
                SecurityPayloadKind::AttachmentAccessRevoked,
            ),
        ];

        for (logical_event, payload_kind) in controls {
            // The client app's own background senders (presence, keepalive) advance this session
            // too, so the test encrypts the way the app does: under the session lock, with the
            // session as stored now — a copy kept from the previous round is refused as stale.
            let _session_guard = super::lock_app_session_guard(&client.app.session_lock).unwrap();
            client_session = client
                .app
                .storage
                .get_session_record(&client_session.session_address)
                .unwrap()
                .expect("client session is still stored");
            let outgoing = omni_messaging::build_encrypted_logical_event_envelope(
                &client_session,
                &logical_event,
                payload_kind,
                current_unix_ms(),
            )
            .unwrap();
            client_session = outgoing.updated_session;
            client
                .app
                .storage
                .store_session_record(&mut client_session)
                .unwrap();
            transport
                .send_wire_envelope_for_dialog_key(server.dialog_key.as_str(), &outgoing.envelope)
                .unwrap();
        }

        thread::sleep(Duration::from_millis(700));

        for metadata_id in metadata_ids {
            assert!(
                server
                    .app
                    .get_message_security_details(&metadata_id)
                    .unwrap()
                    .is_none(),
                "pending contact attachment control should not persist metadata for {metadata_id}"
            );
        }

        shutdown_stage5_peer(&client);
        shutdown_stage5_peer(&server);
        thread::sleep(Duration::from_millis(250));
    }

    #[test]
    fn receiver_approval_promotes_requester_before_first_reply() {
        let _guard = network_test_guard();
        let receiver = create_stage5_peer("approve-reply-receiver", "Bob", "Bob PC");
        let requester = create_stage5_peer("approve-reply-requester", "Alice", "Alice PC");

        pair_stage5_over_mesh(&requester, &receiver);

        let pending = receiver
            .app
            .list_contacts()
            .unwrap()
            .into_iter()
            .find(|contact| {
                contact.relationship_state == ContactRelationshipState::Pending
                    && contact.remote_account_id.as_ref()
                        == Some(&requester.account.descriptor.account_id)
            })
            .expect("receiver should see the requester as pending before approval");

        let approved = receiver.app.approve_contact(&pending.contact_id).unwrap();
        assert_eq!(approved.relationship_state, ContactRelationshipState::Approved);

        wait_for_condition(Duration::from_secs(40), || {
            requester
                .app
                .storage
                .find_contact_by_remote_account_id(&receiver.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_some_and(|contact| {
                    contact.relationship_state == ContactRelationshipState::Approved
                })
        });

        let reply_chat = receiver
            .app
            .start_chat_from_contact(&approved.contact_id)
            .expect("receiver should be able to open chat immediately after approval");
        let sent = receiver
            .app
            .send_text(reply_chat.dialog_key.as_str(), "reply after first approval")
            .expect("receiver should be able to send the first reply after approval");
        assert!(matches!(
            sent.status,
            MessageStatus::Sent
                | MessageStatus::Queued
                | MessageStatus::Retrying
                | MessageStatus::Delivered
        ));

        let requester_contact = requester
            .app
            .storage
            .find_contact_by_remote_account_id(&receiver.account.descriptor.account_id)
            .unwrap()
            .expect("requester contact should still exist after approval");
        let requester_dialog_key = requester_contact
            .primary_dialog_key
            .clone()
            .unwrap_or_else(|| {
                requester
                    .app
                    .start_chat_from_contact(&requester_contact.contact_id)
                    .expect("requester should be able to open the approved chat")
                    .dialog_key
            });

        wait_for_message_count(&requester.app, requester_dialog_key.as_str(), 1);
        wait_for_message_status(
            &requester.app,
            requester_dialog_key.as_str(),
            "reply after first approval",
            MessageStatus::Delivered,
        );
    }

    #[test]
    fn approved_contact_active_session_keeps_presence_online() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("presence-online-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("presence-online-bob", "Bob", "Bob PC");
        let alice_contact = link_stage5_contact(&alice.app, &bob, "Bob peer");
        let bob_contact = link_stage5_contact(&bob.app, &alice, "Alice peer");

        link_stage5_peers_over_mesh(&alice, &bob);
        wait_for_condition(Duration::from_secs(5), || {
            alice
                .app
                .get_contact_card(&alice_contact.contact_id)
                .ok()
                .is_some_and(|card| card.contact.has_met)
                && bob
                    .app
                    .get_contact_card(&bob_contact.contact_id)
                    .ok()
                    .is_some_and(|card| card.contact.has_met)
        });

        let now_unix_ms = current_unix_ms();
        alice
            .app
            .storage
            .record_contact_presence_recently(&alice_contact.contact_id, now_unix_ms)
            .unwrap();
        bob.app
            .storage
            .record_contact_presence_recently(&bob_contact.contact_id, now_unix_ms)
            .unwrap();

        wait_for_condition(Duration::from_secs(3), || {
            alice
                .app
                .get_contact_card(&alice_contact.contact_id)
                .ok()
                .is_some_and(|card| {
                    card.contact.presence_state
                            == omni_identity::ContactPresenceState::Online
                })
                && bob
                    .app
                    .get_contact_card(&bob_contact.contact_id)
                    .ok()
                    .is_some_and(|card| {
                        card.contact.presence_state
                                == omni_identity::ContactPresenceState::Online
                    })
        });
    }

    #[test]
    /// A device of the contact with other keys than we saw before is not an identity change: every
    /// bundle we take is certified by the contact's account master key (checked on receipt), and the
    /// safety number is of the account alone (docs/bugs/safety-number-differs-per-own-device.md). It
    /// used to flip the contact to "identity changed" — falsely, on every switch between two of its
    /// devices, since each device has its own keys and the observation kept one per account.
    fn new_device_keys_of_a_verified_contact_keep_it_verified() {
        let local = create_stage5_peer("mismatch-local", "Local", "Local PC");
        let remote = create_stage5_peer("mismatch-remote", "Remote", "Remote PC");
        let contact = link_stage5_contact(&local.app, &remote, "Remote");

        let verified = local
            .app
            .verify_contact(&contact.contact_id, VerificationMethod::ManualCompare)
            .unwrap();
        assert_eq!(verified.trust_state, TrustState::Verified);

        local
            .app
            .storage
            .record_remote_device_observation(
                &remote.account.descriptor.account_id,
                &remote.device.device_id,
                "ffffffffffffffff",
                current_unix_ms(),
            )
            .unwrap();

        let state = local
            .app
            .get_contact_verification_state(&contact.contact_id)
            .unwrap();
        assert_eq!(state.trust_state, TrustState::Verified);

        let summary = local
            .app
            .get_chat_security_summary(remote.dialog_key.as_str())
            .unwrap();
        assert_ne!(summary.warning, SecurityWarningKind::IdentityChanged);
    }

    #[test]
    fn chat_security_summary_does_not_report_pending_bootstrap_as_established() {
        let local = create_stage5_peer("pending-summary-local", "Local", "Local PC");
        let remote = create_stage5_peer("pending-summary-remote", "Remote", "Remote PC");
        link_stage5_contact(&local.app, &remote, "Remote");

        local
            .app
            .bootstrap_session_from_prekey_bundle(&remote.bundle)
            .unwrap();

        let summary = local
            .app
            .get_chat_security_summary(remote.dialog_key.as_str())
            .unwrap();

        assert_eq!(
            summary.session_state,
            Some(omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated)
        );
        assert!(!summary.has_established_session);
    }

    #[test]
    fn paused_chat_session_activation_does_not_clear_manual_end() {
        let local = create_stage5_peer("paused-activation-local", "Local", "Local PC");
        let remote = create_stage5_peer("paused-activation-remote", "Remote", "Remote PC");
        let contact = link_stage5_contact(&local.app, &remote, "Remote");
        let chat = local.app.start_chat_from_contact(&contact.contact_id).unwrap();

        local
            .app
            .open_chat_session_lease(chat.dialog_key.as_str())
            .unwrap();
        let ended = local.app.end_chat_session(chat.dialog_key.as_str()).unwrap();
        assert_eq!(ended.session_state, omni_types::ChatSessionState::Paused);
        assert!(ended.auto_reconnect_paused);

        let runtime_storage = Arc::new(Mutex::new(Storage::open(&local.db_path).unwrap()));
        runtime_storage.lock().unwrap().migrate().unwrap();
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:40125".to_string(),
            dialog_key: remote.dialog_key.clone(),
            remote_instance_id: remote.instance_id.clone(),
            remote_prekey_bundle: Some(remote.bundle.clone()),
            initiated_locally: false,
        };

        handle_session_activated(
            &runtime_storage,
            &local.app.transport_handle,
            &local.app.session_lock,
            &receive_context,
        )
        .unwrap();

        assert!(
            runtime_chat_session_auto_reconnect_paused(&local.db_path, chat.dialog_key.as_str())
                .unwrap()
        );
        let summary = local
            .app
            .open_chat_session_lease(chat.dialog_key.as_str())
            .unwrap();
        assert_eq!(summary.session_state, omni_types::ChatSessionState::Paused);
        assert!(summary.auto_reconnect_paused);
    }

    #[test]
    fn stale_confirmed_keepalive_marks_live_chat_session_reconnecting() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("stale-keepalive-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("stale-keepalive-bob", "Bob", "Bob PC");
        let alice_contact = link_stage5_contact(&alice.app, &bob, "Bob");

        link_stage5_peers_over_mesh(&alice, &bob);

        let chat = alice
            .app
            .start_chat_from_contact(&alice_contact.contact_id)
            .unwrap();
        alice
            .app
            .open_chat_session_lease(chat.dialog_key.as_str())
            .unwrap();
        set_runtime_chat_session_confirmed_keepalive_at(
            &alice.db_path,
            chat.dialog_key.as_str(),
            current_unix_ms(),
        )
        .unwrap();
        assert_eq!(
            alice
                .app
                .open_chat_session_lease(chat.dialog_key.as_str())
                .unwrap()
                .session_state,
            omni_types::ChatSessionState::Connected
        );

        set_runtime_chat_session_confirmed_keepalive_at(
            &alice.db_path,
            chat.dialog_key.as_str(),
            current_unix_ms().saturating_sub(60_000),
        )
        .unwrap();

        let summary = alice
            .app
            .open_chat_session_lease(chat.dialog_key.as_str())
            .unwrap();
        assert_eq!(
            summary.session_state,
            omni_types::ChatSessionState::Reconnecting
        );
    }

    #[test]
    fn ending_chat_session_pauses_the_remote_open_lease() {
        let _guard = network_test_guard();
        let client = create_stage5_peer("end-session-client", "Client", "Client PC");
        let server = create_stage5_peer("end-session-server", "Server", "Server PC");
        let client_contact = link_stage5_contact(&client.app, &server, "Server");
        let server_contact = server
            .app
            .storage
            .find_contact_by_remote_account_id(&client.account.descriptor.account_id)
            .unwrap()
            .expect("server should have the reciprocal client contact");

        link_stage5_peers_over_mesh(&client, &server);

        let client_chat = client
            .app
            .start_chat_from_contact(&client_contact.contact_id)
            .unwrap();
        let server_chat = server
            .app
            .start_chat_from_contact(&server_contact.contact_id)
            .unwrap();
        client
            .app
            .send_text(client_chat.dialog_key.as_str(), "before session end")
            .unwrap();
        wait_for_message_count(&server.app, server_chat.dialog_key.as_str(), 1);
        wait_for_message_status(
            &client.app,
            client_chat.dialog_key.as_str(),
            "before session end",
            MessageStatus::Delivered,
        );

        client
            .app
            .open_chat_session_lease(client_chat.dialog_key.as_str())
            .unwrap();
        let server_before = server
            .app
            .open_chat_session_lease(server_chat.dialog_key.as_str())
            .unwrap();
        assert_ne!(
            server_before.session_state,
            omni_types::ChatSessionState::Paused,
            "the remote lease is open until the client ends the session"
        );

        let ended = client
            .app
            .end_chat_session(client_chat.dialog_key.as_str())
            .unwrap();
        assert_eq!(ended.session_state, omni_types::ChatSessionState::Paused);

        wait_for_state(
            Duration::from_secs(5),
            || {
                let summary = server
                    .app
                    .open_chat_session_lease(server_chat.dialog_key.as_str());
                format!(
                    "server lease: {:?}; client link to server: {}",
                    summary.map(|summary| (summary.session_state, summary.auto_reconnect_paused)),
                    client
                        .app
                        .transport
                        .connection_snapshot_for_dialog(client_chat.dialog_key.as_str())
                        .is_some()
                )
            },
            || {
                server
                    .app
                    .open_chat_session_lease(server_chat.dialog_key.as_str())
                    .ok()
                    .is_some_and(|summary| {
                        summary.session_state == omni_types::ChatSessionState::Paused
                            && summary.auto_reconnect_paused
                    })
            },
        );

        let server_summary = server
            .app
            .open_chat_session_lease(server_chat.dialog_key.as_str())
            .unwrap();
        assert_eq!(
            server_summary.session_state,
            omni_types::ChatSessionState::Paused
        );
        // Ending the chat session pauses the lease; it does not tear the link down. On the pre-mesh
        // transport it also dropped the connection (no endpoint, contact offline). On the mesh every
        // approved contact stays linked (docs/decisions/no-friend-tier.md), so presence is the link's.
    }

    #[test]
    fn approved_simultaneous_bootstrap_collision_keeps_matching_session_roles() {
        let alice = create_stage5_peer("simultaneous-bootstrap-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("simultaneous-bootstrap-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");

        let alice_bootstrap = alice
            .app
            .bootstrap_session_from_prekey_bundle(&bob.bundle)
            .unwrap();
        let bob_bootstrap = bob
            .app
            .bootstrap_session_from_prekey_bundle(&alice.bundle)
            .unwrap();

        transport_accept_session_bootstrap(&alice.app.storage, &bob_bootstrap).unwrap();
        transport_accept_session_bootstrap(&bob.app.storage, &alice_bootstrap).unwrap();

        let alice_session = alice
            .app
            .storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: bob.account.descriptor.account_id.clone(),
                remote_device_id: bob.device.device_id.clone(),
            })
            .unwrap()
            .unwrap();
        let bob_session = bob
            .app
            .storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: alice.account.descriptor.account_id.clone(),
                remote_device_id: alice.device.device_id.clone(),
            })
            .unwrap()
            .unwrap();

        let alice_wins_collision = (
            alice.account.descriptor.account_id.0.as_str(),
            alice.device.device_id.0.as_str(),
        ) < (
            bob.account.descriptor.account_id.0.as_str(),
            bob.device.device_id.0.as_str(),
        );

        shutdown_stage5_peer(&alice);
        shutdown_stage5_peer(&bob);

        if alice_wins_collision {
            assert_eq!(
                alice_session.bootstrap_state,
                omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated
            );
            assert_eq!(
                bob_session.bootstrap_state,
                omni_crypto::SessionBootstrapState::Established
            );
        } else {
            assert_eq!(
                alice_session.bootstrap_state,
                omni_crypto::SessionBootstrapState::Established
            );
            assert_eq!(
                bob_session.bootstrap_state,
                omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated
            );
        }
    }

    fn write_test_transport_mode_sidecar(db_path: &str, transport_mode: omni_types::TransportMode) {
        let mode_value = transport_mode.as_db_value();
        let sidecar_path = transport_mode_sidecar_path(db_path);
        if let Some(parent) = sidecar_path.parent() {
            fs::create_dir_all(parent).expect("transport mode sidecar directory should exist");
        }
        fs::write(
            sidecar_path,
            format!("{{\"transport_mode\":\"{mode_value}\"}}"),
        )
        .expect("transport mode sidecar should be written");
    }

    /// Remote access (remote-access-for-contacts.md) is allowed only once the contact's key is
    /// verified. Negative half: an unverified contact is refused and nothing is stored; revoking
    /// needs no verification; after verifying, it goes through and is remembered.
    #[test]
    fn remote_access_waits_for_a_verified_contact() {
        let alice = create_stage5_peer("ra-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("ra-bob", "Bob", "Bob PC");
        let contact = link_stage5_contact_without_verification(&alice.app, &bob, "Bob");

        assert!(
            alice
                .app
                .set_contact_remote_access_granted(&contact.contact_id, Some(omni_types::RemoteAccessMode::Ask))
                .is_err(),
            "an unverified contact must not be granted remote access"
        );
        assert_eq!(
            alice.app.get_contact_remote_access(&contact.contact_id).unwrap().granted_mode,
            None,
            "a refused grant stores nothing"
        );
        // Revoking is always allowed, verified or not.
        assert!(alice.app.set_contact_remote_access_granted(&contact.contact_id, None).is_ok());

        alice
            .app
            .verify_contact(&contact.contact_id, VerificationMethod::ManualCompare)
            .unwrap();
        let granted = alice
            .app
            .set_contact_remote_access_granted(&contact.contact_id, Some(omni_types::RemoteAccessMode::Ask))
            .unwrap();
        assert_eq!(granted.granted_mode, Some(omni_types::RemoteAccessMode::Ask));
    }

    /// The owner's grant reaches the contact over the session (so it can show "Управлять"), and so
    /// does the revocation. Negative halves: before anything is granted the contact holds nothing;
    /// after the revocation it holds no access — a stale grant must not survive.
    #[test]
    fn remote_access_grant_and_revocation_reach_the_contact() {
        let _guard = network_test_guard();
        let (owner, viewer, approved, owner_at_viewer) = remote_access_pair("ra-send");
        let received = || viewer.app.get_contact_remote_access(&owner_at_viewer).unwrap().received_mode;
        assert_eq!(received(), None, "nothing granted yet, nothing received");

        owner
            .app
            .set_contact_remote_access_granted(&approved.contact_id, Some(omni_types::RemoteAccessMode::Unattended))
            .unwrap();
        wait_for_condition(Duration::from_secs(20), || {
            received() == Some(omni_types::RemoteAccessMode::Unattended)
        });

        owner.app.set_contact_remote_access_granted(&approved.contact_id, None).unwrap();
        wait_for_condition(Duration::from_secs(20), || received().is_none());
        assert!(
            viewer
                .app
                .get_contact_remote_access(&owner_at_viewer)
                .unwrap()
                .received_changed_at_unix_ms
                .is_some(),
            "the revocation is recorded, not merely absent"
        );
    }

    /// A revocation the contact never heard (it was offline) lands when the link comes back: the
    /// owner re-tells its grant state on every link. The revocation is written straight into the
    /// owner's storage while the link is down, so nothing but the on-link re-send can carry it.
    /// Negative half: until the link is back, the contact still holds the old grant.
    #[test]
    fn remote_access_revocation_made_offline_lands_on_relink() {
        let _guard = network_test_guard();
        let (owner, viewer, approved, owner_at_viewer) = remote_access_pair("ra-relink");
        let received = || viewer.app.get_contact_remote_access(&owner_at_viewer).unwrap().received_mode;
        owner
            .app
            .set_contact_remote_access_granted(&approved.contact_id, Some(omni_types::RemoteAccessMode::Ask))
            .unwrap();
        wait_for_condition(Duration::from_secs(20), || received() == Some(omni_types::RemoteAccessMode::Ask));

        let viewer_hex = viewer.app.mesh_node_address_hex().unwrap().expect("viewer mesh is up");
        viewer.app.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || {
            !owner.app.mesh_has_link(&viewer_hex).unwrap_or(true)
        });
        owner
            .app
            .storage
            .set_contact_remote_access_granted(&approved.contact_id, None, current_unix_ms())
            .unwrap();
        thread::sleep(Duration::from_secs(1));
        assert_eq!(
            received(),
            Some(omni_types::RemoteAccessMode::Ask),
            "offline, the contact cannot have heard the revocation"
        );

        link_apps_over_mesh(&viewer.app, &owner.app);
        wait_for_condition(Duration::from_secs(45), || received().is_none());
        viewer.app.stop_mesh();
        owner.app.stop_mesh();
    }

    fn current_call(app: &OmniApp) -> Option<CallSummary> {
        app.get_call_inbox_snapshot().ok().and_then(|inbox| inbox.current_call)
    }

    fn remote_control_of(call: &CallSummary) -> Option<(omni_types::RemoteControlRole, omni_types::RemoteControlPhase)> {
        call.remote_control.map(|session| (session.role, session.phase))
    }

    /// Owner granted the viewer `mode` and the viewer has heard it.
    fn remote_access_pair_granted(
        prefix: &str,
        mode: omni_types::RemoteAccessMode,
    ) -> (Stage5Peer, Stage5Peer, ContactSnapshot, ContactId) {
        let (owner, viewer, approved, owner_at_viewer) = remote_access_pair(prefix);
        owner.app.set_contact_remote_access_granted(&approved.contact_id, Some(mode)).unwrap();
        wait_for_condition(Duration::from_secs(20), || {
            viewer.app.get_contact_remote_access(&owner_at_viewer).unwrap().received_mode == Some(mode)
        });
        (owner, viewer, approved, owner_at_viewer)
    }

    /// Unattended access: the viewer's request goes live with no answer from the owner — both sides
    /// in an active call marked as a remote-control session, roles the right way round. Ending it
    /// leaves a chat line marked as a session. Negative half: the owner's core is the judge — once
    /// the grant is gone on the owner's side, a request from the viewer (who still believes it may)
    /// is refused, and the viewer is not left in a call.
    #[test]
    fn unattended_remote_control_goes_live_and_the_owner_decides_every_session() {
        use omni_types::{RemoteControlPhase as Phase, RemoteControlRole as Role};
        let _guard = network_test_guard();
        let (owner, viewer, approved, owner_at_viewer) =
            remote_access_pair_granted("rc-unattended", omni_types::RemoteAccessMode::Unattended);

        let placed = viewer.app.request_remote_control(&owner_at_viewer).unwrap();
        assert_eq!(remote_control_of(&placed), Some((Role::Controller, Phase::AwaitingOwner)));
        wait_for_condition(Duration::from_secs(20), || {
            current_call(&viewer.app).is_some_and(|call| {
                call.state == CallState::Active && remote_control_of(&call) == Some((Role::Controller, Phase::Approved))
            })
        });
        let at_owner = current_call(&owner.app).expect("the owner is in the session");
        assert_eq!(at_owner.state, CallState::Active, "no question asked");
        assert_eq!(remote_control_of(&at_owner), Some((Role::Owner, Phase::Approved)));
        assert!(
            owner.app.get_contact_remote_access(&approved.contact_id).unwrap().last_session_at_unix_ms.is_some(),
            "the session is on the owner's record"
        );

        // The owner's side reports why input may not land (here: locked) and the controller sees
        // it; nothing is reported before that, and a report without a grant goes nowhere.
        owner.app.report_call_screen_control_state(&at_owner.call_id, omni_types::ScreenControlSharerState::Locked).unwrap();
        thread::sleep(Duration::from_millis(500));
        assert_eq!(viewer.app.call_screen_control_state().peer_state, None, "no grant, no state");
        owner.app.set_call_screen_control(&at_owner.call_id, true).unwrap();
        wait_for_condition(Duration::from_secs(20), || viewer.app.call_screen_control_state().peer_grants_me);
        assert_eq!(viewer.app.call_screen_control_state().peer_state, None);
        owner.app.report_call_screen_control_state(&at_owner.call_id, omni_types::ScreenControlSharerState::Locked).unwrap();
        wait_for_condition(Duration::from_secs(20), || {
            viewer.app.call_screen_control_state().peer_state == Some(omni_types::ScreenControlSharerState::Locked)
        });
        assert!(viewer.app.call_screen_control_state().peer_grants_me, "a state report keeps the grant");

        owner.app.hang_up_call(&at_owner.call_id).unwrap();
        wait_for_condition(Duration::from_secs(20), || current_call(&viewer.app).is_none());
        let history = owner.app.list_messages(&at_owner.dialog_key).unwrap();
        assert!(
            history
                .messages
                .iter()
                .any(|message| message.call_history_event.as_ref().is_some_and(|event| event.remote_control)),
            "the chat line says it was a remote-control session"
        );

        // The owner takes the grant back; the viewer's view is stale until it hears that, so it
        // is the owner's core that has to refuse. Simulate "not heard yet" by revoking in storage.
        owner
            .app
            .storage
            .set_contact_remote_access_granted(&approved.contact_id, None, current_unix_ms())
            .unwrap();
        viewer.app.request_remote_control(&owner_at_viewer).unwrap();
        wait_for_condition(Duration::from_secs(20), || current_call(&viewer.app).is_none());
        assert!(current_call(&owner.app).is_none(), "the owner never entered a session");
    }

    /// "Ask" mode: the owner's side rings with a deadline and nothing is live until the owner
    /// answers; answering is approval on both sides. Negative half: a decline ends it on the
    /// viewer's side, and a viewer that was never granted cannot even place the request.
    #[test]
    fn asked_remote_control_waits_for_the_owner() {
        use omni_types::{RemoteControlPhase as Phase, RemoteControlRole as Role};
        let _guard = network_test_guard();
        let (owner, viewer, _approved, owner_at_viewer) =
            remote_access_pair_granted("rc-ask", omni_types::RemoteAccessMode::Ask);

        viewer.app.request_remote_control(&owner_at_viewer).unwrap();
        wait_for_condition(Duration::from_secs(20), || {
            current_call(&owner.app).is_some_and(|call| call.state == CallState::Ringing)
        });
        let ringing = current_call(&owner.app).unwrap();
        assert_eq!(remote_control_of(&ringing), Some((Role::Owner, Phase::AwaitingOwner)));
        assert!(ringing.remote_control.unwrap().answer_deadline_unix_ms.is_some(), "silence has a deadline");

        owner.app.decline_call(&ringing.call_id, omni_types::CallDeclineReason::Declined).unwrap();
        wait_for_condition(Duration::from_secs(20), || current_call(&viewer.app).is_none());

        viewer.app.request_remote_control(&owner_at_viewer).unwrap();
        wait_for_condition(Duration::from_secs(20), || {
            current_call(&owner.app).is_some_and(|call| call.state == CallState::Ringing)
        });
        let second = current_call(&owner.app).unwrap();
        let answered = owner.app.accept_call(&second.call_id).unwrap();
        assert_eq!(remote_control_of(&answered), Some((Role::Owner, Phase::Approved)));
        wait_for_condition(Duration::from_secs(20), || {
            current_call(&viewer.app)
                .is_some_and(|call| remote_control_of(&call) == Some((Role::Controller, Phase::Approved)))
        });

        // Revoking mid-session ends the session at once.
        let owner_contact = owner
            .app
            .storage
            .find_contact_by_remote_account_id(&viewer.account.descriptor.account_id)
            .unwrap()
            .unwrap();
        owner.app.set_contact_remote_access_granted(&owner_contact.contact_id, None).unwrap();
        assert!(current_call(&owner.app).is_none(), "revocation ends the owner's side immediately");
        wait_for_condition(Duration::from_secs(20), || current_call(&viewer.app).is_none());

        // Never granted (the owner's revocation reached the viewer): refused before any call.
        wait_for_condition(Duration::from_secs(20), || {
            viewer.app.get_contact_remote_access(&owner_at_viewer).unwrap().received_mode.is_none()
        });
        assert!(viewer.app.request_remote_control(&owner_at_viewer).is_err());
    }

    /// Owner and viewer as approved contacts over the mesh, the owner having verified the viewer.
    /// Returns (owner, viewer, the viewer's contact at the owner, the owner's contact at the viewer).
    fn remote_access_pair(prefix: &str) -> (Stage5Peer, Stage5Peer, ContactSnapshot, ContactId) {
        let owner = create_stage5_peer(&format!("{prefix}-owner"), "Bob", "Bob PC");
        let viewer = create_stage5_peer(&format!("{prefix}-viewer"), "Alice", "Alice PC");
        pair_stage5_over_mesh(&viewer, &owner);
        let pending = owner
            .app
            .storage
            .find_contact_by_remote_account_id(&viewer.account.descriptor.account_id)
            .unwrap()
            .expect("owner holds the viewer's request");
        let approved = owner.app.approve_contact(&pending.contact_id).unwrap();
        let owners_card_at_viewer = || {
            viewer
                .app
                .storage
                .find_contact_by_remote_account_id(&owner.account.descriptor.account_id)
                .ok()
                .flatten()
                .filter(|contact| contact.relationship_state == ContactRelationshipState::Approved)
        };
        wait_for_condition(Duration::from_secs(40), || owners_card_at_viewer().is_some());
        let owner_at_viewer = owners_card_at_viewer().unwrap().contact_id;
        owner
            .app
            .verify_contact(&approved.contact_id, VerificationMethod::ManualCompare)
            .unwrap();
        (owner, viewer, approved, owner_at_viewer)
    }

    fn create_stage5_peer(prefix: &str, display_name: &str, device_label: &str) -> Stage5Peer {
        create_stage5_peer_with_transport_mode(prefix, display_name, device_label, None)
    }

    fn create_stage5_peer_with_transport_mode(
        prefix: &str,
        display_name: &str,
        device_label: &str,
        transport_mode: Option<omni_types::TransportMode>,
    ) -> Stage5Peer {
        crate::sweep_stale_test_temp();
        let db_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("{prefix}-{}.db", unique_suffix()))
            .to_string_lossy()
            .into_owned();
        if let Some(transport_mode) = transport_mode {
            write_test_transport_mode_sidecar(&db_path, transport_mode);
        }

        let app = OmniApp::init(AppInitRequest {
            db_path: db_path.clone(),
        })
        .unwrap();
        let account = app.create_account(display_name, device_label).unwrap();
        if let Some(profile_summary) = account.local_profile_summary.as_ref() {
            app.select_active_profile(&profile_summary.profile_id)
                .unwrap();
        }
        let device = app.get_local_device().unwrap().unwrap();
        let bundle = app.get_local_prekey_bundle().unwrap();
        let instance_id = app.storage.get_or_create_local_instance_id().unwrap();
        let dialog_key = dialog_key_from_instance_id(&instance_id);

        Stage5Peer {
            db_path,
            app,
            account,
            device,
            bundle,
            instance_id,
            dialog_key,
        }
    }

    fn link_stage5_contact(local: &OmniApp, remote: &Stage5Peer, petname: &str) -> ContactSnapshot {
        local.store_remote_prekey_bundle(&remote.bundle).unwrap();
        let contact = local
            .add_contact(&AddContactInput {
                petname: petname.to_string(),
                remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                public_display_name: Some(remote.account.descriptor.display_name.clone()),
                remote_instance_id: Some(remote.instance_id.clone()),
                primary_dialog_key: Some(remote.dialog_key.clone()),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some(remote.account.descriptor.account_id.0.clone()),
                    observed_display_name: Some(remote.account.descriptor.display_name.clone()),
                    observed_at_unix_ms: current_unix_ms(),
                }),
            })
            .unwrap();

        local
            .verify_contact(&contact.contact_id, VerificationMethod::ChallengeResponse)
            .unwrap();

        let local_account = local
            .get_local_account()
            .unwrap()
            .expect("local account should exist");
        let local_instance_id = local
            .storage
            .get_or_create_local_instance_id()
            .unwrap();
        let local_dialog_key = dialog_key_from_instance_id(&local_instance_id);
        let local_bundle = local.get_local_prekey_bundle().unwrap();
        remote
            .app
            .store_remote_prekey_bundle(&local_bundle)
            .unwrap();
        let reciprocal = remote
            .app
            .add_contact(&AddContactInput {
                petname: local_account.descriptor.display_name.clone(),
                remote_account_id: Some(local_account.descriptor.account_id.clone()),
                public_display_name: Some(local_account.descriptor.display_name.clone()),
                remote_instance_id: Some(local_instance_id.clone()),
                primary_dialog_key: Some(local_dialog_key.clone()),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some(local_account.descriptor.account_id.0.clone()),
                    observed_display_name: Some(local_account.descriptor.display_name.clone()),
                    observed_at_unix_ms: current_unix_ms(),
                }),
            })
            .unwrap();
        remote
            .app
            .verify_contact(
                &reciprocal.contact_id,
                VerificationMethod::ChallengeResponse,
            )
            .unwrap();

        contact
    }

    fn link_stage5_contact_without_verification(
        local: &OmniApp,
        remote: &Stage5Peer,
        petname: &str,
    ) -> ContactSnapshot {
        local.store_remote_prekey_bundle(&remote.bundle).unwrap();
        local
            .add_contact(&AddContactInput {
                petname: petname.to_string(),
                remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                public_display_name: Some(remote.account.descriptor.display_name.clone()),
                remote_instance_id: Some(remote.instance_id.clone()),
                primary_dialog_key: Some(remote.dialog_key.clone()),
                initial_observation: Some(ContactObservationSnapshot {
                    identity_key: Some(remote.account.descriptor.account_id.0.clone()),
                    observed_display_name: Some(remote.account.descriptor.display_name.clone()),
                    observed_at_unix_ms: current_unix_ms(),
                }),
            })
            .unwrap()
    }

    /// The two-node stand of the product path: start both meshes and dial `b` from `a` at `b`'s
    /// loopback mesh address, then wait until BOTH sides hold the verified link. Dialing at the
    /// address rather than waiting for multicast keeps the stand usable on a host whose multicast is
    /// dead (a VPN/TUN adapter). Replaces the pre-mesh QUIC listener/connect pair the older tests
    /// stood on (docs/decisions/no-friend-tier.md, part 2).
    fn link_stage5_peers_over_mesh(a: &Stage5Peer, b: &Stage5Peer) {
        link_apps_over_mesh(&a.app, &b.app);
    }

    /// `link_stage5_peers_over_mesh` for bare apps — e.g. one reopened after a restart.
    fn link_apps_over_mesh(a: &OmniApp, b: &OmniApp) {
        for app in [a, b] {
            let running = app.mesh.lock().unwrap().is_some();
            if !running {
                app.start_mesh().expect("mesh starts");
            }
        }
        let a_hex = a.mesh_node_address_hex().unwrap().expect("a mesh is up");
        let b_hex = b.mesh_node_address_hex().unwrap().expect("b mesh is up");
        let b_addr = b
            .mesh
            .lock()
            .unwrap()
            .as_ref()
            .expect("b mesh is up")
            .quic_addr()
            .to_string();
        wait_for_state(
            Duration::from_secs(20),
            || {
                format!(
                    "a->b link {}, b->a link {}",
                    a.mesh_has_link(&b_hex).unwrap_or(false),
                    b.mesh_has_link(&a_hex).unwrap_or(false)
                )
            },
            || {
                if !a.mesh_has_link(&b_hex).unwrap_or(false) {
                    // Bounded: a plain dial_at sits out a whole handshake timeout when its first
                    // attempt is lost, and this wait then polled once in 20 s and failed (gate
                    // 2026-10-13, a UPnP-less network). A short try is retried on the next poll.
                    let node = omni_mesh_runtime::MeshNodeAddress::from_bytes(
                        hex::decode(&b_hex).unwrap().try_into().unwrap(),
                    );
                    if let Some(runtime) = a.mesh_runtime() {
                        let _ = runtime.dial_at_within(node, vec![b_addr.parse().unwrap()], Duration::from_secs(3));
                    }
                }
                a.mesh_has_link(&b_hex).unwrap_or(false) && b.mesh_has_link(&a_hex).unwrap_or(false)
            },
        );
    }

    /// First contact the way the product makes it: `requester` imports `receiver`'s mesh pairing
    /// invite and links to it (dialed at the receiver's loopback mesh address, so multicast is not
    /// needed). The pairing handler sends the X3DH bootstrap — the contact request — so on return
    /// the receiver holds an INCOMING-pending contact for the requester and an established session.
    fn pair_stage5_over_mesh(requester: &Stage5Peer, receiver: &Stage5Peer) {
        for peer in [requester, receiver] {
            if peer.app.mesh.lock().unwrap().is_none() {
                peer.app.start_mesh().expect("mesh starts");
            }
        }
        let invite = receiver
            .app
            .generate_mesh_pairing_invite()
            .expect("receiver generates a pairing invite");
        requester
            .app
            .register_mesh_pairing_invite(&invite)
            .expect("requester imports the invite");
        link_apps_over_mesh(&requester.app, &receiver.app);
        wait_for_condition(Duration::from_secs(30), || {
            receiver
                .app
                .storage
                .find_contact_by_remote_account_id(&requester.account.descriptor.account_id)
                .ok()
                .flatten()
                .is_some_and(|contact| {
                    contact.relationship_state == ContactRelationshipState::Pending
                        && contact.pending_request_direction
                            == Some(omni_identity::ContactRequestDirection::Incoming)
                })
        });
    }

    /// Sessions are made lazily, by the first frame. Send one `from` → `to` and wait for its receipt,
    /// so both sides hold an established session — what a contact that has ever talked has.
    fn establish_stage5_session(from: &Stage5Peer, to: &Stage5Peer) {
        let body = format!("session primer {}", unique_suffix());
        from.app.send_text(to.dialog_key.as_str(), &body).expect("primer sends");
        wait_for_message_status(&from.app, to.dialog_key.as_str(), &body, MessageStatus::Delivered);
    }

    fn stop_stage5_meshes(peers: &[&Stage5Peer]) {
        for peer in peers {
            peer.app.stop_mesh();
        }
    }

    #[test]
    fn get_contact_verification_state_returns_existing_card_snapshot() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("verification-state-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact_by_identifier("acct-remote", "Bob", Some("Bob".to_string()))
            .unwrap();

        let state = app
            .get_contact_verification_state(&contact.contact_id)
            .unwrap();

        assert_eq!(state.contact.contact_id, contact.contact_id);
        assert_eq!(state.trust_state, TrustState::Unverified);
    }

    #[test]
    fn create_chat_from_contact_returns_stable_dialog_target() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("contact-chat-target-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Alice".to_string(),
                remote_account_id: Some(AccountId("acct-alice".to_string())),
                public_display_name: Some("Alice".to_string()),
                remote_instance_id: Some("inst-alice".to_string()),
                primary_dialog_key: None,
                initial_observation: None,
            })
            .unwrap();

        let target = app.start_chat_from_contact(&contact.contact_id).unwrap();
        assert_eq!(target.contact_id, contact.contact_id);
        assert_eq!(target.dialog_key, dialog_key_from_instance_id("inst-alice"));

        let card = app.get_contact_card(&contact.contact_id).unwrap();
        assert_eq!(
            card.contact.primary_dialog_key,
            Some(dialog_key_from_instance_id("inst-alice"))
        );
    }

    #[test]
    fn session_activation_repairs_placeholder_instance_and_canonicalizes_account_dialog_selector() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("session-activation-repair-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let remote = create_stage5_peer("repair-bind-remote", "Bob", "Bob PC");
        let account_dialog_key =
            omni_types::dialog_key_from_account_id(&remote.account.descriptor.account_id.0);

        let contact = app
            .add_contact(
                &AddContactInput {
                    petname: "Боб".to_string(),
                    remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                    public_display_name: Some(remote.account.descriptor.display_name.clone()),
                    remote_instance_id: Some(remote.device.device_id.0.clone()),
                    primary_dialog_key: Some(account_dialog_key.clone()),
                    initial_observation: None,
                },
            )
            .unwrap();

        let queued_before_bind = app
            .send_text(&account_dialog_key, "queued before first bind")
            .unwrap();
        assert_eq!(queued_before_bind.dialog_key, account_dialog_key);

        let runtime_storage = Arc::new(Mutex::new(Storage::open(&db_path).unwrap()));
        runtime_storage.lock().unwrap().migrate().unwrap();
        let transport_handle = Arc::new(Mutex::new(None));
        let session_lock = Arc::new(Mutex::new(()));
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:40123".to_string(),
            dialog_key: remote.dialog_key.clone(),
            remote_instance_id: remote.instance_id.clone(),
            remote_prekey_bundle: Some(remote.bundle.clone()),
            initiated_locally: false,
        };

        handle_session_activated(
            &runtime_storage,
            &transport_handle,
            &session_lock,
            &receive_context,
        )
        .unwrap();

        let repaired = app
            .storage
            .get_contact(&contact.contact_id)
            .expect("contact should remain accessible");
        assert_eq!(
            repaired.remote_instance_id.as_deref(),
            Some(remote.instance_id.as_str())
        );
        assert_eq!(
            repaired.primary_dialog_key.as_deref(),
            Some(remote.dialog_key.as_str())
        );

        let rebound_history = app.list_messages(&account_dialog_key).unwrap();
        assert_eq!(rebound_history.dialog_key, remote.dialog_key);
        assert_eq!(rebound_history.messages.len(), 1);
        assert_eq!(rebound_history.messages[0].id, queued_before_bind.id);

        let header = app.get_chat_header_contact_summary(&account_dialog_key).unwrap();
        assert_eq!(header.dialog_key, remote.dialog_key);
        assert_eq!(
            header.contact_card.unwrap().contact.remote_instance_id.as_deref(),
            Some(remote.instance_id.as_str())
        );

        let queued_after_bind = app.send_text(&account_dialog_key, "queued after bind").unwrap();
        assert_eq!(queued_after_bind.dialog_key, remote.dialog_key);
    }

    #[test]
    fn incoming_contact_request_signal_auto_approves_existing_outgoing_pending() {
        let local = create_stage5_peer(
            "explicit-route-crossed-pending-local",
            "Alice",
            "Alice PC",
        );
        let remote = create_stage5_peer(
            "explicit-route-crossed-pending-remote",
            "Bob",
            "Bob PC",
        );
        let now = current_unix_ms();
        let outgoing = local
            .app
            .storage
            .add_outgoing_pending_contact(
                &AddContactInput {
                    petname: remote.account.descriptor.account_id.0.clone(),
                    remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                    public_display_name: Some(remote.account.descriptor.display_name.clone()),
                    remote_instance_id: Some(remote.instance_id.clone()),
                    primary_dialog_key: Some(remote.dialog_key.clone()),
                    initial_observation: None,
                },
                now,
            )
            .unwrap();
        assert_eq!(
            outgoing.pending_request_direction,
            Some(ContactRequestDirection::Outgoing)
        );

        let receive_context = ReceivedChatContext {
            peer_id: "198.51.100.234:54452".to_string(),
            dialog_key: remote.dialog_key.clone(),
            remote_instance_id: remote.instance_id.clone(),
            remote_prekey_bundle: Some(remote.bundle.clone()),
            initiated_locally: false,
        };
        let updated = persist_incoming_contact_request_signal(
            &local.app.storage,
            &receive_context,
            now + 1,
        )
        .unwrap()
        .expect("incoming request signal should update the existing contact");

        assert_eq!(updated.contact_id, outgoing.contact_id);
        assert_eq!(updated.relationship_state, ContactRelationshipState::Approved);
        assert_eq!(updated.pending_request_direction, None);
    }

    #[test]
    fn relay_set_update_live_channel_applies_for_approved_first_party_only() {
        // R1c live-channel propagation (primary path): an approved, linked friend pushes their signed
        // relay set over the live mesh/transport channel and the receiver auto-learns it. Gated like
        // RouteHintsSync: a Pending contact is dropped; an Approved first-party bundle applies; a
        // newer version replaces (strict-newer); a bundle whose issuer != the sender (transitive
        // re-share attempt) is rejected. See docs/decisions/r1c-rotatable-white-relay.md §B.
        let receiver = create_stage5_peer("relay-live-recv", "Bob", "Bob PC");
        let alice = create_stage5_peer("relay-live-alice", "Alice", "Alice PC");
        let mallory = create_stage5_peer("relay-live-mallory", "Mallory", "Mallory PC");

        // The receiver must hold Alice's prekey bundle so it can derive Alice's mesh node (the
        // first-party expected-issuer); a real approved pair always has it (learned at bootstrap).
        receiver.app.store_remote_prekey_bundle(&alice.bundle).unwrap();
        let now = current_unix_ms();
        let pending = receiver
            .app
            .storage
            .add_outgoing_pending_contact(
                &AddContactInput {
                    petname: "Alice".to_string(),
                    remote_account_id: Some(alice.account.descriptor.account_id.clone()),
                    public_display_name: None,
                    remote_instance_id: Some(alice.instance_id.clone()),
                    primary_dialog_key: Some(alice.dialog_key.clone()),
                    initial_observation: None,
                },
                now,
            )
            .unwrap();

        let ctx = ReceivedChatContext {
            peer_id: "127.0.0.1:0".to_string(),
            dialog_key: alice.dialog_key.clone(),
            remote_instance_id: alice.instance_id.clone(),
            remote_prekey_bundle: Some(alice.bundle.clone()),
            initiated_locally: false,
        };
        let update_for = |token: String| omni_types::RelaySetUpdateV1 {
            version: omni_types::RELAY_SET_UPDATE_VERSION_V1,
            bundle_token: token,
            exported_at_unix_ms: current_unix_ms(),
        };
        let tagged_relays = |peer: &Stage5Peer| {
            peer.app
                .storage
                .list_network_nodes()
                .unwrap()
                .into_iter()
                .filter(|n| n.bundle_id.is_some())
                .collect::<Vec<_>>()
        };

        // Alice signs her relay bundle v1.
        let token_v1 = alice
            .app
            .generate_relay_bundle(
                1,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.1:443".to_string()),
                    tcp_endpoint: Some("203.0.113.1:8443".to_string()),
                    label: "A-relay".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();

        // Negative: contact still Pending → the relay set is dropped (approved-only gate).
        persist_relay_set_update(&receiver.app.storage, &update_for(token_v1.clone()), &ctx)
            .expect("drops quietly while Pending");
        assert!(tagged_relays(&receiver).is_empty(), "a Pending contact's set must not apply");

        // Approve Alice → the same update now applies (first-party: issuer == Alice's node).
        receiver
            .app
            .storage
            .approve_contact(&pending.contact_id, current_unix_ms())
            .unwrap();
        persist_relay_set_update(&receiver.app.storage, &update_for(token_v1), &ctx)
            .expect("applies for approved first-party");
        let after_v1 = tagged_relays(&receiver);
        assert_eq!(after_v1.len(), 1, "receiver auto-learned Alice's relay over the live channel");
        assert_eq!(after_v1[0].endpoint.as_deref(), Some("203.0.113.1:443"));

        // Rotation: Alice publishes v2 (a different relay) → strict-newer replaces the set.
        let token_v2 = alice
            .app
            .generate_relay_bundle(
                2,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("203.0.113.9:443".to_string()),
                    label: "A-relay-rotated".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();
        persist_relay_set_update(&receiver.app.storage, &update_for(token_v2), &ctx)
            .expect("applies the rotated set");
        let after_v2 = tagged_relays(&receiver);
        assert_eq!(after_v2.len(), 1, "rotation replaced the prior set");
        assert_eq!(after_v2[0].endpoint.as_deref(), Some("203.0.113.9:443"));

        // Mis-attributed: a bundle signed by Mallory arriving over Alice's session → rejected
        // (first-party — a contact may propagate only its OWN set; no transitive re-share).
        let mallory_token = mallory
            .app
            .generate_relay_bundle(
                5,
                vec![crate::RelayBundleEntryInput {
                    endpoint: Some("198.51.100.9:443".to_string()),
                    label: "M-relay".to_string(),
                    ..Default::default()
                }],
            )
            .unwrap();
        persist_relay_set_update(&receiver.app.storage, &update_for(mallory_token), &ctx)
            .expect("rejects quietly");
        let after_spoof = tagged_relays(&receiver);
        assert_eq!(after_spoof.len(), 1, "a foreign-issuer bundle over Alice's session is rejected");
        assert!(after_spoof
            .iter()
            .all(|n| n.endpoint.as_deref() != Some("198.51.100.9:443")));
    }

    // TM-006 follow-up (2026-05-21): entry-point regression tests for
    // the two omni-app sites the storage helper now gates. Helper-level
    // coverage (Windows system path, drive root, relative path,
    // temp-dir positive) lives in omni-storage/src/tests.rs. These
    // ensure the gates fire at the public API surface before any
    // file-system side effect happens.
    // See docs/decisions/tm-006-destructive-path-confinement.md.

    #[test]
    #[cfg(windows)]
    fn tm006_export_database_backup_rejects_system_path() {
        let peer = create_stage5_peer("tm006-backup-export-local", "Alice", "Alice PC");
        let result = peer
            .app
            .export_current_database_backup_bundle(Path::new(
                r"C:\Windows\System32\omni-test-backup.db",
            ));
        assert!(
            result.is_err(),
            "TM-006 leak: export_database_backup accepted a Windows system path"
        );
        let message = format!("{}", result.unwrap_err());
        assert!(
            message.contains("protected system directory"),
            "unexpected error message: {message}"
        );
        // Sanity: the file was never created, no deletes happened.
        assert!(!Path::new(r"C:\Windows\System32\omni-test-backup.db").exists());
    }

    #[test]
    #[cfg(windows)]
    fn tm006_request_attachment_download_rejects_system_save_path() {
        // Two peers, mutual contact, but for this test we only need
        // the requester to attempt a download with a hostile
        // save_path. We pass a path under a protected system
        // directory; the gate must fire before any DB state changes.
        let server = create_stage5_peer("tm006-attach-server", "Server", "Server PC");
        let client = create_stage5_peer("tm006-attach-client", "Client", "Client PC");
        link_stage5_contact(&client.app, &server, "Server");
        // Fabricate a message id; the gate runs before the message
        // lookup, so any non-empty id triggers the path check first.
        let result = client.app.request_attachment_download(
            "msg-tm006-test",
            r"C:\Windows\System32\stolen-attachment.bin",
        );
        assert!(
            result.is_err(),
            "TM-006 leak: request_attachment_download accepted a Windows system save_path"
        );
        let message = format!("{}", result.unwrap_err());
        assert!(
            message.contains("protected system directory"),
            "unexpected error message: {message}"
        );
    }

    // TM-008 (2026-05-21): sliding-window rate limit on
    // auto-created Pending contacts. Helper-level coverage: 32
    // creations per 60-minute window per profile, drop after that,
    // recover after window scrolls, isolate per db_path. The
    // production caller (`upsert_transport_pending_contact`) gates
    // on the same helper so these tests exercise the actual
    // mitigation. Each test cleans up its registry entry to keep the
    // static state isolated.
    // See docs/decisions/tm-008-pending-contact-rate-limit.md.

    #[test]
    fn tm008_rate_limit_admits_under_threshold() {
        let key = format!("tm008-under-{}", unique_suffix());
        let base_now = 1_000_000_000_i64;
        for i in 0..32 {
            let admitted = record_pending_contact_creation_under_rate_limit(
                &key,
                base_now + i64::from(i),
            );
            assert!(admitted, "32nd-and-under entries must be admitted, i={i}");
        }
        clear_pending_contact_rate_limits_for_profile(&key);
    }

    #[test]
    fn tm008_rate_limit_drops_at_threshold() {
        let key = format!("tm008-drop-{}", unique_suffix());
        let base_now = 2_000_000_000_i64;
        for i in 0..32 {
            assert!(record_pending_contact_creation_under_rate_limit(
                &key,
                base_now + i64::from(i),
            ));
        }
        // 33rd within the same window: dropped.
        let denied = record_pending_contact_creation_under_rate_limit(&key, base_now + 32);
        assert!(
            !denied,
            "TM-008 leak: 33rd pending creation within the window was admitted"
        );
        clear_pending_contact_rate_limits_for_profile(&key);
    }

    #[test]
    fn tm008_rate_limit_recovers_after_window() {
        let key = format!("tm008-window-{}", unique_suffix());
        let base_now = 3_000_000_000_i64;
        for i in 0..32 {
            assert!(record_pending_contact_creation_under_rate_limit(
                &key,
                base_now + i64::from(i),
            ));
        }
        // Just past the 60-minute window from the LAST recorded
        // timestamp — all earlier entries should be pruned and a
        // fresh attempt admitted.
        let last_in_burst = base_now + 31;
        let after_window = last_in_burst + 60 * 60 * 1000 + 1;
        let admitted = record_pending_contact_creation_under_rate_limit(&key, after_window);
        assert!(
            admitted,
            "TM-008 false positive: rate limit failed to recover after window expired"
        );
        clear_pending_contact_rate_limits_for_profile(&key);
    }

    #[test]
    fn tm008_rate_limit_is_per_profile() {
        let key_a = format!("tm008-iso-a-{}", unique_suffix());
        let key_b = format!("tm008-iso-b-{}", unique_suffix());
        let base_now = 4_000_000_000_i64;
        // Exhaust profile A's quota.
        for i in 0..32 {
            assert!(record_pending_contact_creation_under_rate_limit(
                &key_a,
                base_now + i64::from(i),
            ));
        }
        assert!(!record_pending_contact_creation_under_rate_limit(&key_a, base_now + 32));
        // Profile B starts fresh — its first 32 must all be admitted
        // despite profile A being saturated.
        for i in 0..32 {
            assert!(record_pending_contact_creation_under_rate_limit(
                &key_b,
                base_now + i64::from(i),
            ));
        }
        clear_pending_contact_rate_limits_for_profile(&key_a);
        clear_pending_contact_rate_limits_for_profile(&key_b);
    }

    #[test]
    fn redundant_inbound_session_close_keeps_outgoing_pending_request() {
        let local = create_stage5_peer(
            "explicit-route-outgoing-pending-close-local",
            "Alice",
            "Alice PC",
        );
        let remote = create_stage5_peer(
            "explicit-route-outgoing-pending-close-remote",
            "Bob",
            "Bob PC",
        );
        let now = current_unix_ms();
        let outgoing = local
            .app
            .storage
            .add_outgoing_pending_contact(
                &AddContactInput {
                    petname: remote.account.descriptor.account_id.0.clone(),
                    remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                    public_display_name: Some(remote.account.descriptor.display_name.clone()),
                    remote_instance_id: Some(remote.instance_id.clone()),
                    primary_dialog_key: Some(remote.dialog_key.clone()),
                    initial_observation: None,
                },
                now,
            )
            .unwrap();

        let runtime_storage = Arc::new(Mutex::new(Storage::open(&local.db_path).unwrap()));
        runtime_storage.lock().unwrap().migrate().unwrap();
        let transport_handle = Arc::new(Mutex::new(None));
        let receive_context = ReceivedChatContext {
            peer_id: "198.51.100.234:54452".to_string(),
            dialog_key: remote.dialog_key.clone(),
            remote_instance_id: remote.instance_id.clone(),
            remote_prekey_bundle: Some(remote.bundle.clone()),
            initiated_locally: false,
        };

        handle_session_closed(&runtime_storage, &transport_handle, &receive_context).unwrap();

        let still_pending = local
            .app
            .storage
            .find_contact_by_remote_account_id(&remote.account.descriptor.account_id)
            .unwrap()
            .expect("outgoing pending request must survive a redundant inbound close");
        assert_eq!(still_pending.contact_id, outgoing.contact_id);
        assert_eq!(
            still_pending.relationship_state,
            ContactRelationshipState::Pending
        );
        assert_eq!(
            still_pending.pending_request_direction,
            Some(ContactRequestDirection::Outgoing)
        );
    }

    #[test]
    fn session_activation_ignores_exact_self_context() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("session-activation-self-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let local_bundle = app.get_local_prekey_bundle().unwrap();
        let local_instance_id = app.storage.get_or_create_local_instance_id().unwrap();

        let runtime_storage = Arc::new(Mutex::new(Storage::open(&db_path).unwrap()));
        runtime_storage.lock().unwrap().migrate().unwrap();
        let transport_handle = Arc::new(Mutex::new(None));
        let session_lock = Arc::new(Mutex::new(()));
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:40124".to_string(),
            dialog_key: dialog_key_from_instance_id(&local_instance_id),
            remote_instance_id: local_instance_id.clone(),
            remote_prekey_bundle: Some(local_bundle),
            initiated_locally: false,
        };

        handle_session_activated(
            &runtime_storage,
            &transport_handle,
            &session_lock,
            &receive_context,
        )
        .unwrap();

        assert!(
            app.list_contacts().unwrap().is_empty(),
            "self-session activation must not create transport-derived contacts"
        );
    }

    #[test]
    fn transport_contact_resolution_prefers_receive_context_identity_over_local_fallback() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-10-app-tests")
            .join(format!("transport-contact-resolution-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );
        let local_account = app
            .get_local_account()
            .unwrap()
            .expect("local account should exist");
        let local_instance_id = app.storage.get_or_create_local_instance_id().unwrap();
        let remote = create_stage5_peer("transport-resolution-remote", "Bob", "Bob PC");

        let wrong_local_contact = app
            .add_contact(&AddContactInput {
                petname: "Я".to_string(),
                remote_account_id: Some(local_account.descriptor.account_id.clone()),
                public_display_name: Some(local_account.descriptor.display_name.clone()),
                remote_instance_id: Some(local_instance_id),
                primary_dialog_key: Some(remote.dialog_key.clone()),
                initial_observation: None,
            })
            .unwrap();
        let expected_remote_contact = app
            .storage
            .add_pending_contact(
                &AddContactInput {
                    petname: "Боб".to_string(),
                    remote_account_id: Some(remote.account.descriptor.account_id.clone()),
                    public_display_name: Some(remote.account.descriptor.display_name.clone()),
                    remote_instance_id: Some(remote.device.device_id.0.clone()),
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                current_unix_ms(),
            )
            .unwrap();
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:40125".to_string(),
            dialog_key: remote.dialog_key.clone(),
            remote_instance_id: remote.instance_id.clone(),
            remote_prekey_bundle: Some(remote.bundle.clone()),
            initiated_locally: false,
        };

        let resolved =
            resolve_transport_contact_for_receive_context(&app.storage, &receive_context)
                .unwrap()
                .expect("remote contact should resolve from the receive context");

        assert_eq!(resolved.contact_id, expected_remote_contact.contact_id);
        assert_ne!(resolved.contact_id, wrong_local_contact.contact_id);
        assert_eq!(
            resolved.remote_account_id.as_ref(),
            Some(&remote.account.descriptor.account_id)
        );
    }

    #[test]
    fn trust_state_appears_in_chat_header_projection() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("chat-header-trust-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let contact = app
            .add_contact(&AddContactInput {
                petname: "Bob".to_string(),
                remote_account_id: Some(AccountId("acct-bob".to_string())),
                public_display_name: Some("Bob".to_string()),
                remote_instance_id: Some("inst-bob".to_string()),
                primary_dialog_key: Some(dialog_key_from_instance_id("inst-bob")),
                initial_observation: None,
            })
            .unwrap();

        app.record_contact_observation(
            &contact.contact_id,
            &ContactObservationSnapshot {
                identity_key: Some("acct-bob".to_string()),
                observed_display_name: Some("Bob".to_string()),
                observed_at_unix_ms: 100,
            },
        )
        .unwrap();

        let payload = ContactVerificationPayloadV1 {
            version: CONTACT_VERIFICATION_PAYLOAD_VERSION_V1,
            account: AccountPublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                account_id: "acct-bob".to_string(),
                display_name: "Bob".to_string(),
            },
            devices: vec![DevicePublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                device_id: "dev-bob-1".to_string(),
                account_id: "acct-bob".to_string(),
                device_label: "Bob Laptop".to_string(),
            }],
            fingerprint: "fp-bob".to_string(),
            safety_number: "1111 2222 3333".to_string(),
            exported_at_unix_ms: 100,
        };

        let verified = app
            .confirm_contact_verification(
                &contact.contact_id,
                &payload,
                VerificationMethod::ManualCompare,
            )
            .unwrap();
        assert_eq!(verified.trust_state, TrustState::Verified);

        let header = app
            .get_chat_header_contact_summary(dialog_key_from_instance_id("inst-bob"))
            .unwrap();
        assert_eq!(header.title, "Bob");
        assert_eq!(header.contact_card.unwrap().trust_state, TrustState::Verified);
    }

    #[test]
    fn legacy_dialogs_without_contacts_are_rejected() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("legacy-dialog-usable-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let error = app
            .send_text("127.0.0.1:9000", "legacy body")
            .expect_err("legacy dialog without contact should be rejected");
        assert!(
            error
                .to_string()
                .contains("contact must be approved before sending messages")
        );
    }

    #[test]
    fn removing_or_blocking_contact_does_not_corrupt_existing_dialog_history() {
        let db_path = std::env::temp_dir()
            .join("omni-stage-4-app-tests")
            .join(format!("remove-block-history-{}.db", unique_suffix()));

        let app = create_app_with_active_profile(
            db_path.to_string_lossy().into_owned(),
            "Alice",
            "Office PC",
        );

        let dialog_key = dialog_key_from_instance_id("inst-contact");
        let contact = app
            .add_contact(&AddContactInput {
                petname: "Contact".to_string(),
                remote_account_id: Some(AccountId("acct-contact".to_string())),
                public_display_name: Some("Contact".to_string()),
                remote_instance_id: Some("inst-contact".to_string()),
                primary_dialog_key: Some(dialog_key.clone()),
                initial_observation: None,
            })
            .unwrap();

        let queued = app.send_text(&dialog_key, "message keeps history").unwrap();
        assert_eq!(queued.status, MessageStatus::Queued);

        let blocked = app.block_contact(&contact.contact_id).unwrap();
        assert!(blocked.is_blocked);

        let history_after_block = app.list_messages(&dialog_key).unwrap();
        assert_eq!(history_after_block.messages.len(), 1);

        let removed = app.forget_contact(&contact.contact_id).unwrap();
        assert!(removed.removed_at_unix_ms.is_some());

        let history = app.list_messages(&dialog_key).unwrap();
        assert!(history.messages.is_empty());
    }

    #[test]
    fn platform_neutral_device_label_fallback_and_override_work() {
        let fallback = choose_local_device_name(None, None, None);
        assert_eq!(fallback, "Omni Device");

        let host_based = choose_local_device_name(None, Some("DESKTOP-01"), Some("host-01"));
        assert_eq!(host_based, "DESKTOP-01");

        let override_based = choose_local_device_name(
            Some("  Android Companion  "),
            Some("WINDOWS-NAME"),
            Some("HOST-NAME"),
        );
        assert_eq!(override_based, "Android Companion");
    }

    fn wait_for_message_count(app: &OmniApp, peer_addr_or_key: &str, expected_count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);

        while Instant::now() < deadline {
            let history = app
                .list_messages(peer_addr_or_key)
                .expect("history should load while polling");
            if history.messages.len() == expected_count {
                return;
            }

            thread::sleep(Duration::from_millis(25));
        }

        panic!(
            "message history for '{}' did not reach expected count {}",
            peer_addr_or_key, expected_count
        );
    }

    fn wait_for_message_status(
        app: &OmniApp,
        peer_addr_or_key: &str,
        body: &str,
        expected_status: MessageStatus,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);

        while Instant::now() < deadline {
            let history = app
                .list_messages(peer_addr_or_key)
                .expect("history should load while polling");
            if history
                .messages
                .iter()
                .any(|message| message.body == body && message.status == expected_status)
            {
                return;
            }

            thread::sleep(Duration::from_millis(25));
        }

        panic!(
            "message '{}' for '{}' did not reach status {:?}",
            body, peer_addr_or_key, expected_status
        );
    }

    fn wait_for_contact_verification_state(
        app: &OmniApp,
        contact_id: &ContactId,
        expected_state: TrustState,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            let state = app
                .get_contact_verification_state(contact_id)
                .expect("verification state should load while polling");
            if state.trust_state == expected_state {
                return;
            }

            thread::sleep(Duration::from_millis(25));
        }

        panic!(
            "contact '{}' did not reach trust state {:?}",
            contact_id.0, expected_state
        );
    }

    #[track_caller]
    fn wait_for_condition(timeout: Duration, condition: impl Fn() -> bool) {
        wait_for_state(timeout, || String::from("(no state described)"), condition)
    }

    /// A sleep that overshoots by more than this multiple means the host, not the code, decided the
    /// outcome. Four is deliberately forgiving: Windows timer granularity alone can double a 25 ms
    /// sleep, and calling that starvation would relabel real failures as environmental — the exact
    /// mistake this is meant to stop.
    const SCHEDULING_STARVATION_FACTOR: u32 = 4;

    /// Turn the worst observed poll sleep into the sentence a reader needs. Split out of
    /// `wait_for_state` so it can be tested for both verdicts without having to actually starve a
    /// machine, and so the timing test cannot itself flake under load.
    fn describe_scheduling(poll: Duration, worst_sleep: Duration) -> String {
        if worst_sleep > poll * SCHEDULING_STARVATION_FACTOR {
            format!(
                "HOST STARVATION: a {poll:?} sleep took up to {worst_sleep:?} — the box was busy, \
                 so this budget says nothing about the code under test"
            )
        } else {
            format!(
                "scheduling was healthy (worst {poll:?} sleep took {worst_sleep:?}), so the state \
                 genuinely never advanced — treat this as a real failure, not a flake"
            )
        }
    }

    #[test]
    fn a_missed_wait_says_whether_the_host_or_the_code_is_to_blame() {
        // The whole point of the flake work: "condition was not satisfied within 10s" is compatible
        // with both stories, and seven entries in the known-flaky table were filed under the wrong
        // one because nothing distinguished them.
        let poll = Duration::from_millis(25);

        let healthy = describe_scheduling(poll, Duration::from_millis(30));
        assert!(healthy.contains("genuinely never advanced"), "{healthy}");
        assert!(!healthy.contains("STARVATION"), "{healthy}");

        // Windows timer granularity can double a short sleep; that must NOT read as starvation.
        let granularity = describe_scheduling(poll, Duration::from_millis(60));
        assert!(granularity.contains("genuinely never advanced"), "{granularity}");

        let starved = describe_scheduling(poll, Duration::from_millis(800));
        assert!(starved.contains("HOST STARVATION"), "{starved}");
        assert!(starved.contains("says nothing about the code"), "{starved}");
    }

    #[test]
    fn a_described_wait_reports_the_state_it_gave_up_on() {
        let outcome = std::panic::catch_unwind(|| {
            wait_for_state(
                Duration::from_millis(60),
                || "peer=absent link=down".to_string(),
                || false,
            )
        })
        .expect_err("a condition that never holds must fail the test");

        let message = outcome
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("peer=absent link=down"),
            "the panic has to carry the observed state, got: {message}"
        );
    }

    /// `wait_for_condition` that says what it actually saw when it gives up.
    ///
    /// Every entry in the "known flaky" table is a `wait_for_condition` timeout whose recorded
    /// action is "retry isolated", and they have stayed that way for months because the panic
    /// carries a file, a line and a duration — and nothing about the state being waited on. There is
    /// no way to tell "the peer never linked" from "it linked and the message never arrived" after
    /// the fact, so every investigation restarts from zero.
    ///
    /// `describe` is called ONCE, on failure, so a costly description is free on the happy path. It
    /// runs after the deadline, i.e. it reports the state at the moment of giving up.
    ///
    /// This is the tool the table's own lesson asks for: "instrument the actual state machine before
    /// blaming the host" (docs/architecture/test-patterns.md, on the attachment GC root-cause).
    #[track_caller]
    fn wait_for_state(
        timeout: Duration,
        describe: impl Fn() -> String,
        condition: impl Fn() -> bool,
    ) {
        const POLL: Duration = Duration::from_millis(25);
        let deadline = Instant::now() + timeout;
        let mut polls: u32 = 0;
        // The single fact that separates the two explanations this family of failures gets given.
        // We ask for a 25 ms sleep every poll; if the OS hands back 25 ms we were scheduled fine and
        // a missed budget means the state genuinely never advanced — a bug. If it hands back
        // hundreds of milliseconds the box starved us and the budget says nothing about the code.
        // Without this the panic is compatible with both stories, which is exactly why seven
        // entries in the known-flaky table have "retry" as their diagnosis.
        let mut worst_sleep = Duration::ZERO;

        while Instant::now() < deadline {
            if condition() {
                return;
            }
            polls += 1;
            let slept_at = Instant::now();
            thread::sleep(POLL);
            worst_sleep = worst_sleep.max(slept_at.elapsed());
        }

        let caller = std::panic::Location::caller();
        let starvation = describe_scheduling(POLL, worst_sleep);
        panic!(
            "condition at {}:{} was not satisfied within {:?} ({polls} polls); {starvation}; \
             observed state: {}",
            caller.file(),
            caller.line(),
            timeout,
            describe(),
        );
    }

    struct NetworkTestGuard {
        _process_guard: std::sync::MutexGuard<'static, ()>,
        _lock_file: File,
        lock_path: PathBuf,
        /// Keeps the lock file's mtime fresh for as long as this guard lives. See
        /// `NETWORK_LOCK_STALE_AFTER` for what happens without it.
        heartbeat_stop: Arc<std::sync::atomic::AtomicBool>,
        heartbeat: Option<thread::JoinHandle<()>>,
    }

    impl Drop for NetworkTestGuard {
        fn drop(&mut self) {
            self.heartbeat_stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(heartbeat) = self.heartbeat.take() {
                let _ = heartbeat.join();
            }
            let _ = fs::remove_file(&self.lock_path);
            thread::sleep(Duration::from_millis(150));
        }
    }

    /// How long a lock file may go untouched before a waiter treats its owner as dead and takes it.
    ///
    /// This only works because the owner keeps touching it. Without the heartbeat the mtime was set
    /// once at creation and never again, so ANY networked test running longer than this had its lock
    /// stolen out from under it by the next process to come along — and the attachment tests are
    /// allowed 60 seconds. Two networked tests would then run at once on the same ports and the same
    /// multicast group, which is precisely the shape of every entry in the "known flaky" table.
    const NETWORK_LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
    /// Comfortably shorter than the staleness window, so a busy box cannot miss a beat and lose the
    /// lock it is holding.
    const NETWORK_LOCK_HEARTBEAT: Duration = Duration::from_secs(2);

    /// Сериализует тесты, которые реально открывают сокеты: старт mesh, связь двух узлов
    /// (`link_stage5_peers_over_mesh`, `pair_stage5_over_mesh`). Mesh на loopback слушает и
    /// рассылает multicast; два теста, поднявшие его одновременно, видят и друг друга.
    ///
    /// **Бери его только если тест открывает сокет.** Тест, который лишь читает и пишет конфиг или БД, в очередь вставать не должен —
    /// это межпроцессный замок с 30-секундным дедлайном ожидания, и очередь из-за него растёт на
    /// ровном месте. 2026-08-14 таких «сетевых» тестов без единого сокета нашлось 19 из 68.
    fn network_test_guard() -> NetworkTestGuard {
        static NETWORK_TEST_MUTEX: std::sync::OnceLock<std::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        let process_guard = NETWORK_TEST_MUTEX
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lock_path = std::env::temp_dir().join("omni-stage5-network-tests.lock");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(lock_file) => {
                    let heartbeat_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let heartbeat = {
                        let stop = Arc::clone(&heartbeat_stop);
                        let path = lock_path.clone();
                        thread::Builder::new()
                            .name("omni-network-test-lock-heartbeat".to_string())
                            .spawn(move || {
                                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                                    // In short steps: the guard's drop joins this thread, and one
                                    // 2-second sleep rounded every networked test up to the next
                                    // 2 s (measured 2026-10-13: 63 tests, ~1 minute of the gate).
                                    let beat = Instant::now();
                                    while beat.elapsed() < NETWORK_LOCK_HEARTBEAT
                                        && !stop.load(std::sync::atomic::Ordering::Relaxed)
                                    {
                                        thread::sleep(Duration::from_millis(50));
                                    }
                                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                        break;
                                    }
                                    // Gone already means somebody took it despite the heartbeat;
                                    // nothing useful left to do, and re-creating it would hand this
                                    // test a lock the other holder also thinks it owns.
                                    let Ok(file) = OpenOptions::new().write(true).open(&path) else {
                                        break;
                                    };
                                    let _ = file.set_modified(std::time::SystemTime::now());
                                }
                            })
                            .ok()
                    };
                    return NetworkTestGuard {
                        _process_guard: process_guard,
                        _lock_file: lock_file,
                        lock_path,
                        heartbeat_stop,
                        heartbeat,
                    };
                }
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    let stale = fs::metadata(&lock_path)
                        .and_then(|meta| meta.modified())
                        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
                        .map(|age| age > NETWORK_LOCK_STALE_AFTER)
                        .unwrap_or(false);
                    if stale {
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    panic!(
                        "failed to acquire cross-process network test lock '{}': {error}",
                        lock_path.display()
                    );
                }
            }
        }
    }

    #[test]
    fn dialog_message_delta_returns_changes_and_falls_back_for_stale_cursor() {
        let app = create_app_with_active_profile(
            std::env::temp_dir()
                .join("omni-stage-11-app-tests")
                .join(format!("dialog-delta-{}.db", unique_suffix()))
                .to_string_lossy()
                .into_owned(),
            "Alice",
            "Office PC",
        );
        let dialog_key = dialog_key_from_instance_id("inst-delta");
        let empty_summary = app
            .get_dialog_message_refresh_summary(&dialog_key)
            .expect("empty refresh summary should load");
        let empty_cursor = DialogDeltaCursor::from_refresh_summary(&empty_summary);

        app.storage
            .insert_outgoing_message(
                "delta-msg-1",
                &dialog_key,
                "first",
                MessageStatus::Queued,
                1_100,
            )
            .expect("first message should be inserted");
        app.storage
            .upsert_message_security_metadata(
                "delta-msg-1",
                &dialog_key,
                None,
                None,
                None,
                None,
                "encrypted_pending",
                None,
                Some("text"),
                1_100,
                1_100,
            )
            .expect("first security metadata should be inserted");

        let first_delta = app
            .get_dialog_message_delta(&dialog_key, &empty_cursor)
            .expect("first delta should load");
        assert!(first_delta.requires_full_reload);
        assert!(first_delta.changed_messages.is_empty());
        assert!(first_delta.changed_security_details.is_empty());

        let initial_summary = app
            .get_dialog_message_refresh_summary(&dialog_key)
            .expect("initial refresh summary should load");
        let initial_cursor = DialogDeltaCursor::from_refresh_summary(&initial_summary);

        app.storage
            .insert_outgoing_message(
                "delta-msg-2",
                &dialog_key,
                "second",
                MessageStatus::Queued,
                1_200,
            )
            .expect("second message should be inserted");
        app.storage
            .upsert_message_security_metadata(
                "delta-msg-2",
                &dialog_key,
                None,
                None,
                None,
                None,
                "encrypted_pending",
                None,
                Some("text"),
                1_200,
                1_200,
            )
            .expect("second security metadata should be inserted");

        let delta = app
            .get_dialog_message_delta(&dialog_key, &initial_cursor)
            .expect("delta should load");
        assert!(!delta.requires_full_reload);
        assert_eq!(delta.changed_messages.len(), 1);
        assert_eq!(delta.changed_messages[0].id, "delta-msg-2");
        assert_eq!(delta.changed_security_details.len(), 1);
        assert_eq!(delta.changed_security_details[0].message_id, "delta-msg-2");

        let stale_cursor = DialogDeltaCursor {
            last_message_updated_at_unix_ms: Some(9_999),
            ..delta.cursor.clone()
        };
        let fallback = app
            .get_dialog_message_delta(&dialog_key, &stale_cursor)
            .expect("stale delta should still return a typed fallback");
        assert!(fallback.requires_full_reload);
        assert!(fallback.changed_messages.is_empty());
        assert!(fallback.changed_security_details.is_empty());
    }

    /// Several devices ring for one call; one answers, another's «ringing» arrives later (through a
    /// relay it routinely does). It must not put the live call back to «ringing» (stand 2026-10-14:
    /// the caller's window showed «ringing» with the talk going on). A call still calling does move.
    #[test]
    fn a_late_ringing_does_not_put_a_live_call_back_to_ringing() {
        let peer = create_stage5_peer("late-ringing", "Alice", "Alice PC");
        let db_path = peer.app.storage.db_path().to_string_lossy().to_string();
        let ringing = omni_types::CallRingingV1 { call_id: "call-test".to_string(), ringing_at_unix_ms: current_unix_ms() };
        // (state, answered?) → state after a «ringing». An outgoing call waits for an answer in
        // `Connecting` — it must still show «ringing» then (the first version of this fix lost it).
        for (before, answered, after) in [
            (CallState::Active, true, CallState::Active),
            (CallState::Connecting, true, CallState::Connecting),
            (CallState::Connecting, false, CallState::Ringing),
            (CallState::Calling, false, CallState::Ringing),
            (CallState::Reconnecting, false, CallState::Ringing),
            (CallState::Reconnecting, true, CallState::Reconnecting),
        ] {
            let mut runtime = test_active_call_runtime(before);
            runtime.summary.contact_id = None;
            if !answered {
                runtime.summary.connected_at_unix_ms = None;
            }
            set_current_call_runtime(&db_path, Some(runtime)).unwrap();
            crate::handle_incoming_call_ringing(&peer.app.storage, &ringing).unwrap();
            let now = current_call_runtime(&db_path).unwrap().unwrap().summary.state;
            assert_eq!(now, after, "from {before:?}, answered={answered}");
        }
        set_current_call_runtime(&db_path, None).unwrap();
    }

    /// A call nobody answered yet has no link to lose: its dialog may be a dead one of the contact's
    /// devices, and judging that silence ended a call still ringing on the others (stand 2026-10-15).
    #[test]
    fn an_unanswered_call_is_not_put_to_reconnecting_by_a_silent_dialog() {
        let peer = create_stage5_peer("unanswered-silent", "Alice", "Alice PC");
        let db_path = peer.app.storage.db_path().to_string_lossy().to_string();
        let mut runtime = test_active_call_runtime(CallState::Connecting);
        runtime.summary.contact_id = None;
        runtime.summary.connected_at_unix_ms = None;
        set_current_call_runtime(&db_path, Some(runtime)).unwrap();
        crate::tick_call_runtime(&peer.app.storage, None).unwrap();
        let after = current_call_runtime(&db_path).unwrap().expect("the call is still there");
        assert_eq!(after.summary.state, CallState::Connecting, "still waiting for an answer");
        set_current_call_runtime(&db_path, None).unwrap();
    }

    fn test_active_call_runtime(state: CallState) -> ActiveCallRuntime {
        let now_unix_ms = current_unix_ms();
        ActiveCallRuntime {
            summary: CallSummary {
                call_id: "call-test".to_string(),
                dialog_key: dialog_key_from_instance_id("inst-call-test"),
                contact_id: Some("contact-test".to_string()),
                direction: CallDirection::Outgoing,
                state,
                outcome: None,
                decline_reason: None,
                participants: Vec::new(),
                local_mic_muted: false,
                network_quality: Some(CallNetworkQuality::Good),
                started_at_unix_ms: now_unix_ms - 10_000,
                ringing_started_at_unix_ms: Some(now_unix_ms - 9_000),
                connected_at_unix_ms: Some(now_unix_ms - 8_000),
                ended_at_unix_ms: None,
                reconnect_deadline_unix_ms: None,
                updated_at_unix_ms: now_unix_ms,
                remote_control: None,
            },
            remote_account_id: AccountId("acct-remote".to_string()),
            remote_device_id: DeviceId("dev-remote".to_string()),
            invite_expires_at_unix_ms: now_unix_ms + 30_000,
            invited_devices: Vec::new(),
        }
    }

    /// Inbound media goes to the sink wired for its dialog, at once — and to nothing after the
    /// sink is unwired (the negative half: a packet for an unwired call is dropped, not buffered).
    #[test]
    fn inbound_media_reaches_only_the_wired_sink_of_its_dialog() {
        let sinks = RealtimeMediaInboundSinks::default();
        let dialog_key = dialog_key_from_instance_id("inst-media-sink");
        let other_dialog_key = dialog_key_from_instance_id("inst-media-sink-other");
        let got: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let got = Arc::clone(&got);
            sinks
                .set(
                    &dialog_key,
                    Some(Arc::new(move |packet: &[u8]| got.lock().unwrap().push(packet.to_vec()))),
                )
                .expect("wire the sink");
        }

        assert!(sinks.deliver(&dialog_key, &[1, 2, 3]));
        assert!(!sinks.deliver(&other_dialog_key, &[4]), "another dialog has no sink");
        sinks.set(&dialog_key, None).expect("unwire the sink");
        assert!(!sinks.deliver(&dialog_key, &[5]), "an unwired call takes nothing");
        assert_eq!(*got.lock().unwrap(), vec![vec![1, 2, 3]]);
    }

    // Approval-gate regression tests for TM-001/TM-007 datagram-plane
    // defense-in-depth. See docs/decisions/media-approval-gate.md.

    #[test]
    fn dialog_has_active_media_negotiation_returns_false_when_registry_is_empty() {
        // No entries in registry → check returns false → push_incoming silently
        // drops datagrams.
        let unique_db_path = format!(
            "/tmp/omni-test-no-negotiation-{}",
            unique_suffix()
        );
        let dialog_key = dialog_key_from_instance_id("inst-no-negotiation");
        // The registry is a process-wide static, but our (db_path, dialog_key)
        // pair has never been touched, so the negotiation lookup is None.
        assert!(!dialog_has_active_media_negotiation(&unique_db_path, &dialog_key));
    }

    #[test]
    fn push_incoming_realtime_media_datagram_drops_without_active_negotiation() {
        let sinks = RealtimeMediaInboundSinks::default();
        let unique_db_path = format!(
            "/tmp/omni-test-push-drop-{}",
            unique_suffix()
        );
        let dialog_key = dialog_key_from_instance_id("inst-push-drop");
        // The call IS wired — only the negotiation is missing, so a delivery would be the gate's
        // failure, not a missing sink.
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let delivered = Arc::clone(&delivered);
            sinks
                .set(
                    &dialog_key,
                    Some(Arc::new(move |_: &[u8]| {
                        delivered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    })),
                )
                .expect("wire the sink");
        }
        let receive_context = ReceivedChatContext {
            peer_id: "127.0.0.1:60000".to_string(),
            dialog_key: dialog_key.clone(),
            remote_instance_id: "inst-push-drop".to_string(),
            remote_prekey_bundle: None,
            initiated_locally: false,
        };

        // No negotiation exists for this dialog → datagram must be silently dropped.
        push_incoming_realtime_media_datagram(
            &sinks,
            &unique_db_path,
            &receive_context,
            &[0x80, 0x60, 0x00, 0x01],
        )
        .expect("push must succeed (silent drop is Ok)");

        assert_eq!(
            delivered.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "datagram without active media negotiation must not reach the engine"
        );
    }

    /// СМЕНА СВОЕГО КАЧЕСТВА НЕ ИМЕЕТ ПРАВА ЗАБЫВАТЬ ЧУЖИЕ ВОЗМОЖНОСТИ.
    ///
    /// Оболочка на смену разрешения звала `clear_realtime_media_negotiation`, а он сносит запись
    /// целиком — вместе с возможностями пира и выбором. Выбор — это и есть пропуск для входящих
    /// датаграмм (`dialog_has_active_media_negotiation`), поэтому после «поменял себе разрешение»
    /// у нас переставало приниматься ВСЁ, что шлёт собеседник.
    ///
    /// Живой звонок 2026-08-30: 1500+ строк `media/datagram-drop reason=no-active-negotiation`
    /// за 80 секунд — ровно столько датаграмм в это время отправил ПК. Голоса не было слышно не
    /// потому, что он не дошёл.
    ///
    /// Правильная операция здесь — переопубликовать СВОИ возможности: выбор пересобирается из
    /// запомненных чужих, и ворота не закрываются ни на такт.
    #[test]
    fn changing_our_own_quality_must_not_forget_what_the_peer_can_do() {
        let db_path = format!("/tmp/omni-test-quality-change-{}", unique_suffix());
        let dialog_key = dialog_key_from_instance_id("inst-media-quality-change");
        let now = current_unix_ms();

        // Обе стороны объявились: у нас 720p, у пира 720p.
        crate::store_local_realtime_media_capabilities(
            &db_path,
            test_realtime_media_capabilities(
                &dialog_key,
                vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
                (1280, 720, 30),
                (1280, 720, 30),
            ),
            now,
        )
        .expect("publishing local capabilities should succeed");
        {
            let mut registry =
                crate::lock_realtime_media_negotiation_registry().expect("registry lock");
            let state = registry.entry(db_path.clone()).or_default();
            let negotiation = state.by_dialog.entry(dialog_key.clone()).or_default();
            negotiation.remote_capabilities = Some(test_realtime_media_capabilities(
                &dialog_key,
                vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
                (1280, 720, 30),
                (1280, 720, 30),
            ));
            crate::refresh_realtime_media_selection(&dialog_key, negotiation, now);
        }
        assert!(
            dialog_has_active_media_negotiation(&db_path, &dialog_key),
            "оба объявились — ворота для входящей медиа обязаны быть открыты"
        );

        // Человек посреди звонка выбрал 640x360. Меняются ТОЛЬКО наши возможности.
        let ours = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (640, 360, 30),
            (640, 360, 30),
        );
        let ours_id = ours.capabilities_id.clone();
        crate::store_local_realtime_media_capabilities(&db_path, ours, now + 1)
        .expect("republishing local capabilities should succeed");

        assert!(
            dialog_has_active_media_negotiation(&db_path, &dialog_key),
            "смена своего качества не должна закрывать ворота для чужой медиа"
        );
        let snapshot = crate::realtime_media_negotiation_snapshot_for_dialog(&db_path, &dialog_key)
            .expect("snapshot should be readable");
        assert_eq!(
            snapshot.status,
            RealtimeMediaNegotiationStatus::Negotiated,
            "возможности пира должны были остаться на месте"
        );
        let video = snapshot
            .selected
            .as_ref()
            .and_then(|selection| selection.video.as_ref())
            .expect("после переобъявления должен быть выбор с видео");
        let sent = stream_sent_by(video, &ours_id);
        assert_eq!(
            (sent.width, sent.height),
            (640, 360),
            "выбор обязан пойти за новым нашим качеством, а не остаться прежним"
        );

        // ОТРИЦАТЕЛЬНАЯ ПОЛОВИНА: очистка вместо переобъявления — это и есть закрытые ворота.
        // Если кто-то вернёт её на путь смены настроек, здесь станет видно, чем именно это плохо.
        clear_realtime_media_negotiation_for_dialog(&db_path, &dialog_key)
            .expect("clearing should succeed");
        assert!(
            !dialog_has_active_media_negotiation(&db_path, &dialog_key),
            "очистка закрывает ворота — ради этого её и нельзя звать на смену своих настроек"
        );
    }

    #[test]
    fn clear_realtime_media_negotiation_for_dialog_is_idempotent_on_unknown_dialog() {
        let unique_db_path = format!(
            "/tmp/omni-test-clear-idempotent-{}",
            unique_suffix()
        );
        let dialog_key = dialog_key_from_instance_id("inst-clear-idempotent");
        // Clearing a non-existent entry must not error — `end_runtime_call`
        // relies on idempotent cleanup since it's called from many lifecycle
        // paths without coordination.
        clear_realtime_media_negotiation_for_dialog(&unique_db_path, &dialog_key)
            .expect("clearing a missing dialog should be Ok");
        clear_realtime_media_negotiation_for_dialog(&unique_db_path, &dialog_key)
            .expect("clearing twice should still be Ok");
    }

    #[test]
    fn a_call_stays_active_while_the_peer_keeps_answering() {
        // The live failure this replaces: liveness was read from the LEGACY transport's
        // connection record, which the mesh path never populates, so the caller's own call
        // sat in `reconnecting` while its audio was flowing perfectly.
        let now = current_unix_ms();
        let health = call_link_health(Some(now - 3_000), now);
        assert_eq!(health, CallLinkHealth::Responsive);
        assert_eq!(
            call_link_transition(CallState::Active, health, None, now),
            CallLinkTransition::Keep
        );
    }

    #[test]
    fn a_call_whose_peer_vanished_gives_up_instead_of_hanging_forever() {
        // The other end of the same wrong signal: the callee held an `active` call after
        // the peer's app had exited, and only an explicit hangup could clear it.
        let now = current_unix_ms();
        let health = call_link_health(Some(now - CALL_PEER_SILENCE_MS - 1), now);
        assert_eq!(health, CallLinkHealth::Silent);

        let CallLinkTransition::StartReconnecting { deadline_unix_ms } =
            call_link_transition(CallState::Active, health, None, now)
        else {
            panic!("a silent peer must put the call into the reconnect window");
        };
        assert_eq!(deadline_unix_ms, now + CALL_RECONNECT_GRACE_MS);

        // Still silent once the window is up: the call ends by itself.
        assert_eq!(
            call_link_transition(
                CallState::Reconnecting,
                CallLinkHealth::Silent,
                Some(deadline_unix_ms),
                deadline_unix_ms,
            ),
            CallLinkTransition::EndConnectionLost
        );
    }

    #[test]
    fn a_peer_that_comes_back_inside_the_window_keeps_the_call() {
        let now = current_unix_ms();
        assert_eq!(
            call_link_transition(
                CallState::Reconnecting,
                CallLinkHealth::Responsive,
                Some(now + 5_000),
                now,
            ),
            CallLinkTransition::Recovered
        );
        // And a silent peer inside the window is not given up on yet.
        assert_eq!(
            call_link_transition(
                CallState::Reconnecting,
                CallLinkHealth::Silent,
                Some(now + 5_000),
                now,
            ),
            CallLinkTransition::Keep
        );
    }

    #[test]
    fn a_peer_we_have_never_heard_from_does_not_kill_the_call() {
        // No presence row yet (a call placed the instant the dialog opened). Unknown must
        // not be read as dead — that would end calls that are merely young.
        let now = current_unix_ms();
        assert_eq!(call_link_health(None, now), CallLinkHealth::Responsive);
    }

    #[test]
    fn the_bitrate_level_scales_both_directions_and_economy_wins_over_max() {
        let dialog_key = dialog_key_from_instance_id("inst-media-bitrate-level");
        let passport = || {
            test_realtime_media_capabilities(
                &dialog_key,
                vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
                (1280, 720, 30),
                (1280, 720, 30),
            )
        };
        let rates = |local_level, remote_level| {
            let mut local = passport();
            local.bitrate_level = local_level;
            let mut remote = passport();
            remote.bitrate_level = remote_level;
            let (_, video) = negotiated_video(&dialog_key, &local, &remote);
            (
                stream_sent_by(&video, &local.capabilities_id).target_bitrate_bps,
                stream_sent_by(&video, &remote.capabilities_id).target_bitrate_bps,
            )
        };
        use omni_types::RealtimeVideoBitrateLevel::{Auto, Economy, Max};

        assert_eq!(rates(Auto, Auto), (6_000_000, 6_000_000));
        // The receiver pays for the bytes too: its Economy halves what comes to it.
        assert_eq!(rates(Economy, Auto), (3_000_000, 3_000_000));
        // Max needs only the side that asks.
        assert_eq!(rates(Auto, Max), (9_000_000, 9_000_000));
        assert_eq!(rates(Max, Economy), (3_000_000, 3_000_000));
    }

    #[test]
    fn realtime_media_negotiation_prefers_common_h264_and_each_direction_keeps_its_format() {
        let dialog_key = dialog_key_from_instance_id("inst-media-negotiate");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 30),
            (1920, 1080, 30),
        );
        let remote = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );

        let (selection, video) = negotiated_video(&dialog_key, &local, &remote);

        assert_eq!(video.streams.len(), 2);
        for stream in &video.streams {
            assert_eq!(stream.codec, RealtimeVideoCodec::H264);
            assert_eq!(stream.payload_type, 102);
        }
        // The 720p peer no longer pulls the 1080p one down: before, both went 1280x720.
        let from_local = stream_sent_by(&video, &local.capabilities_id);
        assert_eq!((from_local.width, from_local.height, from_local.fps), (1920, 1080, 30));
        assert_eq!(from_local.target_bitrate_bps, 12_000_000);
        let from_remote = stream_sent_by(&video, &remote.capabilities_id);
        assert_eq!((from_remote.width, from_remote.height, from_remote.fps), (1280, 720, 30));
        assert_eq!(from_remote.target_bitrate_bps, 6_000_000);
        assert_eq!(selection.max_datagram_bytes, 1000);
        assert_eq!(video.capture_pixel_format, RealtimeVideoPixelFormat::Nv12);
        assert!(!video.rtcp_feedback_enabled);
    }

    #[test]
    fn a_peer_without_a_camera_still_gets_a_session_so_the_call_has_sound() {
        // The live failure this whole shape exists for: the machine with no camera
        // published nothing, no selection was ever made, and NEITHER side started its
        // audio pipeline. Two connected calls, complete silence, and a camera button
        // that stayed grey on the peer that DID have a camera.
        let dialog_key = dialog_key_from_instance_id("inst-media-no-camera");
        let with_camera = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut without_camera = with_camera.clone();
        without_camera.video.as_mut().unwrap().can_send_video = false;

        // It can still DECODE, so video one way plus audio both ways is the right deal.
        let (_, video) = negotiated_video(&dialog_key, &with_camera, &without_camera);
        assert_eq!(video.streams.len(), 1, "only the camera's direction carries video");
        assert_eq!((video.streams[0].width, video.streams[0].height), (1280, 720));

        // And with no camera on either end there is still a session — just no video.
        let mut other_without_camera = without_camera.clone();
        other_without_camera.video.as_mut().unwrap().can_send_video = false;
        let note = negotiated_audio_only_note(&dialog_key, &without_camera, &other_without_camera);
        assert!(note.contains("neither peer has a camera"), "{note}");
    }

    /// A group member without a camera still decodes the others and can share its screen: its
    /// self-negotiation counts the rest of the call as possible senders. Negative half: a member
    /// with no video pipeline at all (nothing it could decode) stays audio-only.
    #[test]
    fn a_camera_less_group_member_still_negotiates_video() {
        let app = create_app_with_active_profile(
            isolated_test_db_path("omni-group-media-no-camera", "pc"),
            "PC",
            "Desktop",
        );
        let dialog_key = app.create_group("Squad").unwrap().dialog_key;
        let mut without_camera = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        without_camera.video.as_mut().unwrap().can_send_video = false;
        let snapshot = app.publish_group_media_capabilities(without_camera.clone()).unwrap();
        let video = snapshot.selected.and_then(|selected| selected.video).expect("video despite no camera");
        assert!(!video.streams.is_empty());

        let mut no_pipeline = without_camera;
        no_pipeline.codecs.clear();
        no_pipeline.video = None;
        let snapshot = app.publish_group_media_capabilities(no_pipeline).unwrap();
        assert!(snapshot.selected.expect("audio still selected").video.is_none());
    }

    #[test]
    fn a_peer_with_no_video_pipeline_at_all_still_gets_a_session() {
        // No hardware H.264 / capture stack that failed to probe: nothing to offer the
        // video negotiation whatsoever. Still a call, still audio.
        let dialog_key = dialog_key_from_instance_id("inst-media-no-video");
        let full = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut audio_only = full.clone();
        audio_only.codecs.clear();
        audio_only.video = None;

        let note = negotiated_audio_only_note(&dialog_key, &full, &audio_only);
        assert!(note.contains("no video pipeline"), "{note}");
    }

    #[test]
    fn realtime_media_negotiation_enables_rtcp_only_when_both_peers_support_it() {
        let dialog_key = dialog_key_from_instance_id("inst-media-rtcp");
        let mut local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut remote = local.clone();
        local.supports_rtcp_feedback = true;
        remote.supports_rtcp_feedback = false;

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);
        assert!(!video.rtcp_feedback_enabled);

        remote.supports_rtcp_feedback = true;
        let (_, video) = negotiated_video(&dialog_key, &local, &remote);
        assert!(video.rtcp_feedback_enabled);
    }

    #[test]
    fn realtime_media_negotiation_prefers_native_friendly_capture_pixel_format() {
        let dialog_key = dialog_key_from_instance_id("inst-media-format");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut remote = local.clone();
        remote.capabilities_id = "caps-remote".to_string();

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);
        assert!(video.streams.iter().all(|s| s.codec == RealtimeVideoCodec::H264));
        assert_eq!(video.capture_pixel_format, RealtimeVideoPixelFormat::Nv12);
    }

    #[test]
    fn realtime_media_negotiation_uses_bgra_when_nv12_is_not_common() {
        let dialog_key = dialog_key_from_instance_id("inst-media-format-bgra");
        let mut local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut remote = local.clone();
        test_video_profile(&mut local).pixel_formats =
            vec![RealtimeVideoPixelFormat::Nv12, RealtimeVideoPixelFormat::Bgra];
        test_video_profile(&mut remote).pixel_formats = vec![RealtimeVideoPixelFormat::Bgra];

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);

        assert_eq!(video.capture_pixel_format, RealtimeVideoPixelFormat::Bgra);
    }

    #[test]
    fn realtime_media_negotiation_prefers_nv12_when_peer_preferences_disagree() {
        let dialog_key = dialog_key_from_instance_id("inst-media-format-mixed");
        let mut local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut remote = local.clone();
        test_video_profile(&mut local).pixel_formats = vec![
            RealtimeVideoPixelFormat::Nv12,
            RealtimeVideoPixelFormat::Bgra,
        ];
        test_video_profile(&mut remote).pixel_formats = vec![
            RealtimeVideoPixelFormat::Bgra,
            RealtimeVideoPixelFormat::Nv12,
        ];

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);

        assert_eq!(video.capture_pixel_format, RealtimeVideoPixelFormat::Nv12);
    }

    #[test]
    fn realtime_media_negotiation_drops_video_without_common_capture_pixel_format() {
        let dialog_key = dialog_key_from_instance_id("inst-media-no-format");
        let mut local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let mut remote = local.clone();
        test_video_profile(&mut local).pixel_formats = vec![RealtimeVideoPixelFormat::Nv12];
        test_video_profile(&mut remote).pixel_formats = vec![RealtimeVideoPixelFormat::Bgra];

        let note = negotiated_audio_only_note(&dialog_key, &local, &remote);

        assert!(note.contains("no common realtime video capture pixel format"), "{note}");
    }

    #[test]
    fn realtime_media_negotiation_sends_each_peers_own_preferred_quality() {
        let dialog_key = dialog_key_from_instance_id("inst-media-quality");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 30),
            (1920, 1080, 30),
        );
        let remote = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);

        let from_local = stream_sent_by(&video, &local.capabilities_id);
        assert_eq!((from_local.width, from_local.height, from_local.fps), (1920, 1080, 30));
        assert_eq!(from_local.target_bitrate_bps, 12_000_000);
        // The remote could send 1080p but prefers 720p: it sends 720p.
        let from_remote = stream_sent_by(&video, &remote.capabilities_id);
        assert_eq!((from_remote.width, from_remote.height), (1280, 720));
    }

    #[test]
    fn realtime_media_negotiation_selects_requested_1080p60_when_common() {
        let dialog_key = dialog_key_from_instance_id("inst-media-quality-60");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 60),
            (1920, 1080, 60),
        );
        let remote = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 60),
            (1920, 1080, 60),
        );

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);

        for stream in &video.streams {
            assert_eq!((stream.width, stream.height, stream.fps), (1920, 1080, 60));
            assert_eq!(stream.target_bitrate_bps, 15_000_000);
        }
    }

    /// The receiver limits a direction only by what it decodes in that codec, keeping the picture's
    /// proportions; the frame rate too.
    #[test]
    fn a_direction_is_fitted_to_what_its_receiver_decodes() {
        let dialog_key = dialog_key_from_instance_id("inst-media-decoder-limit");
        let sender = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (2560, 1440, 60),
            (2560, 1440, 60),
        );
        let mut receiver = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let decode = receiver.codecs[0].decode.as_mut().unwrap();
        decode.max_width = 1920;
        decode.max_height = 1080;
        decode.max_fps = 30;

        let (_, video) = negotiated_video(&dialog_key, &sender, &receiver);

        let sent = stream_sent_by(&video, &sender.capabilities_id);
        assert_eq!((sent.width, sent.height, sent.fps), (1920, 1080, 30));
    }

    /// A shared screen gets the bits of the 30 fps rung even at 15 fps (text needs bits per frame),
    /// and the camera at the same format does not.
    #[test]
    fn a_shared_screen_keeps_the_30_fps_budget_at_a_lower_frame_rate() {
        let dialog_key = dialog_key_from_instance_id("inst-media-screen-budget");
        let mut screen = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1920, 1080, 15),
            (1920, 1080, 15),
        );
        screen.content = omni_types::VideoContent::Screen;
        let mut camera = screen.clone();
        camera.capabilities_id = format!("{}-camera", screen.capabilities_id);
        camera.content = omni_types::VideoContent::Camera;
        let viewer = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );

        let (_, video) = negotiated_video(&dialog_key, &screen, &viewer);
        assert_eq!(stream_sent_by(&video, &screen.capabilities_id).target_bitrate_bps, 12_000_000);
        let (_, video) = negotiated_video(&dialog_key, &camera, &viewer);
        assert_eq!(stream_sent_by(&video, &camera.capabilities_id).target_bitrate_bps, 6_000_000);
    }

    #[test]
    fn realtime_media_negotiation_rejects_software_h264_on_both_ends() {
        // Both peers advertise H.264 SW-only — architecture forbids forced
        // software H.264 in production. Video must be refused visibly with
        // "no common realtime video codec" (the call itself still runs, audio only).
        // See docs/decisions/h264-runtime-proof.md.
        let dialog_key = dialog_key_from_instance_id("inst-media-h264-sw");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, false)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let remote = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, false)],
            (1280, 720, 30),
            (1280, 720, 30),
        );

        let note = negotiated_audio_only_note(&dialog_key, &local, &remote);
        assert!(note.contains("no common realtime video codec"), "{note}");
    }

    #[test]
    fn realtime_media_negotiation_with_one_software_h264_side_sends_only_from_the_hardware_one() {
        // One peer has H.264 HW, the other H.264 SW only. Software H.264 ENCODING is forbidden
        // (docs/decisions/h264-runtime-proof.md), decoding it in software is not: per direction
        // (per-pair-video-codec-selection.md) the hardware side sends, the software side only
        // receives.
        let dialog_key = dialog_key_from_instance_id("inst-media-h264-mixed");
        let local = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        let remote = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, false)],
            (1280, 720, 30),
            (1280, 720, 30),
        );

        let (_, video) = negotiated_video(&dialog_key, &local, &remote);
        assert_eq!(video.streams.len(), 1);
        assert_eq!(video.streams[0].sender_capabilities_id, local.capabilities_id);
        assert_eq!(video.streams[0].codec, RealtimeVideoCodec::H264);
    }

    /// The stand (per-pair-video-codec-selection.md): the laptop encodes AV1, the PC does not —
    /// each direction gets its own codec, and each side reads its own two out of one selection.
    #[test]
    fn realtime_media_negotiation_picks_a_codec_per_direction() {
        let dialog_key = dialog_key_from_instance_id("inst-media-per-direction");
        let all = |codec| test_realtime_video_codec(codec, omni_types::default_video_payload_type(codec), true);
        let laptop = test_realtime_media_capabilities(
            &dialog_key,
            vec![all(RealtimeVideoCodec::Av1), all(RealtimeVideoCodec::H265), all(RealtimeVideoCodec::H264)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );
        let mut pc_av1 = all(RealtimeVideoCodec::Av1);
        pc_av1.encode = None;
        let pc = test_realtime_media_capabilities(
            &dialog_key,
            vec![pc_av1, all(RealtimeVideoCodec::H265), all(RealtimeVideoCodec::H264)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );
        let (_, video) = negotiated_video(&dialog_key, &laptop, &pc);
        let from = |id: &str| video.streams.iter().find(|s| s.sender_capabilities_id == id).unwrap();
        assert_eq!(from(&laptop.capabilities_id).codec, RealtimeVideoCodec::Av1);
        assert_eq!(from(&pc.capabilities_id).codec, RealtimeVideoCodec::H265);
        assert!(from(&pc.capabilities_id).reason.contains("AV1"));
        // AV1 needs fewer bits than HEVC for the same picture.
        assert!(from(&laptop.capabilities_id).target_bitrate_bps < from(&pc.capabilities_id).target_bitrate_bps);
    }

    /// A screen share is announced again, and the sharer's direction is chosen for the screen. A PC
    /// with hardware AV1 from a weaker vendor (AMD, ADR §3) and hardware HEVC sends its camera in
    /// HEVC and its screen in AV1: AV1's screen tools outweigh the vendor's penalty. Both hardware —
    /// software AV1 never wins over a hardware path (per-pair ADR §14), so that is not the lever.
    #[test]
    fn realtime_media_negotiation_chooses_the_sharers_direction_for_the_screen() {
        let dialog_key = dialog_key_from_instance_id("inst-media-screen");
        let all = |codec| test_realtime_video_codec(codec, omni_types::default_video_payload_type(codec), true);
        let laptop = test_realtime_media_capabilities(
            &dialog_key,
            vec![all(RealtimeVideoCodec::Av1), all(RealtimeVideoCodec::H265), all(RealtimeVideoCodec::H264)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );
        let mut pc_av1 = all(RealtimeVideoCodec::Av1);
        pc_av1.implementation = Some("amd".to_string());
        let mut pc = test_realtime_media_capabilities(
            &dialog_key,
            vec![pc_av1, all(RealtimeVideoCodec::H265), all(RealtimeVideoCodec::H264)],
            (1280, 720, 30),
            (1920, 1080, 30),
        );
        let from_pc = |pc: &RealtimeMediaCapabilitiesV1| {
            let (_, video) = negotiated_video(&dialog_key, &laptop, pc);
            video.streams.into_iter().find(|s| s.sender_capabilities_id == pc.capabilities_id).unwrap().codec
        };
        assert_eq!(from_pc(&pc), RealtimeVideoCodec::H265);
        pc.content = omni_types::VideoContent::Screen;
        assert_eq!(from_pc(&pc), RealtimeVideoCodec::Av1);
    }

    /// A mid-call re-announcement keeps the id of the first one: the peer's selection, computed
    /// from the earlier announcement, must still name our direction.
    #[test]
    fn realtime_media_re_announcement_keeps_the_capabilities_id() {
        let dialog_key = dialog_key_from_instance_id("inst-media-stable-id");
        let mut fresh = test_realtime_media_capabilities(
            &dialog_key,
            vec![test_realtime_video_codec(RealtimeVideoCodec::H264, 102, true)],
            (1280, 720, 30),
            (1280, 720, 30),
        );
        fresh.capabilities_id.clear();
        let first = with_capabilities_id(fresh.clone(), None);
        assert!(first.capabilities_id.starts_with("caps-"));
        let again = with_capabilities_id(fresh.clone(), Some(first.capabilities_id.clone()));
        assert_eq!(again.capabilities_id, first.capabilities_id);
        // An id the shell sets itself is left alone.
        let mut named = fresh;
        named.capabilities_id = "caps-shell".to_string();
        assert_eq!(with_capabilities_id(named, Some("caps-old".to_string())).capabilities_id, "caps-shell");
    }

    /// Negotiate and insist the pair agreed on video. Returns the session and its
    /// video half; panics with the reason if the pair came out audio-only.
    fn negotiated_video(
        dialog_key: &str,
        local: &RealtimeMediaCapabilitiesV1,
        remote: &RealtimeMediaCapabilitiesV1,
    ) -> (RealtimeMediaSelectionV1, RealtimeVideoSelectionV1) {
        let (selection, note) =
            select_realtime_media_configuration(dialog_key, local, remote, current_unix_ms());
        let video = selection
            .video
            .clone()
            .unwrap_or_else(|| panic!("expected a video selection, got audio only: {note:?}"));
        (selection, video)
    }

    /// The direction `sender` sends.
    fn stream_sent_by<'a>(video: &'a RealtimeVideoSelectionV1, sender: &str) -> &'a omni_types::RealtimeVideoStreamV1 {
        video
            .streams
            .iter()
            .find(|stream| stream.sender_capabilities_id == sender)
            .unwrap_or_else(|| panic!("no stream sent by {sender}: {video:?}"))
    }

    /// Negotiate and insist the pair came out audio-only — the session still exists,
    /// which is the whole point — returning the note that says why video is off.
    fn negotiated_audio_only_note(
        dialog_key: &str,
        local: &RealtimeMediaCapabilitiesV1,
        remote: &RealtimeMediaCapabilitiesV1,
    ) -> String {
        let (selection, note) =
            select_realtime_media_configuration(dialog_key, local, remote, current_unix_ms());
        assert!(
            selection.video.is_none(),
            "expected an audio-only session, got video {:?}",
            selection.video
        );
        note.expect("an audio-only session must say why it carries no video")
    }

    fn test_video_profile(
        capabilities: &mut RealtimeMediaCapabilitiesV1,
    ) -> &mut RealtimeVideoProfileCapabilityV1 {
        capabilities
            .video
            .as_mut()
            .expect("test capabilities always carry a video profile")
    }

    fn test_realtime_media_capabilities(
        dialog_key: &str,
        codecs: Vec<RealtimeVideoCodecCapabilityV1>,
        preferred: (u32, u32, u32),
        max: (u32, u32, u32),
    ) -> RealtimeMediaCapabilitiesV1 {
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        RealtimeMediaCapabilitiesV1 {
            dialog_key: dialog_key.to_string(),
            capabilities_id: format!(
                "caps-test-{}",
                NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            codecs,
            power: Default::default(),
            cpu_load: Default::default(),
            codec_ceiling: None,
            bitrate_level: Default::default(),
            content: Default::default(),
            video: Some(RealtimeVideoProfileCapabilityV1 {
                min_width: 1,
                min_height: 1,
                min_fps: 1,
                preferred_width: preferred.0,
                preferred_height: preferred.1,
                preferred_fps: preferred.2,
                max_width: max.0,
                max_height: max.1,
                max_fps: max.2,
                pixel_formats: vec![
                    RealtimeVideoPixelFormat::Nv12,
                    RealtimeVideoPixelFormat::Bgra,
                ],
                can_send_video: true,
            }),
            max_datagram_bytes: 1000,
            supports_rtcp_feedback: false,
            sent_at_unix_ms: current_unix_ms(),
        }
    }

    fn test_realtime_video_codec(
        codec: RealtimeVideoCodec,
        payload_type: u8,
        hardware: bool,
    ) -> RealtimeVideoCodecCapabilityV1 {
        let side = Some(omni_types::RealtimeVideoCodecSideV1 {
            hardware,
            max_width: 0,
            max_height: 0,
            max_fps: 0,
        });
        RealtimeVideoCodecCapabilityV1 {
            codec,
            payload_type,
            encode: side.clone(),
            decode: side,
            implementation: None,
        }
    }

    #[test]
    fn call_network_quality_is_v2_control_state_only_until_media_diagnostics_exist() {
        let now_unix_ms = current_unix_ms();
        let mut runtime = test_active_call_runtime(CallState::Active);
        assert_eq!(
            derive_call_network_quality(&runtime, now_unix_ms),
            Some(CallNetworkQuality::Good)
        );

        runtime.summary.state = CallState::Connecting;
        assert_eq!(
            derive_call_network_quality(&runtime, now_unix_ms),
            Some(CallNetworkQuality::Good)
        );

        runtime.summary.state = CallState::Reconnecting;
        assert_eq!(
            derive_call_network_quality(&runtime, now_unix_ms),
            Some(CallNetworkQuality::Recovering)
        );
    }

    #[test]
    fn session_lock_recovers_from_poisoning() {
        // Provoke poisoning: spawn a thread that locks `session_lock` and panics
        // while holding the guard. After the thread finishes, the lock is
        // poisoned. The next `lock_app_session_guard` call must still succeed
        // (recover-and-continue) and the diagnostic counter must increment.
        // See docs/decisions/session-lock-recovery.md.
        let session_lock = Arc::new(Mutex::new(()));
        let before = session_lock_poison_recovery_count();

        let poisoning_lock = Arc::clone(&session_lock);
        let join = std::thread::spawn(move || {
            let _guard = poisoning_lock.lock().expect("fresh lock should succeed");
            panic!("intentional panic to poison the session lock");
        });
        // Joining a panicked thread yields Err; we don't unwrap.
        let _ = join.join();
        assert!(
            session_lock.is_poisoned(),
            "session lock should be poisoned after panicking guard holder"
        );

        let _recovered_guard =
            lock_app_session_guard(&session_lock).expect("poison recovery must succeed");
        drop(_recovered_guard);

        let after = session_lock_poison_recovery_count();
        assert!(
            after > before,
            "poison-recovery counter must increment (before={before}, after={after})"
        );

        // A subsequent acquire on the same lock (still poisoned at the OS level,
        // but clearable since Rust 1.0 keeps Mutex poisoned until reconstructed)
        // must continue to succeed without returning an error.
        let _second_guard =
            lock_app_session_guard(&session_lock).expect("second acquire must also recover");
    }

    fn unique_suffix() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos()
    }

    fn isolated_test_db_path(namespace: &str, prefix: &str) -> String {
        crate::sweep_stale_test_temp();
        std::env::temp_dir()
            .join(namespace)
            .join(format!("{prefix}-{}", unique_suffix()))
            .join("omni.db")
            .to_string_lossy()
            .into_owned()
    }

    // --- Достижимость как свойство сессии (шаг 1: сессия ЗНАЕТ про подтверждение) -------------
    // docs/decisions/reachability-belongs-to-the-session.md

    fn session_reachability_test_db_path(prefix: &str) -> String {
        isolated_test_db_path("omni-session-reachability", prefix)
    }

    #[test]
    fn unanswered_probes_never_look_like_an_acknowledgement() {
        // Ровно тот случай, что дал 496 `outcome=sent` при нулевом входящем: мы шлём и шлём, а
        // подтверждения нет. Сессия обязана остаться в состоянии «не подтверждено».
        let db_path = session_reachability_test_db_path("unanswered");
        let session_key = "acct-a::dev-a";
        for index in 0..3 {
            note_session_probe_sent(&db_path, session_key, &format!("ping-{index}"), 1_000 + index)
                .expect("probe must register");
        }

        let ack = session_acknowledgement(&db_path, session_key).expect("session must be tracked");
        assert_eq!(
            ack.acknowledged_at_unix_ms, None,
            "отправка сама по себе ничего не подтверждает"
        );
        assert_eq!(ack.unanswered_probes, 3);
        assert_eq!(ack.oldest_unanswered_probe_at_unix_ms, Some(1_000));
        assert_eq!(ack.probes_sent, 3);
        assert_eq!(ack.probes_answered, 0);

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn a_pong_acknowledges_its_probe_and_everything_older() {
        let db_path = session_reachability_test_db_path("acknowledged");
        let session_key = "acct-b::dev-b";
        note_session_probe_sent(&db_path, session_key, "ping-1", 1_000).expect("probe 1");
        note_session_probe_sent(&db_path, session_key, "ping-2", 2_000).expect("probe 2");
        note_session_probe_sent(&db_path, session_key, "ping-3", 3_000).expect("probe 3");

        let correlated = note_session_probe_answered(&db_path, session_key, "ping-2", 2_180)
            .expect("pong must be recorded");
        assert!(correlated, "ответ на известную пробу обязан с ней связаться");

        let ack = session_acknowledgement(&db_path, session_key).expect("session must be tracked");
        assert_eq!(ack.acknowledged_at_unix_ms, Some(2_180));
        assert_eq!(ack.round_trip_ms, Some(180));
        assert_eq!(
            ack.unanswered_probes, 1,
            "доказанный круг перекрывает всё, что отправлено не позже отвеченной пробы"
        );
        assert_eq!(ack.oldest_unanswered_probe_at_unix_ms, Some(3_000));
        assert_eq!(ack.uncorrelated_answers, 0);

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn an_answer_without_its_probe_still_proves_the_round_trip_but_yields_no_rtt() {
        let db_path = session_reachability_test_db_path("uncorrelated");
        let session_key = "acct-c::dev-c";
        note_session_probe_sent(&db_path, session_key, "ping-1", 1_000).expect("probe 1");

        let correlated =
            note_session_probe_answered(&db_path, session_key, "approval-signal-77", 1_500)
                .expect("pong must be recorded");
        assert!(!correlated);

        let ack = session_acknowledgement(&db_path, session_key).expect("session must be tracked");
        assert_eq!(
            ack.acknowledged_at_unix_ms,
            Some(1_500),
            "pong мог появиться только если наш кадр доехал"
        );
        assert_eq!(
            ack.round_trip_ms, None,
            "без своей пробы RTT считать не из чего — врать нельзя"
        );
        assert_eq!(ack.uncorrelated_answers, 1);
        assert_eq!(
            ack.unanswered_probes, 1,
            "чужой ответ не закрывает нашу собственную неотвеченную пробу"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn a_probe_older_than_the_expiry_window_proves_nothing_when_it_finally_answers() {
        let db_path = session_reachability_test_db_path("expired");
        let session_key = "acct-d::dev-d";
        note_session_probe_sent(&db_path, session_key, "ping-old", 1_000).expect("probe");

        let answered_at = 1_000 + SESSION_PROBE_EXPIRY_MS + 1;
        let correlated = note_session_probe_answered(&db_path, session_key, "ping-old", answered_at)
            .expect("pong must be recorded");
        assert!(
            !correlated,
            "круг длиной больше окна не отличим от «канал восстановился только что»"
        );

        let ack = session_acknowledgement(&db_path, session_key).expect("session must be tracked");
        assert_eq!(ack.round_trip_ms, None);
        assert_eq!(ack.unanswered_probes, 0, "просроченная проба вычищена");

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn tracking_stays_bounded_and_keeps_the_newest_probes() {
        let db_path = session_reachability_test_db_path("bounded");
        let session_key = "acct-e::dev-e";
        let total = SESSION_PROBE_TRACKING_LIMIT + 5;
        for index in 0..total {
            note_session_probe_sent(
                &db_path,
                session_key,
                &format!("ping-{index}"),
                1_000 + index as i64,
            )
            .expect("probe must register");
        }

        let ack = session_acknowledgement(&db_path, session_key).expect("session must be tracked");
        assert_eq!(ack.unanswered_probes, SESSION_PROBE_TRACKING_LIMIT);
        assert_eq!(ack.probes_sent, total as u64);
        assert_eq!(
            ack.oldest_unanswered_probe_at_unix_ms,
            Some(1_000 + (total - SESSION_PROBE_TRACKING_LIMIT) as i64),
            "вытесняются самые старые пробы, а не самые свежие"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn a_session_we_never_probed_reports_nothing_rather_than_ok() {
        let db_path = session_reachability_test_db_path("unknown");
        assert!(
            session_acknowledgement(&db_path, "acct-f::dev-f").is_none(),
            "«не знаю» не должно кодироваться как «всё хорошо»"
        );
    }

    #[test]
    fn forgetting_a_contact_drops_every_device_session_it_owned() {
        let db_path = session_reachability_test_db_path("forget");
        note_session_probe_sent(&db_path, "acct-g::dev-1", "ping-1", 1_000).expect("probe 1");
        note_session_probe_sent(&db_path, "acct-g::dev-2", "ping-2", 1_000).expect("probe 2");
        note_session_probe_sent(&db_path, "acct-h::dev-1", "ping-3", 1_000).expect("probe 3");

        forget_session_reachability_for_remote_account(&db_path, "acct-g");

        assert!(session_acknowledgement(&db_path, "acct-g::dev-1").is_none());
        assert!(session_acknowledgement(&db_path, "acct-g::dev-2").is_none());
        assert!(
            session_acknowledgement(&db_path, "acct-h::dev-1").is_some(),
            "чужой контакт не должен пострадать"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn profiles_do_not_share_what_they_know_about_reachability() {
        let first_db_path = session_reachability_test_db_path("profile-a");
        let second_db_path = session_reachability_test_db_path("profile-b");
        let session_key = "acct-i::dev-i";
        note_session_probe_sent(&first_db_path, session_key, "ping-1", 1_000).expect("probe");
        note_session_probe_answered(&first_db_path, session_key, "ping-1", 1_100).expect("pong");

        assert!(session_acknowledgement(&second_db_path, session_key).is_none());

        clear_session_reachability_for_profile(&first_db_path);
        assert!(session_acknowledgement(&first_db_path, session_key).is_none());
    }

    // --- Троичный вердикт (шаг 2: считается, но ещё ничего не решает) -------------------------

    fn acknowledgement_at(
        acknowledged_at_unix_ms: Option<i64>,
        oldest_unanswered_probe_at_unix_ms: Option<i64>,
        probes_sent: u64,
    ) -> SessionAcknowledgement {
        SessionAcknowledgement {
            acknowledged_at_unix_ms,
            round_trip_ms: None,
            unanswered_probes: usize::from(oldest_unanswered_probe_at_unix_ms.is_some()),
            oldest_unanswered_probe_at_unix_ms,
            probes_sent,
            probes_answered: u64::from(acknowledged_at_unix_ms.is_some()),
            uncorrelated_answers: 0,
        }
    }

    #[test]
    fn a_fresh_acknowledgement_outranks_a_carrier_that_sees_no_path() {
        // Переворот, ради которого всё затевалось: доказательство сильнее способности.
        let now = 1_000_000;
        let ack = acknowledgement_at(Some(now - 1_000), None, 4);
        let assessment = assess_session_delivery(Some(&ack), false, true, now);
        assert_eq!(assessment.verdict, SessionDeliveryVerdict::DeliverNow);
        assert_eq!(
            assessment.evidence,
            SessionDeliveryEvidence::PeerAcknowledged { age_ms: 1_000 }
        );
    }

    #[test]
    fn a_dead_outbound_leg_is_not_rescued_by_a_carrier_claiming_a_live_path() {
        // Тот самый замер: 496 `carrier/out outcome=sent` при нулевом входящем. has_link=true,
        // а пробы висят без ответа — вердикт обязан перестать говорить «доставлю сейчас».
        let now = 1_000_000;
        let ack = acknowledgement_at(None, Some(now - SESSION_ACK_DEAD_MS), 9);
        let assessment = assess_session_delivery(Some(&ack), true, true, now);
        assert_eq!(assessment.verdict, SessionDeliveryVerdict::DeliverLater);
        assert_eq!(
            assessment.evidence,
            SessionDeliveryEvidence::ProbesUnanswered {
                age_ms: SESSION_ACK_DEAD_MS
            }
        );
    }

    #[test]
    fn a_dead_outbound_leg_without_durable_cannot_deliver_at_all() {
        let now = 1_000_000;
        let ack = acknowledgement_at(None, Some(now - SESSION_ACK_DEAD_MS), 9);
        let assessment = assess_session_delivery(Some(&ack), true, false, now);
        assert_eq!(assessment.verdict, SessionDeliveryVerdict::CannotDeliver);
    }

    #[test]
    fn no_evidence_yet_leaves_the_answer_to_the_carrier_rather_than_guessing() {
        // «Не знаю» не должно кодироваться ни как «всё хорошо», ни как «не доставлю»: пока
        // доказательств нет, отвечает способность несущей, и это видно в evidence.
        let now = 1_000_000;
        let with_path = assess_session_delivery(None, true, true, now);
        assert_eq!(with_path.verdict, SessionDeliveryVerdict::DeliverNow);
        assert_eq!(with_path.evidence, SessionDeliveryEvidence::NotProbed);

        let without_path = assess_session_delivery(None, false, true, now);
        assert_eq!(without_path.verdict, SessionDeliveryVerdict::DeliverLater);
        assert_eq!(without_path.evidence, SessionDeliveryEvidence::NotProbed);
    }

    #[test]
    fn a_stale_acknowledgement_stops_being_an_answer_but_stays_evidence() {
        // Между «свежо» и «мертво» серая зона: подтверждение уже не действует, но и мёртвым плечо
        // объявлять не за что — решает несущая, а evidence честно показывает возраст.
        let now = 1_000_000;
        let age_ms = SESSION_ACK_FRESH_MS + 1;
        let ack = acknowledgement_at(Some(now - age_ms), Some(now - 1_000), 5);
        let assessment = assess_session_delivery(Some(&ack), true, true, now);
        assert_eq!(assessment.verdict, SessionDeliveryVerdict::DeliverNow);
        assert_eq!(
            assessment.evidence,
            SessionDeliveryEvidence::PeerAcknowledged { age_ms }
        );

        let no_path = assess_session_delivery(Some(&ack), false, true, now);
        assert_eq!(no_path.verdict, SessionDeliveryVerdict::DeliverLater);
    }

    #[test]
    fn probes_that_went_out_and_were_never_answered_are_evidence_even_before_the_dead_threshold() {
        let now = 1_000_000;
        let ack = acknowledgement_at(None, Some(now - 1_000), 1);
        let assessment = assess_session_delivery(Some(&ack), true, true, now);
        assert_eq!(
            assessment.evidence,
            SessionDeliveryEvidence::ProbesUnanswered { age_ms: 1_000 },
            "молчание короче порога — ещё не приговор, но и не «нечего сказать»"
        );
        assert_eq!(
            assessment.verdict,
            SessionDeliveryVerdict::DeliverNow,
            "до порога решает несущая"
        );
    }

    #[test]
    fn with_a_live_path_only_a_proven_dead_leg_can_take_delivery_now_away() {
        // Защитное свойство гейта отправки (шаг 3): подавление `Sent` завязано на
        // `verdict != DeliverNow` при живом пути. Значит оно ОБЯЗАНО молчать везде, кроме
        // доказанной смерти плеча, иначе мы начнём гнать в durable исправные сообщения.
        let now = 1_000_000;
        let cases: Vec<(&str, Option<SessionAcknowledgement>)> = vec![
            ("ничего не знаем", None),
            (
                "свежее подтверждение",
                Some(acknowledgement_at(Some(now - 500), None, 3)),
            ),
            (
                "подтверждение устарело, но плечо не приговорено",
                Some(acknowledgement_at(Some(now - SESSION_ACK_FRESH_MS - 1), None, 3)),
            ),
            (
                "проба висит, но меньше порога",
                Some(acknowledgement_at(None, Some(now - SESSION_ACK_DEAD_MS + 1), 1)),
            ),
        ];
        for (label, acknowledgement) in cases {
            let assessment = assess_session_delivery(acknowledgement.as_ref(), true, true, now);
            assert_eq!(
                assessment.verdict,
                SessionDeliveryVerdict::DeliverNow,
                "при живом пути вердикт не должен отбирать доставку: {label}"
            );
        }

        let proven_dead = acknowledgement_at(None, Some(now - SESSION_ACK_DEAD_MS), 9);
        assert_ne!(
            assess_session_delivery(Some(&proven_dead), true, true, now).verdict,
            SessionDeliveryVerdict::DeliverNow,
            "а вот доказанно мёртвое плечо — обязано"
        );
    }

    #[test]
    fn the_carrier_can_reach_the_session_by_dialog_key_without_touching_storage() {
        let db_path = session_reachability_test_db_path("dialog-index");
        let dialog_key = "instance:peer-dialog-index";
        assert!(session_key_for_dialog(&db_path, dialog_key).is_none());

        link_session_to_dialog(&db_path, dialog_key, "acct-j::dev-j");
        assert_eq!(
            session_key_for_dialog(&db_path, dialog_key).as_deref(),
            Some("acct-j::dev-j")
        );

        forget_session_reachability_for_remote_account(&db_path, "acct-j");
        assert!(
            session_key_for_dialog(&db_path, dialog_key).is_none(),
            "забытый контакт не должен оставлять за собой указатель на свою сессию"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    // --- Измерение отдельно от сноса (шаг 4) ---------------------------------------------------

    #[test]
    fn a_session_we_know_nothing_about_counts_as_blind_so_we_probe() {
        // Пассивная сторона раньше не пробовала вовсе, потому что чужой трафик считался
        // доказательством. Незнание обязано означать «пробуй», а не «всё хорошо».
        let db_path = session_reachability_test_db_path("blind");
        let dialog_key = "instance:peer-blind";
        assert_eq!(
            session_acknowledgement_age_for_dialog(&db_path, dialog_key, 1_000),
            None,
            "про неизвестную сессию мы не знаем, слышат ли нас"
        );

        link_session_to_dialog(&db_path, dialog_key, "acct-k::dev-k");
        note_session_probe_sent(&db_path, "acct-k::dev-k", "ping-1", 1_000).expect("probe");
        assert_eq!(
            session_acknowledgement_age_for_dialog(&db_path, dialog_key, 1_100),
            None,
            "отправленная проба — ещё не подтверждение"
        );

        note_session_probe_answered(&db_path, "acct-k::dev-k", "ping-1", 1_100).expect("pong");
        assert_eq!(
            session_acknowledgement_age_for_dialog(&db_path, dialog_key, 1_200),
            Some(100)
        );
        assert_eq!(
            session_acknowledgement_age_for_dialog(&db_path, dialog_key, 41_100),
            Some(40_000),
            "возраст растёт, а не обнуляется молча"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn recovery_needs_proof_not_silence() {
        let db_path = session_reachability_test_db_path("recovery-proof");
        let dialog_key = "instance:peer-recovery";
        let session_key = "acct-l::dev-l";
        assert!(
            !session_outbound_leg_is_proven_dead(&db_path, dialog_key, 1_000),
            "про неизвестную сессию ничего не доказано — чинить нечего"
        );

        link_session_to_dialog(&db_path, dialog_key, session_key);
        note_session_probe_sent(&db_path, session_key, "ping-1", 1_000).expect("probe");
        assert!(
            !session_outbound_leg_is_proven_dead(&db_path, dialog_key, 1_000 + SESSION_ACK_DEAD_MS - 1),
            "молчание короче порога — не приговор"
        );
        assert!(session_outbound_leg_is_proven_dead(
            &db_path,
            dialog_key,
            1_000 + SESSION_ACK_DEAD_MS
        ));

        note_session_probe_answered(&db_path, session_key, "ping-1", 1_000 + SESSION_ACK_DEAD_MS)
            .expect("pong");
        assert!(
            !session_outbound_leg_is_proven_dead(&db_path, dialog_key, 1_000 + SESSION_ACK_DEAD_MS),
            "ответ снимает приговор"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    /// Пробы, застрявшие в очереди за мегабайтами файла, — не доказательство смерти, пока сам файл
    /// уходит: запись в поток стоит на обратном давлении получателя (живьём 2026-10-10 связь сносили
    /// посреди передачи 100 МБ).
    #[test]
    fn a_transfer_in_progress_keeps_the_link_from_being_judged_dead() {
        let db_path = session_reachability_test_db_path("recovery-progress");
        let dialog_key = "instance:peer-progress";
        let session_key = "acct-p::dev-p";
        link_session_to_dialog(&db_path, dialog_key, session_key);
        note_session_probe_sent(&db_path, session_key, "ping-1", 1_000).expect("probe");
        let judged_at = 1_000 + SESSION_ACK_DEAD_MS;
        assert!(session_outbound_leg_is_proven_dead(&db_path, dialog_key, judged_at));

        note_session_outbound_progress_for_dialog(&db_path, dialog_key, judged_at - 1);
        assert!(
            !session_outbound_leg_is_proven_dead(&db_path, dialog_key, judged_at),
            "файл продвигается — пир нас слышит"
        );
        assert!(!session_outbound_leg_is_proven_dead_for_remote_account(&db_path, "acct-p", judged_at));
        assert!(
            session_outbound_leg_is_proven_dead(&db_path, dialog_key, judged_at - 1 + SESSION_ACK_DEAD_MS),
            "передача встала — снова решают пробы"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn recovery_fires_once_per_window_not_once_per_tick() {
        // Такт очереди 250 мс. Без этой защиты снос пути повторялся бы четыре раза в секунду и
        // превратил бы лечение в ту же петлю «измерение → снос → измерение», от которой уходим.
        let db_path = session_reachability_test_db_path("recovery-rate");
        let dialog_key = "instance:peer-rate";
        let session_key = "acct-m::dev-m";
        link_session_to_dialog(&db_path, dialog_key, session_key);
        note_session_probe_sent(&db_path, session_key, "ping-1", 0).expect("probe");

        let first = SESSION_ACK_DEAD_MS;
        assert!(allow_session_recovery_attempt(&db_path, dialog_key, first));
        assert!(!allow_session_recovery_attempt(&db_path, dialog_key, first + 250));
        assert!(!allow_session_recovery_attempt(
            &db_path,
            dialog_key,
            first + SESSION_ACK_DEAD_MS - 1
        ));
        assert!(allow_session_recovery_attempt(
            &db_path,
            dialog_key,
            first + SESSION_ACK_DEAD_MS
        ));

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn a_contact_is_reachable_while_any_of_its_devices_still_answers() {
        // Вердикт — про КОНТАКТА, а не про железку. Одно молчащее устройство не делает контакта
        // недостижимым, пока отвечает другое; и наоборот, приговор требует молчания всех.
        let db_path = session_reachability_test_db_path("account-verdict");
        let account = "acct-n";
        for device in ["dev-1", "dev-2"] {
            note_session_probe_sent(&db_path, &format!("{account}::{device}"), "ping-1", 0)
                .expect("probe");
        }
        note_session_probe_answered(&db_path, &format!("{account}::dev-2"), "ping-1", 100)
            .expect("pong");

        let now = SESSION_ACK_DEAD_MS + 1;
        assert!(
            !session_outbound_leg_is_proven_dead_for_remote_account(&db_path, account, now),
            "одно устройство молчит, второе ответило — контакт достижим"
        );

        // Теперь замолчали оба: у dev-2 подтверждение устарело и его проба висит.
        note_session_probe_sent(&db_path, &format!("{account}::dev-2"), "ping-2", 100)
            .expect("probe");
        assert!(session_outbound_leg_is_proven_dead_for_remote_account(
            &db_path,
            account,
            100 + SESSION_ACK_DEAD_MS
        ));

        assert!(
            !session_outbound_leg_is_proven_dead_for_remote_account(&db_path, "acct-other", now),
            "про чужой аккаунт ничего не доказано"
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn a_contacts_freshest_device_defines_how_long_ago_we_were_heard() {
        let db_path = session_reachability_test_db_path("account-age");
        let account = "acct-o";
        note_session_probe_sent(&db_path, &format!("{account}::dev-1"), "ping-1", 0).expect("probe");
        note_session_probe_answered(&db_path, &format!("{account}::dev-1"), "ping-1", 1_000)
            .expect("pong");
        note_session_probe_sent(&db_path, &format!("{account}::dev-2"), "ping-1", 0).expect("probe");
        note_session_probe_answered(&db_path, &format!("{account}::dev-2"), "ping-1", 5_000)
            .expect("pong");

        assert_eq!(
            session_acknowledgement_age_for_remote_account(&db_path, account, 6_000),
            Some(1_000),
            "берётся самое свежее подтверждение среди устройств"
        );
        assert_eq!(
            session_acknowledgement_age_for_remote_account(&db_path, "acct-none", 6_000),
            None
        );

        clear_session_reachability_for_profile(&db_path);
    }

    #[test]
    fn recovery_is_refused_for_a_dialog_that_maps_to_no_session() {
        let db_path = session_reachability_test_db_path("recovery-unknown");
        assert!(
            !allow_session_recovery_attempt(&db_path, "instance:peer-unknown", 1_000),
            "без сессии чинить нечего, и разрешения быть не должно"
        );
    }

    // --- Повтор не должен жечь счётчик сессии --------------------------------------------------
    // docs/bugs/session-wedged-by-counter-gap.md

    fn wire_envelope_for_cache_test(marker: &str) -> omni_types::WireEnvelopeV1 {
        omni_types::WireEnvelopeV1::chat_text(marker, marker, 1)
    }

    fn cached_envelope_marker(envelope: &omni_types::WireEnvelopeV1) -> Option<String> {
        match &envelope.payload {
            WirePayloadV1::ChatText(text) => Some(text.message_id.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_pending_envelope_is_reused_until_the_session_changes() {
        // Пересборка на каждую попытку тратила счётчик; при такте ретраев ~4/с окно приёма пира (64)
        // вылетало за ~16 секунд, и дальше отвергалось ВСЁ, включая presence-пинги. Замер на живом
        // стенде: 955 отказов «counter exceeded the supported receive gap» при идеальном транспорте.
        let db_path = isolated_test_db_path("omni-outgoing-cache", "reuse");
        let envelope = wire_envelope_for_cache_test("msg-1");

        assert!(
            cached_outgoing_envelope(&db_path, "msg-1", "acct-a::dev-a").is_none(),
            "пустой кэш ничего не отдаёт"
        );

        remember_outgoing_envelope(&db_path, "msg-1", "acct-a::dev-a", &envelope);
        let reused = cached_outgoing_envelope(&db_path, "msg-1", "acct-a::dev-a");
        assert_eq!(
            reused.as_ref().and_then(cached_envelope_marker),
            Some("msg-1".to_string()),
            "та же сессия — тот же конверт"
        );

        // ВНИМАНИЕ: здесь меняется device id, а это НЕ то, что происходит при пересборке сессии —
        // устройство пира остаётся тем же. Настоящий признак пересборки проверяет
        // `session_instance_key_changes_when_the_session_is_rebuilt`; без него этот assert был
        // зелёным и на сломанном кэше.
        assert!(
            cached_outgoing_envelope(&db_path, "msg-1", "acct-a::dev-NEW").is_none(),
            "другой пир — чужой конверт отдавать нельзя"
        );

        forget_outgoing_envelope(&db_path, "msg-1");
        assert!(
            cached_outgoing_envelope(&db_path, "msg-1", "acct-a::dev-a").is_none(),
            "подтверждённое сообщение не должно оставаться в кэше"
        );

        clear_outgoing_envelopes_for_profile(&db_path);
    }

    /// Пересборка сессии обязана обесценивать запомненный конверт.
    ///
    /// Кэш инвалидировался по `SessionAddress::storage_key` — паре (account_id, device_id). Она
    /// пересборку ПЕРЕЖИВАЕТ: у пира тот же аккаунт и то же устройство, меняются только ключи.
    /// Поэтому после пересборки повтор доставал из кэша конверт под мёртвыми ключами, приёмник
    /// отвечал `authentication failed`, и так бесконечно — на живом двухмашинном прогоне 51 отказ
    /// подряд и очередь, которая не разбиралась вовсе.
    /// См. docs/bugs/queue-never-drains-after-relink.md.
    #[test]
    fn session_instance_key_changes_when_the_session_is_rebuilt() {
        let alice = create_stage5_peer("rxwin-key-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("rxwin-key-bob", "Bob", "Bob PC");
        let bob_bootstrap = bob
            .app
            .bootstrap_session_from_prekey_bundle(&alice.bundle)
            .unwrap();
        transport_accept_session_bootstrap(&alice.app.storage, &bob_bootstrap).unwrap();

        let session = alice
            .app
            .storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: bob.account.descriptor.account_id.clone(),
                remote_device_id: bob.device.device_id.clone(),
            })
            .unwrap()
            .unwrap();

        // Пересобранная сессия: тот же пир, новый ратчет-ключ (X3DH рождает его заново).
        let mut rebuilt = session.clone();
        let mut fresh = rebuilt.local_ratchet_public_key.0;
        fresh[0] ^= 0xff;
        rebuilt.local_ratchet_public_key = omni_crypto::EphemeralPublicKey(fresh);

        assert_eq!(
            session.session_address.storage_key(),
            rebuilt.session_address.storage_key(),
            "адрес пира пересборку переживает — именно поэтому по нему нельзя различать сессии"
        );
        assert_ne!(
            crate::session_instance_key(&session),
            crate::session_instance_key(&rebuilt),
            "ключ ЭКЗЕМПЛЯРА сессии обязан смениться, иначе кэш отдаст конверт под мёртвыми ключами"
        );
    }

    #[test]
    fn in_flight_envelopes_are_counted_per_session_so_the_sender_can_respect_the_window() {
        // Отправитель обязан держать число РАЗНЫХ неподтверждённых конвертов в пределах окна приёма
        // (`MAX_RECEIVE_GAP`): выложив больше, он гарантированно выносит окно, и сессия заклинивает —
        // отвергается уже всё, включая presence-пинги. Живой замер 2026-08-14: очередь из 200
        // сообщений дала разрыв 377 при окне 64, и ровно этим ломалась сама пересборка — новая сессия
        // немедленно перешифровывала всю очередь и выносила окно заново.
        let db_path = isolated_test_db_path("omni-outgoing-cache", "in-flight");
        assert_eq!(
            outgoing_envelopes_in_flight_for_session(&db_path, "acct-a::dev-a"),
            0
        );

        for index in 0..5 {
            let id = format!("msg-{index}");
            remember_outgoing_envelope(
                &db_path,
                &id,
                "acct-a::dev-a",
                &wire_envelope_for_cache_test(&id),
            );
        }
        // Конверт другой сессии в счёт этой не идёт: у каждой сессии своё окно.
        remember_outgoing_envelope(
            &db_path,
            "msg-other",
            "acct-a::dev-OTHER",
            &wire_envelope_for_cache_test("msg-other"),
        );

        assert_eq!(
            outgoing_envelopes_in_flight_for_session(&db_path, "acct-a::dev-a"),
            5,
            "считаем только конверты этой сессии"
        );

        // Расписка о доставке освобождает место — иначе противодавление стало бы вечным.
        forget_outgoing_envelope(&db_path, "msg-0");
        assert_eq!(
            outgoing_envelopes_in_flight_for_session(&db_path, "acct-a::dev-a"),
            4,
            "подтверждённое сообщение перестаёт занимать окно"
        );

        clear_outgoing_envelopes_for_profile(&db_path);
        assert_eq!(
            outgoing_envelopes_in_flight_for_session(&db_path, "acct-a::dev-a"),
            0
        );
    }

    #[test]
    fn the_envelope_cache_stays_bounded_and_keeps_the_newest() {
        let db_path = isolated_test_db_path("omni-outgoing-cache", "bounded");
        let total = OUTGOING_ENVELOPE_CACHE_CAP + 10;
        for index in 0..total {
            let id = format!("msg-{index}");
            remember_outgoing_envelope(&db_path, &id, "acct-b::dev-b", &wire_envelope_for_cache_test(&id));
        }

        assert!(
            cached_outgoing_envelope(&db_path, "msg-0", "acct-b::dev-b").is_none(),
            "самые старые вытесняются — их сообщения просто пересоберутся"
        );
        let newest = format!("msg-{}", total - 1);
        assert!(
            cached_outgoing_envelope(&db_path, &newest, "acct-b::dev-b").is_some(),
            "самые свежие остаются"
        );

        clear_outgoing_envelopes_for_profile(&db_path);
    }

    #[test]
    fn profiles_do_not_share_pending_envelopes() {
        let first = isolated_test_db_path("omni-outgoing-cache", "profile-a");
        let second = isolated_test_db_path("omni-outgoing-cache", "profile-b");
        remember_outgoing_envelope(&first, "msg-1", "acct-c::dev-c", &wire_envelope_for_cache_test("msg-1"));

        assert!(cached_outgoing_envelope(&second, "msg-1", "acct-c::dev-c").is_none());

        clear_outgoing_envelopes_for_profile(&first);
        assert!(cached_outgoing_envelope(&first, "msg-1", "acct-c::dev-c").is_none());
    }

    // --- Приёмник просит пересборку сессии (часть 2) --------------------------------------------

    #[test]
    fn one_counter_gap_is_not_yet_a_resync_request() {
        // Одиночный разрыв нормален: конверт разъехался путями, пир перезапустился между отправками.
        // Просить пересборку на каждый отвергнутый конверт значило бы заменить один шторм другим —
        // их приходят сотни (955 на живом замере).
        let db_path = isolated_test_db_path("omni-session-resync", "threshold");
        let dialog = "instance:peer-resync";
        for _ in 1..SESSION_RESYNC_GAP_THRESHOLD {
            assert!(!note_session_counter_gap(&db_path, dialog, 1_000));
        }
        assert!(
            note_session_counter_gap(&db_path, dialog, 1_000),
            "накопился порог — пора просить"
        );

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn an_accepted_payload_resets_the_gap_streak() {
        // Считаем ПОДРЯД идущие разрывы: один принятый конверт означает, что сессия жива, и
        // накопленное больше ничего не доказывает.
        let db_path = isolated_test_db_path("omni-session-resync", "streak");
        let dialog = "instance:peer-streak";
        for _ in 1..SESSION_RESYNC_GAP_THRESHOLD {
            note_session_counter_gap(&db_path, dialog, 1_000);
        }
        note_session_payload_accepted(&db_path, dialog);
        assert!(
            !note_session_counter_gap(&db_path, dialog, 1_000),
            "после принятого конверта счёт начинается заново"
        );

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn a_hostile_peer_cannot_force_endless_session_rebuilds() {
        // Реакция на разрыв счётчика неизбежно срабатывает ДО проверки подлинности: чтобы проверить
        // MAC, надо довести цепочку ключей до счётчика конверта, а именно этого при разрыве и не
        // выходит. Значит одобренный, но враждебный контакт может вызывать пересборку, не подделав ни
        // одной подписи. Частоты мало — она размазывает нагрузку, но не ограничивает её объём.
        let db_path = isolated_test_db_path("omni-session-resync", "budget");
        let dialog = "instance:peer-hostile";
        let mut allowed = 0;
        let mut now = 1_000i64;

        // Давим бесконечно, соблюдая паузы, чтобы упереться именно в бюджет, а не в частоту.
        for _ in 0..(SESSION_REBUILD_BUDGET * 4) {
            for _ in 0..SESSION_RESYNC_GAP_THRESHOLD {
                note_session_counter_gap(&db_path, dialog, now);
            }
            if note_session_counter_gap(&db_path, dialog, now) {
                allowed += 1;
            }
            now += SESSION_RESYNC_MIN_INTERVAL_MS * 2;
        }

        assert!(
            allowed <= SESSION_REBUILD_BUDGET,
            "бюджет пересборок обязан ограничивать ОБЩЕЕ число, а не только частоту (разрешено {allowed})"
        );

        // Через окно бюджет восстанавливается — иначе одна враждебная вспышка навсегда лишала бы
        // диалог самолечения. Смотрим на СЕРИЮ, а не на конкретный вызов: разрешение достаётся
        // первому подошедшему, а следующий за ним в ту же миллисекунду отсекается уже по частоте.
        now += SESSION_REBUILD_BUDGET_WINDOW_MS * 2;
        let mut allowed_after_window = false;
        for _ in 0..=SESSION_RESYNC_GAP_THRESHOLD {
            if note_session_counter_gap(&db_path, dialog, now) {
                allowed_after_window = true;
            }
        }
        assert!(
            allowed_after_window,
            "после окна бюджет обязан восстановиться"
        );

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn diverged_session_auth_failures_ask_for_a_rebuild() {
        // Разошедшаяся сессия (стороны вывели разные ключи) не расшифровывает НИ ОДНОГО кадра —
        // «authentication failed» подряд. Раньше на это не реагировал никто, и пара молчала навсегда
        // (docs/bugs/repair-on-restart-diverges-session.md, часть 2).
        let db_path = isolated_test_db_path("omni-session-resync", "auth-diverged");
        let dialog = "instance:peer-diverged";
        let created = 1_000;
        let now = created + SESSION_RESYNC_FRESH_SESSION_GRACE_MS + 1;
        for _ in 1..SESSION_RESYNC_AUTH_FAILURE_THRESHOLD {
            assert!(!note_session_auth_failure(&db_path, dialog, now, created), "одиночные провалы — ещё не повод");
        }
        assert!(note_session_auth_failure(&db_path, dialog, now, created), "порог набран — пересобираем");

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn auth_failures_on_a_fresh_session_or_between_accepted_frames_do_not_rebuild() {
        // Отрицательная половина. Только что собранная сессия ещё получает кадры прошлой: считай мы
        // их — пересборка вызывала бы следующую.
        let db_path = isolated_test_db_path("omni-session-resync", "auth-fresh");
        let dialog = "instance:peer-fresh";
        let created = 1_000;
        let within_grace = created + SESSION_RESYNC_FRESH_SESSION_GRACE_MS - 1;
        for _ in 0..(SESSION_RESYNC_AUTH_FAILURE_THRESHOLD * 3) {
            assert!(!note_session_auth_failure(&db_path, dialog, within_grace, created));
        }
        // Живая сессия с редким битым кадром: принятый кадр обнуляет счёт подряд.
        let later = created + SESSION_RESYNC_FRESH_SESSION_GRACE_MS * 2;
        for _ in 0..(SESSION_RESYNC_AUTH_FAILURE_THRESHOLD * 3) {
            assert!(!note_session_auth_failure(&db_path, dialog, later, created));
            note_session_payload_accepted(&db_path, dialog);
        }
        // Порог выше, чем у разрыва счётчика: столько провалов подряд ещё не доказательство.
        for _ in 0..SESSION_RESYNC_GAP_THRESHOLD {
            assert!(!note_session_auth_failure(&db_path, dialog, later, created));
        }

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn resync_requests_are_rate_limited_per_dialog() {
        let db_path = isolated_test_db_path("omni-session-resync", "rate");
        let dialog = "instance:peer-rate";
        for _ in 0..SESSION_RESYNC_GAP_THRESHOLD {
            note_session_counter_gap(&db_path, dialog, 1_000);
        }
        assert!(!note_session_counter_gap(&db_path, dialog, 1_001));
        assert!(!note_session_counter_gap(
            &db_path,
            dialog,
            1_000 + SESSION_RESYNC_MIN_INTERVAL_MS - 1
        ));
        assert!(note_session_counter_gap(
            &db_path,
            dialog,
            1_000 + SESSION_RESYNC_MIN_INTERVAL_MS
        ));

        clear_session_resync_for_profile(&db_path);
    }

    #[test]
    fn dialogs_and_profiles_count_their_gaps_separately() {
        let first = isolated_test_db_path("omni-session-resync", "iso-a");
        let second = isolated_test_db_path("omni-session-resync", "iso-b");
        for _ in 0..SESSION_RESYNC_GAP_THRESHOLD {
            note_session_counter_gap(&first, "instance:peer-1", 1_000);
        }
        assert!(
            !note_session_counter_gap(&first, "instance:peer-2", 1_000),
            "чужой диалог не должен наследовать чужой счёт"
        );
        assert!(!note_session_counter_gap(&second, "instance:peer-1", 1_000));

        clear_session_resync_for_profile(&first);
        clear_session_resync_for_profile(&second);
    }

    fn tiny_png_bytes() -> &'static [u8] {
        &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
            0x89, 0x00, 0x00, 0x00, 0x01, 0x73, 0x52, 0x47,
            0x42, 0x00, 0xAE, 0xCE, 0x1C, 0xE9, 0x00, 0x00,
            0x00, 0x04, 0x67, 0x41, 0x4D, 0x41, 0x00, 0x00,
            0xB1, 0x8F, 0x0B, 0xFC, 0x61, 0x05, 0x00, 0x00,
            0x00, 0x09, 0x70, 0x48, 0x59, 0x73, 0x00, 0x00,
            0x0E, 0xC3, 0x00, 0x00, 0x0E, 0xC3, 0x01, 0xC7,
            0x6F, 0xA8, 0x64, 0x00, 0x00, 0x00, 0x0D, 0x49,
            0x44, 0x41, 0x54, 0x18, 0x57, 0x63, 0xF8, 0xCF,
            0xC0, 0xF0, 0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF,
            0xA6, 0x5C, 0x9B, 0x5D, 0x00, 0x00, 0x00, 0x00,
            0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    // --- One Omni-ID on several equal devices (docs/decisions/multi-device-equal-nodes.md) -------

    /// A second device of `peer`'s account: its profile folder copied to another place, opened.
    fn copy_to_second_device(peer: &Stage5Peer, label: &str) -> OmniApp {
        let copy_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("{label}-{}", unique_suffix()))
            .join("omni.db");
        fs::create_dir_all(copy_path.parent().unwrap()).unwrap();
        peer.app
            .export_current_database_backup_bundle(&copy_path)
            .expect("the folder copies");
        // On another computer the DPAPI seals of the folder do not open: the profile secrets (root
        // secret, phrase) are unreadable there. Same machine in the test, so break the seal the
        // way another machine would see it — the copy must work without them.
        let connection = omni_storage::Storage::open_keyed_connection(&copy_path).unwrap();
        let sealed = connection
            .execute(
                "UPDATE profile_secrets SET profile_master_key_blob_hex = '00'
                 WHERE profile_master_key_blob_hex IS NOT NULL",
                [],
            )
            .unwrap();
        assert_eq!(sealed, 1, "the copy carries the sealed profile secrets");
        drop(connection);
        OmniApp::init(AppInitRequest {
            db_path: copy_path.to_string_lossy().into_owned(),
        })
        .expect("the copy opens as a new device")
    }

    /// A folder under a password, copied to another computer: the password opens it and certifies
    /// the new device, but does NOT open the root secret — that stays sealed to the machine that made
    /// it, or a thief with the folder and the password could out-migrate the owner. The phrase opens
    /// the copy too, and seals the root again for the new machine.
    #[test]
    fn a_password_folder_on_another_computer_opens_without_giving_up_the_root() {
        let alice = create_stage5_peer("md-pw-alice", "Alice", "Alice Laptop");
        let profile_id = alice.account.local_profile_summary.as_ref().unwrap().profile_id.clone();
        let phrase = alice.account.recovery_phrase_v2.clone().expect("the laptop holds the phrase");
        alice.app.set_local_profile_password(&profile_id, "correct-password").unwrap();

        let desktop = copy_to_second_device(&alice, "md-pw-desktop");
        assert!(
            desktop.storage.get_local_device_certificate_chain().unwrap().is_none(),
            "the working key waits for the password"
        );
        desktop.unlock_profile_with_password(&profile_id, "correct-password").unwrap();
        assert!(desktop.storage.get_local_device_certificate_chain().unwrap().is_some());
        assert!(
            desktop
                .reveal_current_profile_recovery_phrase_with_password("correct-password")
                .is_err(),
            "the folder and the password must not yield the phrase"
        );
        assert!(desktop.storage.get_local_account().unwrap().unwrap().recovery_phrase_v2.is_none());

        let tablet = copy_to_second_device(&alice, "md-pw-tablet");
        tablet.unlock_profile_with_mnemonic(&profile_id, &phrase.mnemonic).unwrap();
        assert!(tablet.storage.get_local_device_certificate_chain().unwrap().is_some());
        assert_eq!(
            tablet
                .reveal_current_profile_recovery_phrase_with_password("correct-password")
                .unwrap()
                .mnemonic,
            phrase.mnemonic,
            "the phrase sealed the root for this machine"
        );
        assert_eq!(
            tablet.storage.get_local_account().unwrap().unwrap().recovery_phrase_v2,
            Some(phrase)
        );
    }

    /// A new device of an approved contact may reach us first with a bare session bootstrap — no
    /// hello, relayed by another node — whose receive context has the device's keys and certificate
    /// chain but only a placeholder prekey. It is still that contact's device: no contact request,
    /// the contact stays approved and bound where it was (live 2026-10-07 it became a fresh request
    /// and the contact was rebound to the new device). A device without a valid chain claiming the
    /// same account is turned away without touching the contact.
    #[test]
    fn a_bare_bootstrap_from_a_new_device_of_a_contact_is_not_a_contact_request() {
        let alice = create_stage5_peer("md-bare-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-bare-bob", "Bob", "Bob PC");
        let approved = link_stage5_contact(&bob.app, &alice, "Alice");
        let desktop = copy_to_second_device(&alice, "md-bare-desktop");
        let bare_context = |mut bundle: PrekeyBundleV1, instance_id: String| {
            // What `received_context_from_bootstrap` builds: real keys and chain, placeholder prekey.
            bundle.signed_prekey.public_key = String::new();
            bundle.signed_prekey.signature_r = String::new();
            bundle.signed_prekey.signature_s = String::new();
            bundle.one_time_prekey = None;
            omni_transport::ReceivedChatContext {
                peer_id: String::new(),
                dialog_key: dialog_key_from_instance_id(&instance_id),
                remote_instance_id: instance_id,
                remote_prekey_bundle: Some(bundle),
                initiated_locally: false,
            }
        };

        let desktop_instance = desktop.storage.get_or_create_local_instance_id().unwrap();
        let context = bare_context(desktop.get_local_prekey_bundle().unwrap(), desktop_instance);
        let contact = crate::persist_incoming_contact_request_signal(&bob.app.storage, &context, current_unix_ms())
            .unwrap()
            .expect("Alice's device resolves to Alice");
        assert_eq!(contact.contact_id, approved.contact_id);
        assert_eq!(contact.relationship_state, ContactRelationshipState::Approved);
        let contacts = bob.app.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1, "no second contact for Alice");
        assert_eq!(contacts[0].remote_instance_id, approved.remote_instance_id, "not rebound");

        // A stranger's device that names Alice's account but whose chain is its own.
        let mallory = create_stage5_peer("md-bare-mallory", "Mallory", "Mallory PC");
        let mut forged = mallory.bundle.clone();
        forged.account.account_id = alice.account.descriptor.account_id.0.clone();
        forged.device.account_id = alice.account.descriptor.account_id.0.clone();
        let context = bare_context(forged, mallory.instance_id.clone());
        assert!(crate::persist_incoming_contact_request_signal(&bob.app.storage, &context, current_unix_ms())
            .unwrap()
            .is_none());
        let contacts = bob.app.list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].relationship_state, ContactRelationshipState::Approved);
        assert_eq!(contacts[0].remote_instance_id, approved.remote_instance_id);
    }

    /// docs/decisions/attachment-content-tree.md, «свои устройства получателя»: a file Bob downloaded
    /// on his PC comes to his laptop from the PC — even with Alice away; a file the PC does not have
    /// sends the laptop on to Alice at once, not after a stall.
    #[test]
    fn a_file_comes_from_another_device_of_the_receiver_that_has_it() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("own-src-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("own-src-bob", "Bob", "Bob PC");
        // Bob's laptop as a second device comes about: his PC talks with Alice, its folder is
        // copied, the copy says hello to her (the setup `a_second_device_of_a_contact_…` uses).
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        bob.app.send_text(&alice.dialog_key, "from the pc").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 1);
        let laptop = copy_to_second_device(&bob, "own-src-laptop");
        link_apps_over_mesh(&laptop, &alice.app);
        link_apps_over_mesh(&laptop, &bob.app);
        laptop.send_text(&alice.dialog_key, "the laptop is here").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 2);
        // Both of Bob's devices hear Alice.
        alice.app.send_text(&bob.dialog_key, "to both of you").unwrap();
        for device in [&bob.app, &laptop] {
            wait_for_condition(Duration::from_secs(20), || {
                device.list_messages(&alice.dialog_key).is_ok_and(|history| history.messages.iter().any(|m| m.body == "to both of you"))
            });
        }

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let suffix = unique_suffix();
        let send = |body: &str, seed: usize| {
            let source = dir.join(format!("own-src-{seed}-{suffix}.bin"));
            let payload: Vec<u8> = (0..2 * 1024 * 1024 + seed).map(|at| ((at * 7 + seed) % 251) as u8).collect();
            fs::write(&source, &payload).unwrap();
            alice
                .app
                .send_attachment(&bob.dialog_key, body, source.to_string_lossy().as_ref(), None, omni_types::AttachmentMediaProtectionMode::E2eeMedia)
                .unwrap();
            payload
        };
        let row = |app: &OmniApp, body: &str| {
            app.list_messages(&alice.dialog_key)
                .ok()
                .and_then(|history| history.messages.into_iter().find(|message| message.body == body))
        };
        let state = |app: &OmniApp, body: &str| row(app, body).and_then(|message| message.attachment).map(|attachment| attachment.state);
        let offered_at_both = |body: &str| {
            wait_for_state(
                Duration::from_secs(30),
                || format!("{body}: at the pc {:?}, at the laptop {:?}", state(&bob.app, body), state(&laptop, body)),
                || state(&bob.app, body).is_some() && state(&laptop, body) == Some(omni_types::AttachmentTransferState::Offered),
            );
            row(&laptop, body).unwrap().id
        };

        // Not on the PC: the PC says so, and the laptop goes to Alice at once.
        let first = send("not on the pc", 1);
        let first_id = offered_at_both("not on the pc");
        let at_laptop = dir.join(format!("own-src-laptop-1-{suffix}.bin"));
        let asked = Instant::now();
        laptop.request_attachment_download(&first_id, at_laptop.to_string_lossy().as_ref()).unwrap();
        wait_for_state(
            Duration::from_secs(30),
            || format!("laptop download {:?}", state(&laptop, "not on the pc")),
            || at_laptop.exists() && state(&laptop, "not on the pc") == Some(omni_types::AttachmentTransferState::Downloaded),
        );
        assert_eq!(fs::read(&at_laptop).unwrap(), first);
        assert!(asked.elapsed() < Duration::from_secs(15), "not after a stall: {:?}", asked.elapsed());

        // On the PC, Alice away: from the PC.
        let second = send("on the pc", 2);
        let second_id = offered_at_both("on the pc");
        let at_pc = dir.join(format!("own-src-pc-2-{suffix}.bin"));
        bob.app.request_attachment_download(&second_id, at_pc.to_string_lossy().as_ref()).unwrap();
        wait_for_condition(Duration::from_secs(30), || state(&bob.app, "on the pc") == Some(omni_types::AttachmentTransferState::Downloaded));
        let alice_hex = alice.app.mesh_node_address_hex().unwrap().expect("alice's mesh is up");
        alice.app.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || !laptop.mesh_has_link(&alice_hex).unwrap_or(true));
        let at_laptop = dir.join(format!("own-src-laptop-2-{suffix}.bin"));
        laptop.request_attachment_download(&second_id, at_laptop.to_string_lossy().as_ref()).unwrap();
        wait_for_state(
            Duration::from_secs(30),
            || format!("laptop download {:?}", state(&laptop, "on the pc")),
            || at_laptop.exists() && state(&laptop, "on the pc") == Some(omni_types::AttachmentTransferState::Downloaded),
        );
        assert_eq!(fs::read(&at_laptop).unwrap(), second);
        // Alice's dialog being down does not set the download back to «waiting» while the PC streams
        // it — that asked the PC again every tick until the first chunk came.
        let asked = crate::download_requests_sent_for(&laptop.storage.db_path().to_string_lossy(), &second_id);
        assert!(asked <= 2, "the laptop asked for the file {asked} times");
        laptop.stop_mesh();
        bob.app.stop_mesh();
    }

    /// docs/bugs/contact-device-missed-while-session-built.md: Bob's laptop is a copy of his PC's
    /// folder, made before he and Alice exchanged a word — Alice has no session with the PC yet. Her
    /// first message goes out at once to the laptop (its session came up with its hello) and waits on
    /// a session for the PC; reaching one device made it `sent`, and the PC, its link up all along,
    /// never got it. The session coming up now hands the PC what it missed.
    #[test]
    fn a_contact_device_whose_session_comes_up_late_still_gets_the_message() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("late-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("late-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let laptop = copy_to_second_device(&bob, "late-laptop");
        link_apps_over_mesh(&laptop, &alice.app);
        link_apps_over_mesh(&laptop, &bob.app);
        laptop.send_text(&alice.dialog_key, "the laptop is here").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 1);

        alice.app.send_text(&bob.dialog_key, "to both").unwrap();
        let has = |app: &OmniApp| {
            app.list_messages(&alice.dialog_key)
                .map(|history| history.messages.iter().any(|message| message.body == "to both"))
                .unwrap_or(false)
        };
        wait_for_state(
            Duration::from_secs(15),
            || format!("the pc has it: {}, the laptop: {}", has(&bob.app), has(&laptop)),
            || has(&bob.app) && has(&laptop),
        );
        laptop.stop_mesh();
        bob.app.stop_mesh();
        alice.app.stop_mesh();
    }

    /// docs/decisions/attachment-content-tree.md, 3.3: Alice's laptop sent a file and her desktop
    /// downloaded its copy. The laptop goes away: Bob's download goes to the desktop, which hands its
    /// copy on — the file does not wait for the device that sent it.
    #[test]
    fn a_file_comes_from_the_senders_other_device_when_the_sending_one_is_away() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-file-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-file-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let desktop = copy_to_second_device(&alice, "md-file-desktop");
        link_apps_over_mesh(&desktop, &bob.app);
        link_apps_over_mesh(&desktop, &alice.app);
        desktop.send_text(&bob.dialog_key, "the desktop is here").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let suffix = unique_suffix();
        let source = dir.join(format!("md-file-source-{suffix}.bin"));
        let payload: Vec<u8> = (0..3 * 1024 * 1024 + 99).map(|at| (at % 241) as u8).collect();
        fs::write(&source, &payload).unwrap();
        alice
            .app
            .send_attachment(&bob.dialog_key, "from the laptop", source.to_string_lossy().as_ref(), None, omni_types::AttachmentMediaProtectionMode::E2eeMedia)
            .unwrap();
        let row = |app: &OmniApp, dialog: &str| {
            app.list_messages(dialog)
                .ok()
                .and_then(|history| history.messages.into_iter().find(|message| message.body == "from the laptop"))
        };
        let state = |app: &OmniApp, dialog: &str| row(app, dialog).and_then(|message| message.attachment).map(|attachment| attachment.state);

        // The desktop gets the copy and fetches its file from the laptop.
        wait_for_condition(Duration::from_secs(30), || state(&desktop, &bob.dialog_key).is_some());
        let message_id = row(&desktop, &bob.dialog_key).unwrap().id;
        let at_desktop = dir.join(format!("md-file-desktop-{suffix}.bin"));
        desktop.request_attachment_download(&message_id, at_desktop.to_string_lossy().as_ref()).unwrap();
        wait_for_state(
            Duration::from_secs(30),
            || format!("desktop copy {:?}", state(&desktop, &bob.dialog_key)),
            || state(&desktop, &bob.dialog_key) == Some(omni_types::AttachmentTransferState::Downloaded),
        );

        // The laptop goes away; Bob asks.
        wait_for_condition(Duration::from_secs(30), || state(&bob.app, &alice.dialog_key) == Some(omni_types::AttachmentTransferState::Offered));
        let laptop = alice.app.mesh_node_address_hex().unwrap().expect("the laptop's mesh is up");
        alice.app.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || !bob.app.mesh_has_link(&laptop).unwrap_or(true));
        let at_bob = dir.join(format!("md-file-bob-{suffix}.bin"));
        bob.app.request_attachment_download(&message_id, at_bob.to_string_lossy().as_ref()).unwrap();
        wait_for_state(
            Duration::from_secs(30),
            || format!("bob's download {:?}", row(&bob.app, &alice.dialog_key).and_then(|m| m.attachment).map(|a| (a.state, a.bytes_transferred))),
            || at_bob.exists() && state(&bob.app, &alice.dialog_key) == Some(omni_types::AttachmentTransferState::Downloaded),
        );
        assert_eq!(fs::read(&at_bob).unwrap(), payload);
        desktop.stop_mesh();
        bob.app.stop_mesh();
    }

    #[test]
    fn a_second_device_of_a_contact_writes_into_the_same_chat() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        alice.app.send_text(&bob.dialog_key, "from the laptop").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);

        let desktop = copy_to_second_device(&alice, "md-alice-desktop");
        assert_ne!(
            desktop.get_local_device().unwrap().unwrap().device_id,
            alice.device.device_id,
            "the copy is another device"
        );
        // Without the root secret it is still the same account, with the same public identity and
        // Omni-ID — only the phrase is not on it.
        let desktop_account = desktop.storage.get_local_account().unwrap().unwrap();
        let laptop_account = alice.app.storage.get_local_account().unwrap().unwrap();
        assert!(desktop_account.recovery_phrase_v2.is_none());
        assert_eq!(desktop_account.public_summary, laptop_account.public_summary);
        assert_eq!(desktop.get_my_omni_id().unwrap(), alice.app.get_my_omni_id().unwrap());
        link_apps_over_mesh(&desktop, &bob.app);
        desktop.send_text(&bob.dialog_key, "from the desktop").unwrap();

        // Bob still has ONE Alice and ONE chat with her, holding both messages.
        wait_for_message_count(&bob.app, &alice.dialog_key, 2);
        assert_eq!(bob.app.list_contacts().unwrap().len(), 1);

        // Bob answers once; BOTH of Alice's devices get it.
        bob.app.send_text(&alice.dialog_key, "to both of you").unwrap();
        // (The laptop does not have the desktop's own message yet: copies to own devices are 2.4.)
        for (device, expected) in [(&alice.app, 2), (&desktop, 3)] {
            wait_for_message_count(device, &bob.dialog_key, expected);
            assert!(device
                .list_messages(&bob.dialog_key)
                .unwrap()
                .messages
                .iter()
                .any(|message| message.body == "to both of you"));
        }

        // The desktop goes offline; Bob writes; the laptop confirms it. Back online, the desktop
        // still gets it — the sender catches a returning device up.
        desktop.stop_mesh();
        bob.app.send_text(&alice.dialog_key, "while you were away").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 3);
        wait_for_message_status(
            &bob.app,
            &alice.dialog_key,
            "while you were away",
            MessageStatus::Delivered,
        );
        // Catching up a device that is still away does not put the delivered message back in the
        // queue: no "sending" spinner that only that device's receipt could ever clear.
        let away_device = desktop.get_local_device().unwrap().unwrap().device_id;
        let away_link = bob
            .app
            .storage
            .list_contact_device_dialogs(&bob.app.list_contacts().unwrap()[0].contact_id)
            .unwrap()
            .into_iter()
            .find(|link| link.remote_device_id.as_ref() == Some(&away_device))
            .expect("Bob knows the desktop's dialog");
        crate::catch_up_contact_device(&bob.app.storage, &bob.app.transport, &away_link.dialog_key, &away_device)
            .unwrap();
        assert!(bob
            .app
            .list_messages(&alice.dialog_key)
            .unwrap()
            .messages
            .iter()
            .any(|message| message.body == "while you were away" && message.status == MessageStatus::Delivered));
        assert_eq!(bob.app.queue_diagnostics().unwrap().queue_length, 0);
        link_apps_over_mesh(&desktop, &bob.app);
        wait_for_message_count(&desktop, &bob.dialog_key, 4);

        // Alice's own devices: what one of them sends, the other shows as sent too.
        link_apps_over_mesh(&desktop, &alice.app);
        alice.app.send_text(&bob.dialog_key, "sent from the laptop").unwrap();
        wait_for_condition(Duration::from_secs(15), || {
            desktop.list_messages(&bob.dialog_key).is_ok_and(|history| {
                history.messages.iter().any(|message| {
                    message.body == "sent from the laptop"
                        && message.direction == ChatDirection::Outgoing
                })
            })
        });
        // Bob confirms to every device of Alice: the desktop's copy turns delivered too.
        wait_for_message_status(&desktop, &bob.dialog_key, "sent from the laptop", MessageStatus::Delivered);
        // A receipt can beat the copy it confirms; the copy then lands delivered.
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        alice.app.storage.record_delivery_receipt("md-early-receipt", &bob.dialog_key, 5, 6).unwrap();
        assert!(alice
            .app
            .storage
            .insert_sent_copy("md-early-receipt", &bob.dialog_key, "late copy", 4, &desktop_id)
            .unwrap());
        assert!(alice
            .app
            .list_messages(&bob.dialog_key)
            .unwrap()
            .messages
            .iter()
            .any(|message| message.id == "md-early-receipt" && message.status == MessageStatus::Delivered));

        // Rendezvous keeps every device reachable across networks, each on its own pair topic:
        // Bob has both of Alice's devices, and each Alice device has Bob and her other device.
        for (app, expected) in [(&bob.app, 2), (&alice.app, 2), (&desktop, 2)] {
            let peers = crate::rendezvous_peers(&app.storage);
            assert_eq!(peers.len(), expected, "{peers:?}");
            let secrets: std::collections::HashSet<_> = peers.iter().map(|peer| peer.secret).collect();
            assert_eq!(secrets.len(), expected, "each device has its own pair topic");
        }
        // Bob's presence probe on each of Alice's dialogs goes under THAT device's session and is
        // answered: neither device looks dead. Live 2026-10-07 the probe took the account's latest
        // session and went out on another device's dialog — undecryptable there, so the laptop's
        // leg was "proven dead" and a call to Alice was dropped as never placed.
        let bob_db = bob.app.storage.db_path().to_string_lossy().to_string();
        let alice_contact = bob.app.list_contacts().unwrap().remove(0);
        let alice_account = alice.account.descriptor.account_id.clone();
        for link in bob.app.storage.list_contact_device_dialogs(&alice_contact.contact_id).unwrap() {
            let device = link.remote_device_id.clone().expect("each dialog knows its device");
            let session = bob
                .app
                .storage
                .get_session_for_dialog(&alice_account, &link.dialog_key)
                .unwrap()
                .expect("a session for each device");
            assert_eq!(session.session_address.remote_device_id, device);
            crate::send_presence_ping_to_session(
                &bob.app.storage,
                &bob.app.transport,
                &alice_contact,
                &link.dialog_key,
                current_unix_ms(),
            )
            .unwrap();
            wait_for_condition(Duration::from_secs(10), || {
                crate::session_delivery_for_dialog(&bob_db, &link.dialog_key, true, true).verdict
                    == SessionDeliveryVerdict::DeliverNow
                    && crate::session_key_for_dialog(&bob_db, &link.dialog_key).as_deref()
                        == Some(session.session_address.storage_key().as_str())
            });
        }
        // Per-account rendezvous names each of Alice's devices with ITS node and pair secret — the
        // on-demand dial used the latest bundle's node with the latest session's secret, which with
        // two devices could belong to different ones and never meet.
        let alice_peers = crate::rendezvous_peers_for_account(&bob.app.storage, &alice_account);
        assert_eq!(alice_peers.len(), 2, "{alice_peers:?}");
        let topics = bob
            .app
            .rendezvous_topic_for_contact(&alice_account)
            .unwrap()
            .expect("Alice's devices have pair topics");
        let per_device = crate::rendezvous_topics(&alice_peers[0].secret, current_unix_ms() as u64).len();
        assert_eq!(topics.len(), 2 * per_device, "topics of both devices");
        // The chat is secure with Alice while it is secure with either of her devices.
        assert!(bob
            .app
            .get_chat_security_summary(alice.dialog_key.as_str())
            .unwrap()
            .has_established_session);

        // Devices that never met find each other on the account's own topic: both of Alice's
        // devices hold its secret, Bob's account has another.
        let account_topic = alice.app.storage.get_account_rendezvous_secret().unwrap();
        assert!(account_topic.is_some());
        assert_eq!(desktop.storage.get_account_rendezvous_secret().unwrap(), account_topic);
        assert_ne!(bob.app.storage.get_account_rendezvous_secret().unwrap(), account_topic);

        // Bob calls Alice: both of her devices ring. The desktop answers — Bob's call goes on with
        // the desktop, and the laptop stops ringing.
        let call = bob.app.place_call(&alice.dialog_key).unwrap();
        for device in [&alice.app, &desktop] {
            wait_for_condition(Duration::from_secs(10), || {
                current_call(device).is_some_and(|incoming| {
                    incoming.call_id == call.call_id && incoming.state == CallState::Ringing
                })
            });
        }
        desktop.accept_call(&call.call_id).unwrap();
        wait_for_condition(Duration::from_secs(10), || {
            current_call(&bob.app).is_some_and(|ongoing| ongoing.state == CallState::Active)
        });
        let desktop_dialog =
            dialog_key_from_instance_id(&desktop.storage.get_or_create_local_instance_id().unwrap());
        assert_eq!(current_call(&bob.app).unwrap().dialog_key, desktop_dialog);
        wait_for_condition(Duration::from_secs(10), || current_call(&alice.app).is_none());
        bob.app.hang_up_call(&call.call_id).unwrap();
        // The call was with the desktop, and it is in Bob's one chat with Alice.
        assert!(bob
            .app
            .list_messages(&alice.dialog_key)
            .unwrap()
            .messages
            .iter()
            .any(|entry| entry.call_history_event.is_some()));
    }

    /// A file one of Alice's devices sends to Bob shows on her other device too, and that device
    /// fetches it from the device that sent it — not from Bob, who may never download it at all.
    /// The file streams to whoever asked, so only the chat's contact and our own devices may ask.
    #[test]
    fn a_file_sent_from_one_device_downloads_on_another_from_the_sender() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-file-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-file-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let desktop = copy_to_second_device(&alice, "md-file-desktop");
        // Bob and the desktop have a session (as on the live stand): receipts to the sender's other
        // devices go only over established sessions.
        link_apps_over_mesh(&desktop, &bob.app);
        desktop.send_text(&bob.dialog_key, "the desktop says hi").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);
        link_apps_over_mesh(&desktop, &alice.app);

        let dir = std::env::temp_dir().join("omni-stage-5-app-tests");
        fs::create_dir_all(&dir).unwrap();
        let source_path = dir.join(format!("md-file-source-{}.bin", unique_suffix()));
        let save_path = dir.join(format!("md-file-desktop-{}.bin", unique_suffix()));
        let payload: Vec<u8> = (0..(1024 * 1024 + 77)).map(|index| (index % 241) as u8).collect();
        fs::write(&source_path, &payload).unwrap();
        let sent = alice
            .app
            .send_attachment(
                bob.dialog_key.as_str(),
                "",
                source_path.to_string_lossy().as_ref(),
                None,
                omni_types::AttachmentMediaProtectionMode::E2eeMedia,
            )
            .unwrap();

        let copy_state = |state: omni_types::AttachmentTransferState| {
            desktop.list_messages(&bob.dialog_key).is_ok_and(|history| {
                history.messages.iter().any(|message| {
                    message.id == sent.id
                        && message.direction == ChatDirection::Outgoing
                        && message.attachment.as_ref().is_some_and(|attachment| attachment.state == state)
                })
            })
        };
        wait_for_condition(Duration::from_secs(30), || {
            copy_state(omni_types::AttachmentTransferState::Offered)
        });
        // Bob's receipt for a message with a file reaches every device of Alice, as for text (live
        // 2026-10-08 the copy stayed "sent": only text sent receipts to the other devices).
        wait_for_condition(Duration::from_secs(30), || {
            desktop.list_messages(&bob.dialog_key).is_ok_and(|history| {
                history
                    .messages
                    .iter()
                    .any(|message| message.id == sent.id && message.status == MessageStatus::Delivered)
            })
        });
        desktop
            .request_attachment_download(sent.id.as_str(), save_path.to_string_lossy().as_ref())
            .unwrap();
        wait_for_condition(Duration::from_secs(60), || {
            save_path.exists() && copy_state(omni_types::AttachmentTransferState::Downloaded)
        });
        assert_eq!(fs::read(&save_path).unwrap(), payload);
        // The laptop served its own device: that is not Bob downloading the file.
        let laptop_attachment = alice
            .app
            .list_messages(&bob.dialog_key)
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.id == sent.id)
            .and_then(|message| message.attachment)
            .expect("the laptop keeps its file");
        assert_eq!(laptop_attachment.state, omni_types::AttachmentTransferState::Available);

        let address = |account_id: &AccountId, device_id: &DeviceId| {
            omni_crypto::SessionAddress {
                remote_account_id: account_id.clone(),
                remote_device_id: device_id.clone(),
            }
        };
        let allowed = |requester| crate::attachment_requester_allowed(&alice.app.storage, &sent.id, &requester).unwrap();
        let desktop_device = desktop.get_local_device().unwrap().unwrap().device_id;
        assert!(allowed(address(&alice.account.descriptor.account_id, &desktop_device)));
        assert!(allowed(address(&bob.account.descriptor.account_id, &bob.device.device_id)));
        let mallory = create_stage5_peer("md-file-mallory", "Mallory", "Mallory PC");
        assert!(
            !allowed(address(&mallory.account.descriptor.account_id, &mallory.device.device_id)),
            "a file sent to Bob does not stream to someone else"
        );
    }

    /// The account takes its desktop back: the laptop stops routing to it, the revoked device and the
    /// contact both hear it from the laptop, and what the revoked device sends afterwards is not
    /// listened to.
    #[test]
    fn a_revoked_device_is_dropped_by_its_other_devices_and_its_contacts() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-revoke-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-revoke-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let desktop = copy_to_second_device(&alice, "md-revoke-desktop");
        link_apps_over_mesh(&desktop, &bob.app);
        link_apps_over_mesh(&desktop, &alice.app);
        desktop.send_text(&bob.dialog_key, "before the revocation").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);

        let account_id = alice.account.descriptor.account_id.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().is_ok_and(|devices| {
                devices.iter().any(|(device_id, _)| device_id == &desktop_id)
            })
        });

        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        assert!(alice.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap());
        assert!(alice.app.storage.list_own_device_dialogs().unwrap().is_empty());

        // Bob and the desktop itself hear it from the laptop.
        wait_for_condition(Duration::from_secs(30), || {
            bob.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap_or(false)
        });
        // The desktop learns it was taken back and erases its profile at once.
        wait_for_condition(Duration::from_secs(30), || {
            desktop.storage.get_local_account().is_ok_and(|account| account.is_none())
        });

        alice.app.send_text(&bob.dialog_key, "from the laptop").unwrap();
        wait_for_condition(Duration::from_secs(30), || {
            bob.app.list_messages(&alice.dialog_key).is_ok_and(|history| {
                history.messages.iter().any(|message| message.body == "from the laptop")
            })
        });
        // What the revoked device sent is not listened to; the laptop still is. (The desktop has
        // erased itself by now, so the guard is exercised on bob's side directly.)
        let address = |device_id: &DeviceId| omni_crypto::SessionAddress {
            remote_account_id: account_id.clone(),
            remote_device_id: device_id.clone(),
        };
        let text = omni_types::WirePayloadV1::ChatText(omni_types::ChatTextV1 {
            message_id: "m-after".to_string(),
            body: "after the revocation".to_string(),
            created_at_unix_ms: 1,
        });
        let revoked_session = bob.app.storage.get_session_record(&address(&desktop_id)).unwrap().unwrap();
        assert!(crate::sender_is_revoked_device(&bob.app.storage, &revoked_session, &text).unwrap());
        let laptop_session = bob
            .app
            .storage
            .get_session_for_dialog(&account_id, &alice.dialog_key)
            .unwrap()
            .expect("bob talks to the laptop on its dialog");
        assert_ne!(laptop_session.session_address.remote_device_id, desktop_id);
        assert!(!crate::sender_is_revoked_device(&bob.app.storage, &laptop_session, &text).unwrap());
        // ...except that it may still be told it was taken back.
        let revocation_notice = omni_types::WirePayloadV1::LogicalEvent(
            omni_types::LogicalEventEnvelopeV1::device_revocation("{}".to_string()),
        );
        assert!(!crate::sender_is_revoked_device(&bob.app.storage, &revoked_session, &revocation_notice).unwrap());

        let history = bob.app.list_messages(&alice.dialog_key).unwrap();
        assert!(history.messages.iter().any(|message| message.body == "before the revocation"));
    }

    /// Two devices that are linked but have never exchanged a message have no session to carry a
    /// revocation (found on a two-instance run, 2026-10-09). Taking the second one back starts the
    /// session, and the revocation follows it.
    #[test]
    fn a_revocation_reaches_a_device_that_has_never_talked_to_the_revoker() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-revoke-fresh-alice", "Alice", "Alice Laptop");
        let desktop = copy_to_second_device(&alice, "md-revoke-fresh-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().is_ok_and(|devices| {
                devices.iter().any(|(device_id, _)| device_id == &desktop_id)
            })
        });

        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();

        wait_for_condition(Duration::from_secs(60), || {
            desktop.storage.get_local_account().is_ok_and(|account| account.is_none())
        });
        assert!(alice.app.storage.get_local_account().unwrap().is_some());
    }

    /// A contact that was away when a device was revoked hears it when it is next reachable.
    #[test]
    fn a_contact_that_was_offline_learns_of_a_revocation_when_it_returns() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-revoke-late-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-revoke-late-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let desktop = copy_to_second_device(&alice, "md-revoke-late-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        alice.app.send_text(&bob.dialog_key, "so that bob and alice have a session").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);
        let account_id = alice.account.descriptor.account_id.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().is_ok_and(|devices| {
                devices.iter().any(|(device_id, _)| device_id == &desktop_id)
            })
        });

        // Bob is away while the desktop is taken back.
        bob.app.stop_mesh();
        thread::sleep(Duration::from_millis(500));
        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        assert!(!bob.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap());

        // Bob comes back (dialed at the address of his fresh mesh, as the other tests link nodes).
        link_apps_over_mesh(&alice.app, &bob.app);
        wait_for_condition(Duration::from_secs(60), || {
            bob.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap_or(false)
        });

        // The frame held for him while he was away is not the only way he hears: every revocation
        // goes to each of his devices once per live link (the held frame does not cover a contact
        // that had no session with us at the time, or a hold that expired).
        let bob_account = bob.account.descriptor.account_id.clone();
        let bob_device = bob.app.get_local_device().unwrap().unwrap().device_id;
        // On a miss: Alice's dialogs with Bob's devices (is the link up?) and her session with him.
        let describe = || {
            let Ok(Some(contact)) = alice.app.storage.find_contact_by_remote_account_id(&bob_account) else {
                return "Bob is not a contact of Alice".to_string();
            };
            let dialogs: Vec<String> = alice
                .app
                .storage
                .list_contact_device_dialogs(&contact.contact_id)
                .unwrap_or_default()
                .into_iter()
                .map(|dialog| {
                    format!(
                        "{} device={:?} link={}",
                        dialog.dialog_key,
                        dialog.remote_device_id,
                        alice.app.transport.connection_snapshot_for_dialog(&dialog.dialog_key).is_some()
                    )
                })
                .collect();
            let session = alice
                .app
                .storage
                .get_session_record(&omni_crypto::SessionAddress {
                    remote_account_id: bob_account.clone(),
                    remote_device_id: bob_device.clone(),
                })
                .map(|record| record.map(|record| format!("{:?}", record.bootstrap_state)));
            format!("dialogs={dialogs:?} alice->bob session={session:?}")
        };
        // Told as soon as his dialog is up. Not at once: the frame held for him rides the node and
        // gets through the moment the link is back, the dialog (its hello, the session over it) a
        // moment later — under load the pass ran in between and had nowhere to send (full core gate
        // 2026-10-15; reproduced with every core busy).
        let told = std::cell::RefCell::new(std::collections::HashSet::new());
        wait_for_state(Duration::from_secs(30), describe, || {
            crate::tell_contacts_about_revocations(&alice.app.storage, &alice.app.transport, &mut told.borrow_mut()) >= 1
        });
        assert_eq!(
            crate::tell_contacts_about_revocations(&alice.app.storage, &alice.app.transport, &mut told.borrow_mut()),
            0,
            "and only once per link"
        );
        // Its link drops and returns: told again.
        bob.app.stop_mesh();
        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            crate::tell_contacts_about_revocations(&alice.app.storage, &alice.app.transport, &mut told.borrow_mut()),
            0,
            "nothing to send down a dead link"
        );
        link_apps_over_mesh(&alice.app, &bob.app);
        wait_for_condition(Duration::from_secs(30), || {
            crate::tell_contacts_about_revocations(&alice.app.storage, &alice.app.transport, &mut told.borrow_mut()) >= 1
        });
    }

    /// The account key is stolen: the owner moves to working key 2 keeping the desktop. The desktop
    /// gets the new key, the contact learns the new certificate (not the key), and the device the
    /// owner did not keep — a "thief" holding the old key — is out for the owner and for the contact.
    #[test]
    fn moving_the_account_to_a_new_working_key_keeps_the_desktop_and_drops_the_thief() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-move-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-move-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let desktop = copy_to_second_device(&alice, "md-move-desktop");
        let thief = copy_to_second_device(&alice, "md-move-thief");
        for device in [&desktop, &thief] {
            link_apps_over_mesh(device, &alice.app);
            link_apps_over_mesh(device, &bob.app);
        }
        // The thief has talked to Bob: Bob knows its device.
        thief.send_text(&bob.dialog_key, "hello from the thief").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);
        // The laptop and Bob have talked: there is a session to carry the move to him.
        alice.app.send_text(&bob.dialog_key, "hello from the laptop").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 2);

        let account_id = alice.account.descriptor.account_id.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        let thief_id = thief.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            let devices = alice.app.storage.list_own_device_dialogs().unwrap_or_default();
            [&desktop_id, &thief_id]
                .iter()
                .all(|wanted| devices.iter().any(|(device_id, _)| device_id == *wanted))
        });
        assert!(!bob.app.storage.is_device_revoked(&account_id, &thief_id).unwrap());

        let summary = alice
            .app
            .move_account_to_new_working_key(std::slice::from_ref(&desktop_id.0), None, None)
            .unwrap();
        assert_eq!(summary.new_working_key_version, 2);
        assert_eq!(summary.kept_devices, 2); // the laptop and the desktop

        // The thief is out on the laptop at once.
        assert!(alice.app.storage.is_device_revoked(&account_id, &thief_id).unwrap());
        assert!(!alice.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap());

        // The desktop got the move and the new working key: it signs with version 2.
        wait_for_condition(Duration::from_secs(60), || {
            desktop
                .storage
                .get_account_keys(&account_id)
                .ok()
                .flatten()
                .is_some_and(|keys| keys.working_certificate.version == 2)
        });
        let desktop_chain = desktop.storage.get_local_device_certificate_chain().unwrap().unwrap();
        assert_eq!(desktop_chain.working.version, 2);

        // Bob heard the move (without the key): version 2, the thief dropped, the desktop kept.
        wait_for_condition(Duration::from_secs(60), || {
            bob.app.storage.current_working_key_version(&account_id).ok().flatten() == Some(2)
        });
        assert!(bob.app.storage.is_device_revoked(&account_id, &thief_id).unwrap());
        assert!(!bob.app.storage.is_device_revoked(&account_id, &desktop_id).unwrap());
        assert!(
            bob.app.storage.get_account_keys(&account_id).unwrap().is_none(),
            "a contact never holds the account's working key"
        );
    }

    /// What a folder already holds when it is copied was sent by the device it came from; the copy
    /// must not hand those messages back to that device as "copies".
    #[test]
    fn a_copied_folder_does_not_echo_its_inherited_messages_back_to_their_author() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-inherit-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-inherit-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        let sent = alice.app.send_text(&bob.dialog_key, "written before the copy").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);
        // Still in the queue when the folder is copied (a dialog nobody answers, so it stays there):
        // nothing confirmed it, and a confirmation by our own device would read as "served" to the
        // queue's fallback target and strand it.
        alice
            .app
            .storage
            .insert_outgoing_message(
                "md-inherit-queued",
                "dialog:instance:md-inherit-nobody",
                "still queued at the copy",
                MessageStatus::Queued,
                current_unix_ms(),
            )
            .unwrap();

        let desktop = copy_to_second_device(&alice, "md-inherit-desktop");
        let laptop_id = alice.device.device_id.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        assert_ne!(laptop_id, desktop_id);

        // The laptop has the message by definition: the desktop does not owe it a copy.
        assert!(desktop.storage.message_delivered_devices(&sent.id).unwrap().contains(&laptop_id));
        assert!(desktop.storage.message_delivered_devices("md-inherit-queued").unwrap().is_empty());
    }

    /// Contacts carry over between our devices (multi-device-equal-nodes.md, «Синхронизация
    /// контактов»): a contact approved on the laptop appears on a desktop that never met it, with its
    /// devices; a rename follows; and a removal made while the desktop was away reaches it when it is
    /// back.
    #[test]
    fn a_contact_carries_over_to_our_other_device_and_its_removal_reaches_it_after_it_was_away() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-csync-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-csync-bob", "Bob", "Bob PC");
        // The desktop is a copy of the folder made before Alice knew Bob.
        let desktop = copy_to_second_device(&alice, "md-csync-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            alice
                .app
                .storage
                .list_own_device_dialogs()
                .unwrap_or_default()
                .iter()
                .any(|(device_id, _)| device_id == &desktop_id)
        });

        let bob_on_laptop = link_stage5_contact(&alice.app, &bob, "Bob");
        let bob_account = bob.account.descriptor.account_id.clone();
        let bob_on_desktop = || desktop.storage.find_contact_by_remote_account_id(&bob_account).ok().flatten();
        wait_for_condition(Duration::from_secs(90), || {
            bob_on_desktop().is_some_and(|contact| {
                contact.relationship_state == ContactRelationshipState::Approved && contact.petname == "Bob"
            })
        });
        assert!(
            !desktop
                .storage
                .list_remote_prekey_bundles_for_account(&bob_account)
                .unwrap()
                .is_empty(),
            "the desktop knows Bob's device, so it can reach him"
        );

        alice.app.rename_contact(&bob_on_laptop.contact_id, "Bobby").unwrap();
        wait_for_condition(Duration::from_secs(90), || {
            bob_on_desktop().is_some_and(|contact| contact.petname == "Bobby")
        });

        // The desktop goes away; Alice removes Bob on the laptop meanwhile.
        let desktop_hex = desktop.mesh_node_address_hex().unwrap().expect("desktop mesh is up");
        desktop.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || {
            !alice.app.mesh_has_link(&desktop_hex).unwrap_or(true)
        });
        alice.app.forget_contact(&bob_on_laptop.contact_id).unwrap();
        thread::sleep(Duration::from_secs(2));
        assert!(bob_on_desktop().is_some(), "the desktop was away: it cannot have heard yet");

        // Back: the removal reaches it, and the desktop does not bring Bob back to the laptop.
        link_apps_over_mesh(&desktop, &alice.app);
        wait_for_condition(Duration::from_secs(90), || bob_on_desktop().is_none());
        thread::sleep(Duration::from_secs(2));
        assert!(alice.app.storage.find_contact_by_remote_account_id(&bob_account).unwrap().is_none());
    }

    /// Linked contacts learn where each other posts and looks, and hear again when that changes: Alice
    /// turns ntfy to "only read", and Bob learns she no longer posts there but still looks.
    #[test]
    fn linked_contacts_learn_each_others_couriers_and_hear_a_change() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("courier-set-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("courier-set-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        // A courier set rides the pair's session: one message each way sets it up.
        alice.app.send_text(bob.dialog_key.as_str(), "hi Bob").unwrap();
        wait_for_message_status(&alice.app, bob.dialog_key.as_str(), "hi Bob", MessageStatus::Delivered);
        let alice_account = alice.account.descriptor.account_id.clone();
        let alice_device = alice.app.get_local_device().unwrap().unwrap().device_id;
        let heard = || bob.app.storage.get_peer_courier_set(&alice_account, &alice_device).ok().flatten();
        wait_for_condition(Duration::from_secs(60), || {
            heard().is_some_and(|set| set.publish.contains(&"https://ntfy.sh".to_string()))
        });
        let bob_account = bob.account.descriptor.account_id.clone();
        let bob_device = bob.app.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(60), || {
            alice.app.storage.get_peer_courier_set(&bob_account, &bob_device).ok().flatten().is_some()
        });

        let ntfy = alice
            .app
            .list_rendezvous_providers()
            .unwrap()
            .into_iter()
            .find(|p| p.url == "https://ntfy.sh")
            .unwrap();
        alice.app.set_rendezvous_provider_mode(ntfy.id, "read").unwrap();
        wait_for_condition(Duration::from_secs(60), || {
            heard().is_some_and(|set| {
                !set.publish.contains(&"https://ntfy.sh".to_string()) && set.read.contains(&"https://ntfy.sh".to_string())
            })
        });
    }

    /// A pair goes where the other device looks and looks where it posts: its own server (not in our
    /// list) becomes a pair-only courier, a courier we only read is not posted to, one we turned off
    /// is used for nobody — and when nothing is common one way, that way falls back to our own set and
    /// says so (for the contact's warning).
    #[test]
    fn a_pair_goes_where_the_other_device_looks_and_looks_where_it_posts() {
        let alice = create_stage5_peer("pair-couriers-alice", "Alice", "Alice PC");
        let storage = &alice.app.storage;
        let bob = AccountId("acc-bob".into());
        let phone = DeviceId("bob-phone".into());
        assert_eq!(crate::pair_couriers(storage, &bob, &phone), crate::PairCouriers::default(), "untold: our own set");

        let set = |publish: &[&str], read: &[&str]| omni_storage::PeerCourierSet {
            publish: publish.iter().map(|s| s.to_string()).collect(),
            read: read.iter().map(|s| s.to_string()).collect(),
        };
        storage
            .record_peer_courier_set(
                &bob,
                &phone,
                &set(&["https://bob.example", "wss://nos.lol"], &["https://bob.example", "wss://nostr.mom", "https://ntfy.sh"]),
                1_000,
            )
            .unwrap();
        assert!(
            alice.app.configured_rendezvous_providers().contains(&"https://bob.example#pair".to_string()),
            "Bob's own server is a pair-only courier for us"
        );
        let mode = |url: &str, mode: &str| {
            let id = alice.app.list_rendezvous_providers().unwrap().into_iter().find(|p| p.url == url).unwrap().id;
            alice.app.set_rendezvous_provider_mode(id, mode).unwrap();
        };
        mode("https://ntfy.sh", "read");
        let pair = crate::pair_couriers(storage, &bob, &phone);
        assert_eq!(
            pair.publish_to.as_deref(),
            Some(&["https://bob.example".to_string(), "wss://nostr.mom".to_string()][..]),
            "where Bob looks — not ntfy, which we only read"
        );
        assert_eq!(
            pair.read_from.as_deref(),
            Some(&["https://bob.example".to_string(), "wss://nos.lol".to_string()][..]),
            "where Bob posts"
        );

        mode("wss://nos.lol", "off");
        storage
            .record_peer_courier_set(&bob, &phone, &set(&["wss://nos.lol"], &["wss://nos.lol", "https://ntfy.sh"]), 2_000)
            .unwrap();
        let pair = crate::pair_couriers(storage, &bob, &phone);
        assert!(pair.known && !pair.publish_common && !pair.read_common, "{pair:?}");
        assert_eq!((pair.publish_to, pair.read_from), (None, None), "nothing common: our own set");
    }

    /// With no courier in common, the contact's connection state says so and names the courier of
    /// ours to turn on; turning it on clears the warning.
    #[test]
    fn a_contact_without_a_common_courier_is_flagged_with_what_to_turn_on() {
        let alice = create_stage5_peer("courier-gap-alice", "Alice", "Alice PC");
        let bob = create_stage5_peer("courier-gap-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let bob_account = bob.account.descriptor.account_id.clone();
        let bob_device = bob.app.get_local_device().unwrap().unwrap().device_id;
        let only_mom = omni_storage::PeerCourierSet {
            publish: vec!["wss://nostr.mom".to_string()],
            read: vec!["wss://nostr.mom".to_string()],
        };
        alice.app.storage.record_peer_courier_set(&bob_account, &bob_device, &only_mom, 1_000).unwrap();
        assert_eq!(crate::contact_courier_gap(&alice.app.storage, &bob_account), (false, Vec::new()));

        let mom = alice
            .app
            .list_rendezvous_providers()
            .unwrap()
            .into_iter()
            .find(|p| p.url == "wss://nostr.mom")
            .unwrap();
        alice.app.set_rendezvous_provider_mode(mom.id, "off").unwrap();
        assert_eq!(
            crate::contact_courier_gap(&alice.app.storage, &bob_account),
            (true, vec!["wss://nostr.mom".to_string()])
        );
        alice.app.set_rendezvous_provider_mode(mom.id, "read").unwrap();
        assert_eq!(
            crate::contact_courier_gap(&alice.app.storage, &bob_account),
            (true, vec!["wss://nostr.mom".to_string()]),
            "we look there now, but Bob looks only there and we do not post: still one way short"
        );
        alice.app.set_rendezvous_provider_mode(mom.id, "publish_read").unwrap();
        assert_eq!(crate::contact_courier_gap(&alice.app.storage, &bob_account), (false, Vec::new()));
    }

    /// A courier in memory for the account's mailbox (topic → posted blobs).
    #[derive(Default)]
    struct MailboxCourier {
        topics: Mutex<std::collections::HashMap<String, Vec<String>>>,
    }

    impl crate::RendezvousCourier for MailboxCourier {
        fn publish(
            &self,
            topic_hex: &str,
            _dhtkey_hex: &str,
            blob_hex: &str,
            _ttl_secs: u32,
            _priority: crate::PublishPriority,
        ) -> crate::OmniResult<()> {
            self.topics.lock().unwrap().entry(topic_hex.to_string()).or_default().push(blob_hex.to_string());
            Ok(())
        }
        fn fetch(&self, topic_hex: &str, _dhtkey_hex: &str, _legs: crate::CourierLegs) -> crate::OmniResult<Vec<String>> {
            Ok(self.topics.lock().unwrap().get(topic_hex).cloned().unwrap_or_default())
        }
    }

    /// The laptop removes Bob and is switched off before the desktop comes back: no device of ours is
    /// there to tell it, but the note the laptop left at the couriers is. A note whose entries were
    /// changed after signing, and the desktop's own note, change nothing.
    #[test]
    fn a_removal_reaches_our_device_through_the_account_mailbox_while_the_remover_is_off() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-mailbox-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-mailbox-bob", "Bob", "Bob PC");
        let carol = create_stage5_peer("md-mailbox-carol", "Carol", "Carol PC");
        let desktop = copy_to_second_device(&alice, "md-mailbox-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        let bob_on_laptop = link_stage5_contact(&alice.app, &bob, "Bob");
        link_stage5_contact(&alice.app, &carol, "Carol");
        let bob_account = bob.account.descriptor.account_id.clone();
        let carol_account = carol.account.descriptor.account_id.clone();
        let on_desktop = |account: &omni_identity::AccountId| desktop.storage.find_contact_by_remote_account_id(account).ok().flatten();
        wait_for_condition(Duration::from_secs(90), || {
            on_desktop(&bob_account).is_some() && on_desktop(&carol_account).is_some()
        });

        let desktop_hex = desktop.mesh_node_address_hex().unwrap().expect("desktop mesh is up");
        desktop.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || {
            !alice.app.mesh_has_link(&desktop_hex).unwrap_or(true)
        });
        alice.app.forget_contact(&bob_on_laptop.contact_id).unwrap();

        let courier = MailboxCourier::default();
        let secret = alice.app.storage.get_account_rendezvous_secret().unwrap().unwrap();
        assert_eq!(desktop.storage.get_account_rendezvous_secret().unwrap(), Some(secret));
        let now = crate::current_unix_ms();
        let note = crate::build_account_mailbox_note(&alice.app.storage, now).expect("a removal is worth a note");
        assert_eq!(crate::publish_account_mailbox_with(&courier, &secret, &note, now), 1);

        // A forgery: the laptop's note, its removal turned onto Carol.
        let mut forged: crate::AccountMailboxNoteV1 = serde_json::from_slice(&note).unwrap();
        forged.entries[0].account_id = carol_account.0.clone();
        let forged = serde_json::to_vec(&forged).unwrap();
        // The desktop's own note (nothing to say yet, so a hand-made one that would remove Carol).
        let mut own = forged.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        let mut own_note: crate::AccountMailboxNoteV1 = serde_json::from_slice(&own).unwrap();
        own_note.device_id = desktop_id.0.clone();
        own = serde_json::to_vec(&own_note).unwrap();
        for content in [&forged, &own] {
            crate::publish_account_mailbox_with(&courier, &secret, content, now);
        }

        let desktop_db = desktop.storage.db_path().to_string_lossy().to_string();
        let notes = crate::read_account_mailbox_with(&courier, &secret, now + 60_000);
        assert_eq!(notes.len(), 3);
        crate::with_account_mailbox(&desktop_db, |state| state.inbox.extend(notes));
        crate::apply_account_mailbox_inbox(&desktop.storage, &desktop.transport);

        assert!(on_desktop(&bob_account).is_none(), "the removal came through the mailbox");
        assert!(on_desktop(&carol_account).is_some(), "neither the forgery nor our own note removed Carol");
        assert_eq!(
            desktop.storage.get_contact_sync_record(&bob_account).unwrap().unwrap().state,
            omni_storage::ContactSyncState::Removed
        );
    }

    /// After the start, a contact's devices are sent to only once the beacons of every pair with its
    /// account were read and what they pointed to is applied — or 15 s passed.
    #[test]
    fn sending_to_a_contact_waits_for_its_pairs_beacons_after_the_start() {
        let _guard = network_test_guard();
        let (laptop, desktop) = ([1u8; 32], [2u8; 32]);
        let mut state = crate::RevocationNoticeState::default();
        let start = 1_000_000;
        assert!(!crate::revocations_settled_in(&mut state, &[laptop, desktop], start), "nothing read yet");
        state.beacons_read.insert(laptop);
        assert!(!crate::revocations_settled_in(&mut state, &[laptop, desktop], start + 1_000), "one pair of two");
        state.beacons_read.insert(desktop);
        assert!(crate::revocations_settled_in(&mut state, &[laptop, desktop], start + 2_000));
        state.inbox.push((desktop, sample_device_revocation()));
        assert!(!crate::revocations_settled_in(&mut state, &[laptop, desktop], start + 3_000), "read, not applied");
        assert!(crate::revocations_settled_in(&mut state, &[laptop, desktop], start + 15_000), "the grace ran out");
        let mut fresh = crate::RevocationNoticeState::default();
        assert!(crate::revocations_settled_in(&mut fresh, &[], start), "an account without pairs");
    }

    /// After a key move, what the old key revoked is not passed on any more (whoever applies it refuses
    /// it); what the new key revokes is.
    #[test]
    fn revocations_under_an_older_working_key_are_not_passed_on() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-oldrev-alice", "Alice", "Alice Laptop");
        let account = alice.account.descriptor.account_id.clone();
        let mut ids = Vec::new();
        let mut copies = Vec::new();
        for name in ["md-oldrev-desktop", "md-oldrev-tablet"] {
            let copy = copy_to_second_device(&alice, name);
            let id = copy.get_local_device().unwrap().unwrap().device_id;
            link_apps_over_mesh(&copy, &alice.app);
            wait_for_condition(Duration::from_secs(30), || {
                alice.app.storage.list_own_device_dialogs().unwrap_or_default().iter().any(|(known, _)| known == &id)
            });
            ids.push(id);
            copies.push(copy);
        }
        alice.app.revoke_own_device(&ids[0].0, None).unwrap();
        assert_eq!(alice.app.storage.list_device_revocations_under_current_key(&account).unwrap().len(), 1);
        alice.app.move_account_to_new_working_key(&[ids[1].0.clone()], None, None).unwrap();
        assert_eq!(alice.app.storage.list_device_revocations(&account).unwrap().len(), 1, "still on record");
        assert!(alice.app.storage.list_device_revocations_under_current_key(&account).unwrap().is_empty());
        alice.app.revoke_own_device(&ids[1].0, None).unwrap();
        let current = alice.app.storage.list_device_revocations_under_current_key(&account).unwrap();
        assert_eq!(current.iter().map(|revocation| revocation.device_id.clone()).collect::<Vec<_>>(), vec![ids[1].clone()]);
    }

    /// A device we took back is passed over on the account's topic: it still posts there.
    #[test]
    fn a_revoked_own_device_is_not_dialed_on_the_account_topic() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-acctskip-alice", "Alice", "Alice Laptop");
        let desktop = copy_to_second_device(&alice, "md-acctskip-desktop");
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        let desktop_node = desktop.mesh_node_address_hex().ok().flatten();
        link_apps_over_mesh(&desktop, &alice.app);
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().unwrap_or_default().iter().any(|(id, _)| id == &desktop_id)
        });
        assert!(crate::revoked_own_device_nodes(&alice.app.storage).is_empty());
        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        let skipped: Vec<String> = crate::revoked_own_device_nodes(&alice.app.storage).iter().map(hex::encode).collect();
        assert_eq!(skipped.len(), 1);
        if let Some(node) = desktop_node {
            assert_eq!(skipped[0], node);
        }
    }

    fn sample_device_revocation() -> omni_crypto::DeviceRevocationV1 {
        let alice = create_stage5_peer("md-gate-alice", "Alice", "Alice Laptop");
        let desktop = copy_to_second_device(&alice, "md-gate-desktop");
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        link_apps_over_mesh(&desktop, &alice.app);
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().unwrap_or_default().iter().any(|(id, _)| id == &desktop_id)
        });
        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        alice.app.storage.list_device_revocations(&alice.account.descriptor.account_id).unwrap().remove(0)
    }

    /// The desktop is away when the laptop takes it back, and no device of ours will set up a session
    /// with it after that: it learns from the laptop's mailbox note and erases its profile. A note
    /// that says nothing else is still written, for the revocation alone.
    #[test]
    fn a_revoked_device_away_at_the_time_learns_it_from_the_account_mailbox() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-mbrev-alice", "Alice", "Alice Laptop");
        let desktop = copy_to_second_device(&alice, "md-mbrev-desktop");
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        link_apps_over_mesh(&desktop, &alice.app);
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().unwrap_or_default().iter().any(|(id, _)| id == &desktop_id)
        });
        let desktop_hex = desktop.mesh_node_address_hex().unwrap().expect("desktop mesh is up");
        desktop.stop_mesh();
        wait_for_condition(Duration::from_secs(40), || !alice.app.mesh_has_link(&desktop_hex).unwrap_or(true));
        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        assert!(desktop.storage.get_local_account().unwrap().is_some(), "the desktop was away");

        let courier = MailboxCourier::default();
        let secret = alice.app.storage.get_account_rendezvous_secret().unwrap().unwrap();
        let now = crate::current_unix_ms();
        let note = crate::build_account_mailbox_note(&alice.app.storage, now).expect("a revocation is worth a note");
        assert_eq!(crate::publish_account_mailbox_with(&courier, &secret, &note, now), 1);
        let notes = crate::read_account_mailbox_with(&courier, &secret, now + 60_000);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].revocations.len(), 1);
        let desktop_db = desktop.storage.db_path().to_string_lossy().to_string();
        crate::with_account_mailbox(&desktop_db, |state| state.inbox.extend(notes));
        crate::apply_account_mailbox_inbox(&desktop.storage, &desktop.transport);

        assert!(desktop.storage.get_local_account().unwrap().is_none(), "the desktop erased itself");
    }

    /// A contact the desktop only got from the laptop is a real contact there: the desktop reaches it
    /// with the devices that came along and writes to it, and the message lands in the contact's one
    /// chat with Alice.
    #[test]
    fn a_device_that_got_a_contact_from_another_device_of_ours_can_write_to_it() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-csync-write-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-csync-write-bob", "Bob", "Bob PC");
        let desktop = copy_to_second_device(&alice, "md-csync-write-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        link_stage5_contact(&alice.app, &bob, "Bob");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);

        let bob_account = bob.account.descriptor.account_id.clone();
        wait_for_condition(Duration::from_secs(90), || {
            desktop
                .storage
                .find_contact_by_remote_account_id(&bob_account)
                .ok()
                .flatten()
                .is_some()
        });
        let bob_on_desktop = desktop.storage.find_contact_by_remote_account_id(&bob_account).unwrap().unwrap();
        let chat = bob_on_desktop.primary_dialog_key.clone().expect("the synced contact has a chat");

        link_apps_over_mesh(&desktop, &bob.app);
        let sent = desktop.send_text(&chat, "from the desktop that never met Bob").unwrap();
        let alice_account = alice.account.descriptor.account_id.clone();
        let desktop_device = desktop.get_local_device().unwrap().unwrap().device_id;
        let bob_device = bob.app.get_local_device().unwrap().unwrap().device_id;
        // On a miss, say where it stuck: the message and its queue row, the desktop's dialogs with
        // Bob's devices, and the session each side holds with the other.
        let describe = || {
            let session = |storage: &omni_storage::Storage, account: &crate::AccountId, device: &crate::DeviceId| {
                storage
                    .get_session_record(&omni_crypto::SessionAddress {
                        remote_account_id: account.clone(),
                        remote_device_id: device.clone(),
                    })
                    .map(|record| record.map(|record| format!("{:?}", record.bootstrap_state)))
            };
            format!(
                "message={:?} queue={:?} desktop_dialogs_with_bob={:?} desktop->bob={:?} bob->desktop={:?}",
                desktop.storage.get_message(&sent.id).map(|message| message.status),
                desktop.storage.outbound_queue_snapshot().map(|snapshot| {
                    snapshot.items.into_iter().filter(|item| item.message_id == sent.id).collect::<Vec<_>>()
                }),
                desktop.storage.list_contact_device_dialogs(&bob_on_desktop.contact_id),
                session(&desktop.storage, &bob_account, &bob_device),
                session(&bob.app.storage, &alice_account, &desktop_device),
            )
        };
        wait_for_state(Duration::from_secs(90), describe, || {
            bob.app
                .list_messages(alice.dialog_key.as_str())
                .map(|history| history.messages.iter().any(|message| message.body == "from the desktop that never met Bob"))
                .unwrap_or(false)
        });
    }

    /// The profile follows across our devices (multi-device-equal-nodes.md, «Синхронизация
    /// профиля»): a name and avatar set on the laptop reach the desktop, a rename on the desktop
    /// comes back without bouncing, and a cleared avatar clears it everywhere.
    #[test]
    fn the_profile_follows_across_our_devices_both_ways() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-psync-alice", "Alice", "Alice Laptop");
        let desktop = copy_to_second_device(&alice, "md-psync-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        let avatar_path = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("own-profile-avatar-{}.png", unique_suffix()));
        fs::create_dir_all(avatar_path.parent().unwrap()).unwrap();
        fs::write(&avatar_path, tiny_png_bytes()).unwrap();
        let profile = |app: &OmniApp| app.storage.get_local_public_profile_state().unwrap();

        let laptop = alice.app.update_local_public_profile("Alice Renamed", Some(&avatar_path), false).unwrap();
        wait_for_condition(Duration::from_secs(90), || {
            let state = profile(&desktop);
            state.display_name == "Alice Renamed"
                && state.avatar_hash_hex == laptop.public_avatar_hash_hex
                && state.avatar_bytes.is_some()
        });
        assert_eq!(
            profile(&desktop).updated_at_unix_ms,
            profile(&alice.app).updated_at_unix_ms,
            "the change keeps the moment it was made"
        );

        desktop.update_local_public_profile("Alice on Desktop", None, false).unwrap();
        wait_for_condition(Duration::from_secs(90), || profile(&alice.app).display_name == "Alice on Desktop");
        thread::sleep(Duration::from_secs(3));
        assert_eq!(profile(&desktop).display_name, "Alice on Desktop", "no bounce back");
        assert_eq!(profile(&alice.app).avatar_hash_hex, laptop.public_avatar_hash_hex, "the avatar stays");

        alice.app.update_local_public_profile("Alice on Desktop", None, true).unwrap();
        wait_for_condition(Duration::from_secs(90), || profile(&desktop).avatar_hash_hex.is_none());
    }

    /// A name changed on a device that never met Bob reaches him through the device of ours that did:
    /// what a device takes from another of ours, it passes on to our contacts (stand 2026-10-10: the
    /// PC kept the laptop's older name, set before the copy renamed the account).
    #[test]
    fn a_profile_change_from_our_other_device_reaches_our_contacts() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-pfwd-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-pfwd-bob", "Bob", "Bob PC");
        let desktop = copy_to_second_device(&alice, "md-pfwd-desktop");
        link_stage5_contact(&alice.app, &bob, "Bob");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        establish_stage5_session(&alice, &bob);
        establish_stage5_session(&bob, &alice);
        link_apps_over_mesh(&desktop, &alice.app);
        let alice_account = alice.account.descriptor.account_id.clone();
        let bob_sees = || {
            bob.app
                .storage
                .find_contact_by_remote_account_id(&alice_account)
                .unwrap()
                .and_then(|contact| contact.public_display_name)
        };

        desktop.update_local_public_profile("Alice on Desktop", None, false).unwrap();
        wait_for_condition(Duration::from_secs(90), || {
            alice.app.storage.get_local_public_profile_state().unwrap().display_name == "Alice on Desktop"
        });
        wait_for_condition(Duration::from_secs(90), || bob_sees().as_deref() == Some("Alice on Desktop"));
    }

    /// Bob is away when Alice takes her desktop back, and later never meets a device of hers that is
    /// still hers: the revocation reaches him through the couriers — the mark rides the pair beacon,
    /// the signed revocation waits in the pair's notice. A tampered copy in the notice revokes
    /// nothing, and a notice already read is not fetched for again.
    #[test]
    fn a_revocation_reaches_a_contact_through_the_pair_notice_at_the_couriers() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-notice-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-notice-bob", "Bob", "Bob PC");
        let desktop = copy_to_second_device(&alice, "md-notice-desktop");
        link_stage5_contact(&alice.app, &bob, "Bob");
        link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_peers_over_mesh(&alice, &bob);
        establish_stage5_session(&alice, &bob);
        establish_stage5_session(&bob, &alice);
        let alice_account = alice.account.descriptor.account_id.clone();
        let bob_account = bob.account.descriptor.account_id.clone();
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        // Alice must know the desktop to revoke it.
        link_apps_over_mesh(&desktop, &alice.app);
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_own_device_dialogs().unwrap_or_default().iter().any(|(id, _)| id == &desktop_id)
        });

        bob.app.stop_mesh();
        thread::sleep(Duration::from_millis(50));
        alice.app.revoke_own_device(&desktop_id.0, None).unwrap();
        assert!(!bob.app.storage.is_device_revoked(&alice_account, &desktop_id).unwrap(), "Bob was away");

        let peer = crate::rendezvous_peers(&alice.app.storage)
            .into_iter()
            .find(|peer| peer.account_id == bob_account)
            .expect("Alice rendezvouses with Bob");
        let notice = peer.revocation_notice.clone().expect("Bob may not have heard: the beacon carries a mark");
        let courier = MailboxCourier::default();
        let now = crate::current_unix_ms();
        let alice_db = alice.app.storage.db_path().to_string_lossy().to_string();
        crate::publish_revocation_notice_if_due(&courier, &alice_db, &peer.secret, &notice, now);
        // A tampered copy: the revocation turned onto another device.
        let mut tampered: Vec<omni_crypto::DeviceRevocationV1> = serde_json::from_slice(&notice.content).unwrap();
        tampered[0].device_id = omni_identity::DeviceId("dev-someone-else".to_string());
        let (topic, dht_key) = omni_rendezvous::mailbox_topics_with_keys(&peer.secret, now as u64)[0];
        let sealed = omni_rendezvous::seal_mailbox(&peer.secret, &topic, &serde_json::to_vec(&tampered).unwrap());
        crate::RendezvousCourier::publish(&courier, &topic.id(), &hex::encode(dht_key), &hex::encode(sealed), 60, crate::PublishPriority::Background).unwrap();

        // Bob's side: the pair secret is the same at both ends.
        let alice_on_bob = crate::rendezvous_peers(&bob.app.storage)
            .into_iter()
            .find(|peer| peer.account_id == alice_account)
            .expect("Bob rendezvouses with Alice's laptop");
        assert_eq!(alice_on_bob.secret, peer.secret);
        let bob_db = bob.app.storage.db_path().to_string_lossy().to_string();
        crate::read_revocation_notice_if_marked(&courier, &bob_db, &peer.secret, notice.mark_ms, now);
        crate::apply_revocation_notice_inbox(&bob.app.storage, &bob.app.transport);
        assert!(bob.app.storage.is_device_revoked(&alice_account, &desktop_id).unwrap());
        assert!(!bob
            .app
            .storage
            .is_device_revoked(&alice_account, &omni_identity::DeviceId("dev-someone-else".to_string()))
            .unwrap());

        // Read once per mark: the same mark again fetches nothing.
        courier.topics.lock().unwrap().clear();
        crate::publish_revocation_notice_if_due(&courier, &alice_db, &peer.secret, &notice, now + 1_000);
        assert!(courier.topics.lock().unwrap().is_empty(), "an unchanged notice is not posted again at once");
        crate::read_revocation_notice_if_marked(&courier, &bob_db, &peer.secret, notice.mark_ms, now + 1_000);
        assert!(crate::with_revocation_notices(&bob_db, |state| state.inbox.is_empty()));
    }

    /// The bug this closes (docs/bugs/safety-number-differs-per-own-device.md): Alice has a laptop
    /// and a desktop, Bob added her from the laptop. Read out loud, the code on either of her devices
    /// is the one Bob sees for her; both of her devices show two devices; and Bob, once he verified
    /// her, stays verified whichever of her devices he talks to.
    #[test]
    fn the_safety_number_is_the_same_on_every_device_of_the_account_and_at_the_contact() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("md-safety-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("md-safety-bob", "Bob", "Bob PC");
        let desktop = copy_to_second_device(&alice, "md-safety-desktop");
        link_apps_over_mesh(&desktop, &alice.app);
        let desktop_id = desktop.get_local_device().unwrap().unwrap().device_id;
        wait_for_condition(Duration::from_secs(30), || {
            alice.app.storage.list_account_devices(&alice.account.descriptor.account_id).unwrap_or_default().len() == 2
                && desktop.storage.list_account_devices(&alice.account.descriptor.account_id).unwrap_or_default().len() == 2
        });
        let alice_on_bob = link_stage5_contact(&bob.app, &alice, "Alice");
        link_stage5_contact(&alice.app, &bob, "Bob");

        let on_laptop = alice.app.get_my_verification_payload().unwrap();
        let on_desktop = desktop.get_my_verification_payload().unwrap();
        assert_eq!(on_laptop.safety_number, on_desktop.safety_number, "the same code on both of her devices");
        assert_eq!((on_laptop.devices.len(), on_desktop.devices.len()), (2, 2));
        let card = bob.app.get_contact_card(&alice_on_bob.contact_id).unwrap();
        assert_eq!(card.verification.current_safety_number.as_deref(), Some(on_laptop.safety_number.as_str()));

        bob.app.verify_contact(&alice_on_bob.contact_id, VerificationMethod::ManualCompare).unwrap();
        // Bob gets to know the desktop too: its bundle arrives, the trust stays.
        link_apps_over_mesh(&desktop, &bob.app);
        let alice_account = alice.account.descriptor.account_id.clone();
        wait_for_condition(Duration::from_secs(60), || {
            bob.app
                .storage
                .list_remote_prekey_bundles_for_account(&alice_account)
                .unwrap_or_default()
                .iter()
                .any(|bundle| bundle.device_id == desktop_id)
        });
        let card = bob.app.get_contact_card(&alice_on_bob.contact_id).unwrap();
        assert_eq!(card.trust_state, TrustState::Verified);
        assert_eq!(card.verification.device_count, 2);
        assert_eq!(card.verification.verified_safety_number, card.verification.current_safety_number);
    }

    #[test]
    fn tmp_measure_beacon_sizes_with_relay_bundles() {
        let alice = create_stage5_peer("tmp-size-alice", "Alice", "Alice Laptop");
        let relay = |n: usize, pin: bool| crate::RelayBundleEntryInput {
            endpoint: Some(format!("203.0.113.{n}:51820")),
            tcp_endpoint: Some(format!("203.0.113.{n}:443")),
            label: format!("vps-{n}"),
            cert_spki_pin_hex: pin.then(|| "ab".repeat(32)),
            node_address: Some([n as u8; 20]),
        };
        for (count, pin) in [(0usize, false), (1, false), (1, true), (2, false), (2, true), (3, true)] {
            let token = if count == 0 { None } else {
                Some(alice.app.generate_relay_bundle(1, (1..=count).map(|n| relay(n, pin)).collect()).unwrap())
            };
            let mut beacon = crate::build_self_beacon([7u8; 20], vec!["203.0.113.7:51820".into()], omni_rendezvous::NatType::Cone, token.clone());
            beacon.forwarded = vec!["[2001:db8::1]:51820".into()];
            beacon.tcp_direct = vec!["203.0.113.7:443".into()];
            beacon.revoked_through_ms = Some(1_791_000_000_000);
            let topics = omni_rendezvous::rendezvous_topics_with_keys(&[1u8; 32], 0);
            let sealed = omni_rendezvous::seal_beacon(&[1u8; 32], &topics[0].0, &beacon).to_wire();
            eprintln!("SIZE relays={count} pin={pin} token={} sealed={}", token.map_or(0, |t| t.len()), sealed.len());
        }
    }

    /// A real signed relay bundle — three relays with SPKI pins — in a beacon seals within a BT-DHT
    /// record (it used to be ~1050 bytes as JSON in base64 in JSON), and still verifies after the trip.
    #[test]
    fn a_beacon_with_a_signed_three_relay_bundle_fits_a_dht_record() {
        let alice = create_stage5_peer("relay-binary-alice", "Alice", "Alice Laptop");
        let relays = (1..=3)
            .map(|n| crate::RelayBundleEntryInput {
                endpoint: Some(format!("203.0.113.{n}:51820")),
                tcp_endpoint: Some(format!("[2001:db8::{n}]:443")),
                label: format!("vps-{n}"),
                cert_spki_pin_hex: Some("ab".repeat(32)),
                node_address: Some([n as u8; 20]),
            })
            .collect();
        let token = alice.app.generate_relay_bundle(7, relays).unwrap();
        let secret = [3u8; 32];
        let topic = omni_rendezvous::rendezvous_topics(&secret, 0)[0];
        let mut beacon = crate::build_self_beacon([7u8; 20], vec!["203.0.113.7:51820".into()], omni_rendezvous::NatType::Cone, Some(token.clone()));
        beacon.forwarded = vec!["[2001:db8::7]:51820".into()];
        beacon.tcp_direct = vec!["203.0.113.7:443".into()];
        beacon.revoked_through_ms = Some(1_791_000_000_000);
        let sealed = omni_rendezvous::seal_beacon(&secret, &topic, &beacon).to_wire();
        assert!(sealed.len() <= omni_rendezvous::DHT_MAX_BLOB, "{} B, token {} chars", sealed.len(), token.len());
        let opened = omni_rendezvous::open_beacon(
            &secret,
            &topic,
            &omni_rendezvous::SealedBeacon::from_wire(&sealed).unwrap(),
            beacon.ts_ms,
            &mut omni_rendezvous::ReplayGuard::new(),
        )
        .unwrap();
        assert_eq!(opened.relay_bundle.as_deref(), Some(token.as_str()));
        let bundle = crate::parse_relay_bundle(&token).unwrap();
        crate::verify_relay_bundle(&bundle).expect("the signature holds over the binary form");
        assert_eq!(bundle.relays.len(), 3);
        // A changed byte breaks it.
        let mut tampered = bundle.clone();
        tampered.bundle_version += 1;
        assert!(crate::verify_relay_bundle(&tampered).is_err());
    }

    /// One owner round of the account passport now, as the queue tick runs it (under the session
    /// lock), without waiting for its once-a-minute floor.
    fn run_passport_round(app: &OmniApp) {
        let db_path = app.storage.db_path().to_string_lossy().to_string();
        crate::with_account_passport_state(&db_path, |state| state.checked_at_unix_ms = 0);
        let _guard = crate::lock_app_session_guard(&app.session_lock).unwrap();
        crate::account_passport_round(&app.storage, &app.transport, current_unix_ms());
    }

    /// Alice loses her only device and types the phrase on a new one. Her holder Bob found by his
    /// mark, the new device gets the passport back from him: her name, her contacts with the names
    /// she gave them, nothing she had removed — and it did not hand out its own empty passport first.
    #[test]
    fn a_device_restored_from_the_phrase_gets_the_account_back_from_a_holder() {
        // The holder reaches the new device before its mark is read (it starts the session).
        restore_from_a_holder(false);
    }

    /// The order the product mostly sees: the mark first, then the link.
    #[test]
    fn a_device_restored_from_the_phrase_reads_the_mark_before_it_meets_the_holder() {
        restore_from_a_holder(true);
    }

    fn restore_from_a_holder(mark_before_link: bool) {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("pp-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("pp-bob", "Bob", "Bob PC");
        let carol = create_stage5_peer("pp-carol", "Carol", "Carol PC");
        let dave = create_stage5_peer("pp-dave", "Dave", "Dave PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let carol_contact = link_stage5_contact(&alice.app, &carol, "Carol");
        let dave_contact = link_stage5_contact(&alice.app, &dave, "Dave");
        alice.app.rename_contact(&carol_contact.contact_id, "Карина").unwrap();
        alice.app.forget_contact(&dave_contact.contact_id).unwrap();
        alice.app.update_local_public_profile("Алиса", None, false).unwrap();

        // Bob is a contact Alice talks to both ways: a holder candidate. Carol never wrote back.
        link_stage5_peers_over_mesh(&alice, &bob);
        alice.app.send_text(&bob.dialog_key, "привет").unwrap();
        wait_for_message_count(&bob.app, &alice.dialog_key, 1);
        bob.app.send_text(&alice.dialog_key, "и тебе").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 2);

        let bob_holds = || bob.app.storage.get_held_passport(&alice.account.descriptor.account_id).unwrap();
        wait_for_condition(Duration::from_secs(30), || {
            run_passport_round(&alice.app);
            bob_holds().is_some()
                && alice.app.storage.list_passport_holders().unwrap().iter().any(|holder| holder.confirmed_version > 0)
        });
        let holders = alice.app.storage.list_passport_holders().unwrap();
        assert_eq!(holders.len(), 1, "only the contact we talk to both ways holds it: {holders:?}");
        assert_eq!(holders[0].account_id, bob.account.descriptor.account_id);
        let held = bob_holds().unwrap();

        // The laptop is gone.
        alice.app.stop_mesh();
        let restored_db = std::env::temp_dir()
            .join("omni-stage-5-app-tests")
            .join(format!("pp-alice-restored-{}.db", unique_suffix()));
        let restored = OmniApp::init(AppInitRequest { db_path: restored_db.to_string_lossy().into_owned() }).unwrap();
        let account = restored
            .restore_account_from_mnemonic(
                "Alice",
                &alice.account.recovery_phrase_v2.as_ref().unwrap().mnemonic,
                "Alice New Laptop",
            )
            .unwrap();
        if let Some(summary) = account.local_profile_summary.as_ref() {
            restored.select_active_profile(&summary.profile_id).unwrap();
        }
        // Before it finds its passport, the new device hands out nothing.
        run_passport_round(&restored);
        assert!(restored.storage.get_own_passport().unwrap().is_none());

        // Bob's mark, as the couriers carry it: his identity beacon sealed under the mark secret.
        restored.start_mesh().unwrap();
        if !mark_before_link {
            link_apps_over_mesh(&restored, &bob.app);
        }
        let mark_secret: [u8; 32] = hex::decode(&held.mark_secret_hex).unwrap().try_into().unwrap();
        let restored_secret = restored.storage.get_account_rendezvous_secret().unwrap().unwrap();
        assert_eq!(
            omni_rendezvous::passport_mark_secret(&restored_secret),
            mark_secret,
            "the phrase alone gives the mark secret"
        );
        let beacon =
            crate::build_signed_identity_beacon(&bob.app.storage, &bob.app.mesh_runtime().unwrap(), None).unwrap();
        let topic = omni_rendezvous::mailbox_topics_with_keys(&mark_secret, current_unix_ms() as u64)[0].0;
        let wire = omni_rendezvous::seal_mailbox(&mark_secret, &topic, &beacon.to_wire());
        let opened = omni_rendezvous::open_mailbox(&mark_secret, &topic, &wire).unwrap();
        let mark = omni_rendezvous::IdentityBeacon::from_wire(&opened).unwrap();
        assert!(crate::adopt_passport_holder_mark(&restored.storage, &mark, current_unix_ms()).unwrap());
        assert!(
            !crate::adopt_passport_holder_mark(&restored.storage, &mark, current_unix_ms()).unwrap(),
            "a mark read twice is one holder"
        );
        if mark_before_link {
            link_apps_over_mesh(&restored, &bob.app);
        }

        wait_for_state(
            Duration::from_secs(60),
            || {
                let contacts = restored.list_contacts().unwrap();
                format!(
                    "bob={} restored contacts: {:?}; links: {:?}; recovery: {:?}",
                    bob.account.descriptor.account_id.0,
                    contacts.iter().map(|c| (c.petname.clone(), c.remote_account_id.clone(), c.relationship_state)).collect::<Vec<_>>(),
                    contacts.iter().map(|c| restored.storage.list_contact_device_dialogs(&c.contact_id).unwrap()).collect::<Vec<_>>(),
                    restored.storage.get_app_meta("account_recovery_v1").unwrap(),
                )
            },
            || {
                run_passport_round(&restored);
                restored.list_contacts().unwrap().iter().any(|contact| contact.petname == "Карина")
            },
        );
        let contacts = restored.list_contacts().unwrap();
        let named = |petname: &str| contacts.iter().find(|contact| contact.petname == petname);
        assert!(named("Карина").is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved));
        assert!(
            named("Bob").is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved),
            "{contacts:?}"
        );
        assert_eq!(contacts.len(), 2, "Dave was removed, Bob is not doubled: {contacts:?}");
        assert_eq!(restored.storage.get_local_public_profile_state().unwrap().display_name, "Алиса");
        // Bob still keeps the real passport: nothing smaller replaced it while the device was empty.
        assert!(bob_holds().unwrap().version >= held.version);

        // The conversation comes back from Bob's copy, and matches the fingerprint the passport kept.
        let bob_account = bob.account.descriptor.account_id.clone();
        wait_for_state(
            Duration::from_secs(60),
            || format!("check={:?}", restored.storage.get_chat_history_check(&bob_account).unwrap()),
            || {
                run_history_round(&restored);
                restored.storage.get_chat_history_check(&bob_account).unwrap() == Some(omni_storage::ChatHistoryCheck::Verified)
            },
        );
        let bob_contact = restored.storage.find_contact_by_remote_account_id(&bob_account).unwrap().unwrap();
        let chat = restored.storage.chat_dialog_key_for(bob_contact.primary_dialog_key.as_deref().unwrap()).unwrap();
        let lines = restored.storage.list_chat_history_lines(&chat).unwrap();
        let bodies: Vec<(&str, bool)> = lines.iter().map(|line| (line.body.as_str(), line.outgoing)).collect();
        assert_eq!(bodies, vec![("и тебе", false), ("привет", true)], "newest first");
        assert_eq!(restored.storage.history_import_source(&lines[0].id).unwrap(), Some(bob_account.clone()), "«со слов» Bob");
        let shown = restored.list_messages(&chat).unwrap().messages;
        assert!(shown.iter().all(|message| message.told_by_contact), "the shell is told: {shown:?}");
        let header = restored.get_chat_header_contact_summary(chat.clone()).unwrap();
        assert!(!header.subtitle.contains("история"), "a verified chat says nothing: {}", header.subtitle);
    }

    /// The passport's robustness rules on their own: a passport sealed for another account, one
    /// written by another account's device, a changed, broken or oversized one — each is refused
    /// without a panic and without touching what the device has.
    #[test]
    fn a_broken_or_foreign_passport_changes_nothing() {
        let alice = create_stage5_peer("pp-rules-alice", "Alice", "Alice Laptop");
        let mallory = create_stage5_peer("pp-rules-mallory", "Mallory", "Mallory PC");
        let alice_secret = alice.app.storage.get_account_rendezvous_secret().unwrap().unwrap();
        let mallory_secret = mallory.app.storage.get_account_rendezvous_secret().unwrap().unwrap();
        let content =
            crate::current_passport_content(&alice.app.storage, &alice.account.descriptor.account_id).unwrap();
        let sealed =
            crate::seal_passport_content(&alice.app.storage, &alice_secret, content.clone(), 1_000).unwrap().unwrap();
        assert!(crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &sealed).is_ok());

        // Sealed under another account's secret: does not open.
        assert!(crate::open_and_verify_passport(&alice.app.storage, &mallory_secret, &sealed).is_err());
        // Mallory's own passport sealed under Alice's secret: her chain is not Alice's account.
        let mallory_content =
            crate::current_passport_content(&mallory.app.storage, &mallory.account.descriptor.account_id).unwrap();
        let forged =
            crate::seal_passport_content(&mallory.app.storage, &alice_secret, mallory_content, 1_000).unwrap().unwrap();
        assert!(crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &forged).is_err());
        // Changed after signing: the signature does not hold.
        let mut tampered = crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &sealed).unwrap();
        tampered.groups.push(crate::PassportGroupV1 { group_id: "g".into(), name: "x".into(), role: "admin".into() });
        let repacked =
            omni_rendezvous::seal_passport(&alice_secret, &crate::deflate(&serde_json::to_vec(&tampered).unwrap()));
        assert!(crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &repacked).is_err());
        // Not deflate, not JSON, a compression bomb.
        let junk = omni_rendezvous::seal_passport(&alice_secret, b"junk");
        assert!(crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &junk).is_err());
        let not_json = omni_rendezvous::seal_passport(&alice_secret, &crate::deflate(b"{\"contacts\": 5"));
        assert!(crate::open_and_verify_passport(&alice.app.storage, &alice_secret, &not_json).is_err());
        let bomb = crate::deflate(&vec![b' '; crate::PASSPORT_PLAIN_MAX_BYTES + 10]);
        assert!(bomb.len() < 64 * 1024, "a bomb is small packed");
        assert!(crate::inflate_bounded(&bomb).is_none());
        // Fields a newer build may add are ignored; ones it leaves out default.
        let mut json: serde_json::Value = serde_json::to_value(&content).unwrap();
        json["from_the_future"] = serde_json::json!({"x": 1});
        json.as_object_mut().unwrap().remove("groups");
        let parsed: crate::AccountPassportV1 = serde_json::from_value(json).unwrap();
        assert!(parsed.groups.is_empty());

        // Merging an empty passport adds nothing and removes nothing.
        let bob = create_stage5_peer("pp-rules-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let before = alice.app.list_contacts().unwrap();
        let recovery = crate::AccountRecoveryStateV1 { started_at_unix_ms: 0, ..Default::default() };
        let summary = crate::apply_recovered_passport(
            &alice.app.storage,
            &crate::AccountPassportV1::default(),
            &recovery,
            current_unix_ms(),
        );
        assert!(summary.contains("contacts=0"), "{summary}");
        assert_eq!(alice.app.list_contacts().unwrap(), before);
        // A contact entry that fails its shape check costs only itself.
        let mut bad = crate::AccountPassportV1::default();
        bad.contacts.push(omni_types::ContactSyncEntryV1 {
            account_id: "acc-x".into(),
            state: "befriended".into(),
            state_version: 1,
            state_writer: "w".into(),
            petname: "X".into(),
            petname_version: 1,
            petname_writer: "w".into(),
            display_name: None,
            bundles_json: Vec::new(),
        });
        let summary = crate::apply_recovered_passport(&alice.app.storage, &bad, &recovery, current_unix_ms());
        assert!(summary.contains("failed=1"), "{summary}");
        assert_eq!(alice.app.list_contacts().unwrap(), before);
    }

    /// A thousand contacts with their device bundles do not fit one message; the passport leaves
    /// the bundles of the oldest out and still names every contact.
    #[test]
    fn a_passport_of_many_contacts_fits_one_message() {
        let alice = create_stage5_peer("pp-many-alice", "Alice", "Alice Laptop");
        let secret = alice.app.storage.get_account_rendezvous_secret().unwrap().unwrap();
        let mut content =
            crate::current_passport_content(&alice.app.storage, &alice.account.descriptor.account_id).unwrap();
        let template = serde_json::to_value(crate::prekey_bundle_from_protocol(&alice.bundle).unwrap()).unwrap();
        // Every byte array of the bundle (keys, signatures) random: real bundles do not repeat, and a
        // repeated one would make the compression look far better than it is.
        fn randomize(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Array(items) if items.iter().all(|item| item.is_u64()) && items.len() >= 16 => {
                    let random = omni_crypto::random_bytes(items.len()).unwrap();
                    for (item, byte) in items.iter_mut().zip(random) {
                        *item = serde_json::Value::from(byte);
                    }
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(randomize),
                serde_json::Value::Object(map) => map.values_mut().for_each(randomize),
                _ => {}
            }
        }
        let mut bundle_len = 0;
        for index in 0..1000 {
            let mut value = template.clone();
            randomize(&mut value);
            let bundle = serde_json::to_string(&value).unwrap();
            bundle_len = bundle.len();
            content.contacts.push(omni_types::ContactSyncEntryV1 {
                account_id: format!("acc-{index:04}-{}", "f".repeat(40)),
                state: "approved".into(),
                state_version: 3,
                state_writer: alice.device.device_id.0.clone(),
                petname: format!("Друг номер {index}"),
                petname_version: 2,
                petname_writer: alice.device.device_id.0.clone(),
                display_name: Some(format!("Friend {index}")),
                bundles_json: vec![bundle],
            });
            content.contact_device_keys.push((
                format!("acc-{index:04}-{}", "f".repeat(40)),
                vec![hex::encode(omni_crypto::random_bytes(32).unwrap())],
            ));
        }
        let plain = serde_json::to_vec(&content).unwrap();
        let packed = crate::deflate(&plain);
        eprintln!(
            "passport of 1000 contacts with bundles: {} bytes JSON, {} packed (bundle {} bytes)",
            plain.len(),
            packed.len(),
            bundle_len
        );
        let sealed = crate::seal_passport_content(&alice.app.storage, &secret, content, 1_000).unwrap().unwrap();
        assert!(sealed.len() <= omni_protocol::ACCOUNT_PASSPORT_SEALED_MAX_BYTES);
        let opened = crate::open_and_verify_passport(&alice.app.storage, &secret, &sealed).unwrap();
        assert_eq!(opened.contacts.len(), 1000, "every contact is named");
        let with_bundles = opened.contacts.iter().filter(|entry| !entry.bundles_json.is_empty()).count();
        eprintln!("contacts that kept their device bundles: {with_bundles}");
        assert!(with_bundles > 0);
        assert_eq!(opened.contacts.last().unwrap().bundles_json.len(), 1, "the newest keep theirs");
        assert_eq!(opened.contact_device_keys.len(), 1000, "every contact can still be dialed by node");

        // A contact named without its bundle is dialed by the node of its device key.
        let carol = create_stage5_peer("pp-many-carol", "Carol", "Carol PC");
        let restored_like = create_stage5_peer("pp-many-restored", "Alice2", "Alice PC");
        restored_like
            .app
            .storage
            .add_contact(
                &AddContactInput {
                    petname: "Carol".into(),
                    remote_account_id: Some(carol.account.descriptor.account_id.clone()),
                    public_display_name: None,
                    remote_instance_id: None,
                    primary_dialog_key: None,
                    initial_observation: None,
                },
                1_000,
            )
            .unwrap();
        let signing_key = crate::prekey_bundle_from_protocol(&carol.bundle).unwrap().identity_signing_key;
        let mut named = crate::AccountPassportV1::default();
        named.contact_device_keys.push((carol.account.descriptor.account_id.0.clone(), vec![hex::encode(signing_key.0)]));
        named.contact_device_keys.push(("acc-unknown".into(), vec![hex::encode(signing_key.0)]));
        named.contact_device_keys.push((carol.account.descriptor.account_id.0.clone(), vec!["zz".into()]));
        let dial = crate::contacts_to_dial(&restored_like.app.storage, &named);
        let node = omni_mesh_runtime::node_address_from_ed25519_pubkey(signing_key.0).unwrap().to_string();
        assert_eq!(dial, vec![(carol.account.descriptor.account_id.0.clone(), node)], "only an approved contact, only a real key");
        restored_like.app.store_remote_prekey_bundle(&carol.bundle).unwrap();
        assert!(crate::contacts_to_dial(&restored_like.app.storage, &named).is_empty(), "a bundle in hand: nothing to dial");
    }

    /// The Mainline card tells "joining" from "the network drops DHT": the core counts how long the
    /// helper's table has been empty, and a node seen resets it (live 2026-10-12, the laptop's network
    /// let no DHT packet through after a wake while QUIC to the PC worked).
    #[test]
    fn an_empty_dht_table_is_timed_and_a_node_resets_it() {
        let db = format!("dht-empty-{}", unique_suffix());
        assert_eq!(crate::note_dht_node_count(&db, 0, 1_000), 0);
        assert_eq!(crate::note_dht_node_count(&db, 0, 121_000), 120_000);
        assert_eq!(crate::note_dht_node_count(&db, 5, 130_000), 0);
        assert_eq!(crate::note_dht_node_count(&db, 0, 140_000), 0, "a fresh empty spell starts over");
        crate::clear_dht_empty_since(&db);
        assert_eq!(crate::note_dht_node_count(&db, 0, 200_000), 0);
    }

    /// DHT goes over a friend only when our own Mainline is dead (half a minute with no node at all,
    /// Mainline on), and a friend serves only our devices and approved contacts — a stranger's node
    /// is refused (owner decision 2026-10-12).
    #[test]
    fn dht_goes_over_a_peer_only_while_our_own_sees_no_swarm_and_only_for_our_people() {
        let app = create_app_with_active_profile(isolated_test_db_path("dht-over-peer", "me"), "Me", "Laptop");
        let db = app.storage.db_path().to_string_lossy().to_string();
        app.set_dht_rendezvous_enabled(true).unwrap();
        let now = current_unix_ms();
        crate::note_dht_node_count(&db, 0, now - 10_000);
        assert!(!crate::dht_locally_blocked(&db), "ten seconds is still joining");
        crate::clear_dht_empty_since(&db);
        crate::note_dht_node_count(&db, 0, now - 130_000);
        assert!(crate::dht_locally_blocked(&db), "half a minute with no node: the network drops DHT");
        app.set_dht_rendezvous_enabled(false).unwrap();
        assert!(!crate::dht_locally_blocked(&db), "Mainline off: not used at all");
        app.set_dht_rendezvous_enabled(true).unwrap();
        crate::note_dht_node_count(&db, 12, now);
        assert!(!crate::dht_locally_blocked(&db), "our own engine works again");

        let stranger = omni_mesh_runtime::MeshNodeAddress::from_bytes([9; 20]);
        assert!(!crate::dht_proxy_origin_is_ours(&db, &stranger), "a stranger's node is not served");
    }

    /// A DHT way for the leg tests: answers (keeping what it is given) or not at all.
    struct FakeSlots {
        name: &'static str,
        answers: bool,
        kept: std::cell::RefCell<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
        asked: std::cell::Cell<usize>,
    }

    impl FakeSlots {
        fn new(name: &'static str, answers: bool) -> Self {
            Self { name, answers, kept: Default::default(), asked: Default::default() }
        }
    }

    impl crate::DhtSlotWay for &FakeSlots {
        fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>> {
            self.asked.set(self.asked.get() + 1);
            let slot = [&key[..], salt].concat();
            self.answers.then(|| self.kept.borrow().get(&slot).cloned().unwrap_or_default())
        }

        fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]) {
            if self.answers {
                self.kept.borrow_mut().insert([&key[..], salt].concat(), value.to_vec());
            }
        }

        fn name(&self) -> &'static str {
            self.name
        }
    }

    /// With our own Mainline dead, a friend's engine goes first; a friend that does not answer is
    /// asked once a sweep, and the pkarr relays — the last hope — take its place (owner, 2026-10-12).
    #[test]
    fn a_dht_leg_falls_to_the_relays_when_the_friend_does_not_answer() {
        use crate::DhtOverPeerLeg as _;
        let friend = FakeSlots::new("friend", false);
        let relays = FakeSlots::new("pkarr relays", true);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>, Box::new(&relays)]);
        let key = [3u8; 32];
        leg.publish(&key, b"sealed beacon");
        assert_eq!(friend.asked.get(), 1, "the friend is asked once, then passed over");
        assert_eq!(relays.kept.borrow().len(), 1, "the beacon went to the relays");
        assert_eq!(leg.fetch(&key), vec![b"sealed beacon".to_vec()]);
        assert_eq!(friend.asked.get(), 1, "not asked again this sweep");

        // A friend that answers keeps the relays out of it.
        let friend = FakeSlots::new("friend", true);
        let relays = FakeSlots::new("pkarr relays", true);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>, Box::new(&relays)]);
        leg.publish(&key, b"sealed beacon");
        assert_eq!(leg.fetch(&key), vec![b"sealed beacon".to_vec()]);
        assert_eq!((relays.asked.get(), relays.kept.borrow().len()), (0, 0), "the relays are not used");

        // Nobody answers: nothing is put, nothing is found.
        let friend = FakeSlots::new("friend", false);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>]);
        leg.publish(&key, b"sealed beacon");
        assert!(leg.fetch(&key).is_empty());
    }

    /// The Mainline card says which way the DHT goes while our own engine is dead — through a friend,
    /// through the pkarr relays, or neither answers — and says nothing once our own engine works.
    #[test]
    fn the_mainline_status_tells_the_way_into_the_swarm() {
        use crate::DhtOverPeerLeg as _;
        let app = create_app_with_active_profile(isolated_test_db_path("dht-way-in", "me"), "Me", "Laptop");
        let db = app.storage.db_path().to_string_lossy().to_string();
        app.set_dht_rendezvous_enabled(true).unwrap();
        let now = current_unix_ms();
        crate::note_dht_node_count(&db, 0, now - 130_000);
        assert_eq!(crate::dht_way_in(&db, now), None, "no op yet");

        let key = [4u8; 32];
        let friend = FakeSlots::new("friend", false);
        let relays = FakeSlots::new("pkarr relays", true);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>, Box::new(&relays)])
            .for_profile(Some(&db));
        leg.fetch(&key);
        assert_eq!(crate::dht_way_in(&db, current_unix_ms()), Some("pkarr relays"));

        let friend = FakeSlots::new("friend", true);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>]).for_profile(Some(&db));
        leg.fetch(&key);
        assert_eq!(crate::dht_way_in(&db, current_unix_ms()), Some("friend"));

        let friend = FakeSlots::new("friend", false);
        let leg = crate::SlotDhtLeg::new(vec![Box::new(&friend) as Box<dyn crate::DhtSlotWay + '_>]).for_profile(Some(&db));
        leg.fetch(&key);
        assert_eq!(crate::dht_way_in(&db, current_unix_ms()), Some("none"));
        assert_eq!(crate::dht_way_in(&db, current_unix_ms() + 16 * 60_000), None, "a stale way is not shown");

        crate::note_dht_node_count(&db, 9, current_unix_ms());
        assert_eq!(crate::dht_way_in(&db, current_unix_ms()), None, "our own engine works again");
    }

    /// Through which friends a DHT op goes while our own Mainline is dead: our own devices first, the
    /// rest in the runtime's nearest-first order, three at most (each silent one costs a deadline).
    #[test]
    fn dht_friends_go_own_devices_first_then_nearest_three_at_most() {
        let node = |b: u8| omni_mesh_runtime::MeshNodeAddress::from_bytes([b; 20]);
        // As the runtime ranked them by RTT: 1 nearest … 5 farthest; 4 is our own device.
        let ranked = vec![node(1), node(2), node(3), node(4), node(5)];
        let order = crate::dht_friends_in_order(ranked, &[[4; 20]]);
        assert_eq!(order, vec![node(4), node(1), node(2)]);
        assert!(crate::dht_friends_in_order(Vec::new(), &[[4; 20]]).is_empty());
    }

    /// One history sync round now (own devices, and a restored device's contacts), under the session
    /// lock, without the ten-minute floor.
    fn run_history_round(app: &OmniApp) {
        let db_path = app.storage.db_path().to_string_lossy().to_string();
        crate::clear_history_sync_state_for_profile(&db_path);
        let _guard = crate::lock_app_session_guard(&app.session_lock).unwrap();
        crate::history_sync_round(&app.storage, &app.transport, current_unix_ms());
        crate::history_recovery_round(&app.storage, &app.transport, current_unix_ms());
    }

    /// Both sides of a chat fingerprint it the same, though for each the other's lines are incoming.
    #[test]
    fn both_sides_of_a_chat_fingerprint_it_the_same() {
        let me = AccountId("acc-me".into());
        let you = AccountId("acc-you".into());
        let line = |id: &str, outgoing: bool, body: &str, at: i64| omni_storage::ChatHistoryLine {
            id: id.into(),
            outgoing,
            body: body.into(),
            created_at_unix_ms: at,
        };
        let mine = crate::LocalChat {
            contact_account_id: you.clone(),
            chat_dialog_key: "d".into(),
            lines: vec![line("m1", true, "hi", 1), line("m2", false, "hey", 2)],
        };
        let yours = crate::LocalChat {
            contact_account_id: me.clone(),
            chat_dialog_key: "d".into(),
            lines: vec![line("m2", true, "hey", 2), line("m1", false, "hi", 1)],
        };
        let a = crate::local_chat_digest(&me, &mine);
        let b = crate::local_chat_digest(&you, &yours);
        assert_eq!(a, b);
        assert_eq!(a.count, 2);
        let edited = crate::LocalChat { lines: vec![line("m1", true, "hi!", 1), line("m2", false, "hey", 2)], ..mine };
        let c = crate::local_chat_digest(&me, &edited);
        assert_ne!(c.root, a.root, "a changed text changes the fingerprint");
        assert_eq!(c.buckets.iter().zip(&a.buckets).filter(|(x, y)| x != y).count(), 1, "in one bucket only");
    }

    /// Bob writes to Alice's laptop while her desktop is away, then Bob's device is gone for good:
    /// nobody will hand the desktop that message — but the laptop has it, and history sync between
    /// Alice's own devices brings it over.
    #[test]
    fn a_message_whose_sender_is_gone_reaches_our_other_device_from_the_one_that_got_it() {
        let _guard = network_test_guard();
        let alice = create_stage5_peer("hs-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("hs-bob", "Bob", "Bob PC");
        link_stage5_contact(&bob.app, &alice, "Alice");
        let desktop = copy_to_second_device(&alice, "hs-alice-desktop");
        link_stage5_peers_over_mesh(&alice, &bob);
        bob.app.send_text(&alice.dialog_key, "только ноутбуку").unwrap();
        wait_for_message_count(&alice.app, &bob.dialog_key, 1);
        bob.app.stop_mesh();

        link_apps_over_mesh(&desktop, &alice.app);
        let desktop_has = || {
            desktop
                .list_messages(&bob.dialog_key)
                .is_ok_and(|history| history.messages.iter().any(|message| message.body == "только ноутбуку"))
        };
        wait_for_condition(Duration::from_secs(30), || {
            run_history_round(&alice.app);
            run_history_round(&desktop);
            desktop_has()
        });
        let line = desktop
            .list_messages(&bob.dialog_key)
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.body == "только ноутбуку")
            .unwrap();
        assert_eq!(line.direction, ChatDirection::Incoming);
        // Once both hold the same, nothing more travels: a second round adds nothing.
        let before = desktop.list_messages(&bob.dialog_key).unwrap().messages.len();
        run_history_round(&alice.app);
        run_history_round(&desktop);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(desktop.list_messages(&bob.dialog_key).unwrap().messages.len(), before);
        assert!(desktop.storage.history_import_source(&line.id).unwrap().is_none(), "from our own device, not «со слов»");
    }

    /// A file line in history sync brings its file's offer: the taker fetches the file from the device
    /// the line came from — not from the chat's other party, who may be the one that is gone. A file
    /// line counts in the chat's fingerprint like any other; one carrying another message's file
    /// takes only its text.
    #[test]
    fn a_file_line_from_history_sync_is_fetched_from_the_device_that_handed_it_over() {
        let alice = create_stage5_peer("hs-file-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("hs-file-bob", "Bob", "Bob PC");
        link_stage5_contact(&alice.app, &bob, "Bob");
        let own = alice.account.descriptor.account_id.clone();
        let bob_account = bob.account.descriptor.account_id.clone();
        let chat = crate::local_chat(&alice.app.storage, &bob_account).unwrap();
        let peer = crate::HistoryPeer { own_account_id: own.clone(), contact_account_id: bob_account.clone(), from_contact: false };
        let laptop = omni_crypto::SessionAddress { remote_account_id: own.clone(), remote_device_id: DeviceId("alice-laptop".into()) };
        let offer = |id: &str| omni_types::AttachmentOfferV1 {
            message_id: id.into(),
            body: "отчёт".into(),
            file_name: "report.txt".into(),
            file_size_bytes: 10,
            mime_type: Some("text/plain".into()),
            content_id_hex: "ab".repeat(32),
            content_encryption: Some("chacha20_poly1305_v1".into()),
            content_key_hex: Some("11".repeat(32)),
            content_nonce_seed_hex: Some("22".repeat(16)),
            content_chunk_size_bytes: Some(16_384),
            created_at_unix_ms: 30,
            presentation: None,
        };
        let line = |id: &str, file: Option<omni_types::AttachmentOfferV1>| omni_types::HistoryLineV1 {
            message_id: id.into(),
            author_account_id: bob_account.0.clone(),
            body: "отчёт".into(),
            created_at_unix_ms: 30,
            attachment: file,
        };
        let lines = [line("f1", Some(offer("f1"))), line("f2", Some(offer("other")))];
        // The wire refuses a line with another message's file outright.
        assert!(omni_protocol::validate_history_sync_message_v1(&omni_types::HistorySyncMessageV1::Lines {
            chat_account_id: bob_account.0.clone(),
            lines: lines.to_vec(),
        })
        .is_err());
        let added = crate::merge_history_lines(&alice.app.storage, &laptop, "dialog:alice-laptop", &peer, &chat, &lines, 1_000);
        assert_eq!(added, 2);
        let file = alice.app.storage.get_attachment_runtime_info("f1").unwrap().expect("the file line keeps its file");
        assert_eq!(file.dialog_key, "dialog:alice-laptop", "fetched from the device that handed it over");
        assert_eq!(file.attachment.state, omni_types::AttachmentTransferState::Offered);
        assert!(alice.app.storage.get_attachment_runtime_info("f2").unwrap().is_none(), "another message's file is not taken");
        let after = crate::local_chat(&alice.app.storage, &bob_account).unwrap();
        assert!(after.lines.iter().any(|l| l.id == "f1"), "a file line is part of the chat's fingerprint");
    }

    /// A contact's copy is taken only as far as it is ours with it: a line by a third account is
    /// dropped; a copy that adds a line we never had shows as "mismatch" against the passport,
    /// one missing lines as "incomplete" — the chat is marked, nothing is rewritten.
    #[test]
    fn a_contacts_copy_with_foreign_or_missing_lines_is_marked_not_trusted() {
        let alice = create_stage5_peer("hs-check-alice", "Alice", "Alice Laptop");
        let bob = create_stage5_peer("hs-check-bob", "Bob", "Bob PC");
        let contact = link_stage5_contact(&alice.app, &bob, "Bob");
        let own = alice.account.descriptor.account_id.clone();
        let bob_account = bob.account.descriptor.account_id.clone();
        let chat = crate::local_chat(&alice.app.storage, &bob_account).unwrap();
        let peer = crate::HistoryPeer { own_account_id: own.clone(), contact_account_id: bob_account.clone(), from_contact: true };
        let from = omni_crypto::SessionAddress { remote_account_id: bob_account.clone(), remote_device_id: DeviceId("bob-pc".into()) };
        let line = |id: &str, author: &AccountId, body: &str, at: i64| omni_types::HistoryLineV1 {
            message_id: id.into(),
            author_account_id: author.0.clone(),
            body: body.into(),
            created_at_unix_ms: at,
            attachment: None,
        };
        // What the passport counted: two lines.
        let counted = crate::LocalChat {
            contact_account_id: bob_account.clone(),
            chat_dialog_key: chat.chat_dialog_key.clone(),
            lines: vec![
                omni_storage::ChatHistoryLine { id: "h1".into(), outgoing: true, body: "a".into(), created_at_unix_ms: 10 },
                omni_storage::ChatHistoryLine { id: "h2".into(), outgoing: false, body: "b".into(), created_at_unix_ms: 20 },
            ],
        };
        let digest = crate::local_chat_digest(&own, &counted);
        crate::start_account_recovery(&alice.app.storage, 0);
        let mut recovery = crate::load_account_recovery(&alice.app.storage).unwrap();
        recovery.chat_fingerprints.push((bob_account.0.clone(), digest.count, hex::encode(digest.root), 100));
        crate::save_account_recovery(&alice.app.storage, &recovery);

        let mallory = AccountId("acc-mallory".into());
        let added = crate::merge_history_lines(
            &alice.app.storage,
            &from,
            "dialog:bob-pc",
            &peer,
            &chat,
            &[line("h1", &own, "a", 10), line("x1", &mallory, "not in this chat", 15)],
            1_000,
        );
        assert_eq!(added, 1, "the third account's line is dropped");
        crate::check_recovered_chat(&alice.app.storage, &peer, 1_000);
        assert_eq!(alice.app.storage.get_chat_history_check(&bob_account).unwrap(), Some(omni_storage::ChatHistoryCheck::Incomplete));
        // Bob "remembers" a line from Alice she never wrote, instead of his own.
        crate::merge_history_lines(&alice.app.storage, &from, "dialog:bob-pc", &peer, &chat, &[line("h9", &own, "forged", 20)], 1_001);
        crate::check_recovered_chat(&alice.app.storage, &peer, 1_001);
        assert_eq!(alice.app.storage.get_chat_history_check(&bob_account).unwrap(), Some(omni_storage::ChatHistoryCheck::Mismatch));
        let header = alice.app.get_chat_header_contact_summary(contact.primary_dialog_key.clone().unwrap()).unwrap();
        assert!(header.subtitle.contains("история не сходится"), "{}", header.subtitle);
    }

    /// A real identity beacon fits what ntfy reads (4096 hex characters = 2 KB), and the finder gets
    /// the whole prekey bundle back from its compact parts (it was JSON of 4.5 KB — read by Nostr only).
    #[test]
    fn a_real_identity_beacon_fits_ntfy_and_its_bundle_comes_back_whole() {
        let app = create_app_with_active_profile(isolated_test_db_path("omni-identity-size", "a"), "Alice", "Laptop");
        let now = current_unix_ms();
        let bundle = app.storage.build_local_prekey_bundle(now).unwrap();
        let chain = bundle.device_certificate_chain.clone().unwrap();
        let beacon = omni_rendezvous::IdentityBeacon {
            omni_id: omni_crypto::omni_id_bytes(&chain.working.master_public_key),
            chain_compact: chain.to_compact(),
            node: [0; 20],
            prekey_compact: bundle.to_compact(),
            device_label: "Laptop".into(),
            reflexive: vec!["203.0.113.9:51820".into(), "[2001:db8::1]:51820".into()],
            tcp_direct: vec!["203.0.113.9:40000".into()],
            relay_bundle: None,
            ts_ms: now as u64,
            ttl_secs: 46_800,
            seq: now as u64,
            signature: vec![7; 64],
        };
        let wire = beacon.to_wire();
        assert!(wire.len() <= 1_000, "identity beacon is {} bytes", wire.len());
        let back = omni_rendezvous::IdentityBeacon::from_wire(&wire).unwrap();
        let bundle_v1 = crate::identity_beacon_prekey_bundle(&back).unwrap();
        omni_protocol::validate_prekey_bundle_v1(&bundle_v1).unwrap();
        let domain = crate::prekey_bundle_from_protocol(&bundle_v1).unwrap();
        omni_crypto::verify_prekey_bundle(&domain).unwrap();
        assert_eq!(domain.signed_prekey, bundle.signed_prekey);
        assert_eq!(bundle_v1.device.device_label, "Laptop");
    }
}
