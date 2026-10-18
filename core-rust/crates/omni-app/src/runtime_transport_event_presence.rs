// Contact-presence persistence: applies inbound presence / ping / pong updates to storage.
// include!d into runtime_transport_events.rs - same module scope + imports. Split for size.

/// `presence` намеренно не читается: его `observed_at_unix_ms` снят ЧУЖИМИ часами, а всё, что мы
/// здесь записываем, — это возраст события по нашим. Оставлен в сигнатуре, потому что это форма
/// обработчика входящего кадра.
fn persist_contact_presence(
    storage: &Storage,
    _presence: &omni_types::ContactPresenceV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    if runtime_chat_session_auto_reconnect_paused(&db_path, &receive_context.dialog_key)? {
        let _ = mark_contact_offline_for_dialog(
            storage,
            &receive_context.dialog_key,
            current_unix_ms(),
        );
        return Ok(());
    }

    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    let Some(contact) = contact else {
        return Ok(());
    };
    if !matches!(
        contact.relationship_state,
        ContactRelationshipState::Approved
    ) {
        return Ok(());
    }

    // Тоже по нашим часам: «видели пира» — событие, случившееся ЗДЕСЬ. Беря максимум с чужой
    // отметкой, мы позволяли пиру с убежавшими вперёд часами оказаться «виденным в будущем», а от
    // этого числа считается возраст присутствия.
    let observed_at_unix_ms = current_unix_ms();
    let _ = storage.record_contact_presence_seen(&contact.contact_id, observed_at_unix_ms);
    if !contact.has_met {
        let _ = storage.mark_contact_met(&contact.contact_id, observed_at_unix_ms);
    }
    Ok(())
}

fn persist_contact_presence_ping(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    ping: &omni_types::ContactPresencePingV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let now = current_unix_ms().max(ping.sent_at_unix_ms);
    let approval_signal_received = is_contact_approval_signal_ping_id(&ping.ping_id);
    let request_signal_received = is_contact_request_signal_ping_id(&ping.ping_id);
    if is_session_resync_signal_ping_id(&ping.ping_id) {
        // Пир говорит, что наши конверты ему больше не расшифровать: наш счётчик ушёл за его окно
        // приёма. Сводится это только пересборкой, и начать её должны МЫ — сессия выводится из
        // нашего bootstrap'а. Сносим свою запись сессии: путь отправки, не найдя established-сессии,
        // сам отправит bootstrap на следующей же попытке (`send_existing_message_encrypted`).
        // См. docs/bugs/session-wedged-by-counter-gap.md.
        return handle_session_resync_request(storage, receive_context, session);
    }
    omni_app_trace_log(
        "ping/entry",
        &format!(
            "ping_id={} approval={} request={} dialog_key={}",
            ping.ping_id, approval_signal_received, request_signal_received, receive_context.dialog_key
        ),
    );
    // Сверка состояния звонка. Стоит СРАЗУ после разбора пинга и до всей работы с контактом: она не
    // зависит ни от чего из неё, а фантомный звонок с открытым микрофоном ждать не должен.
    reconcile_call_with_peer_presence(
        storage,
        &receive_context.dialog_key,
        ping.call_state.as_ref(),
    );
    if request_signal_received {
        let _ = mark_runtime_chat_session_request_signal_observed(
            &db_path,
            &receive_context.dialog_key,
        );
    }
    let mut contact = if let Some(contact) =
        resolve_transport_contact_for_receive_context(storage, receive_context)?
    {
        contact
    } else {
        let Some(contact) = upsert_transport_pending_contact(
            storage,
            receive_context,
            receive_context
                .remote_prekey_bundle
                .as_ref()
                .map(|bundle| bundle.account.account_id.clone())
                .unwrap_or_default(),
            None,
            now,
        )?
        else {
            return Ok(());
        };
        storage.record_contact_presence_activity(&contact.contact_id, now)?;
        if approval_signal_received || request_signal_received {
            let _ = touch_runtime_chat_session_interaction(
                &db_path,
                &receive_context.dialog_key,
                if approval_signal_received {
                    omni_types::ChatSessionInteractionKind::Approval
                } else {
                    omni_types::ChatSessionInteractionKind::Session
                },
                now,
            );
        }
        return Ok(());
    };
    if transport_contact_needs_instance_repair(&contact, receive_context) {
        contact = storage.bind_contact_to_remote_instance(
            &contact.contact_id,
            &receive_context.remote_instance_id,
            None,
            now,
        )?;
    }
    if request_signal_received {
        if let Some(updated_contact) =
            persist_incoming_contact_request_signal(storage, receive_context, now)?
        {
            contact = updated_contact;
        }
    }
    if !matches!(
        contact.relationship_state,
        ContactRelationshipState::Approved
    ) {
        if approval_signal_received || request_signal_received {
            let _ = touch_runtime_chat_session_interaction(
                &db_path,
                &receive_context.dialog_key,
                if approval_signal_received {
                    omni_types::ChatSessionInteractionKind::Approval
                } else {
                    omni_types::ChatSessionInteractionKind::Session
                },
                now,
            );
        }
        return Ok(());
    }

    if !approval_signal_received
        && !request_signal_received
        && runtime_chat_session_auto_reconnect_paused(&db_path, &receive_context.dialog_key)?
    {
        let _ = mark_contact_offline_for_dialog(storage, &receive_context.dialog_key, now);
        return Ok(());
    }

    let _ = storage.record_contact_presence_seen(&contact.contact_id, now);
    let _ = storage.record_contact_presence_activity(&contact.contact_id, now);
    if !contact.has_met {
        let _ = storage.mark_contact_met(&contact.contact_id, now);
    }

    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    if let Some(connection) = transport.connection_snapshot_for_dialog(&receive_context.dialog_key)
    {
        let _ = set_runtime_chat_session_endpoints(
            &db_path,
            &receive_context.dialog_key,
            connection.local_endpoint.as_deref(),
            connection.peer_endpoint.as_deref(),
        );
    }
    if presence_deaf_for_tests() && !request_signal_received && !approval_signal_received {
        // Тестовый шов: keepalive принят, но ответа не будет. Всё остальное (в т.ч. отметки
        // активности выше) отрабатывает как обычно — глохнет ровно ответ.
        omni_app_trace_log(
            "ping/deaf",
            &format!("ping_id={} dialog_key={}", ping.ping_id, receive_context.dialog_key),
        );
        return Ok(());
    }
    if request_signal_received {
        send_contact_approval_signal_for_session(
            storage,
            &transport,
            session,
            Some(&receive_context.dialog_key),
        )?;
    } else {
        let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
            session,
            &omni_types::LogicalEventEnvelopeV1::contact_presence_pong(
                ping.ping_id.clone(),
                current_unix_ms(),
            ),
            SecurityPayloadKind::ContactPresencePong,
            current_unix_ms(),
        )?;
        storage.store_session_record(&mut outgoing.updated_session)?;
        transport
            .send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    }
    let _ = storage.record_contact_presence_activity(&contact.contact_id, current_unix_ms());
    if request_signal_received
        && contact_for_receive_context_needs_verification(storage, receive_context)?
    {
        schedule_verification_challenge_retry_for_active_transport(
            storage,
            Arc::clone(transport_handle),
            Arc::clone(session_lock),
            &contact.contact_id,
            &receive_context.dialog_key,
            &receive_context.peer_id,
        );
    }
    let _ = set_runtime_chat_session_confirmed_keepalive_at(
        &db_path,
        &receive_context.dialog_key,
        current_unix_ms(),
    );
    let _ = touch_runtime_chat_session_interaction(
        &db_path,
        &receive_context.dialog_key,
        if approval_signal_received || request_signal_received {
            omni_types::ChatSessionInteractionKind::Approval
        } else {
            omni_types::ChatSessionInteractionKind::Keepalive
        },
        now,
    );
    Ok(())
}

/// Обработка просьбы пересобрать сессию: снести свою запись, чтобы путь отправки начал заново.
///
/// Кэш исходящих конвертов тоже чистится — он привязан к ключу старой сессии и после пересборки
/// бесполезен (см. `runtime_outgoing_envelopes.rs`).
fn handle_session_resync_request(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    // Только для одобренного контакта: иначе это способ для постороннего сбрасывать чужие сессии.
    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    if !matches!(
        contact.as_ref().map(|value| value.relationship_state),
        Some(ContactRelationshipState::Approved)
    ) {
        omni_app_trace_log("session/resync", "ignored=contact_not_approved");
        return Ok(());
    }
    let deleted = storage
        .delete_session_state_for_session_address(&session.session_address)
        .is_ok();
    clear_outgoing_envelopes_for_profile(&db_path);
    omni_app_trace_log(
        "session/resync",
        &format!(
            "accepted dialog_key={} session={} dropped={deleted} (next send re-bootstraps)",
            receive_context.dialog_key,
            session.session_address.storage_key()
        ),
    );
    Ok(())
}

fn persist_contact_presence_pong(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    pong: &omni_types::ContactPresencePongV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let approval_signal_received = is_contact_approval_signal_ping_id(&pong.ping_id);
    let request_signal_received = is_contact_request_signal_ping_id(&pong.ping_id);
    omni_app_trace_log(
        "pong/entry",
        &format!(
            "ping_id={} approval={} request={} dialog_key={}",
            pong.ping_id, approval_signal_received, request_signal_received, receive_context.dialog_key
        ),
    );
    if request_signal_received {
        let _ = mark_runtime_chat_session_request_signal_observed(
            &db_path,
            &receive_context.dialog_key,
        );
    }
    // Момент ответа — ПО НАШИМ ЧАСАМ, без оглядки на часы пира.
    //
    // Раньше здесь стояло `current_unix_ms().max(pong.responded_at_unix_ms)`, то есть чужая отметка
    // могла победить нашу. А из этого числа вычитается время отправки НАШЕЙ пробы — и расхождение
    // часов между машинами целиком попадает в «пинг». Живой замер 2026-08-14: часы разошлись на
    // 2.6 с, настоящий круг был 254 мс (проба `…330982`, ответ `…331217`), а в интерфейсе стояло
    // 2330 мс. Асимметрия «292 на одной машине, 2330 на другой» — та же причина: одной стороне
    // перекос прибавляется, другой вычитается.
    //
    // Этот код уже спотыкался о расхождение часов: защита от взаимного пейринга пала на разнице
    // 4.3 с и была переписана на сравнение часов пира с часами пира
    // (docs/bugs/mutual-pairing-wedges-session.md). Правило то же: разность двух отметок имеет смысл,
    // только если обе сняты одними часами.
    let observed_at_unix_ms = current_unix_ms();
    let mut contact = if is_contact_approval_signal_ping_id(&pong.ping_id) {
        maybe_promote_transport_pending_contact_to_approved(
            storage,
            receive_context,
            observed_at_unix_ms,
        )?
        .or_else(|| {
            storage
                .resolve_contact_for_dialog(&receive_context.dialog_key)
                .ok()
                .flatten()
        })
    } else {
        storage.resolve_contact_for_dialog(&receive_context.dialog_key)?
    };
    let Some(contact) = contact.take() else {
        return Ok(());
    };
    if !matches!(
        contact.relationship_state,
        ContactRelationshipState::Approved
    ) {
        return Ok(());
    }

    if !approval_signal_received
        && !request_signal_received
        && runtime_chat_session_auto_reconnect_paused(&db_path, &receive_context.dialog_key)?
    {
        let _ = mark_contact_offline_for_dialog(
            storage,
            &receive_context.dialog_key,
            observed_at_unix_ms,
        );
        return Ok(());
    }

    let _ = storage.record_contact_presence_pong(&contact.contact_id, observed_at_unix_ms);
    // Пир ответил — значит наш ping до него доехал. Записываем это в СЕССИЮ: ни has_link, ни
    // last_seen, ни outcome=sent такого доказательства не дают (замер на мобильном:
    // docs/bugs/mesh-link-flaps-on-mobile.md). Шаг 1 из
    // docs/decisions/reachability-belongs-to-the-session.md — только записываем, вердикт отдельно.
    if let Ok(session) = load_receive_session(storage, receive_context) {
        let session_key = session.session_address.storage_key();
        link_session_to_dialog(&db_path, &receive_context.dialog_key, &session_key);
        let _ = note_session_probe_answered(
            &db_path,
            &session_key,
            &pong.ping_id,
            observed_at_unix_ms,
        );
    }
    if !contact.has_met {
        let _ = storage.mark_contact_met(&contact.contact_id, observed_at_unix_ms);
    }
    // ЗДЕСЬ СЧИТАЛСЯ «ПИНГ», И ЭТО БЫЛ НЕ ПИНГ.
    //
    // Момент отправки вытаскивался из строки идентификатора (`presence-<ms>-<hex>`) и
    // штамповался, когда идентификатор ПОРОДИЛИ. В разницу попадал весь наш конвейер: тик
    // очереди, шифрование, session lock, storage lock, отправка несущей — и то же самое на
    // обратном пути. Живой замер 2026-08-31: медиана 322 мс (n=306) при 49 мс на транспорте
    // и 11 мс, которые тратит на ответ собеседник. Показывать это как «Пинг: 335 мс» рядом с
    // «город Б–город А» — врать про сеть.
    //
    // Проход туда-обратно уже меряет транспорт (сглаженный RTT QUIC, RFC 9002), и теперь он
    // доходит до панели своим путём: `peer_link_facts` -> `build_chat_session_summary`.
    // Presence-обмен остаётся тем, чем и был — признаком живости (строка «Keepalive» ниже).
    // См. docs/bugs/ping-was-our-own-pipeline.md.
    let _ = set_runtime_chat_session_confirmed_keepalive_at(
        &db_path,
        &receive_context.dialog_key,
        observed_at_unix_ms,
    );
    let _ = touch_runtime_chat_session_interaction(
        &db_path,
        &receive_context.dialog_key,
        if approval_signal_received {
            omni_types::ChatSessionInteractionKind::Approval
        } else if request_signal_received {
            omni_types::ChatSessionInteractionKind::Session
        } else {
            omni_types::ChatSessionInteractionKind::Keepalive
        },
        observed_at_unix_ms,
    );
    if let Some(transport) = clone_active_transport(transport_handle)? {
        if let Some(connection) =
            transport.connection_snapshot_for_dialog(&receive_context.dialog_key)
        {
            let _ = set_runtime_chat_session_endpoints(
                &db_path,
                &receive_context.dialog_key,
                connection.local_endpoint.as_deref(),
                connection.peer_endpoint.as_deref(),
            );
        }
    }
    if approval_signal_received {
        mark_verification_challenge_ready(
            &db_path,
            &receive_context.dialog_key,
            &receive_context.peer_id,
        );
        if contact_for_receive_context_needs_verification(storage, receive_context)? {
            let _ = maybe_send_verification_challenge(storage, transport_handle, receive_context);
        }
        if let Some(transport) = clone_active_transport(transport_handle)? {
            if let Ok(session) = load_receive_session(storage, receive_context) {
                let _ = force_send_current_public_profile_sync_for_session(
                    storage,
                    &transport,
                    &session,
                    Some(&receive_context.dialog_key),
                );
            }
            // Each send advances the ratchet and stores the session first, so the next one needs the
            // copy just stored: the relay set used to go out of the same old copy, `store_session_record`
            // refused the stale revision, and `let _` swallowed it — my relays never reached a contact on
            // link-up (CONTRIBUTING.md invariant on ratchet sessions).
            if let Ok(session) = load_receive_session(storage, receive_context) {
                // R1c: the contact has just approved us (this branch runs on the approval signal, not
                // on every link-up — there the beacon carries the set) → push my current relay set over
                // the live channel so their mesh learns it at once. See r1c-rotatable-white-relay.md.
                let _ = force_send_relay_set_update_for_session(
                    storage,
                    &transport,
                    &session,
                    Some(&receive_context.dialog_key),
                );
            }
        }
    }
    Ok(())
}
