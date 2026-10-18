use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use omni_crypto::{random_bytes, sign_message, verify_signature};
use omni_identity::ContactRelationshipState;

include!("runtime_transport_event_helpers.rs");
include!("runtime_transport_event_senders.rs");
include!("runtime_transport_event_presence.rs");
include!("runtime_transport_event_verification_registry.rs");

fn maybe_promote_transport_pending_contact_to_approved(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    let mut contact = match resolve_transport_contact_for_receive_context(storage, receive_context)?
    {
        Some(contact) => {
            omni_app_trace_log(
                "promote/branch",
                &format!(
                    "resolved=existing dialog_key={} state={:?} initiated_locally={}",
                    receive_context.dialog_key,
                    contact.relationship_state,
                    receive_context.initiated_locally
                ),
            );
            contact
        }
        None => {
            let db_path = storage.db_path().to_string_lossy().to_string();
            let request_signal_sent = runtime_chat_session_request_signal_sent_for_receive_context(
                &db_path,
                receive_context,
            )?;
            omni_app_trace_log(
                "promote/branch",
                &format!(
                    "resolved=none dialog_key={} initiated_locally={} request_signal_sent={}",
                    receive_context.dialog_key,
                    receive_context.initiated_locally,
                    request_signal_sent
                ),
            );
            if !receive_context.initiated_locally && !request_signal_sent {
                omni_app_trace_log(
                    "promote/gate",
                    "dropped: !initiated_locally && !request_signal_sent",
                );
                return Ok(None);
            }
            let Some(contact) = upsert_transport_approved_contact_from_approval_signal(
                storage,
                receive_context,
                now_unix_ms,
            )?
            else {
                omni_app_trace_log("promote/upsert", "result=none");
                return Ok(None);
            };
            omni_app_trace_log(
                "promote/upsert",
                &format!("result=created contact_id={:?}", contact.contact_id),
            );
            contact
        }
    };
    if transport_contact_needs_instance_repair(&contact, receive_context) {
        contact = storage.bind_contact_to_remote_instance(
            &contact.contact_id,
            &receive_context.remote_instance_id,
            Some(&receive_context.dialog_key),
            now_unix_ms,
        )?;
    }
    if contact.relationship_state != ContactRelationshipState::Pending {
        return Ok(Some(contact));
    }

    let mut approved = storage.approve_contact(&contact.contact_id, now_unix_ms)?;
    let _ = storage.record_contact_presence_activity(&approved.contact_id, now_unix_ms);
    if !approved.has_met {
        approved = storage.mark_contact_met(&approved.contact_id, now_unix_ms)?;
    }
    Ok(Some(approved))
}

fn runtime_chat_session_request_signal_sent_for_receive_context(
    db_path: &str,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let dialog_keys = runtime_chat_session_equivalent_dialog_keys(receive_context);
    for dialog_key in dialog_keys {
        if runtime_chat_session_request_signal_sent(db_path, &dialog_key)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn runtime_chat_session_approval_signal_pending_for_receive_context(
    db_path: &str,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let dialog_keys = runtime_chat_session_equivalent_dialog_keys(receive_context);
    for dialog_key in dialog_keys {
        if runtime_chat_session_approval_signal_pending(db_path, &dialog_key)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn runtime_chat_session_equivalent_dialog_keys(
    receive_context: &ReceivedChatContext,
) -> Vec<String> {
    let mut dialog_keys = Vec::new();
    dialog_keys.push(receive_context.dialog_key.clone());
    dialog_keys.push(dialog_key_from_instance_id(&receive_context.remote_instance_id));
    if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        dialog_keys.push(omni_types::dialog_key_from_account_id(
            &remote_bundle.account.account_id,
        ));
    }

    dialog_keys.sort();
    dialog_keys.dedup();
    dialog_keys
}

fn upsert_transport_approved_contact_from_approval_signal(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return Ok(None);
    };
    if transport_receive_context_is_local_identity(storage, receive_context)? {
        return Ok(None);
    }

    let remote_instance_id = receive_context.remote_instance_id.trim();
    let mut contact = storage.add_contact(
        &AddContactInput {
            petname: remote_bundle.account.account_id.clone(),
            remote_account_id: Some(AccountId(remote_bundle.account.account_id.clone())),
            public_display_name: None,
            remote_instance_id: (!remote_instance_id.is_empty())
                .then(|| remote_instance_id.to_string()),
            primary_dialog_key: Some(receive_context.dialog_key.clone()),
            initial_observation: Some(transport_contact_observation_from_bundle(
                remote_bundle,
                now_unix_ms,
            )),
        },
        now_unix_ms,
    )?;
    if transport_contact_needs_instance_repair(&contact, receive_context) {
        contact = storage.bind_contact_to_remote_instance(
            &contact.contact_id,
            remote_instance_id,
            Some(&receive_context.dialog_key),
            now_unix_ms,
        )?;
    }
    let _ = storage.record_contact_presence_activity(&contact.contact_id, now_unix_ms);
    if !contact.has_met {
        contact = storage.mark_contact_met(&contact.contact_id, now_unix_ms)?;
    }
    Ok(Some(contact))
}

fn upsert_transport_pending_contact(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    petname: String,
    public_display_name: Option<String>,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return Ok(None);
    };
    if transport_receive_context_is_local_identity(storage, receive_context)? {
        return Ok(None);
    }
    // TM-008 (2026-05-21): rate-limit auto-created pending contacts.
    // Only NEW peers (no existing entry for the bundle account_id) are
    // counted; idempotent calls for already-Pending entries pass freely
    // because they just update remote_instance_id / observation
    // metadata. Beyond TM008_PENDING_RATE_LIMIT new pendings inside the
    // sliding window, the function returns None silently — the
    // session stays alive but the spam peer simply never surfaces in
    // the contacts UI. See
    // `docs/decisions/tm-008-pending-contact-rate-limit.md`.
    let remote_account_id = AccountId(remote_bundle.account.account_id.clone());
    let already_known = storage
        .find_contact_by_remote_account_id(&remote_account_id)?
        .is_some();
    if !already_known {
        let db_path = storage.db_path().to_string_lossy().to_string();
        if !record_pending_contact_creation_under_rate_limit(&db_path, now_unix_ms) {
            omni_app_trace_log(
                "tm008/pending/drop",
                &format!(
                    "rate_limit_exceeded remote_account={} remote_instance={}",
                    remote_bundle.account.account_id,
                    receive_context.remote_instance_id
                ),
            );
            return Ok(None);
        }
    }
    let remote_instance_id = receive_context.remote_instance_id.trim();
    let mut contact = match storage.add_pending_contact(
        &AddContactInput {
            petname,
            remote_account_id: Some(remote_account_id),
            public_display_name,
            remote_instance_id: (!remote_instance_id.is_empty())
                .then(|| remote_instance_id.to_string()),
            primary_dialog_key: Some(receive_context.dialog_key.clone()),
            initial_observation: Some(transport_contact_observation_from_bundle(
                remote_bundle,
                now_unix_ms,
            )),
        },
        now_unix_ms,
    ) {
        Ok(contact) => {
            omni_app_trace_log(
                "recv/contact_request",
                &format!("upsert created pending contact={}", contact.contact_id.0),
            );
            contact
        }
        Err(error) => {
            omni_app_trace_log(
                "recv/contact_request",
                &format!("upsert add_pending_contact FAILED: {error}"),
            );
            return Err(error);
        }
    };
    if transport_contact_needs_instance_repair(&contact, receive_context) {
        contact = storage.bind_contact_to_remote_instance(
            &contact.contact_id,
            remote_instance_id,
            Some(&receive_context.dialog_key),
            now_unix_ms,
        )?;
    }
    Ok(Some(contact))
}

fn persist_incoming_contact_request_signal(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    // Receiver-side visibility (2026-07-20): the add-by-Omni-ID sender's log showed the request
    // bootstrap "sent over mesh", but the receiver never surfaced a Pending. This trace makes the
    // receive side observable — did the request frame arrive and get turned into a contact?
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        omni_app_trace_log(
            "recv/contact_request",
            &format!(
                "dropped: no remote prekey bundle dialog_key={}",
                receive_context.dialog_key
            ),
        );
        return Ok(None);
    };
    omni_app_trace_log(
        "recv/contact_request",
        &format!(
            "incoming from account={} dialog_key={}",
            remote_bundle.account.account_id, receive_context.dialog_key
        ),
    );

    let mut contact = match resolve_transport_contact_for_receive_context(storage, receive_context)?
    {
        Some(contact) => contact,
        None => {
            // The account is a contact already, yet this device did not prove it is one of its
            // devices: no request — it must not touch (or rebind) the contact it claims to be.
            let claimed = AccountId(remote_bundle.account.account_id.clone());
            if storage.find_contact_by_remote_account_id(&claimed)?.is_some() {
                omni_app_trace_log(
                    "recv/contact_request",
                    &format!(
                        "dropped: device {} is not a certified device of known account {}",
                        remote_bundle.device.device_id, claimed.0
                    ),
                );
                return Ok(None);
            }
            return upsert_transport_pending_contact(
                storage,
                receive_context,
                remote_bundle.account.account_id.clone(),
                None,
                now_unix_ms,
            );
        }
    };

    if transport_contact_needs_instance_repair(&contact, receive_context) {
        contact = storage.bind_contact_to_remote_instance(
            &contact.contact_id,
            &receive_context.remote_instance_id,
            Some(&receive_context.dialog_key),
            now_unix_ms,
        )?;
    }

    let should_auto_approve_crossed_request = contact.relationship_state
        == ContactRelationshipState::Pending
        && contact.pending_request_direction == Some(ContactRequestDirection::Outgoing);

    if contact.relationship_state == ContactRelationshipState::Pending {
        contact = match upsert_transport_pending_contact(
            storage,
            receive_context,
            remote_bundle.account.account_id.clone(),
            None,
            now_unix_ms,
        )? {
            Some(contact) => contact,
            None => return Ok(None),
        };
        let _ = storage.record_contact_presence_activity(&contact.contact_id, now_unix_ms);

        if should_auto_approve_crossed_request {
            let approved = storage.approve_contact(&contact.contact_id, now_unix_ms)?;
            let _ = storage.record_contact_presence_activity(&approved.contact_id, now_unix_ms);
            omni_app_trace_log(
                "recv/contact_request",
                &format!("auto-approved crossed request contact={}", approved.contact_id.0),
            );
            return Ok(Some(approved));
        }
    }

    omni_app_trace_log(
        "recv/contact_request",
        &format!(
            "persisted contact={} state={:?}",
            contact.contact_id.0, contact.relationship_state
        ),
    );
    Ok(Some(contact))
}

/// An approved contact, or another device of ours: a copy of our own message fetches its file from
/// the device of ours that sent it.
fn attachment_control_event_allowed(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<bool> {
    let from = &session.session_address;
    if storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        return Ok(true);
    }
    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    Ok(matches!(
        contact.as_ref().map(|value| value.relationship_state),
        Some(ContactRelationshipState::Approved)
    ))
}

/// Whether `requester` may fetch the file of `message_id`: the contact the message went to (any of
/// its devices), or another device of ours. The file streams to whoever asked, so this is what
/// keeps one contact from fetching a file sent to another. On a device of ours that holds a copy of
/// the message (its file came from the device that sent it), the contact is the chat's — the copy's
/// file row sits under the dialog of our device it came from.
fn attachment_requester_allowed(
    storage: &Storage,
    message_id: &str,
    requester: &omni_crypto::SessionAddress,
) -> OmniResult<bool> {
    let Some(runtime) = storage.get_attachment_runtime_info(message_id)? else {
        return Ok(true); // nothing to leak: the sender reports the file as unavailable
    };
    if storage.is_own_other_device(&requester.remote_account_id, &requester.remote_device_id)? {
        return Ok(true);
    }
    let chat_dialog_key = storage.get_message(message_id).map(|message| message.dialog_key).ok();
    for dialog_key in std::iter::once(runtime.dialog_key.clone()).chain(chat_dialog_key) {
        let contact_account = storage.resolve_contact_for_dialog(&dialog_key)?.and_then(|contact| contact.remote_account_id);
        if contact_account.is_some_and(|account_id| account_id == requester.remote_account_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn contact_for_receive_context_needs_verification(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let Some(contact) = resolve_transport_contact_for_receive_context(storage, receive_context)?
    else {
        return Ok(false);
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(false);
    }

    Ok(storage.get_contact_card(&contact.contact_id)?.trust_state
        != omni_identity::TrustState::Verified)
}

fn decrypted_payload_defers_verification_challenge(payload: &omni_types::WirePayloadV1) -> bool {
    match payload {
        omni_types::WirePayloadV1::LogicalEvent(event) => match &event.payload {
            omni_types::LogicalEventPayloadV1::ContactPresencePing(ping) => {
                is_contact_request_signal_ping_id(&ping.ping_id)
                    || is_contact_approval_signal_ping_id(&ping.ping_id)
            }
            omni_types::LogicalEventPayloadV1::ContactPresencePong(pong) => {
                is_contact_request_signal_ping_id(&pong.ping_id)
                    || is_contact_approval_signal_ping_id(&pong.ping_id)
            }
            _ => false,
        },
        _ => false,
    }
}

/// Приёмник просит отправителя пересобрать сессию, когда его счётчик ушёл за окно приёма — или когда
/// кадры одобренного контакта подряд не проходят аутентификацию.
///
/// Повтор штатен (конверт приехал двумя путями) — на него не реагируем. Провал аутентификации раньше
/// тоже считался «чем-то другим, что пересборкой не лечится», и это оказалось неверно: так выглядит
/// РАЗОШЕДШАЯСЯ сессия — стороны вывели разные ключи (повторное спаривание поверх рабочей сессии,
/// гонка встречных bootstrap). Транспорт аутентифицирован сам, так что порчи кадра в пути тут нет, а
/// разошедшаяся сессия лечится ровно пересборкой: наш свежий bootstrap новее всего, что пир от нас
/// видел, и для одобренного контакта он принимается (`existing_session_should_absorb_incoming_bootstrap`).
/// docs/bugs/repair-on-restart-diverges-session.md, часть 2. Порог для провала аутентификации выше,
/// и у свежей сессии есть льготное окно — `runtime_session_resync.rs`.
///
/// Сигнал уходит presence-пингом со специальным `ping_id` по ОБРАТНОМУ направлению, счётчики
/// которого независимы и потому целы. Лучшая попытка: не смогли отправить — попробуем на следующем
/// отвергнутом конверте.
fn maybe_request_session_resync(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    error: &OmniError,
) {
    let message = error.to_string();
    let counter_gap = message.contains("encrypted payload counter exceeded");
    if !counter_gap && !message.contains("encrypted payload authentication failed") {
        return;
    }
    let db_path = storage.db_path().to_string_lossy().to_string();
    let now = current_unix_ms();
    let due = if counter_gap {
        note_session_counter_gap(&db_path, &receive_context.dialog_key, now)
    } else {
        // Сессия, которую мы сами только что (пере)собрали, ещё получает кадры старой — из пути и
        // из durable-хранения пира. Они и не обязаны расшифровываться; считать их — значит
        // пересобирать в ответ на собственную пересборку.
        let session_created_at = load_receive_session(storage, receive_context)
            .map(|session| session.created_at_unix_ms)
            .unwrap_or(now);
        note_session_auth_failure(&db_path, &receive_context.dialog_key, now, session_created_at)
    };
    if !due {
        return;
    }
    let Ok(session) = load_receive_session(storage, receive_context) else {
        omni_app_trace_log("session/resync", "skip=no_session");
        return;
    };
    omni_app_trace_log(
        "session/resync",
        &format!(
            "cause={} dialog_key={}",
            if counter_gap { "counter_gap" } else { "auth_failed" },
            receive_context.dialog_key
        ),
    );
    // ПОРЯДОК: сначала просьба, потом снос своей стороны. Наоборот — нельзя, и это уже стоило одного
    // прогона: успешная отправка просьбы сохраняет сессию обратно (`store_session_record` ниже), то
    // есть ровно ту запись, которую снос только что удалил. В трассе это выглядело как честная
    // работа — `session/rebuild dropped=true` каждые 30 секунд, — а на деле мы сносили и тут же
    // восстанавливали одно и то же, и пара не сходилась никогда.
    //
    // Сама просьба оставлена, потому что дёшева и помогает, когда сломано ОДНО направление. Но
    // рассчитывать на неё нельзя: она едет зашифрованной той самой сессией, которая сломана. Живой
    // замер 2026-08-14 — заклинило оба направления, обе стороны отправили по 158 просьб, принятых
    // ноль. Механизм, которому нужен исправный канал, чтобы починить канал, не работает.
    let ping_id = format!("{SESSION_RESYNC_SIGNAL_PREFIX}{}", current_unix_ms());
    let outgoing = match omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::contact_presence_ping(
            ping_id.clone(),
            current_unix_ms(),
        ),
        SecurityPayloadKind::ContactPresencePing,
        current_unix_ms(),
    ) {
        Ok(value) => value,
        Err(error) => {
            omni_app_trace_log("session/resync", &format!("skip=build_failed error={error}"));
            return;
        }
    };
    // Отправка — с отдельного потока, по той же причине, что и у bootstrap'а ниже: мы под локами
    // сессии и хранилища, а фасад отправки блокирующий. Раньше эта строка не морозила интерфейс лишь
    // потому, что отправка сразу падала с «no active QUIC carrier session»; как только карьер
    // починили, она стала успевать блокировать по-настоящему.
    //
    // Обновлённую запись сессии НЕ сохраняем: строкой ниже мы её всё равно сносим, и сохранение
    // ровно этим и ломало механизм в прошлом прогоне.
    {
        let transport = transport.clone();
        let dialog_key = receive_context.dialog_key.clone();
        let envelope = outgoing.envelope.clone();
        std::thread::spawn(move || {
            match transport.send_wire_envelope_for_dialog_key(&dialog_key, &envelope) {
                Ok(()) => omni_app_trace_log(
                    "session/resync",
                    &format!("requested dialog_key={dialog_key}"),
                ),
                Err(error) => omni_app_trace_log(
                    "session/resync",
                    &format!("skip=send_failed error={error}"),
                ),
            }
        });
    }
    // А теперь — своя сторона, и это последнее действие: после него запись сессии никто не вернёт.
    drop_own_session_after_counter_gap(storage, transport, receive_context, &session);
}

/// Сносим СВОЮ запись сессии, когда разрыв счётчика доказал, что она разошлась с той стороной.
///
/// Это часть 3 из `docs/bugs/session-wedged-by-counter-gap.md`, и живой замер перевёл её из
/// «страховки» в ОСНОВНОЙ механизм: просьба к пиру (часть 2) едет по сломанной сессии и потому
/// физически не может доехать, когда сломаны оба направления.
///
/// Только для одобренного контакта — по той же причине, что и у обработчика просьбы: иначе
/// посторонний, засыпав нас мусорными конвертами, сбрасывал бы чужие сессии.
fn drop_own_session_after_counter_gap(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) {
    let Ok(contact) = storage.resolve_contact_for_dialog(&receive_context.dialog_key) else {
        return;
    };
    if !matches!(
        contact.as_ref().map(|value| value.relationship_state),
        Some(ContactRelationshipState::Approved)
    ) {
        omni_app_trace_log("session/rebuild", "skip=contact_not_approved");
        return;
    }
    let dropped = storage
        .delete_session_state_for_session_address(&session.session_address)
        .is_ok();
    clear_outgoing_envelopes_for_profile(&storage.db_path().to_string_lossy());
    omni_app_trace_log(
        "session/rebuild",
        &format!(
            "dropped own session dialog_key={} session={} dropped={dropped}",
            receive_context.dialog_key,
            session.session_address.storage_key()
        ),
    );
    send_fresh_bootstrap_after_rebuild(storage, transport, receive_context, session);
}

/// Снести сессию мало — надо ещё ЗАГОВОРИТЬ. «Следующая отправка пересоберёт» работает, только если
/// нам есть что отправлять; у стороны, которая всё своё уже доставила, следующей отправки просто не
/// случается, и пара стоит с обеих сторон молча. Живой прогон 2026-08-14: `session/rebuild` каждые
/// 30 секунд и ноль движения.
///
/// Поэтому после сноса отправляем bootstrap сами — живьём и в durable. Bootstrap сессией не
/// шифруется, поэтому доезжает даже к пиру, у которого приём по этой сессии заклинен.
fn send_fresh_bootstrap_after_rebuild(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) {
    let remote_account_id = session.session_address.remote_account_id.clone();
    let bundle = match storage.get_latest_remote_prekey_bundle_for_account(&remote_account_id) {
        Ok(Some(mut bundle)) => {
            // Одноразовый предключ пира НЕ тратим: пересборок может быть много, а пул у той стороны
            // конечный — именно его исчерпание сегодня сделало профиль неоткрываемым
            // (docs/bugs/one-time-prekeys-run-out-and-brick-the-profile.md). Для повторного
            // знакомства с уже известным контактом X3DH без одноразового ключа допустим.
            bundle.one_time_prekey = None;
            bundle
        }
        _ => {
            omni_app_trace_log("session/rebuild", "skip=no_remote_bundle");
            return;
        }
    };
    let bootstrap = match bootstrap_session_for_remote_bundle(storage, &bundle, current_unix_ms()) {
        Ok((_session, bootstrap, _)) => bootstrap,
        Err(error) => {
            omni_app_trace_log("session/rebuild", &format!("skip=bootstrap_failed error={error}"));
            return;
        }
    };
    let envelope = match omni_messaging::wrap_session_bootstrap(bootstrap) {
        Ok(envelope) => envelope,
        Err(error) => {
            omni_app_trace_log("session/rebuild", &format!("skip=wrap_failed error={error}"));
            return;
        }
    };
    // ОТПРАВКА — НА ОТДЕЛЬНОМ ПОТОКЕ, и это не оптимизация. Мы находимся внутри обработчика приёма,
    // под захваченными локами сессии и хранилища (`wire_env/session-locked`, `wire_env/storage-locked`),
    // а `send_wire_envelope_for_dialog_key` — блокирующий фасад с `block_on` внутри. Вызванный отсюда
    // напрямую, он держит локи всё время отправки: интерфейс замирает на любом обращении к
    // хранилищу, окно перестаёт нажиматься. Поймано живьём 2026-08-14 — `state` отвечал мгновенно,
    // `chats` не отвечал за 30 с.
    //
    // Это уже известный этому коду класс ошибки: `2bc0efa` (`on_wire` исполнялся inline на
    // tokio-воркере и дедлочил памп) и `finish_mesh_pairing_from_hello`, где ровно по этой причине
    // стоит `std::thread::spawn` с таким же комментарием. Сборку bootstrap'а оставляем здесь — она
    // работает с хранилищем, которое мы и так держим; наружу уходит только отправка.
    let transport = transport.clone();
    let dialog_key = receive_context.dialog_key.clone();
    std::thread::spawn(move || {
        let sent = transport
            .send_wire_envelope_for_dialog_key(&dialog_key, &envelope)
            .is_ok();
        let held = transport.hold_durable_for_dialog_key(&dialog_key, &envelope);
        omni_app_trace_log(
            "session/rebuild",
            &format!("fresh bootstrap sent dialog_key={dialog_key} live={sent} durable={held}"),
        );
    });
}

fn should_ignore_stale_encrypted_payload_error(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    error: &OmniError,
) -> OmniResult<bool> {
    let message = error.to_string();
    let looks_like_stale_ratchet_payload = message
        .contains("encrypted payload authentication failed")
        || message.contains("encrypted payload counter exceeded")
        || message.contains("encrypted payload counter was replayed")
        || message.contains("encrypted payload was rejected by replay window")
        || message.contains("encrypted payload is from an unknown ratchet chain");
    if !looks_like_stale_ratchet_payload {
        return Ok(false);
    }

    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    Ok(!matches!(
        contact.as_ref().map(|value| value.relationship_state),
        Some(ContactRelationshipState::Approved)
    ))
}

fn lock_verification_registry(
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, VerificationChallengeRegistry>>> {
    verification_registry_for_db("").lock().map_err(|_| {
        OmniError::Internal("verification challenge registry lock is poisoned".to_string())
    })
}

fn remove_registry_entry_if_empty(
    registry: &mut HashMap<String, VerificationChallengeRegistry>,
    db_path: &str,
) {
    if registry
        .get(db_path)
        .is_some_and(|entry| entry.pending_by_id.is_empty() && entry.ready_session_keys.is_empty())
    {
        registry.remove(db_path);
    }
}

/// Lock a lazily-initialised global `HashMap` registry, mapping a poisoned lock to a uniform
/// internal error. Collapses the per-registry `get_or_init(..).lock().map_err(..)` boilerplate that
/// was copy-pasted across the runtime registries.
fn lock_global_registry<V>(
    cell: &'static OnceLock<Mutex<HashMap<String, V>>>,
    name: &str,
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, V>>> {
    cell.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| OmniError::Internal(format!("{name} registry lock is poisoned")))
}

fn lock_public_profile_sync_registry(
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, PublicProfileSyncRegistryEntry>>> {
    lock_global_registry(&PUBLIC_PROFILE_SYNCS_SENT, "public profile sync")
}

fn lock_relay_set_push_registry(
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, i64>>> {
    lock_global_registry(&RELAY_SET_PUSH_SENT, "relay set push")
}

fn relay_set_push_registry_key(
    db_path: &str,
    remote_account_id: &AccountId,
    remote_device_id: &DeviceId,
) -> String {
    format!("{}::{}::{}", db_path, remote_account_id.0, remote_device_id.0)
}

#[allow(clippy::type_complexity)] // guards the transfer-id → (offset → chunk) reorder buffers
fn lock_attachment_reorder_registry(
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, BTreeMap<i64, Vec<u8>>>>> {
    lock_global_registry(&ATTACHMENT_REORDER_BUFFERS, "attachment reorder")
}

fn attachment_reorder_registry_key(db_path: &str, transfer_id: &str) -> String {
    format!("{db_path}::{transfer_id}")
}

fn clear_attachment_reorder_buffer(db_path: &str, transfer_id: &str) -> OmniResult<()> {
    let key = attachment_reorder_registry_key(db_path, transfer_id);
    lock_attachment_reorder_registry()?.remove(&key);
    Ok(())
}

fn clear_attachment_transfer_runtime_buffers(db_path: &str, transfer_id: &str) -> OmniResult<()> {
    let key = attachment_reorder_registry_key(db_path, transfer_id);
    lock_attachment_reorder_registry()?.remove(&key);
    drop_attachment_receive_writer(db_path, transfer_id);
    Ok(())
}

/// A download the sender ended for good (failed, revoked): its writer lets go of the temp file and
/// its held chunks go — a writer left behind kept the file open and the download counted as
/// streaming (`pause_inflight_attachment_downloads`).
fn end_attachment_download_runtime(storage: &Storage, message_id: &str) {
    if let Ok(Some(transfer_id)) =
        storage.get_attachment_runtime_info(message_id).map(|runtime| runtime.and_then(|runtime| runtime.active_transfer_id))
    {
        let _ = clear_attachment_transfer_runtime_buffers(&storage.db_path().to_string_lossy(), &transfer_id);
    }
}

fn buffer_attachment_chunk_for_reorder(
    db_path: &str,
    transfer_id: &str,
    file_size_bytes: i64,
    offset_bytes: i64,
    data: Vec<u8>,
) -> OmniResult<()> {
    if offset_bytes < 0 {
        return Err(OmniError::Storage(
            "attachment transfer received a negative out-of-order chunk offset".to_string(),
        ));
    }
    let incoming_end = offset_bytes.checked_add(data.len() as i64).ok_or_else(|| {
        OmniError::Storage("attachment transfer chunk offset overflowed".to_string())
    })?;
    if incoming_end > file_size_bytes {
        return Err(OmniError::Storage(
            "attachment transfer chunk exceeds the offered file size".to_string(),
        ));
    }
    let registry_key = attachment_reorder_registry_key(db_path, transfer_id);
    let mut registry = lock_attachment_reorder_registry()?;
    let entry = registry.entry(registry_key).or_insert_with(BTreeMap::new);
    if let Some(existing) = entry.get(&offset_bytes) {
        if existing == &data {
            return Ok(());
        }
        return Err(OmniError::Storage(
            "attachment transfer received conflicting out-of-order chunk data".to_string(),
        ));
    }
    let buffered_bytes = entry.values().map(|chunk| chunk.len()).sum::<usize>();
    if buffered_bytes.saturating_add(data.len()) > ATTACHMENT_REORDER_BUFFER_MAX_BYTES {
        return Err(OmniError::Storage(format!(
            "attachment transfer out-of-order buffer exceeds {} bytes",
            ATTACHMENT_REORDER_BUFFER_MAX_BYTES
        )));
    }
    entry.insert(offset_bytes, data);
    Ok(())
}

fn public_profile_sync_registry_key(
    db_path: &str,
    remote_account_id: &AccountId,
    remote_device_id: &DeviceId,
) -> String {
    format!(
        "{}::{}::{}",
        db_path, remote_account_id.0, remote_device_id.0
    )
}

fn should_send_public_profile_descriptor_sync(
    entry: Option<&PublicProfileSyncRegistryEntry>,
    profile_updated_at_unix_ms: i64,
    session_created_at_unix_ms: i64,
) -> bool {
    !entry.is_some_and(|value| {
        value.descriptor_updated_at_unix_ms >= profile_updated_at_unix_ms
            && value.descriptor_session_created_at_unix_ms == Some(session_created_at_unix_ms)
    })
}

fn reset_public_profile_avatar_sync_entry(
    entry: &mut PublicProfileSyncRegistryEntry,
    avatar_hash_hex: Option<&str>,
    session_created_at_unix_ms: i64,
) {
    let desired_avatar_hash_hex = avatar_hash_hex.map(str::to_owned);
    if entry.avatar_hash_hex != desired_avatar_hash_hex
        || entry.avatar_session_created_at_unix_ms != Some(session_created_at_unix_ms)
    {
        entry.avatar_hash_hex = desired_avatar_hash_hex;
        entry.avatar_session_created_at_unix_ms = Some(session_created_at_unix_ms);
        entry.avatar_last_attempt_at_unix_ms = None;
        entry.avatar_attempt_count = 0;
    }
}

fn public_profile_avatar_sync_backoff_ms(attempt_count: u8) -> i64 {
    if attempt_count == 0 {
        return 0;
    }
    let backoff_index = attempt_count.saturating_sub(1) as usize;
    PUBLIC_PROFILE_AVATAR_SYNC_RETRY_BACKOFF_MS
        .get(backoff_index)
        .copied()
        .unwrap_or_else(|| {
            *PUBLIC_PROFILE_AVATAR_SYNC_RETRY_BACKOFF_MS
                .last()
                .unwrap_or(&0)
        })
}

fn should_send_public_profile_avatar_sync(
    entry: Option<&PublicProfileSyncRegistryEntry>,
    avatar_hash_hex: &str,
    session_created_at_unix_ms: i64,
    now_unix_ms: i64,
) -> bool {
    let Some(entry) = entry else {
        return true;
    };
    if entry.avatar_hash_hex.as_deref() != Some(avatar_hash_hex)
        || entry.avatar_session_created_at_unix_ms != Some(session_created_at_unix_ms)
    {
        return true;
    }
    if entry.avatar_attempt_count >= PUBLIC_PROFILE_AVATAR_SYNC_MAX_ATTEMPTS {
        return false;
    }
    let Some(last_attempt_at_unix_ms) = entry.avatar_last_attempt_at_unix_ms else {
        return true;
    };
    now_unix_ms.saturating_sub(last_attempt_at_unix_ms)
        >= public_profile_avatar_sync_backoff_ms(entry.avatar_attempt_count)
}

#[cfg(test)]
mod runtime_transport_events_unit_tests {
    use super::*;

    #[test]
    fn descriptor_sync_resends_for_new_session_with_same_profile_version() {
        let entry = PublicProfileSyncRegistryEntry {
            descriptor_updated_at_unix_ms: 42,
            descriptor_session_created_at_unix_ms: Some(100),
            ..PublicProfileSyncRegistryEntry::default()
        };

        assert!(!should_send_public_profile_descriptor_sync(
            Some(&entry),
            42,
            100,
        ));
        assert!(should_send_public_profile_descriptor_sync(
            Some(&entry),
            42,
            101,
        ));
    }

    #[test]
    fn avatar_sync_retries_are_bounded_with_backoff() {
        let avatar_hash_hex = "hash-a";
        let session_created_at_unix_ms = 100;
        let mut entry = PublicProfileSyncRegistryEntry::default();

        reset_public_profile_avatar_sync_entry(
            &mut entry,
            Some(avatar_hash_hex),
            session_created_at_unix_ms,
        );
        assert!(should_send_public_profile_avatar_sync(
            Some(&entry),
            avatar_hash_hex,
            session_created_at_unix_ms,
            1_000,
        ));

        entry.avatar_hash_hex = Some(avatar_hash_hex.to_string());
        entry.avatar_session_created_at_unix_ms = Some(session_created_at_unix_ms);
        entry.avatar_last_attempt_at_unix_ms = Some(1_000);
        entry.avatar_attempt_count = 1;

        assert!(!should_send_public_profile_avatar_sync(
            Some(&entry),
            avatar_hash_hex,
            session_created_at_unix_ms,
            1_000 + public_profile_avatar_sync_backoff_ms(1) - 1,
        ));
        assert!(should_send_public_profile_avatar_sync(
            Some(&entry),
            avatar_hash_hex,
            session_created_at_unix_ms,
            1_000 + public_profile_avatar_sync_backoff_ms(1),
        ));

        entry.avatar_attempt_count = PUBLIC_PROFILE_AVATAR_SYNC_MAX_ATTEMPTS;
        assert!(!should_send_public_profile_avatar_sync(
            Some(&entry),
            avatar_hash_hex,
            session_created_at_unix_ms,
            99_999,
        ));
    }

    #[test]
    fn avatar_sync_resets_attempt_budget_for_new_session() {
        let avatar_hash_hex = "hash-a";
        let entry = PublicProfileSyncRegistryEntry {
            avatar_hash_hex: Some(avatar_hash_hex.to_string()),
            avatar_session_created_at_unix_ms: Some(100),
            avatar_last_attempt_at_unix_ms: Some(1_000),
            avatar_attempt_count: PUBLIC_PROFILE_AVATAR_SYNC_MAX_ATTEMPTS,
            ..PublicProfileSyncRegistryEntry::default()
        };

        assert!(should_send_public_profile_avatar_sync(
            Some(&entry),
            avatar_hash_hex,
            101,
            1_001,
        ));
    }
}

fn verification_challenge_ready_session_key(dialog_key: &str, peer_id: &str) -> String {
    format!("{dialog_key}::{peer_id}")
}

fn mark_verification_challenge_ready(db_path: &str, dialog_key: &str, peer_id: &str) {
    let Ok(mut registry) = lock_verification_registry() else {
        return;
    };
    let entry = registry
        .entry(db_path.to_string())
        .or_insert_with(VerificationChallengeRegistry::default);
    entry
        .ready_session_keys
        .insert(verification_challenge_ready_session_key(
            dialog_key,
            peer_id,
        ));
}

fn verification_challenge_is_ready(
    db_path: &str,
    dialog_key: &str,
    peer_id: &str,
) -> bool {
    let Ok(mut registry) = lock_verification_registry() else {
        return false;
    };
    let Some(entry) = registry.get_mut(db_path) else {
        return false;
    };
    let ready = entry
        .ready_session_keys
        .contains(&verification_challenge_ready_session_key(
            dialog_key,
            peer_id,
        ));
    remove_registry_entry_if_empty(&mut registry, db_path);
    ready
}

fn clear_verification_challenge_ready(db_path: &str, dialog_key: &str, peer_id: &str) {
    let Ok(mut registry) = lock_verification_registry() else {
        return;
    };
    if let Some(entry) = registry.get_mut(db_path) {
        entry
            .ready_session_keys
            .remove(&verification_challenge_ready_session_key(
                dialog_key,
                peer_id,
            ));
    }
    remove_registry_entry_if_empty(&mut registry, db_path);
}

pub(crate) fn clear_verification_challenges_for_contact(
    storage: &Storage,
    contact: &ContactSnapshot,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let dialog_key = contact.primary_dialog_key.as_deref();
    let remote_account_id = contact
        .remote_account_id
        .as_ref()
        .map(|value| value.0.as_str());
    if dialog_key.is_none() && remote_account_id.is_none() {
        return Ok(());
    }

    let mut registry = lock_verification_registry()?;
    if let Some(entry) = registry.get_mut(&db_path) {
        if let Some(dialog_key) = dialog_key {
            let ready_prefix = format!("{dialog_key}::");
            entry
                .ready_session_keys
                .retain(|session_key| !session_key.starts_with(&ready_prefix));
        }
        entry.pending_by_id.retain(|_, pending| {
            let matches_dialog = dialog_key.is_some_and(|value| pending.dialog_key == value);
            let matches_account = remote_account_id
                .is_some_and(|value| pending.remote_account_id.as_deref() == Some(value));
            !matches_dialog && !matches_account
        });
    }
    remove_registry_entry_if_empty(&mut registry, &db_path);
    Ok(())
}

fn delete_session_state_for_received_context(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return;
    };
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: AccountId(remote_bundle.account.account_id.clone()),
        remote_device_id: DeviceId(remote_bundle.device.device_id.clone()),
    };
    let _ = storage.delete_session_state_for_session_address(&session_address);
}

fn delete_unestablished_session_state_for_received_context(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return Ok(());
    };
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: AccountId(remote_bundle.account.account_id.clone()),
        remote_device_id: DeviceId(remote_bundle.device.device_id.clone()),
    };
    let Some(session) = storage.get_session_record(&session_address)? else {
        return Ok(());
    };
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        storage.delete_session_state_for_session_address(&session_address)?;
    }
    Ok(())
}

fn disconnect_active_transport_for_verification_failure(
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
) {
    if let Ok(Some(transport)) = clone_active_transport(transport_handle) {
        let _ = transport.disconnect_and_reset();
    }
}

fn fail_closed_verification_challenge_response(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    db_path: &str,
    pending: &PendingVerificationChallenge,
    receive_context: &ReceivedChatContext,
    message: impl Into<String>,
) -> OmniResult<()> {
    clear_verification_challenge_ready(db_path, &pending.dialog_key, &pending.peer_id);
    delete_session_state_for_received_context(storage, receive_context);
    disconnect_active_transport_for_verification_failure(transport_handle);
    Err(OmniError::Transport(message.into()))
}

fn handle_incoming_wire_envelope(
    runtime_storage: &Arc<Mutex<Storage>>,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    envelope: omni_types::WireEnvelopeV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    // Entry/lock brackets with the thread id: the live stand loses wire frames between the mesh
    // pump handing them to on_wire and this handler's own logging — a frame whose `enter` never
    // gains `session-locked` is parked on the app-wide session lock, and the tid says WHICH
    // processing lane (mesh pump / durable loop / queue worker) is involved. See findings pair #6.
    let kind = match &envelope.payload {
        omni_types::WirePayloadV1::SessionHello(_) => "hello",
        omni_types::WirePayloadV1::SessionBootstrap(_) => "bootstrap",
        omni_types::WirePayloadV1::EncryptedPayload(_) => "encrypted",
        _ => "other",
    };
    omni_app_trace_log(
        "wire_env/enter",
        &format!("kind={kind} tid={:?}", std::thread::current().id()),
    );
    let _session_guard = lock_session(session_lock)?;
    omni_app_trace_log(
        "wire_env/session-locked",
        &format!("kind={kind} tid={:?}", std::thread::current().id()),
    );
    validate_wire_envelope_v1(&envelope)?;
    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    let storage_guard = lock_runtime_storage(runtime_storage)?;
    omni_app_trace_log(
        "wire_env/storage-locked",
        &format!("kind={kind} tid={:?}", std::thread::current().id()),
    );
    let storage = &*storage_guard;
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }

    match &envelope.payload {
        omni_types::WirePayloadV1::SessionHello(_) => Err(OmniError::Internal(
            "transport session hello payload was routed to wire-message handler unexpectedly"
                .to_string(),
        )),
        omni_types::WirePayloadV1::SessionBootstrap(message) => {
            let now_unix_ms = current_unix_ms();
            let mut contact = storage.find_contact_by_remote_account_id(&AccountId(
                message.initiator_account_id.clone(),
            ))?;
            if matches!(
                contact.as_ref().map(|value| value.relationship_state),
                Some(ContactRelationshipState::Declined)
            ) {
                return Ok(());
            }
            storage.record_remote_device_observation(
                &AccountId(message.initiator_account_id.clone()),
                &DeviceId(message.initiator_device_id.clone()),
                &message.initiator_device_public_key,
                now_unix_ms,
            )?;
            if let Some(updated_contact) =
                persist_incoming_contact_request_signal(storage, receive_context, now_unix_ms)?
            {
                contact = Some(updated_contact);
                let db_path = storage.db_path().to_string_lossy().to_string();
                let _ = mark_runtime_chat_session_request_signal_observed(
                    &db_path,
                    &receive_context.dialog_key,
                );
            } else {
                contact = storage.find_contact_by_remote_account_id(&AccountId(
                    message.initiator_account_id.clone(),
                ))?;
            }
            storage.upsert_message_security_metadata(
                &format!(
                    "bootstrap::{}::{}::{}",
                    message.initiator_account_id,
                    message.initiator_device_id,
                    message.created_at_unix_ms
                ),
                &receive_context.dialog_key,
                contact.as_ref().map(|value| &value.contact_id),
                None,
                Some(&AccountId(message.initiator_account_id.clone())),
                Some(&DeviceId(message.initiator_device_id.clone())),
                MessageEncryptionState::BootstrapSent.as_db_value(),
                Some(omni_crypto::SessionBootstrapState::ResponderBootstrapAccepted),
                Some(SecurityPayloadKind::SessionBootstrap.as_db_value()),
                message.created_at_unix_ms,
                now_unix_ms,
            )?;
            let bootstrap_outcome = transport_accept_session_bootstrap(storage, message)?;
            let absorbed_as_initiator =
                bootstrap_outcome == InboundBootstrapOutcome::AbsorbedAsInitiator;
            if absorbed_as_initiator {
                // We won the collision, so we keep our own bootstrap and there is nothing here to
                // ack -- asking the ack path anyway makes it read OUR initiator session and fail
                // with "not established yet", which is what the live log was full of.
                //
                // What the peer actually needs is our bootstrap again: it discarded nothing, it
                // simply never got ours (or got it before it made its own). Nudging our pending
                // sends re-emits it. Without this the pair deadlocks permanently -- the loser keeps
                // re-sending a bootstrap we discard, and we keep waiting for an ack only the loser
                // can produce, from a bootstrap it never receives.
                let now_unix_ms = current_unix_ms();
                match storage.pending_queue_for_dialog(&receive_context.dialog_key, i64::MAX) {
                    Ok(pending) => {
                        for item in pending {
                            let _ = storage.mark_queue_retry(
                                &item.message_id,
                                omni_types::QueueFailureKind::NoActiveSession,
                                "re-sending our bootstrap after absorbing the peer's",
                                now_unix_ms,
                                now_unix_ms,
                            );
                        }
                    }
                    Err(error) => omni_app_trace_log(
                        "recv/bootstrap",
                        &format!("could not reschedule our own bootstrap: {error}"),
                    ),
                }
                // Deliberately NOT returning here: `process_pending_for_active_dialog` at the end of
                // this arm is what actually flushes the queue, i.e. what puts our bootstrap back on
                // the wire. Returning early would mark the retry and then never perform it.
            }
            // Best-effort ACK (relay-less first-contact, 2026-07-20). The ack advances the INITIATOR
            // to an established session, but the RECEIVER's incoming Pending (already persisted above)
            // must NOT depend on the ack going out — over a lossy link (or a durable-delivered
            // bootstrap with no live return path yet) the ack send can fail, and a `?` here used to
            // discard the whole request. On failure the bootstrap is re-delivered (durable retry) and
            // the ack re-attempted, so eventual delivery still completes the session. Log, don't drop.
            let ack_result = if absorbed_as_initiator {
                Ok(())
            } else {
                send_session_bootstrap_ack(storage, &transport, message, receive_context)
            };
            if let Err(error) = ack_result {
                omni_app_trace_log(
                    "recv/contact_request",
                    &format!("ack deferred (will retry on redelivery): {error}"),
                );
            }
            // One of our own devices opened a session with us: it is ready now, so hand it the
            // copies it lacks without waiting for the next reconcile pass.
            let initiator_device = DeviceId(message.initiator_device_id.clone());
            if storage.is_own_other_device(
                &AccountId(message.initiator_account_id.clone()),
                &initiator_device,
            )? {
                let _ = catch_up_own_device(
                    storage,
                    &transport,
                    &initiator_device,
                    &receive_context.dialog_key,
                );
            }
            if contact
                .as_ref()
                .is_some_and(|value| value.relationship_state == ContactRelationshipState::Approved)
            {
                let session_address = omni_crypto::SessionAddress {
                    remote_account_id: AccountId(message.initiator_account_id.clone()),
                    remote_device_id: DeviceId(message.initiator_device_id.clone()),
                };
                if let Some(session) = storage.get_session_record(&session_address)? {
                    let _ = send_current_public_profile_sync_for_session(
                        storage, &transport, &session, None,
                    );
                }
            }
            process_pending_for_active_dialog(storage, &transport, &receive_context.dialog_key)?;
            Ok(())
        }
        omni_types::WirePayloadV1::EncryptedPayload(payload) => {
            if envelope_deaf_for_tests() {
                omni_app_trace_log(
                    "recv/deaf",
                    &format!(
                        "dropped an encrypted envelope before decrypt dialog_key={}",
                        receive_context.dialog_key
                    ),
                );
                return Ok(());
            }
            let session_was_pending =
                if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
                    storage
                        .get_session_record(&omni_crypto::SessionAddress {
                            remote_account_id: AccountId(remote_bundle.account.account_id.clone()),
                            remote_device_id: DeviceId(remote_bundle.device.device_id.clone()),
                        })?
                        .is_some_and(|session| {
                            session.bootstrap_state
                                == omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated
                        })
                } else {
                    false
                };
            let mut session = load_receive_session(storage, receive_context)?;
            let mut replay_window =
                storage.ensure_replay_window(&session.session_address, current_unix_ms())?;
            let decrypted_payload =
                match decrypt_wire_payload(&mut session, &mut replay_window, payload) {
                    Ok(value) => {
                        note_session_payload_accepted(
                            &storage.db_path().to_string_lossy(),
                            &receive_context.dialog_key,
                        );
                        // Пара к `queue/encrypted` у отправителя: вдвоём они дают РАЗРЫВ прямым
                        // числом, а не выводом из количества отказов. Окно приёма — 64
                        // (`MAX_RECEIVE_GAP`), и без этих двух строк никто не мог сказать, насколько
                        // близко к нему подошёл живой прогон.
                        // Счётчик — САМОГО кадра: кадр, обогнанный более поздним, открывается
                        // запомненным ключом и счётчик сессии не двигает. Что внутри — чтобы по
                        // логу приёмника было видно, дошло ли конкретное сообщение
                        // (docs/bugs/message-stuck-sending-after-restart.md).
                        omni_app_trace_log(
                            "recv/accepted",
                            &format!(
                                "counter={} receive_counter={} session={} {}",
                                payload.counter,
                                session.receive_counter,
                                session.session_address.storage_key(),
                                decrypted_payload_trace_label(&value)
                            ),
                        );
                        value
                    }
                    Err(error) => {
                        // Что именно не открылось: без этого «разрыв» и «чужая цепочка» в трассе
                        // неотличимы, а лечатся по-разному.
                        omni_app_trace_log(
                            "recv/rejected",
                            &format!(
                                "counter={} chain={} prev_len={} receive_counter={} session={} error={error}",
                                payload.counter,
                                if omni_crypto::ephemeral_public_key_to_wire(session.remote_ratchet_public_key)
                                    == payload.ratchet_public_key_hex.trim()
                                {
                                    "current"
                                } else {
                                    "other"
                                },
                                payload.previous_chain_length,
                                session.receive_counter,
                                session.session_address.storage_key(),
                            ),
                        );
                        // Разрыв счётчика — это не «плохой кадр», а рассинхронизация сессии, из
                        // которой ни одна сторона не выходит сама: мы отвергаем ВСЁ, включая
                        // presence-пинги, а отправителю наше молчание неотличимо от офлайна.
                        // Просим пересобрать. См. docs/bugs/session-wedged-by-counter-gap.md.
                        maybe_request_session_resync(storage, &transport, receive_context, &error);
                        if should_ignore_stale_encrypted_payload_error(
                            storage,
                            receive_context,
                            &error,
                        )? {
                            return Ok(());
                        }
                        return Err(error);
                    }
                };
            storage.store_session_record(&mut session)?;
            storage.store_replay_window(&replay_window)?;
            // Подлинное, но непонятное (вид события из другой сборки): состояние сессии уже
            // сохранено строками выше — ради этого оно и не ошибка, — а само событие отбрасывается.
            let decrypted_payload = match decrypted_payload {
                omni_messaging::DecryptedWirePayload::Payload(payload) => payload,
                omni_messaging::DecryptedWirePayload::Unrecognized(reason) => {
                    omni_app_trace_log(
                        "recv/unrecognized",
                        &format!("dialog_key={} reason={reason}", receive_context.dialog_key),
                    );
                    return Ok(());
                }
            };
            if session_was_pending
                && session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
            {
                if let Ok(updated_session) = send_contact_request_signal_for_transport_session(
                    storage,
                    &transport,
                    &session,
                    Some(&receive_context.dialog_key),
                ) {
                    session = updated_session;
                }
                if resolve_transport_contact_for_receive_context(storage, receive_context)?
                    .as_ref()
                    .is_some_and(|contact| {
                        contact.relationship_state == ContactRelationshipState::Approved
                    })
                {
                    let db_path = storage.db_path().to_string_lossy().to_string();
                    if runtime_chat_session_approval_signal_pending_for_receive_context(
                        &db_path,
                        receive_context,
                    )? {
                        if let Ok(updated_session) = send_contact_approval_signal_for_session(
                            storage,
                            &transport,
                            &session,
                            Some(&receive_context.dialog_key),
                        ) {
                            session = updated_session;
                        }
                    }
                    mark_verification_challenge_ready(
                        &db_path,
                        &receive_context.dialog_key,
                        &receive_context.peer_id,
                    );
                    let _ = send_current_public_profile_sync_for_session(
                        storage, &transport, &session, None,
                    );
                    if contact_for_receive_context_needs_verification(storage, receive_context)? {
                        let _ = maybe_send_verification_challenge(
                            storage,
                            transport_handle,
                            receive_context,
                        );
                    }
                }
            }
            // A device the account took back is not listened to (but may still be told it was).
            if sender_is_revoked_device(storage, &session, &decrypted_payload)? {
                omni_app_trace_log(
                    "own_devices/revocation_in",
                    &format!(
                        "dropped a frame from revoked device {}",
                        session.session_address.remote_device_id.0
                    ),
                );
                return Ok(());
            }
            let result = handle_application_wire_payload(
                storage,
                &transport,
                transport_handle,
                session_lock,
                &decrypted_payload,
                true,
                receive_context,
            );
            // Our session with one of our own devices just became usable (its first answer):
            // hand it the copies it lacks now rather than on the next reconcile pass. A contact's
            // device likewise gets what went out while its session was being built: a message that
            // reached another of the contact's devices is `sent`, the queue is done with it, and the
            // link — up all along — brings no catch-up of its own (a second device of the contact
            // made before the first message, docs/bugs/contact-device-missed-while-session-built.md).
            if session_was_pending && session.bootstrap_state == omni_crypto::SessionBootstrapState::Established {
                let device_id = &session.session_address.remote_device_id;
                let _ = if storage.is_own_other_device(&session.session_address.remote_account_id, device_id)? {
                    catch_up_own_device(storage, &transport, device_id, &receive_context.dialog_key)
                } else {
                    let known_since = storage.contact_dialog_link_known_since(&receive_context.dialog_key)?.unwrap_or(i64::MAX);
                    catch_up_contact_device_since(storage, &transport, &receive_context.dialog_key, device_id, known_since)
                };
            }
            if result.is_ok()
                && !decrypted_payload_defers_verification_challenge(&decrypted_payload)
                && contact_for_receive_context_needs_verification(storage, receive_context)?
            {
                let _ =
                    maybe_send_verification_challenge(storage, transport_handle, receive_context);
            }
            result
        }
        omni_types::WirePayloadV1::VerificationChallenge(_)
        | omni_types::WirePayloadV1::VerificationChallengeResponse(_)
        | omni_types::WirePayloadV1::ChatText(_)
        | omni_types::WirePayloadV1::LogicalEvent(_) => Err(OmniError::Transport(
            "plaintext application payloads are not accepted".to_string(),
        )),
    }
}

#[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
fn handle_application_wire_payload(
    storage: &Storage,
    transport: &TransportManager,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    payload: &omni_types::WirePayloadV1,
    received_encrypted: bool,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    match payload {
        omni_types::WirePayloadV1::VerificationChallenge(challenge) => {
            mark_contact_activity_for_dialog(
                storage,
                &receive_context.dialog_key,
                current_unix_ms(),
            )?;
            handle_incoming_verification_challenge(
                storage,
                transport_handle,
                session_lock,
                challenge,
                received_encrypted,
                receive_context,
            )
        }
        omni_types::WirePayloadV1::VerificationChallengeResponse(response) => {
            mark_contact_activity_for_dialog(
                storage,
                &receive_context.dialog_key,
                current_unix_ms(),
            )?;
            handle_incoming_verification_challenge_response(
                storage,
                transport_handle,
                session_lock,
                response,
                received_encrypted,
                receive_context,
            )
        }
        omni_types::WirePayloadV1::ChatText(chat_text) => {
            let mut session = load_receive_session(storage, receive_context)?;
            storage.store_session_record(&mut session)?;
            let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
            if !matches!(
                contact.as_ref().map(|value| value.relationship_state),
                Some(ContactRelationshipState::Approved)
            ) {
                return Ok(());
            }
            handle_incoming_text(
                storage,
                transport,
                chat_text.clone(),
                receive_context,
                &session,
                received_message_encryption_state(received_encrypted),
                SecurityPayloadKind::Text,
            )
        }
        omni_types::WirePayloadV1::LogicalEvent(event) => match &event.payload {
            omni_types::LogicalEventPayloadV1::ChatText(chat_text) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Message,
                    current_unix_ms(),
                );
                let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
                if !matches!(
                    contact.as_ref().map(|value| value.relationship_state),
                    Some(ContactRelationshipState::Approved)
                ) {
                    return Ok(());
                }
                handle_incoming_text(
                    storage,
                    transport,
                    chat_text.clone(),
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                    SecurityPayloadKind::Text,
                )
            }
            omni_types::LogicalEventPayloadV1::DeliveryReceipt(receipt) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Receipt,
                    current_unix_ms(),
                );
                persist_delivery_receipt(
                    storage,
                    receipt,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                    SecurityPayloadKind::DeliveryReceipt,
                )
            }
            omni_types::LogicalEventPayloadV1::AttachmentOffer(offer) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
                if !matches!(
                    contact.as_ref().map(|value| value.relationship_state),
                    Some(ContactRelationshipState::Approved)
                ) {
                    return Ok(());
                }
                handle_incoming_attachment_offer(
                    storage,
                    transport,
                    offer,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )?;
                // A message with a file is a message: the sender's other devices show it as theirs
                // and wait for this receipt too (live 2026-10-08 the copy stayed "sent").
                send_receipt_to_senders_other_devices(storage, transport, &session, &offer.message_id);
                Ok(())
            }
            omni_types::LogicalEventPayloadV1::AttachmentDownloadRequest(request) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                if !attachment_control_event_allowed(storage, receive_context, &session)? {
                    // A silent drop here is one of the two ways a transfer can never start while
                    // both sides look healthy. See docs/bugs/attachment-transfer-never-starts.md.
                    omni_app_trace_log(
                        "attachment/download-request",
                        &format!(
                            "dialog_key={} transfer={} message={} DROPPED: attachment control \
                             events are not allowed for this contact",
                            receive_context.dialog_key, request.transfer_id, request.message_id
                        ),
                    );
                    return Ok(());
                }
                // The arrival point. Until this existed, "the request never got here" and "it got
                // here and went nowhere" were indistinguishable from either side's trace.
                omni_app_trace_log(
                    "attachment/download-request",
                    &format!(
                        "dialog_key={} transfer={} message={} received resume_offset={}",
                        receive_context.dialog_key,
                        request.transfer_id,
                        request.message_id,
                        request.resume_offset_bytes,
                    ),
                );
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                persist_attachment_download_request(
                    storage,
                    transport,
                    request,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            omni_types::LogicalEventPayloadV1::AttachmentTransferFailed(failure) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                if !attachment_control_event_allowed(storage, receive_context, &session)? {
                    return Ok(());
                }
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                persist_attachment_transfer_failed(
                    storage,
                    transport,
                    failure,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            omni_types::LogicalEventPayloadV1::AttachmentDownloaded(downloaded) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                if !attachment_control_event_allowed(storage, receive_context, &session)? {
                    return Ok(());
                }
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                persist_attachment_downloaded(
                    storage,
                    downloaded,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            omni_types::LogicalEventPayloadV1::AttachmentAccessRevoked(revoked) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                if !attachment_control_event_allowed(storage, receive_context, &session)? {
                    return Ok(());
                }
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                persist_attachment_access_revoked(
                    storage,
                    revoked,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            omni_types::LogicalEventPayloadV1::ContactPresence(presence) => {
                persist_contact_presence(storage, presence, receive_context)
            }
            omni_types::LogicalEventPayloadV1::ContactPresencePing(ping) => {
                persist_contact_presence_ping(
                    storage,
                    transport_handle,
                    session_lock,
                    ping,
                    receive_context,
                    &load_receive_session(storage, receive_context)?,
                )
            }
            omni_types::LogicalEventPayloadV1::ContactPresencePong(pong) => {
                persist_contact_presence_pong(storage, transport_handle, pong, receive_context)
            }
            omni_types::LogicalEventPayloadV1::CallInvite(invite) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_invite(storage, transport, receive_context, invite, &session)
            }
            omni_types::LogicalEventPayloadV1::CallRinging(ringing) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_ringing(storage, ringing)
            }
            omni_types::LogicalEventPayloadV1::CallAccept(accept) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_accept(storage, transport, accept, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::CallDecline(decline) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_decline_or_busy(
                    storage,
                    &decline.call_id,
                    CallOutcome::Declined,
                    Some(decline.reason),
                    decline.declined_at_unix_ms,
                )
            }
            omni_types::LogicalEventPayloadV1::CallBusy(busy) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_decline_or_busy(
                    storage,
                    &busy.call_id,
                    CallOutcome::Busy,
                    None,
                    busy.busy_at_unix_ms,
                )
            }
            omni_types::LogicalEventPayloadV1::CallCancel(cancel) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_cancel_or_hangup_or_timeout(
                    storage,
                    &cancel.call_id,
                    CallOutcome::Cancelled,
                    cancel.cancelled_at_unix_ms,
                )
            }
            omni_types::LogicalEventPayloadV1::CallHangup(hangup) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_cancel_or_hangup_or_timeout(
                    storage,
                    &hangup.call_id,
                    CallOutcome::Completed,
                    hangup.ended_at_unix_ms,
                )
            }
            omni_types::LogicalEventPayloadV1::CallTimeout(timeout) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_cancel_or_hangup_or_timeout(
                    storage,
                    &timeout.call_id,
                    CallOutcome::Missed,
                    timeout.timed_out_at_unix_ms,
                )
            }
            omni_types::LogicalEventPayloadV1::CallReconnect(reconnect) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_call_reconnect(storage, reconnect)
            }
            omni_types::LogicalEventPayloadV1::CallScreenControl(control) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                crate::app_screen_control::handle_incoming_call_screen_control(storage, control)
            }
            omni_types::LogicalEventPayloadV1::SentTranscript(transcript) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_sent_transcript(storage, transport, transcript, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::AccountKeyMove(sync) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_account_key_move(storage, transport, sync, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::ContactSync(sync) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_contact_sync(storage, transport, sync, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::ContactSyncAck(ack) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_contact_sync_ack(storage, ack, &session)
            }
            omni_types::LogicalEventPayloadV1::AccountPassport(message) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_account_passport(storage, transport, message, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::HistorySync(message) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_history_sync(storage, transport, message, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::CourierSet(set) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_courier_set(storage, set, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::DeviceRevocation(sync) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                handle_incoming_device_revocation(storage, transport, sync, receive_context, &session)
            }
            omni_types::LogicalEventPayloadV1::RemoteAccessGrant(grant) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(storage, &receive_context.dialog_key, current_unix_ms())?;
                crate::app_remote_access::handle_incoming_remote_access_grant(storage, &receive_context.dialog_key, grant)
            }
            omni_types::LogicalEventPayloadV1::RealtimeMediaCapabilities(capabilities) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_realtime_media_capabilities(
                    storage,
                    transport,
                    receive_context,
                    capabilities,
                )
            }
            omni_types::LogicalEventPayloadV1::RealtimeMediaSelection(selection) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                handle_incoming_realtime_media_selection(storage, receive_context, selection)
            }
            // УПРАВЛЯЮЩИЙ КАНАЛ МЕДИА СНЯТ (2026-10-03): NACK, PLI, отчёты RR/SR и RTT ходят
            // настоящим RTCP вместе с медиа (docs/bugs/media-rode-the-shells-timer.md, шаги 3–5).
            // Вид события остаётся в протоколе, чтобы собеседник на прежней сборке, который ещё шлёт
            // отчёты раз в секунду, разбирался, а не сыпал ошибками; его события отбрасываются.
            omni_types::LogicalEventPayloadV1::RealtimeMediaControl(control) => {
                omni_app_trace_log(
                    "media/control-ignored",
                    &format!(
                        "dialog_key={} type={:?} (peer on a build before the control channel was removed)",
                        receive_context.dialog_key, control.control_type
                    ),
                );
                Ok(())
            }
            omni_types::LogicalEventPayloadV1::PauseAutoReconnect(_pause) => {
                let now_unix_ms = current_unix_ms();
                let mut pause_dialog_keys = vec![receive_context.dialog_key.clone()];
                if let Some(contact) =
                    resolve_transport_contact_for_receive_context(storage, receive_context)?
                {
                    pause_dialog_keys.extend(contact_dialog_keys_for_peer_reset(&contact));
                }
                pause_dialog_keys.sort();
                pause_dialog_keys.dedup();
                for pause_dialog_key in &pause_dialog_keys {
                    let _ = touch_runtime_chat_session_interaction(
                        &db_path,
                        pause_dialog_key,
                        omni_types::ChatSessionInteractionKind::Session,
                        now_unix_ms,
                    );
                    let _ = pause_chat_session_for_profile(storage, pause_dialog_key);
                    let _ =
                        mark_contact_offline_for_dialog(storage, pause_dialog_key, now_unix_ms);
                }
                if let Some(transport) = clone_active_transport(transport_handle)? {
                    let _ = transport.disconnect_dialog(&receive_context.dialog_key);
                }
                Ok(())
            }
            omni_types::LogicalEventPayloadV1::PublicProfileDescriptorSync(profile) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                persist_public_profile_descriptor_sync(
                    storage,
                    profile,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            omni_types::LogicalEventPayloadV1::PublicProfileAvatarSync(profile) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                mark_contact_activity_for_dialog(
                    storage,
                    &receive_context.dialog_key,
                    current_unix_ms(),
                )?;
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    &receive_context.dialog_key,
                    omni_types::ChatSessionInteractionKind::Session,
                    current_unix_ms(),
                );
                persist_public_profile_avatar_sync(
                    storage,
                    profile,
                    receive_context,
                    &session,
                    received_message_encryption_state(received_encrypted),
                )
            }
            // Direct-route hints fed the pre-mesh transport's dialer, which is gone
            // (docs/decisions/no-friend-tier.md, part 2). An older peer may still send them.
            omni_types::LogicalEventPayloadV1::RouteHintsSync(_) => Ok(()),
            omni_types::LogicalEventPayloadV1::RelaySetUpdate(update) => {
                persist_relay_set_update(storage, update, receive_context)
            }
            omni_types::LogicalEventPayloadV1::GroupControl(control) => {
                let mut session = load_receive_session(storage, receive_context)?;
                storage.store_session_record(&mut session)?;
                // Group consent flow (G3) over the pairwise channel: invite /
                // accept / decline / welcome. Only an approved contact may reach
                // us here (1:1 gate); membership itself is opt-in on accept.
                // Our own other device may too: it asks to be added to our groups (stage 4 of
                // docs/decisions/multi-device-equal-nodes.md).
                let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
                let own_device = storage
                    .is_own_other_device(&session.session_address.remote_account_id, &session.session_address.remote_device_id)?;
                if !own_device
                    && !matches!(
                        contact.as_ref().map(|value| value.relationship_state),
                        Some(ContactRelationshipState::Approved)
                    )
                {
                    return Ok(());
                }
                crate::app_groups::handle_incoming_group_control(
                    storage,
                    transport,
                    receive_context,
                    control,
                )
            }
        },
        omni_types::WirePayloadV1::SessionHello(_) => Err(OmniError::Internal(
            "transport session hello payload was routed to wire-message handler unexpectedly"
                .to_string(),
        )),
        omni_types::WirePayloadV1::SessionBootstrap(_) => Err(OmniError::Internal(
            "session bootstrap payload must be handled before application dispatch".to_string(),
        )),
        omni_types::WirePayloadV1::EncryptedPayload(_) => Err(OmniError::Internal(
            "encrypted payload must be decrypted before application dispatch".to_string(),
        )),
    }
}

fn load_receive_session(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<omni_crypto::SessionRecord> {
    // Fresh-connection path: the ctx carries the remote's prekey bundle, so resolve
    // the exact (account, device) session it names.
    let session = if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        storage.get_session_record(&omni_crypto::SessionAddress {
            remote_account_id: AccountId(remote_bundle.account.account_id.clone()),
            remote_device_id: DeviceId(remote_bundle.device.device_id.clone()),
        })?
    } else {
        // Established-contact path (durable / never-linked this session): the ctx
        // carries no bundle, so resolve the contact by its dialog_key and load the
        // latest session we already hold for that account. This is what lets a
        // store-and-forward frame from a peer we never linked this session decrypt
        // via the contact's persisted ratchet. See docs/decisions/mesh-durable-delivery.md.
        match storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)?
            .and_then(|contact| contact.remote_account_id)
        {
            // The session of the device that sent on this dialog — another device's would not
            // decrypt it.
            Some(remote_account_id) => {
                storage.get_session_for_dialog(&remote_account_id, &receive_context.dialog_key)?
            }
            None => None,
        }
    };
    let session = session.ok_or_else(|| {
        OmniError::Transport("received wire payload for a missing session".to_string())
    })?;
    let session = if session.bootstrap_state
        == omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated
    {
        establish_session(&session, current_unix_ms())
    } else {
        session
    };
    Ok(session)
}

fn established_session_for_receive_context(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<Option<omni_crypto::SessionRecord>> {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return Ok(None);
    };
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: AccountId(remote_bundle.account.account_id.clone()),
        remote_device_id: DeviceId(remote_bundle.device.device_id.clone()),
    };
    let Some(session) = storage.get_session_record(&session_address)? else {
        return Ok(None);
    };
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(None);
    }
    Ok(Some(session))
}

/// TM-004 gate for non-attachment logical event classes (delivery receipts,
/// profile sync, etc). A session-capable peer that is NOT `Approved` should
/// not be able to mutate local metadata via these events. Returns false when
/// the caller should silently drop the event without persisting — silent
/// drop is correct because these are best-effort one-way signals (no
/// acknowledgement protocol). See `docs/decisions/tm004-logical-event-gate.md`.
fn dialog_contact_is_approved_for_logical_events(
    storage: &Storage,
    dialog_key: &str,
) -> bool {
    let Ok(Some(card)) = storage.resolve_contact_card_for_dialog(dialog_key) else {
        return false;
    };
    card.contact.relationship_state == omni_identity::ContactRelationshipState::Approved
}

fn persist_delivery_receipt(
    storage: &Storage,
    receipt: &DeliveryReceiptV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<()> {
    // TM-004: a non-Approved session-capable peer must not be able to flip
    // a delivery receipt for messages we never sent them. We only ever send
    // to Approved contacts (gate in send_text), so legitimate receipts come
    // only from Approved peers. Drop silently — receipt protocol is one-way.
    // From another of our devices it confirms the COPY it got — the message itself is delivered only
    // by the contact (multi-device-equal-nodes.md, 2.4).
    if storage.is_own_other_device(
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    )? {
        return storage.record_message_device_receipt(
            &receipt.message_id,
            &session.session_address.remote_device_id,
            receipt.delivered_at_unix_ms,
        );
    }
    if !dialog_contact_is_approved_for_logical_events(storage, &receive_context.dialog_key) {
        return Ok(());
    }
    storage.record_delivery_receipt(
        &receipt.message_id,
        &storage.chat_dialog_key_for(&receive_context.dialog_key)?,
        receipt.delivered_at_unix_ms,
        current_unix_ms(),
    )?;
    // The device that confirmed it is done; the others still get it (multi-device-equal-nodes.md).
    storage.record_message_device_receipt(
        &receipt.message_id,
        &session.session_address.remote_device_id,
        receipt.delivered_at_unix_ms,
    )?;
    // Сообщение подтверждено — держать его конверт для повторной отправки больше незачем.
    // См. docs/bugs/session-wedged-by-counter-gap.md.
    forget_outgoing_envelope(
        &storage.db_path().to_string_lossy(),
        &receipt.message_id,
    );
    let synthetic_id = format!("receipt::{}", receipt.message_id);
    storage.upsert_message_security_metadata(
        &synthetic_id,
        &receive_context.dialog_key,
        storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)?
            .as_ref()
            .map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(payload_kind.as_db_value()),
        receipt.delivered_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

/// A copy of what another of OUR devices sent to a contact: show it in that chat as sent, and
/// confirm it. Only our own certified devices may hand us these; the contact must be ours too
/// (contacts themselves travel between devices with a later step).
fn handle_incoming_sent_transcript(
    storage: &Storage,
    transport: &TransportManager,
    transcript: &omni_types::SentTranscriptV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    if !storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        omni_app_trace_log(
            "own_devices/copy_in",
            &format!("dropped: {} is not one of our devices", from.remote_device_id.0),
        );
        return Ok(());
    }
    let contact_account_id = AccountId(transcript.contact_account_id.clone());
    let Some(contact) = storage.find_contact_by_remote_account_id(&contact_account_id)? else {
        omni_app_trace_log(
            "own_devices/copy_in",
            &format!("held back: no contact {} here yet", contact_account_id.0),
        );
        return Ok(());
    };
    let chat_dialog_key = contact
        .primary_dialog_key
        .clone()
        .unwrap_or_else(|| omni_types::dialog_key_from_account_id(&contact_account_id.0));
    let stored = storage.insert_sent_copy(
        &transcript.message_id,
        &chat_dialog_key,
        &transcript.body,
        transcript.created_at_unix_ms,
        &from.remote_device_id,
    )?;
    // The file stays on the device that sent it; this record says where to fetch it — that device's
    // dialog, not the contact's (`attempt_send_attachment_download_request`).
    if let Some(offer) = &transcript.attachment {
        if storage.get_attachment_runtime_info(&offer.message_id)?.is_none() {
            storage.store_incoming_attachment_offer(
                &offer.message_id,
                &receive_context.dialog_key,
                &offer.file_name,
                offer.file_size_bytes,
                offer.mime_type.as_deref(),
                &offer.content_id_hex,
                offer.content_encryption.as_deref(),
                offer.content_key_hex.as_deref(),
                offer.content_nonce_seed_hex.as_deref(),
                offer.content_chunk_size_bytes,
                offer.created_at_unix_ms,
            )?;
            accept_received_attachment_presentation(storage, offer)?;
        }
    }
    omni_app_trace_log(
        "own_devices/copy_in",
        &format!(
            "message={} from={} stored={stored} attachment={}",
            transcript.message_id,
            from.remote_device_id.0,
            transcript.attachment.is_some()
        ),
    );
    let mut outgoing = build_encrypted_delivery_receipt_envelope(
        session,
        &DeliveryReceiptV1 {
            message_id: transcript.message_id.clone(),
            delivered_at_unix_ms: current_unix_ms(),
        },
        current_unix_ms(),
    )?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    Ok(())
}

fn handle_incoming_text(
    storage: &Storage,
    transport: &TransportManager,
    chat_text: omni_types::ChatTextV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<()> {
    let dialog_key = receive_context.dialog_key.clone();
    let message_id = chat_text.message_id.clone();
    accept_incoming_text(
        storage,
        chat_text,
        receive_context,
        session,
        encryption_state,
        payload_kind,
        move |outgoing| {
            transport.send_wire_envelope_for_dialog_key(&dialog_key, &outgoing.envelope)
        },
    )?;
    send_receipt_to_senders_other_devices(storage, transport, session, &message_id);
    Ok(())
}

/// The sender's other devices show this message as a copy of their own; the receipt tells them it
/// arrived, as it tells the device that sent it (docs/decisions/multi-device-equal-nodes.md). A
/// device that is away gets it from the durable store when it is back; one that does not have the
/// copy yet keeps the receipt until the copy comes. Best effort: a failure here leaves that copy at
/// "sent" and takes nothing else down.
fn send_receipt_to_senders_other_devices(
    storage: &Storage,
    transport: &TransportManager,
    sender_session: &omni_crypto::SessionRecord,
    message_id: &str,
) {
    let sender = &sender_session.session_address;
    let Ok(Some(contact)) = storage.find_contact_by_remote_account_id(&sender.remote_account_id) else {
        return;
    };
    for link in storage.list_contact_device_dialogs(&contact.contact_id).unwrap_or_default() {
        let Some(device_id) = link.remote_device_id else {
            continue;
        };
        if device_id == sender.remote_device_id {
            continue;
        }
        let session = match storage.get_session_record(&omni_crypto::SessionAddress {
            remote_account_id: sender.remote_account_id.clone(),
            remote_device_id: device_id.clone(),
        }) {
            Ok(Some(session))
                if session.bootstrap_state == omni_crypto::SessionBootstrapState::Established =>
            {
                session
            }
            _ => continue,
        };
        let sent = (|| -> OmniResult<()> {
            let mut outgoing = build_encrypted_delivery_receipt_envelope(
                &session,
                &DeliveryReceiptV1 {
                    message_id: message_id.to_string(),
                    delivered_at_unix_ms: current_unix_ms(),
                },
                current_unix_ms(),
            )?;
            storage.store_session_record(&mut outgoing.updated_session)?;
            transport.send_wire_envelope_for_dialog_key(&link.dialog_key, &outgoing.envelope)
        })();
        if let Err(error) = sent {
            omni_app_trace_log(
                "receipt/other_devices",
                &format!("message={message_id} device={} not told: {error}", device_id.0),
            );
        }
    }
}

fn accept_incoming_text(
    storage: &Storage,
    chat_text: omni_types::ChatTextV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
    payload_kind: SecurityPayloadKind,
    on_ready_to_send_receipt: impl FnOnce(&omni_messaging::OutgoingWireEnvelope) -> OmniResult<()>,
) -> OmniResult<()> {
    storage.insert_incoming_message(
        &chat_text.message_id,
        &receive_context.dialog_key,
        &chat_text.body,
        chat_text.created_at_unix_ms,
    )?;
    // Surface the stored message as an event for the daemon's event plane (push, not
    // poll). See docs/decisions/daemon-ipc-phase2.md §Events plane.
    record_inbound_message_event(
        &storage.db_path().to_string_lossy(),
        &receive_context.dialog_key,
        &chat_text.message_id,
        &chat_text.body,
    );
    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    storage.upsert_message_security_metadata(
        &chat_text.message_id,
        &receive_context.dialog_key,
        contact.as_ref().map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(payload_kind.as_db_value()),
        chat_text.created_at_unix_ms,
        current_unix_ms(),
    )?;

    let mut outgoing = build_encrypted_delivery_receipt_envelope(
        session,
        &DeliveryReceiptV1 {
            message_id: chat_text.message_id.clone(),
            delivered_at_unix_ms: current_unix_ms(),
        },
        current_unix_ms(),
    )?;
    storage.upsert_message_security_metadata(
        &format!("receipt::{}", chat_text.message_id),
        &receive_context.dialog_key,
        contact.as_ref().map(|value| &value.contact_id),
        Some(&outgoing.updated_session.session_address),
        Some(&outgoing.updated_session.session_address.remote_account_id),
        Some(&outgoing.updated_session.session_address.remote_device_id),
        MessageEncryptionState::EncryptedPending.as_db_value(),
        Some(outgoing.updated_session.bootstrap_state),
        Some(outgoing.payload_kind.as_db_value()),
        current_unix_ms(),
        current_unix_ms(),
    )?;
    mark_contact_activity_for_dialog(storage, &receive_context.dialog_key, current_unix_ms())?;
    on_ready_to_send_receipt(&outgoing)?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    Ok(())
}

/// Keep what of a received offer's presentation passes the receiver's limits; a photo that does is
/// downloaded by itself into the profile's photo folder (the queue tick sends the request), as in
/// Telegram. docs/decisions/media-messages.md §1.
fn accept_received_attachment_presentation(
    storage: &Storage,
    offer: &omni_types::AttachmentOfferV1,
) -> OmniResult<()> {
    let accepted = omni_storage::photo::accept_received_photo_presentation(
        offer.presentation.as_ref(),
        offer.file_size_bytes,
    );
    storage.set_attachment_presentation(&offer.message_id, accepted.as_ref())?;
    // TM-005: a contact's photos ask for themselves only up to the bound; past it a photo stays
    // offered, a tap away, instead of the whole message being refused.
    // Only an offer not acted on yet: the same offer delivered again must not fetch a photo that is
    // here, or being fetched, a second time.
    let busy = match storage.get_attachment_runtime_info(&offer.message_id)? {
        Some(runtime) if runtime.attachment.state == omni_types::AttachmentTransferState::Offered => {
            storage.count_inbound_attachments_for_dialog(&runtime.dialog_key)?
        }
        _ => return Ok(()),
    };
    if accepted.is_some() && busy < MAX_INBOUND_ATTACHMENTS_PER_DIALOG {
        let save_path = storage.photo_download_path(&offer.message_id)?;
        storage.mark_attachment_download_requested(
            &offer.message_id,
            &save_path,
            &next_attachment_transfer_id()?,
            current_unix_ms(),
        )?;
    }
    Ok(())
}

fn handle_incoming_attachment_offer(
    storage: &Storage,
    transport: &TransportManager,
    offer: &omni_types::AttachmentOfferV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    storage.insert_incoming_message(
        &offer.message_id,
        &receive_context.dialog_key,
        &offer.body,
        offer.created_at_unix_ms,
    )?;
    storage.store_incoming_attachment_offer(
        &offer.message_id,
        &receive_context.dialog_key,
        &offer.file_name,
        offer.file_size_bytes,
        offer.mime_type.as_deref(),
        &offer.content_id_hex,
        offer.content_encryption.as_deref(),
        offer.content_key_hex.as_deref(),
        offer.content_nonce_seed_hex.as_deref(),
        offer.content_chunk_size_bytes,
        offer.created_at_unix_ms,
    )?;
    accept_received_attachment_presentation(storage, offer)?;
    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    storage.upsert_message_security_metadata(
        &offer.message_id,
        &receive_context.dialog_key,
        contact.as_ref().map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::AttachmentOffer.as_db_value()),
        offer.created_at_unix_ms,
        current_unix_ms(),
    )?;

    let mut outgoing = build_encrypted_delivery_receipt_envelope(
        session,
        &DeliveryReceiptV1 {
            message_id: offer.message_id.clone(),
            delivered_at_unix_ms: current_unix_ms(),
        },
        current_unix_ms(),
    )?;
    storage.upsert_message_security_metadata(
        &format!("receipt::{}", offer.message_id),
        &receive_context.dialog_key,
        contact.as_ref().map(|value| &value.contact_id),
        Some(&outgoing.updated_session.session_address),
        Some(&outgoing.updated_session.session_address.remote_account_id),
        Some(&outgoing.updated_session.session_address.remote_device_id),
        MessageEncryptionState::EncryptedPending.as_db_value(),
        Some(outgoing.updated_session.bootstrap_state),
        Some(outgoing.payload_kind.as_db_value()),
        current_unix_ms(),
        current_unix_ms(),
    )?;
    mark_contact_activity_for_dialog(storage, &receive_context.dialog_key, current_unix_ms())?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    Ok(())
}

/// The dialog an attachment's row sits under (`""` when there is no such attachment).
fn storage_attachment_dialog(storage: &Storage, message_id: &str) -> OmniResult<String> {
    Ok(storage.get_attachment_runtime_info(message_id)?.map(|runtime| runtime.dialog_key).unwrap_or_default())
}

fn persist_attachment_download_request(
    storage: &Storage,
    transport: &TransportManager,
    request: &omni_types::AttachmentDownloadRequestV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    let contact = storage.resolve_contact_for_dialog(&receive_context.dialog_key)?;
    storage.upsert_message_security_metadata(
        &format!("attachment-request::{}", request.message_id),
        &receive_context.dialog_key,
        contact.as_ref().map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::AttachmentDownloadRequest.as_db_value()),
        request.requested_at_unix_ms,
        current_unix_ms(),
    )?;

    if !attachment_requester_allowed(storage, &request.message_id, &session.session_address)? {
        omni_app_trace_log(
            "attachment/download-request",
            &format!(
                "dialog_key={} transfer={} message={} DROPPED: the file was not sent to {}",
                receive_context.dialog_key,
                request.transfer_id,
                request.message_id,
                session.session_address.remote_account_id.0
            ),
        );
        return Ok(());
    }
    // Another device of ours asks first (it may be this one that downloaded it): a downloaded copy
    // is handed on; without the file, we say so at once and it goes to the contact.
    if storage.is_own_other_device(&session.session_address.remote_account_id, &session.session_address.remote_device_id)? {
        let _ = storage.serve_downloaded_attachment(&request.message_id);
        if storage.attachment_serving_blob_path(&request.message_id).ok().flatten().is_none() {
            omni_app_trace_log(
                "attachment/download-request",
                &format!(
                    "dialog_key={} transfer={} message={} not here: telling our device",
                    receive_context.dialog_key, request.transfer_id, request.message_id
                ),
            );
            return send_attachment_not_here_notice(storage, transport, receive_context, &request.message_id);
        }
        return spawn_attachment_transfer_sender(storage, transport, request, receive_context);
    }
    // A device of ours that holds the message as a copy (another device of ours sent it) may be
    // asked because the sending one is away (3.3): it hands on a copy it has downloaded, and stays
    // silent otherwise — "not here" from it is no answer about the file, the asker goes on asking.
    if own_device_for_dialog(storage, &storage_attachment_dialog(storage, &request.message_id)?)?.is_some()
        && storage.attachment_serving_blob_path(&request.message_id).ok().flatten().is_none()
    {
        omni_app_trace_log(
            "attachment/download-request",
            &format!(
                "dialog_key={} transfer={} message={} not here: a copy whose file this device has not downloaded",
                receive_context.dialog_key, request.transfer_id, request.message_id
            ),
        );
        return Ok(());
    }
    spawn_attachment_transfer_sender(storage, transport, request, receive_context)
}

fn persist_attachment_transfer_start_frame(
    storage: &Storage,
    start: AttachmentTransferStartV2,
) -> OmniResult<()> {
    let AttachmentTransferStartV2 {
        transfer_id,
        payload,
        started_at_unix_ms,
    } = start;
    let runtime = storage
        .get_attachment_runtime_info_by_active_transfer_id(&transfer_id)?
        .ok_or_else(|| {
            OmniError::Storage(format!(
                "attachment transfer '{}' was not found",
                transfer_id
            ))
        })?;
    let material = attachment_content_crypto_material(&runtime)?;
    let payload_bytes = open_attachment_transfer_payload_owned(
        &material,
        &transfer_id,
        "start",
        ATTACHMENT_TRANSFER_COUNTER_START,
        started_at_unix_ms,
        payload,
    )?;
    let payload: AttachmentTransferStartPayloadV2 =
        deserialize_attachment_transfer_payload(&payload_bytes)?;
    if runtime.attachment.content_id_hex != payload.content_id_hex {
        return Err(OmniError::Storage(
            "attachment transfer start hash does not match the offered file".to_string(),
        ));
    }
    if runtime.attachment.file_size_bytes != payload.total_size_bytes {
        return Err(OmniError::Storage(
            "attachment transfer start size does not match the offered file".to_string(),
        ));
    }
    if let Some(expected_chunk_size_bytes) = runtime.content_chunk_size_bytes {
        if expected_chunk_size_bytes != payload.chunk_size_bytes {
            return Err(OmniError::Storage(
                "attachment transfer start chunk size does not match the offered file".to_string(),
            ));
        }
    }
    // The layer: kept, so this copy can prove its blocks to others before it is whole.
    storage.adopt_received_tree_layer(&runtime.attachment.message_id, &payload.block_layer)?;
    let (runtime, _) = storage.attachment_download_resume(&runtime.attachment.message_id, current_unix_ms())?;
    storage.mark_attachment_downloading(
        &runtime.attachment.message_id,
        runtime.attachment.bytes_transferred,
        current_unix_ms(),
    )?;
    Ok(())
}

fn persist_public_profile_descriptor_sync(
    storage: &Storage,
    profile: &omni_types::PublicProfileDescriptorSyncV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    if adopt_profile_from_own_device(
        storage,
        session,
        Some(&profile.display_name),
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
        None,
        profile.updated_at_unix_ms,
    )? {
        return Ok(());
    }
    // TM-004: pending / declined peers must not push profile metadata into
    // our contact card. Display name + avatar come from the trusted
    // verification payload / prekey bundle, not from sync events.
    if !dialog_contact_is_approved_for_logical_events(storage, &receive_context.dialog_key) {
        return Ok(());
    }
    let applied_contact = storage.apply_remote_public_profile_descriptor(
        Some(&session.session_address.remote_account_id),
        Some(receive_context.remote_instance_id.as_str()),
        &profile.display_name,
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
        profile.updated_at_unix_ms,
    )?;
    storage.upsert_message_security_metadata(
        &format!(
            "public-profile-descriptor::{}::{}",
            session.session_address.remote_account_id.0, profile.updated_at_unix_ms
        ),
        &receive_context.dialog_key,
        applied_contact.as_ref().map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::PublicProfileDescriptorSync.as_db_value()),
        profile.updated_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

fn persist_public_profile_avatar_sync(
    storage: &Storage,
    profile: &omni_types::PublicProfileAvatarSyncV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    if profile.avatar_hash_hex.is_none()
        && profile.avatar_mime_type.is_none()
        && profile.avatar_bytes_base64.is_none()
    {
        return Ok(());
    }
    let avatar_bytes = profile
        .avatar_bytes_base64
        .as_deref()
        .map(|value| {
            BASE64_STANDARD.decode(value).map_err(|error| {
                OmniError::InvalidInput(format!(
                    "public profile avatar payload could not be decoded: {error}"
                ))
            })
        })
        .transpose()?;
    if adopt_profile_from_own_device(
        storage,
        session,
        None,
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
        Some(avatar_bytes.as_deref().unwrap_or_default()),
        profile.updated_at_unix_ms,
    )? {
        return Ok(());
    }
    // TM-004: mirror of persist_public_profile_descriptor_sync — avatar
    // sync from a non-Approved peer would let them poison our local
    // contact card's avatar field.
    if !dialog_contact_is_approved_for_logical_events(storage, &receive_context.dialog_key) {
        return Ok(());
    }
    let applied_contact = storage.apply_remote_public_profile_avatar(
        Some(&session.session_address.remote_account_id),
        Some(receive_context.remote_instance_id.as_str()),
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
        avatar_bytes.as_deref(),
        profile.updated_at_unix_ms,
    )?;
    storage.upsert_message_security_metadata(
        &format!(
            "public-profile-avatar::{}::{}::{}",
            session.session_address.remote_account_id.0,
            profile.updated_at_unix_ms,
            profile.avatar_hash_hex.as_deref().unwrap_or("missing")
        ),
        &receive_context.dialog_key,
        applied_contact.as_ref().map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::PublicProfileAvatarSync.as_db_value()),
        profile.updated_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

/// R1c relay-set propagation (live-channel primary path): an approved, linked contact pushed me their
/// current signed `om3relay:` bundle. Gated like RouteHintsSync (TM-004 class): approved-only +
/// first-party (the bundle's issuer MUST be this sender, so a contact propagates ONLY its own set — no
/// transitive re-share, honours R1b). `apply_relay_bundle` re-verifies the issuer signature, enforces
/// the first-party `expected_issuer`, and does the strict-newer rotation replace. Idempotent: a replay /
/// older version is ignored. See docs/decisions/r1c-rotatable-white-relay.md §B.
fn persist_relay_set_update(
    storage: &Storage,
    update: &omni_types::RelaySetUpdateV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    omni_protocol::validate_relay_set_update_v1(update)?;
    let contact = resolve_transport_contact_for_receive_context(storage, receive_context)?;
    let approved = contact
        .as_ref()
        .is_some_and(|c| matches!(c.relationship_state, ContactRelationshipState::Approved));
    if !approved {
        omni_app_trace_log(
            "r1c/relay_set/drop",
            &format!(
                "remote_instance={} contact_state={:?}",
                receive_context.remote_instance_id,
                contact.as_ref().map(|c| c.relationship_state),
            ),
        );
        return Ok(());
    }
    // First-party: resolve the sender's MESH NODE (from their account's latest prekey bundle, the same
    // key both sides derive the address from) and hand it to apply_relay_bundle as the expected issuer.
    let Some(account_id) = contact.and_then(|c| c.remote_account_id) else {
        return Ok(());
    };
    let sender_node_hex = match storage.get_latest_remote_prekey_bundle_for_account(&account_id) {
        Ok(Some(bundle)) => match node_address_from_ed25519_pubkey(bundle.identity_signing_key.0) {
            Ok(node) => node.to_string(),
            Err(_) => return Ok(()),
        },
        Ok(None) | Err(_) => return Ok(()),
    };
    match apply_relay_bundle(storage, &update.bundle_token, Some(&sender_node_hex)) {
        Ok(count) => omni_app_trace_log(
            "r1c/relay_set/apply",
            &format!("from={sender_node_hex} relays={count}"),
        ),
        Err(error) => omni_app_trace_log(
            "r1c/relay_set/reject",
            &format!("from={sender_node_hex} error={error}"),
        ),
    }
    Ok(())
}

/// Build the per-transfer receive state: resolve the attachment row, derive the content crypto
/// material and open the temp file once, then take the block map as the temp file allows (which is
/// also what makes a resumed transfer pick up at its first missing block).
fn open_attachment_receive_writer(
    storage: &Storage,
    transfer_id: &str,
) -> OmniResult<AttachmentReceiveWriter> {
    let runtime = storage
        .get_attachment_runtime_info_by_active_transfer_id(transfer_id)?
        .ok_or_else(|| {
            OmniError::Storage(format!("attachment transfer '{}' was not found", transfer_id))
        })?;
    let temp_download_path = runtime.temp_download_path.clone().ok_or_else(|| {
        OmniError::Storage("attachment download was not prepared locally".to_string())
    })?;
    if let Some(parent) = temp_download_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            OmniError::Storage(format!(
                "failed to create temporary attachment directory '{}': {error}",
                parent.display()
            ))
        })?;
    }
    let material = attachment_content_crypto_material(&runtime)?;
    // Encrypted inside Omni's storage (a photo that downloads by itself), plain next to a file the
    // user saves (docs/decisions/db-at-rest-encryption.md, «Вложения»).
    let file = storage.open_attachment_part_writer(&temp_download_path)?;
    // What is here already: the map, as the temp file allows (the Start frame checked it).
    let received = storage.attachment_download_resume(&runtime.attachment.message_id, current_unix_ms())?;
    let blocks = storage.attachment_received_blocks(&received.0.attachment.message_id)?;
    let content_id = hex::decode(&runtime.attachment.content_id_hex)
        .ok()
        .and_then(|bytes| <omni_crypto::ContentHash>::try_from(bytes).ok())
        .ok_or_else(|| OmniError::Storage("attachment content id is not a tree root".to_string()))?;
    Ok(AttachmentReceiveWriter {
        file,
        material,
        message_id: runtime.attachment.message_id.clone(),
        file_size_bytes: runtime.attachment.file_size_bytes,
        progress_bytes: blocks.received_bytes(),
        have: blocks.have,
        written_leaves: std::collections::BTreeMap::new(),
        checkpointed_at_unix_ms: current_unix_ms(),
        content_id,
        block_leaves: std::collections::BTreeMap::new(),
        unsaved_blocks: Vec::new(),
    })
}

fn persist_attachment_transfer_chunk_frame(
    storage: &Storage,
    chunk: AttachmentTransferChunkV2,
    dialog_key: &str,
) -> OmniResult<()> {
    let transfer_id = chunk.transfer_id.clone();
    let db_path = storage.db_path().to_string_lossy().into_owned();
    // Reuse the state built for this transfer; only the first chunk pays the setup. The writer is
    // SHARED (one temp file, one block map, one set of open blocks per transfer) and locked for the
    // duration of one chunk, so chunks arriving on different carrier threads serialise here instead
    // of racing on the same file. When the temp file fails, the cached state is forgotten rather
    // than kept, so the next chunk re-derives it from disk truth.
    let shared = attachment_receive_writer_or_open(&db_path, &transfer_id, || {
        open_attachment_receive_writer(storage, &transfer_id)
    })?;
    let result = {
        let mut writer = shared
            .lock()
            .map_err(|_| OmniError::Storage("attachment receive writer is poisoned".into()))?;
        persist_attachment_transfer_chunk_with_writer(
            storage,
            &db_path,
            chunk,
            dialog_key,
            &mut writer,
        )
    };
    // Forgotten only when the temp file itself failed: a chunk refused by its check (a wrong block
    // from one source) wrote nothing, and the writer's open blocks belong to the other streams of
    // this transfer still writing through it (stage 3, several sources).
    if let Err(error) = &result {
        if error.to_string().contains("attachment temp file") {
            forget_attachment_receive_writer(&db_path, &transfer_id);
        }
    }
    result
}

fn persist_attachment_transfer_chunk_with_writer(
    storage: &Storage,
    db_path: &str,
    chunk: AttachmentTransferChunkV2,
    dialog_key: &str,
    writer: &mut AttachmentReceiveWriter,
) -> OmniResult<()> {
    let AttachmentTransferChunkV2 {
        transfer_id,
        chunk_index,
        payload,
        sent_at_unix_ms,
    } = chunk;
    let chunk_counter = chunk_index.saturating_add(1);
    let payload_bytes = open_attachment_transfer_payload_owned(
        &writer.material,
        &transfer_id,
        "chunk",
        chunk_counter,
        sent_at_unix_ms,
        payload,
    )?;
    let payload: AttachmentTransferChunkPayloadV2 =
        deserialize_attachment_transfer_payload(&payload_bytes)?;
    let plaintext_chunk = payload.data;
    let incoming_end = payload.offset_bytes + plaintext_chunk.len() as i64;
    if payload.offset_bytes < 0 {
        return Err(OmniError::Storage(
            "attachment transfer chunk offset must not be negative".to_string(),
        ));
    }
    if incoming_end > writer.file_size_bytes {
        return Err(OmniError::Storage(
            "attachment transfer chunk exceeds the offered file size".to_string(),
        ));
    }
    let block = payload.offset_bytes as u64 / omni_crypto::CONTENT_TREE_BLOCK_BYTES;
    // A block's proof rides its first chunk sent. It stands on its own (leaves + path against the
    // file's identity), so it is checked and kept now, and every chunk of the block is then checked
    // against its leaf as it is written. A block already here needs none of it.
    let block_here = writer.have.get(block as usize).copied().unwrap_or(false);
    if let Some(proof) = payload.block_proof {
        if !omni_crypto::verify_content_block(
            &writer.content_id,
            writer.file_size_bytes as u64,
            block,
            &proof.leaves,
            &proof.path,
        ) {
            return Err(OmniError::Storage(format!(
                "attachment block {block} proof does not match the offered file"
            )));
        }
        if !block_here {
            writer.block_leaves.entry(block).or_insert(proof.leaves);
        }
    }
    if block_here {
        return Ok(()); // here already (from another source, or before a resume)
    }
    if !writer.block_leaves.contains_key(&block) {
        // Its proof rides another chunk, still on its way (several streams carry no order between
        // them): held in RAM until the proof lands. Named, or a stall here is invisible.
        omni_app_trace_log(
            "attachment/chunk",
            &format!(
                "transfer={transfer_id} offset={} len={} -> BUFFERED (block {block} awaits its proof)",
                payload.offset_bytes,
                plaintext_chunk.len(),
            ),
        );
        buffer_attachment_chunk_for_reorder(
            db_path,
            &transfer_id,
            writer.file_size_bytes,
            payload.offset_bytes,
            plaintext_chunk,
        )?;
        return Ok(());
    }
    // NB: no per-chunk "WRITE" trace here: `omni_app_trace_log` fsyncs each line, and a line per
    // 1 MB chunk was a measured throughput ceiling (~240 Mbit/s same-host).
    write_checked_attachment_chunk(writer, payload.offset_bytes, &plaintext_chunk)?;
    // Chunks of this block that came before its proof: written now.
    let registry_key = attachment_reorder_registry_key(db_path, &transfer_id);
    let block_start = (block * omni_crypto::CONTENT_TREE_BLOCK_BYTES) as i64;
    let block_end = block_start + omni_crypto::CONTENT_TREE_BLOCK_BYTES as i64;
    let waiting: Vec<(i64, Vec<u8>)> = {
        let mut registry = lock_attachment_reorder_registry()?;
        let taken = match registry.get_mut(&registry_key) {
            Some(pending) => {
                let offsets: Vec<i64> = pending.range(block_start..block_end).map(|(offset, _)| *offset).collect();
                offsets.into_iter().filter_map(|offset| pending.remove(&offset).map(|data| (offset, data))).collect()
            }
            None => Vec::new(),
        };
        if registry.get(&registry_key).is_some_and(|pending| pending.is_empty()) {
            registry.remove(&registry_key);
        }
        taken
    };
    for (offset, data) in waiting {
        if writer.have.get(block as usize).copied().unwrap_or(false) {
            break;
        }
        write_checked_attachment_chunk(writer, offset, &data)?;
    }
    let now_unix_ms = current_unix_ms();
    if should_checkpoint_attachment_download_progress(
        writer.file_size_bytes,
        if writer.is_complete() { writer.file_size_bytes } else { writer.progress_bytes },
        writer.checkpointed_at_unix_ms,
        now_unix_ms,
    ) {
        checkpoint_attachment_receive_writer(storage, writer, now_unix_ms)?;
        // Presence activity rides the checkpoint cadence instead of firing per chunk. Every DB write
        // commits with SQLite's default `synchronous=FULL`, so a per-chunk presence UPDATE was an
        // fsync per 1 MB on the receiver — the same tax the per-chunk trace was. Start/Complete still
        // mark activity, and this keeps a long transfer's presence fresh without paying it per chunk.
        let _ = mark_contact_activity_for_dialog(storage, dialog_key, current_unix_ms());
    }
    Ok(())
}

/// Write the chunk at its place after checking it against its leaf of the tree, and close its
/// block when it is the block's last leaf written. A chunk is a leaf (the sender cuts it so), so a
/// chunk that is not one — misplaced, short, long — is refused like a wrong one; a leaf written
/// already is skipped (the same leaf from another source, or sent again).
fn write_checked_attachment_chunk(writer: &mut AttachmentReceiveWriter, offset: i64, data: &[u8]) -> OmniResult<()> {
    let leaf_bytes = omni_crypto::CONTENT_TREE_LEAF_BYTES;
    let size = writer.file_size_bytes as u64;
    let offset_u = offset as u64;
    if !offset_u.is_multiple_of(leaf_bytes) || offset_u >= size.max(1) || data.len() as u64 != leaf_bytes.min(size - offset_u) {
        return Err(OmniError::Storage(format!(
            "attachment chunk {offset}+{} is not a leaf of the file",
            data.len()
        )));
    }
    let leaf = offset_u / leaf_bytes;
    let block = leaf / omni_crypto::CONTENT_TREE_BLOCK_LEAVES;
    let index_in_block = (leaf % omni_crypto::CONTENT_TREE_BLOCK_LEAVES) as usize;
    let leaves = writer.block_leaves.get(&block).ok_or_else(|| {
        OmniError::Storage(format!("attachment chunk of block {block} came without the block's proof"))
    })?;
    if leaves.get(index_in_block) != Some(&omni_crypto::content_leaf_hash(data)) {
        return Err(OmniError::Storage(format!(
            "attachment chunk {offset} does not match its leaf of the file"
        )));
    }
    let leaf_count = leaves.len();
    let written = writer.written_leaves.entry(block).or_insert(0);
    if *written & (1 << index_in_block) != 0 {
        return Ok(());
    }
    writer.file.write_at(offset_u, data)?;
    *written |= 1 << index_in_block;
    writer.progress_bytes += data.len() as i64;
    if *written == (1_u32 << leaf_count) - 1 {
        writer.written_leaves.remove(&block);
        if let Some(leaves) = writer.block_leaves.remove(&block) {
            if let Some(root) = omni_crypto::content_tree_root(&leaves) {
                writer.unsaved_blocks.push((block, root));
            }
        }
        if let Some(have) = writer.have.get_mut(block as usize) {
            *have = true;
        }
    }
    Ok(())
}

/// Make what is written durable, then say so: the data reaches the disk BEFORE the checkpoint that
/// a resume trusts (a block in the map is never fetched again), and the finished blocks' hashes go
/// with it.
fn checkpoint_attachment_receive_writer(
    storage: &Storage,
    writer: &mut AttachmentReceiveWriter,
    now_unix_ms: i64,
) -> OmniResult<()> {
    writer.file.sync_data()?;
    if !writer.unsaved_blocks.is_empty() {
        storage.mark_attachment_blocks_received(&writer.message_id, &writer.unsaved_blocks)?;
        writer.unsaved_blocks.clear();
    }
    storage.mark_attachment_downloading(&writer.message_id, writer.progress_bytes, now_unix_ms)?;
    writer.checkpointed_at_unix_ms = now_unix_ms;
    Ok(())
}

/// Before a resume offset is worked out while a stream may still be writing: what it wrote so far
/// is counted first, so the new request asks for the rest and not for what is already here.
fn settle_live_attachment_receive_writer(storage: &Storage, runtime: &AttachmentRuntimeInfo) {
    if let Some(transfer_id) = runtime.active_transfer_id.as_deref() {
        checkpoint_attachment_receive_writer_by_transfer(storage, &storage.db_path().to_string_lossy(), transfer_id);
    }
}

/// The last word of a stream that ended (or is replaced by a new one): whatever arrived since the
/// last checkpoint is made durable and counted, so the resume starts there and not 250 ms earlier.
fn checkpoint_attachment_receive_writer_by_transfer(storage: &Storage, db_path: &str, transfer_id: &str) {
    let Some(shared) = attachment_receive_writer(db_path, transfer_id) else {
        return;
    };
    let Ok(mut writer) = shared.lock() else {
        return;
    };
    // Complete too: its last blocks go into the map here, and completion reads the map.
    {
        if let Err(error) = checkpoint_attachment_receive_writer(storage, &mut writer, current_unix_ms()) {
            omni_app_trace_log("attachment/checkpoint", &format!("transfer={transfer_id} failed: {error}"));
        }
    }
}

fn persist_attachment_transfer_complete_frame(
    storage: &Storage,
    transport: &TransportManager,
    completion: AttachmentTransferCompleteV2,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let runtime = finish_attachment_transfer(storage, completion)?;
    // A copy of our own message, its file now here too: handed on when the device that sent it is
    // away (3.3; `attachment_requester_allowed` keeps it to the chat's contact and our devices).
    if own_device_for_dialog(storage, &runtime.dialog_key)?.is_some() {
        if let Err(error) = storage.serve_downloaded_attachment(&runtime.attachment.message_id) {
            omni_app_trace_log(
                "attachment/complete",
                &format!("message={} copy not served on: {error}", runtime.attachment.message_id),
            );
        }
    }
    let session = load_receive_session(storage, receive_context)?;
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::attachment_downloaded(
            runtime.attachment.message_id.clone(),
            current_unix_ms(),
        ),
        SecurityPayloadKind::AttachmentDownloaded,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    Ok(())
}

/// A Complete frame: check it against the offer and what arrived, then move the file into place.
fn finish_attachment_transfer(
    storage: &Storage,
    completion: AttachmentTransferCompleteV2,
) -> OmniResult<AttachmentRuntimeInfo> {
    let AttachmentTransferCompleteV2 {
        transfer_id,
        payload,
        completed_at_unix_ms,
    } = completion;
    let runtime = storage
        .get_attachment_runtime_info_by_active_transfer_id(&transfer_id)?
        .ok_or_else(|| {
            OmniError::Storage(format!(
                "attachment transfer '{}' was not found",
                transfer_id
            ))
        })?;
    let material = attachment_content_crypto_material(&runtime)?;
    let payload_bytes = open_attachment_transfer_payload_owned(
        &material,
        &transfer_id,
        "complete",
        ATTACHMENT_TRANSFER_COUNTER_COMPLETE,
        completed_at_unix_ms,
        payload,
    )?;
    let payload: AttachmentTransferCompletePayloadV2 =
        deserialize_attachment_transfer_payload(&payload_bytes)?;
    if runtime.attachment.file_size_bytes != payload.total_size_bytes {
        return Err(OmniError::Storage(
            "attachment transfer complete size does not match the offered file".to_string(),
        ));
    }
    if runtime.attachment.content_id_hex != payload.content_id_hex {
        return Err(OmniError::Storage(
            "attachment transfer complete hash does not match the offered file".to_string(),
        ));
    }
    if !complete_received_attachment(storage, &runtime, &transfer_id)? {
        omni_app_trace_log(
            "attachment/complete",
            &format!("transfer={transfer_id} ended with blocks of the file missing: asks again"),
        );
        return Err(OmniError::Storage(
            "attachment transfer ended with blocks of the file missing".to_string(),
        ));
    }
    Ok(runtime)
}

/// Move a download into place once its map holds every block. Every chunk was checked against the
/// tree as it was written, and every block put in the map once synced: the tree is here, nothing is
/// read again. `false` — a block is still missing, the download stays and asks for it. The caller
/// has dropped the transfer's writer (Windows will not move a file a handle holds).
pub(crate) fn complete_received_attachment(
    storage: &Storage,
    runtime: &AttachmentRuntimeInfo,
    transfer_id: &str,
) -> OmniResult<bool> {
    let size = runtime.attachment.file_size_bytes.max(0) as u64;
    let verified = if size == 0 {
        Some(omni_crypto::ContentTreeBuilder::new().finish())
    } else {
        storage.received_content_tree(&runtime.attachment.message_id)?
    };
    let Some(verified) = verified else {
        return Ok(false);
    };
    storage.complete_attachment_download_from_temp_with_verified_tree(
        &runtime.attachment.message_id,
        current_unix_ms(),
        Some(&verified),
    )?;
    clear_attachment_reorder_buffer(&storage.db_path().to_string_lossy(), transfer_id)?;
    Ok(true)
}

/// How often download progress is written to the DB mid-transfer: the progress bar and the block map
/// (a resume starts at the first block not in it). Blocks closed since the last checkpoint are just
/// fetched again after a break, so checkpointing less often costs at most a block or two and a
/// coarser progress bar.
///
/// It used to be byte-based (`file_size / 2000`, floored at 512 KB), which for any real file meant a
/// checkpoint every 512 KB — i.e. a SQLite commit, and with SQLite's default `synchronous=FULL` an
/// fsync, on EVERY 1 MB chunk. Measured on loopback that was ~5 ms of the ~9 ms each chunk spent in
/// the receive path, while the actual work (decrypt 2 ms, disk and hash ~0) was a fraction of it.
/// Time-based instead: independent of file size and chunk size, ~4 writes/sec is more than a
/// progress bar needs, and completion always checkpoints.
const ATTACHMENT_PROGRESS_CHECKPOINT_INTERVAL_MS: i64 = 250;

fn should_checkpoint_attachment_download_progress(
    file_size_bytes: i64,
    bytes_transferred: i64,
    last_checkpoint_at_unix_ms: i64,
    now_unix_ms: i64,
) -> bool {
    if bytes_transferred >= file_size_bytes {
        return true;
    }
    now_unix_ms.saturating_sub(last_checkpoint_at_unix_ms)
        >= ATTACHMENT_PROGRESS_CHECKPOINT_INTERVAL_MS
}

fn persist_attachment_transfer_failed(
    storage: &Storage,
    transport: &TransportManager,
    failure: &omni_types::AttachmentTransferFailedV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    // A device of ours asked first does not have it: on to the next source now, not at the stall
    // retry. From anyone else the reason means nothing.
    if failure.reason_code.eq_ignore_ascii_case(ATTACHMENT_NOT_HERE_REASON) {
        let address = &session.session_address;
        if storage.is_own_other_device(&address.remote_account_id, &address.remote_device_id)? {
            note_own_device_lacks_file(&storage.db_path().to_string_lossy(), &failure.message_id, &address.remote_device_id);
            let _ = attempt_send_attachment_download_request(storage, transport, &failure.message_id);
        }
        return Ok(());
    }
    // Not about the file: the download stays as it is and is asked for again from where its `.part`
    // ends (the stall retry, or the reconnect). Marking it Failed ended a transfer on any hiccup.
    if failure.reason_code.eq_ignore_ascii_case(ATTACHMENT_TRANSFER_RETRYABLE_REASON) {
        omni_app_trace_log(
            "attachment/failed-notice",
            &format!(
                "message={} sender hit a retryable failure, download kept: {}",
                failure.message_id, failure.reason_message
            ),
        );
        return Ok(());
    }
    let next_state = if failure.reason_code.eq_ignore_ascii_case("file_not_found") {
        omni_types::AttachmentTransferState::Unavailable
    } else {
        omni_types::AttachmentTransferState::Failed
    };
    end_attachment_download_runtime(storage, &failure.message_id);
    storage.mark_attachment_transfer_failed(
        &failure.message_id,
        next_state,
        &failure.reason_message,
        current_unix_ms(),
    )?;
    storage.upsert_message_security_metadata(
        &format!("attachment-failed::{}", failure.message_id),
        &receive_context.dialog_key,
        storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)?
            .as_ref()
            .map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::AttachmentTransferFailed.as_db_value()),
        failure.failed_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

fn persist_attachment_downloaded(
    storage: &Storage,
    downloaded: &omni_types::AttachmentDownloadedV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    let from = &session.session_address;
    if storage.is_own_other_device(&from.remote_account_id, &from.remote_device_id)? {
        return Ok(()); // our own copy fetched it; "downloaded" is about the contact
    }
    storage.mark_attachment_downloaded_by_remote(&downloaded.message_id, current_unix_ms())?;
    storage.upsert_message_security_metadata(
        &format!("attachment-downloaded::{}", downloaded.message_id),
        &receive_context.dialog_key,
        storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)?
            .as_ref()
            .map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::AttachmentDownloaded.as_db_value()),
        downloaded.downloaded_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

fn persist_attachment_access_revoked(
    storage: &Storage,
    revoked: &omni_types::AttachmentAccessRevokedV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
    encryption_state: MessageEncryptionState,
) -> OmniResult<()> {
    end_attachment_download_runtime(storage, &revoked.message_id);
    storage.note_remote_attachment_access_revoked(&revoked.message_id, current_unix_ms())?;
    storage.upsert_message_security_metadata(
        &format!("attachment-revoked::{}", revoked.message_id),
        &receive_context.dialog_key,
        storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)?
            .as_ref()
            .map(|value| &value.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        encryption_state.as_db_value(),
        Some(session.bootstrap_state),
        Some(SecurityPayloadKind::AttachmentAccessRevoked.as_db_value()),
        revoked.revoked_at_unix_ms,
        current_unix_ms(),
    )?;
    Ok(())
}

fn handle_session_activated(
    runtime_storage: &Arc<Mutex<Storage>>,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let _session_guard = lock_session(session_lock)?;
    let storage_guard = lock_runtime_storage(runtime_storage)?;
    let storage = &*storage_guard;
    let now_unix_ms = current_unix_ms();
    let db_path = storage.db_path().to_string_lossy().to_string();
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }
    if transport_receive_context_is_local_instance(storage, receive_context)? {
        if let Some(transport) = clone_active_transport(transport_handle)? {
            let _ = transport.disconnect();
        }
        return Ok(());
    }

    let mut own_device: Option<DeviceId> = None;
    if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        let domain_bundle = prekey_bundle_from_protocol(remote_bundle)?;
        storage.record_remote_prekey_bundle(
            &domain_bundle,
            None,
            Some(&remote_bundle.device.device_label),
            remote_bundle.generated_at_unix_ms,
        )?;
        // Another device of our own account, proven by its certificate chain: one of ours to keep
        // in step, not a contact (docs/decisions/multi-device-equal-nodes.md, 2.4).
        let is_own_account = storage
            .get_local_account()?
            .is_some_and(|account| account.descriptor.account_id == domain_bundle.account_id);
        if let (true, Some(chain)) = (is_own_account, domain_bundle.device_certificate_chain.as_ref()) {
            storage.record_own_device(
                chain,
                &remote_bundle.device.device_label,
                &receive_context.dialog_key,
                now_unix_ms,
            )?;
            own_device = Some(domain_bundle.device_id.clone());
        }
    }

    if !receive_context.peer_id.is_empty() {
        let discovery_dialog_key = dialog_key_from_instance_id(&receive_context.peer_id);
        if discovery_dialog_key != receive_context.dialog_key {
            storage.rebind_dialog_scope(
                &discovery_dialog_key,
                &receive_context.dialog_key,
                now_unix_ms,
            )?;
            migrate_runtime_chat_session_dialog_key(
                &db_path,
                &discovery_dialog_key,
                &receive_context.dialog_key,
            )?;
        }
    }

    if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        let account_dialog_key =
            omni_types::dialog_key_from_account_id(&remote_bundle.account.account_id);
        // Whatever was written to the account before a device was known belongs to the contact's
        // chat — which is this device's dialog only when this is the contact's first device.
        let chat_dialog_key = storage.chat_dialog_key_for(&receive_context.dialog_key)?;
        if account_dialog_key != chat_dialog_key {
            storage.rebind_dialog_scope(&account_dialog_key, &chat_dialog_key, now_unix_ms)?;
            migrate_runtime_chat_session_dialog_key(&db_path, &account_dialog_key, &chat_dialog_key)?;
        }
        if let Some(contact) =
            resolve_transport_contact_for_receive_context(storage, receive_context)?
        {
            // This dialog is that device's: messages to the contact reach it with its own session.
            storage.set_dialog_link_device(
                &receive_context.dialog_key,
                &DeviceId(remote_bundle.device.device_id.clone()),
            )?;
            if transport_contact_needs_instance_repair(&contact, receive_context) {
                let _ = storage.bind_contact_to_remote_instance(
                    &contact.contact_id,
                    &receive_context.remote_instance_id,
                    Some(&receive_context.dialog_key),
                    now_unix_ms,
                )?;
            }
        }
    }

    if runtime_chat_session_auto_reconnect_paused(&db_path, &receive_context.dialog_key)? {
        if runtime_chat_session_paused_activation_allowed(&db_path, now_unix_ms)? {
            let _ = clear_chat_session_pause_for_profile(storage, &receive_context.dialog_key);
        } else {
            if let Some(transport) = clone_active_transport(transport_handle)? {
                let _ = transport.disconnect_dialog(&receive_context.dialog_key);
            }
            return Ok(());
        }
    }

    if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        if let Some(contact) = storage.find_contact_by_remote_account_id(
            &omni_identity::AccountId(remote_bundle.account.account_id.clone()),
        )? {
            if contact.relationship_state == ContactRelationshipState::Pending
                && contact.pending_request_direction == Some(ContactRequestDirection::Outgoing)
                && !runtime_chat_session_request_signal_sent_for_receive_context(
                    &db_path,
                    receive_context,
                )?
            {
                if let Some(transport) = clone_active_transport(transport_handle)? {
                    if let Some(session) =
                        established_session_for_receive_context(storage, receive_context)?
                    {
                        if let Ok(mut updated_session) = send_contact_request_signal_for_transport_session(
                            storage,
                            &transport,
                            &session,
                            Some(&receive_context.dialog_key),
                        ) {
                            let _ = storage.store_session_record(&mut updated_session);
                            let _ = storage
                                .record_contact_presence_activity(&contact.contact_id, now_unix_ms);
                        }
                    }
                }
            }
            if contact.relationship_state == ContactRelationshipState::Approved {
                let now = current_unix_ms();
                let _ = storage.record_contact_presence_activity(&contact.contact_id, now);
                if !contact.has_met {
                    let _ = storage.mark_contact_met(&contact.contact_id, now);
                }
                if let Some(transport) = clone_active_transport(transport_handle)? {
                    if let Some(session) =
                        established_session_for_receive_context(storage, receive_context)?
                    {
                        let session = if runtime_chat_session_approval_signal_pending_for_receive_context(
                            &db_path,
                            receive_context,
                        )? {
                            send_contact_approval_signal_for_session(
                                storage,
                                &transport,
                                &session,
                                Some(&receive_context.dialog_key),
                            )
                            .unwrap_or(session)
                        } else {
                            session
                        };
                        let _ = send_current_public_profile_sync_for_session(
                            storage, &transport, &session, None,
                        );
                        // Remote access: re-tell the contact on every link what we let it do, so a
                        // revocation made while it was offline lands now (remote-access-for-contacts.md).
                        // Reloaded: the profile sync above may have advanced the session.
                        if let Ok(Some(session)) =
                            established_session_for_receive_context(storage, receive_context)
                        {
                            let _ = send_remote_access_grant_for_session(
                                storage,
                                &transport,
                                &session,
                                &receive_context.dialog_key,
                            );
                        }
                    }
                }
            }
        }
    }

    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    let _ = clear_chat_session_pause_for_profile(storage, &receive_context.dialog_key);
    let _ = mark_runtime_chat_session_lease_triggered(&db_path, &receive_context.dialog_key);
    if let Some(connection) = transport.connection_snapshot_for_dialog(&receive_context.dialog_key)
    {
        let _ = set_runtime_chat_session_endpoints(
            &db_path,
            &receive_context.dialog_key,
            connection.local_endpoint.as_deref(),
            connection.peer_endpoint.as_deref(),
        );
    }
    let _ = touch_runtime_chat_session_interaction(
        &db_path,
        &receive_context.dialog_key,
        omni_types::ChatSessionInteractionKind::Session,
        now_unix_ms,
    );
    let _ = handle_call_session_activated(storage, receive_context);
    process_pending_for_active_dialog(storage, &transport, &receive_context.dialog_key)?;
    request_downloads_on_session_up(storage, &transport, &receive_context.dialog_key);
    if let Some(device_id) = own_device.as_ref() {
        return catch_up_own_device(storage, &transport, device_id, &receive_context.dialog_key);
    }
    if let Some(bundle) = receive_context.remote_prekey_bundle.as_ref() {
        catch_up_contact_device(
            storage,
            &transport,
            &receive_context.dialog_key,
            &DeviceId(bundle.device.device_id.clone()),
        )?;
    }
    Ok(())
}

fn handle_session_closed(
    runtime_storage: &Arc<Mutex<Storage>>,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let storage_guard = lock_runtime_storage(runtime_storage)?;
    let storage = &*storage_guard;
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }
    let db_path = storage.db_path().to_string_lossy().to_string();

    if receive_context.initiated_locally {
        if !active_transport_still_matches_closed_context(transport_handle, receive_context)? {
            delete_unestablished_session_state_for_received_context(storage, receive_context)?;
        }
    } else if let Some(contact) = storage.resolve_contact_for_dialog(&receive_context.dialog_key)? {
        if contact.relationship_state == ContactRelationshipState::Approved
            && runtime_chat_session_auto_reconnect_paused(&db_path, &receive_context.dialog_key)?
        {
            let _ = storage.record_contact_presence_offline(&contact.contact_id, current_unix_ms());
        }
        if contact.relationship_state == ContactRelationshipState::Pending
            && contact.pending_request_direction == Some(ContactRequestDirection::Incoming)
            && storage
                .get_dialog_message_refresh_summary(&receive_context.dialog_key)?
                .message_count
                == 0
            && runtime_chat_session_last_interaction_kind(&db_path, &receive_context.dialog_key)?
                != Some(omni_types::ChatSessionInteractionKind::Approval)
            && !runtime_chat_session_request_signal_observed(&db_path, &receive_context.dialog_key)?
        {
            let removed = storage.forget_contact(&contact.contact_id, current_unix_ms())?;
            if let Some(remote_account_id) = removed.remote_account_id.as_ref() {
                let _ = storage.delete_session_state_for_remote_account(remote_account_id);
            }
            let _ = clear_verification_challenges_for_contact(storage, &removed);
            let _ = clear_runtime_chat_session_for_contact(storage, &removed);
            let _ = clear_runtime_contact_ephemera_for_active_profile(storage);
        }
    }
    clear_verification_challenge_ready(
        &db_path,
        &receive_context.dialog_key,
        &receive_context.peer_id,
    );
    let _ = mark_runtime_chat_session_lease_triggered(&db_path, &receive_context.dialog_key);
    let _ = handle_call_session_closed(storage, receive_context);

    Ok(())
}

fn active_transport_still_matches_closed_context(
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    receive_context: &ReceivedChatContext,
) -> OmniResult<bool> {
    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(false);
    };
    let snapshot = transport.snapshot();
    if !matches!(
        snapshot.state,
        TransportState::Connected | TransportState::ListeningAndConnected
    ) {
        return Ok(false);
    }
    if snapshot.remote_instance_id.as_deref() != Some(receive_context.remote_instance_id.as_str()) {
        return Ok(false);
    }
    if let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() {
        if transport.active_remote_account_id().as_deref()
            != Some(remote_bundle.account.account_id.as_str())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Outcome of handling an inbound bootstrap, so the caller can tell "I became the responder"
/// from "I won the collision and kept my own bootstrap" -- two states that need opposite follow-ups
/// and used to be indistinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InboundBootstrapOutcome {
    /// A responder session was created and stored; acking it is what advances the initiator.
    Accepted,
    /// We are the initiator and won the collision, so the peer's bootstrap was discarded. There is
    /// nothing to ack -- what the peer needs is OUR bootstrap, again.
    AbsorbedAsInitiator,
}

fn transport_accept_session_bootstrap(
    storage: &Storage,
    message: &SessionBootstrapMessageV1,
) -> OmniResult<InboundBootstrapOutcome> {
    validate_session_bootstrap_message_v1(message)?;
    storage.ensure_local_crypto_foundation(current_unix_ms())?;

    let (_profile, local_account, local_device) = active_profile_context(storage)?;
    let local_identity_key = storage
        .get_local_identity_key(&local_account.descriptor.account_id)?
        .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
    let local_device_key = storage
        .get_local_device_key(&local_device.device_id)?
        .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: AccountId(message.initiator_account_id.clone()),
        remote_device_id: DeviceId(message.initiator_device_id.clone()),
    };
    let has_approved_contact = storage
        .find_contact_by_remote_account_id(&AccountId(message.initiator_account_id.clone()))?
        .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved);
    // Read the mark BEFORE recording this bootstrap, then record it either way: an echo we ignore
    // must still count as "seen", so every later re-delivery of it is recognised too.
    let db_path = storage.db_path().to_string_lossy().to_string();
    let last_seen_bootstrap_created_at = last_inbound_bootstrap_created_at(
        &db_path,
        &message.initiator_account_id,
        &message.initiator_device_id,
    );
    record_inbound_bootstrap_created_at(
        &db_path,
        &message.initiator_account_id,
        &message.initiator_device_id,
        message.created_at_unix_ms,
    );
    if let Some(existing_session) = storage.get_session_record(&session_address)? {
        if existing_session_should_absorb_incoming_bootstrap(
            &local_account.descriptor.account_id,
            &local_device.device_id,
            &existing_session,
            message,
            has_approved_contact,
            last_seen_bootstrap_created_at,
        ) {
            omni_app_trace_log(
                "recv/bootstrap",
                &format!(
                    "absorbed peer bootstrap from account={} device={}; keeping our own session (state={})",
                    message.initiator_account_id,
                    message.initiator_device_id,
                    existing_session.bootstrap_state.as_db_value(),
                ),
            );
            return Ok(InboundBootstrapOutcome::AbsorbedAsInitiator);
        }
    }
    let signed_prekey = storage
        .get_active_signed_prekey(&local_device.device_id)?
        .ok_or_else(|| OmniError::Storage("active signed prekey is missing".to_string()))?;
    let one_time_prekey = match message.remote_one_time_prekey_id {
        Some(expected_id) => {
            let candidate = storage
                .get_one_time_prekey_for_inbound_bootstrap(&local_device.device_id, expected_id, current_unix_ms())?
                .ok_or_else(|| {
                    OmniError::Storage("expected one-time prekey is missing".to_string())
                })?;
            Some(candidate)
        }
        None => None,
    };

    let bootstrap = session_bootstrap_from_protocol(message)?;
    let session = bootstrap_responder_session(
        &local_account.descriptor.account_id,
        &local_identity_key,
        &local_device_key,
        &signed_prekey,
        one_time_prekey.as_ref(),
        &bootstrap,
        current_unix_ms(),
    )?;
    let mut session = establish_session(&session, current_unix_ms());
    storage.replace_session_record(&mut session)?;
    let replay_window =
        storage.ensure_replay_window(&session.session_address, current_unix_ms())?;
    storage.store_replay_window(&replay_window)?;

    if let Some(prekey) = one_time_prekey {
        storage.mark_one_time_prekey_consumed(
            &prekey.device_id,
            prekey.prekey_id,
            current_unix_ms(),
        )?;
    }

    omni_app_trace_log(
        "recv/bootstrap",
        &format!(
            "accepted as responder from account={} device={}",
            message.initiator_account_id, message.initiator_device_id
        ),
    );
    Ok(InboundBootstrapOutcome::Accepted)
}

fn send_session_bootstrap_ack(
    storage: &Storage,
    transport: &TransportManager,
    message: &SessionBootstrapMessageV1,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: AccountId(message.initiator_account_id.clone()),
        remote_device_id: DeviceId(message.initiator_device_id.clone()),
    };
    let session = storage
        .get_session_record(&session_address)?
        .ok_or_else(|| OmniError::Transport("accepted bootstrap session is missing".to_string()))?;
    // `ResponderBootstrapAccepted` is exactly the state this ack exists to report, and demanding
    // `Established` here deadlocked first contact permanently:
    //
    //   we accept the bootstrap        -> session is ResponderBootstrapAccepted
    //   we refuse to ack, "not established yet"
    //   the initiator never hears back -> stays InitiatorBootstrapCreated, sends nothing but retries
    //   we never reach Established     -> that needs a message from the initiator
    //
    // Both sides then sit on `no_active_session / waiting for encrypted session bootstrap
    // acknowledgement` forever, re-delivering the same bootstrap. Observed live 2026-08-11 on two
    // machines after both forgot the contact and re-paired: nine attempts a side and climbing, chat
    // stuck, "Never met" permanent, and realtime media dead behind it ("no established realtime
    // media signaling session"). The ack itself does not need an established session -- it is an
    // ordinary encrypted envelope built from the responder session we just created, and sending it
    // is what advances the INITIATOR.
    //
    // Anything earlier than ResponderBootstrapAccepted would mean we have no responder keys yet, so
    // that stays a refusal.
    if !matches!(
        session.bootstrap_state,
        omni_crypto::SessionBootstrapState::ResponderBootstrapAccepted
            | omni_crypto::SessionBootstrapState::Established
    ) {
        return Err(OmniError::Transport(format!(
            "accepted bootstrap session is not ready to ack (state={})",
            session.bootstrap_state.as_db_value()
        )));
    }
    let now_unix_ms = current_unix_ms();
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::contact_presence(now_unix_ms),
        SecurityPayloadKind::ContactPresence,
        now_unix_ms,
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    // Durable backstop for the bootstrap ACK (relay-less flaky-link fix, 2026-07-20). The ack is
    // what advances the INITIATOR to an established session; over a lossy mobile link the live send
    // can be lost just like the bootstrap was. Also hold it durable so the mesh reconcile re-delivers
    // it over any surviving link — the initiator has a persisted node↔dialog route (set at pairing)
    // so a durable ack resolves + decrypts on its side. Idempotent: the encrypted ack is deduped by
    // the durable feed/seq and the logical-event handler tolerates a repeat. Symmetry with the
    // bootstrap's own durable backstop (finish_mesh_pairing_from_hello). See mesh-durable-delivery.md.
    //
    // ПОРЯДОК ЗДЕСЬ — НЕ КОСМЕТИКА. Раньше страховка стояла ПОСЛЕ живой отправки и за `?`, то есть
    // срабатывала только когда живая отправка и так удалась — ровно тогда, когда она не нужна. Стоило
    // `send_wire_envelope_for_dialog_key` вернуть ошибку, как функция выходила, ack не уходил ВООБЩЕ
    // и не откладывался, а вызывающий писал «ack deferred (will retry on redelivery)» — про повтор,
    // которого не будет, потому что откладывать было нечего.
    //
    // Живой замер 2026-08-14: после долгого молчания приёмник 184 раза подряд принял bootstrap как
    // responder и 184 раза не смог отправить ack («no active QUIC carrier session for peer») — при
    // здоровом QUIC (RTT 8 мс, потерь 0, 1.6 МБ ушло). Отправитель навсегда остался в
    // `initiator_bootstrap_created` и бесконечно перепосылал bootstrap; 200 сообщений висели 8 минут
    // и продолжали висеть. См. docs/bugs/bootstrap-ack-never-held-durable.md.
    let held_durable =
        transport.hold_durable_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope);
    let live_send =
        transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope);
    if let Err(error) = &live_send {
        omni_app_trace_log(
            "recv/bootstrap",
            &format!("ack live send failed (durable held={held_durable}): {error}"),
        );
    }
    // Ошибку отдаём наверх только когда НИ ОДИН путь не принял ack: тогда повторять действительно
    // нечем. Если конверт лежит в durable, ack доедет reconcile'ом, и падать нельзя — иначе мы
    // вернёмся к той же петле.
    if live_send.is_err() && !held_durable {
        live_send?;
    }
    let db_path = storage.db_path().to_string_lossy().to_string();
    if storage
        .find_contact_by_remote_account_id(&AccountId(message.initiator_account_id.clone()))?
        .as_ref()
        .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved)
        && runtime_chat_session_approval_signal_pending_for_receive_context(&db_path, receive_context)?
    {
        let _ = send_contact_approval_signal_for_session(
            storage,
            transport,
            &outgoing.updated_session,
            Some(&receive_context.dialog_key),
        );
    }
    mark_verification_challenge_ready(
        &db_path,
        &receive_context.dialog_key,
        &receive_context.peer_id,
    );
    let _ =
        maybe_send_verification_challenge_for_receive_context(storage, transport, receive_context);
    Ok(())
}

fn maybe_send_verification_challenge_for_receive_context(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let Some(remote_bundle) = receive_context.remote_prekey_bundle.as_ref() else {
        return Ok(());
    };
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }
    let Some(contact) = storage
        .find_contact_by_remote_account_id(&AccountId(remote_bundle.account.account_id.clone()))?
    else {
        return Ok(());
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }

    let now = current_unix_ms();
    let db_path = storage.db_path().to_string_lossy().to_string();
    let dialog_key = &receive_context.dialog_key;
    let peer_id = &receive_context.peer_id;
    if !verification_challenge_is_ready(&db_path, dialog_key, peer_id) {
        return Ok(());
    }
    if challenge_already_pending(&db_path, dialog_key, peer_id, now) {
        clear_verification_challenge_ready(&db_path, dialog_key, peer_id);
        return Ok(());
    }

    let challenge_id = format!("challenge-{}-{}", now, hex::encode(random_bytes(8)?));
    let nonce_hex = hex::encode(random_bytes(16)?);
    let expires_at = now + 5 * 60 * 1000;
    let challenge = omni_types::VerificationChallengeV1 {
        version: omni_types::OMNI_PROTOCOL_VERSION_V1,
        challenge_id: challenge_id.clone(),
        nonce_hex,
        issued_at_unix_ms: now,
        expires_at_unix_ms: expires_at,
        remote_instance_id: Some(receive_context.remote_instance_id.clone()),
        remote_account_id: Some(remote_bundle.account.account_id.clone()),
    };
    let challenge_bytes = verification_challenge_signing_bytes(&challenge)?;

    register_pending_verification_challenge(
        &db_path,
        challenge_id.clone(),
        PendingVerificationChallenge {
            dialog_key: dialog_key.clone(),
            peer_id: peer_id.clone(),
            remote_instance_id: Some(receive_context.remote_instance_id.clone()),
            remote_account_id: Some(remote_bundle.account.account_id.clone()),
            challenge_bytes,
            expires_at_unix_ms: expires_at,
        },
    );

    let payload = omni_types::WirePayloadV1::VerificationChallenge(challenge.clone());
    let mut outgoing = prepare_encrypted_payload_for_established_session(
        storage,
        &AccountId(remote_bundle.account.account_id.clone()),
        dialog_key,
        &payload,
        SecurityPayloadKind::VerificationChallenge,
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    clear_verification_challenge_ready(&db_path, dialog_key, peer_id);
    let _ = storage.record_contact_presence_activity(&contact.contact_id, now);
    Ok(())
}

fn maybe_send_verification_challenge(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    maybe_send_verification_challenge_for_receive_context(storage, &transport, receive_context)
}

fn handle_incoming_attachment_transfer_frame(
    runtime_storage: &Arc<Mutex<Storage>>,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    session_lock: &Arc<Mutex<()>>,
    frame: AttachmentTransferFrameV2,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let _session_guard = lock_session(session_lock)?;
    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    let storage_guard = lock_runtime_storage(runtime_storage)?;
    let storage = &*storage_guard;
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }

    // Chunk frames mark presence on the checkpoint cadence inside the chunk handler instead — a
    // per-frame presence UPDATE is an fsync per 1 MB during a bulk download (SQLite defaults to
    // `synchronous=FULL`). Every other frame kind is rare, so it marks activity right here.
    let db_path = storage.db_path().to_string_lossy().into_owned();
    if !matches!(frame, AttachmentTransferFrameV2::Chunk(_)) {
        mark_contact_activity_for_dialog(storage, &receive_context.dialog_key, current_unix_ms())?;
    }
    match frame {
        AttachmentTransferFrameV2::Start(start) => {
            // A fresh stream (including a resume) re-derives its state from disk — after what the
            // old one wrote is counted, or the resume would cut it.
            checkpoint_attachment_receive_writer_by_transfer(storage, &db_path, &start.transfer_id);
            drop_attachment_receive_writer(&db_path, &start.transfer_id);
            clear_attachment_reorder_buffer(&db_path, &start.transfer_id)?;
            persist_attachment_transfer_start_frame(storage, start)
        }
        AttachmentTransferFrameV2::Chunk(chunk) => {
            persist_attachment_transfer_chunk_frame(storage, chunk, &receive_context.dialog_key)
        }
        AttachmentTransferFrameV2::Complete(complete) => {
            // NB: the wait for the other streams to finish delivering happens in the stream reader,
            // BEFORE this function is entered. It cannot happen here: this function holds both the
            // session lock and the storage lock for the whole frame, and the chunks being waited for
            // need those same locks to be persisted. Waiting here deadlocked every time -- the
            // chunk threads sat 28.6 s behind the lock, the wait timed out having achieved nothing,
            // and completion then finalised the transfer out from under them.
            // The last blocks go into the map, then the temp file is closed BEFORE completion moves
            // it into place — an open handle blocks the move on Windows.
            checkpoint_attachment_receive_writer_by_transfer(storage, &db_path, &complete.transfer_id);
            drop_attachment_receive_writer(&db_path, &complete.transfer_id);
            persist_attachment_transfer_complete_frame(
                storage,
                &transport,
                complete,
                receive_context,
            )
        }
    }
}

pub(crate) fn maybe_send_verification_challenge_for_active_transport(
    storage: &Storage,
    transport: &TransportManager,
) -> OmniResult<()> {
    send_verification_challenge_for_active_transport(storage, transport, false)
}

fn send_verification_challenge_for_active_transport(
    storage: &Storage,
    transport: &TransportManager,
    allow_existing_pending: bool,
) -> OmniResult<()> {
    let Some(remote_bundle) = transport.active_remote_prekey_bundle() else {
        return Ok(());
    };
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }
    let Some(contact) = storage
        .find_contact_by_remote_account_id(&AccountId(remote_bundle.account.account_id.clone()))?
    else {
        return Ok(());
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }

    let snapshot = transport.snapshot();
    let Some(dialog_key) = snapshot.dialog_key.clone() else {
        return Ok(());
    };
    let Some(peer_id) = snapshot.peer_id.clone() else {
        return Ok(());
    };

    let now = current_unix_ms();
    let db_path = storage.db_path().to_string_lossy().to_string();
    if !verification_challenge_is_ready(&db_path, &dialog_key, &peer_id) {
        return Ok(());
    }
    if challenge_already_pending(&db_path, &dialog_key, &peer_id, now) {
        if allow_existing_pending {
            clear_pending_verification_challenges_for_session(
                &db_path,
                &dialog_key,
                &peer_id,
            );
        } else {
            clear_verification_challenge_ready(&db_path, &dialog_key, &peer_id);
            return Ok(());
        }
    }

    let challenge_id = format!("challenge-{}-{}", now, hex::encode(random_bytes(8)?));
    let nonce_hex = hex::encode(random_bytes(16)?);
    let expires_at = now + 5 * 60 * 1000;
    let challenge = omni_types::VerificationChallengeV1 {
        version: omni_types::OMNI_PROTOCOL_VERSION_V1,
        challenge_id: challenge_id.clone(),
        nonce_hex,
        issued_at_unix_ms: now,
        expires_at_unix_ms: expires_at,
        remote_instance_id: snapshot.remote_instance_id.clone(),
        remote_account_id: Some(remote_bundle.account.account_id.clone()),
    };
    let challenge_bytes = verification_challenge_signing_bytes(&challenge)?;

    register_pending_verification_challenge(
        &db_path,
        challenge_id.clone(),
        PendingVerificationChallenge {
            dialog_key: dialog_key.clone(),
            peer_id: peer_id.clone(),
            remote_instance_id: snapshot.remote_instance_id.clone(),
            remote_account_id: Some(remote_bundle.account.account_id.clone()),
            challenge_bytes,
            expires_at_unix_ms: expires_at,
        },
    );

    let payload = omni_types::WirePayloadV1::VerificationChallenge(challenge.clone());
    let mut outgoing = match prepare_encrypted_payload_for_established_session(
        storage,
        &AccountId(remote_bundle.account.account_id.clone()),
        &dialog_key,
        &payload,
        SecurityPayloadKind::VerificationChallenge,
    ) {
        Ok(value) => value,
        Err(error) => {
            if error
                .to_string()
                .contains("no established session is available")
                || error
                    .to_string()
                    .contains("session is not fully established yet")
            {
                clear_pending_verification_challenge(&db_path, &challenge_id);
                return Ok(());
            }
            clear_pending_verification_challenge(&db_path, &challenge_id);
            return Err(error);
        }
    };
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Err(error) = transport.send_wire_envelope_for_dialog_key(&dialog_key, &outgoing.envelope)
    {
        if error
            .to_string()
            .contains("no established session is available")
            || error
                .to_string()
                .contains("session is not fully established yet")
        {
            clear_pending_verification_challenge(&db_path, &challenge_id);
            return Ok(());
        }
        clear_pending_verification_challenge(&db_path, &challenge_id);
        return Err(error);
    }

    clear_verification_challenge_ready(&db_path, &dialog_key, &peer_id);
    let _ = storage.record_contact_presence_activity(&contact.contact_id, now);
    let _ = touch_runtime_chat_session_interaction(
        &db_path,
        &dialog_key,
        omni_types::ChatSessionInteractionKind::Verification,
        now,
    );

    Ok(())
}

pub(crate) fn schedule_verification_challenge_retry_for_active_transport(
    storage: &Storage,
    transport_handle: Arc<Mutex<Option<TransportManager>>>,
    session_lock: Arc<Mutex<()>>,
    contact_id: &ContactId,
    dialog_key: &str,
    peer_id: &str,
) {
    let db_path = storage.db_path().to_path_buf();
    let contact_id = contact_id.clone();
    let dialog_key = dialog_key.to_string();
    let peer_id = peer_id.to_string();

    let _ = thread::Builder::new()
        .name("omni-verification-retry".to_string())
        .spawn(move || {
            for delay_ms in VERIFICATION_CHALLENGE_RETRY_DELAYS_MS {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));

                let Ok(storage) = Storage::open(&db_path) else {
                    continue;
                };
                let Ok(contact_card) = storage.get_contact_card(&contact_id) else {
                    continue;
                };
                if contact_card.trust_state == omni_identity::TrustState::Verified {
                    return;
                }
                if contact_card.contact.relationship_state != ContactRelationshipState::Approved {
                    return;
                }
                let Ok(Some(transport)) = clone_active_transport(&transport_handle) else {
                    return;
                };

                mark_verification_challenge_ready(
                    &db_path.to_string_lossy(),
                    &dialog_key,
                    &peer_id,
                );
                let Ok(_session_guard) = lock_app_session_guard(&session_lock) else {
                    return;
                };
                let _ =
                    send_verification_challenge_for_active_transport(&storage, &transport, true);
                if !verification_challenge_is_ready(
                    &db_path.to_string_lossy(),
                    &dialog_key,
                    &peer_id,
                ) {
                    return;
                }
            }
        });
}

fn handle_incoming_verification_challenge(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    _session_lock: &Arc<Mutex<()>>,
    challenge: &omni_types::VerificationChallengeV1,
    _received_encrypted: bool,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }

    let local_account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::InvalidInput("local account is not available".to_string()))?;
    let local_instance_id = storage.get_or_create_local_instance_id()?;

    if let Some(expected_remote_instance_id) = challenge.remote_instance_id.as_deref() {
        if expected_remote_instance_id != local_instance_id {
            return Ok(());
        }
    }
    if let Some(expected_remote_account_id) = challenge.remote_account_id.as_deref() {
        if expected_remote_account_id != local_account.descriptor.account_id.0 {
            return Ok(());
        }
    }

    let mut contact = resolve_transport_contact_for_receive_context(storage, receive_context)?;

    let Some(existing) = contact.as_ref() else {
        return Ok(());
    };
    if existing.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }

    if existing.primary_dialog_key.as_deref() != Some(receive_context.dialog_key.as_str()) {
        contact = Some(storage.link_dialog_to_contact(
            &existing.contact_id,
            &receive_context.dialog_key,
            existing.primary_dialog_key.is_none(),
            current_unix_ms(),
        )?);
    }

    let local_payload = build_local_verification_payload(storage)?;
    let verification_payload_json = serde_json::to_string(&local_payload).map_err(|error| {
        OmniError::Internal(format!("failed to serialize verification payload: {error}"))
    })?;
    let local_account_id = AccountId(local_payload.account.account_id.clone());
    let local_identity_key = storage
        .get_local_identity_key(&local_account_id)?
        .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
    let challenge_bytes = verification_challenge_signing_bytes(challenge)?;
    let signature = sign_message(
        &local_identity_key.protected_signing_private_key,
        &challenge_bytes,
    )?;
    let (signature_r, signature_s) = omni_crypto::signature_to_wire(signature);
    let session = load_receive_session(storage, receive_context)?;
    let payload = omni_types::WirePayloadV1::VerificationChallengeResponse(
        omni_types::VerificationChallengeResponseV1 {
            version: omni_types::OMNI_PROTOCOL_VERSION_V1,
            challenge_id: challenge.challenge_id.clone(),
            verification_payload_json,
            signature_r,
            signature_s,
            responded_at_unix_ms: current_unix_ms(),
        },
    );
    let mut outgoing = omni_messaging::build_encrypted_wire_envelope(
        &session,
        &payload,
        SecurityPayloadKind::VerificationChallengeResponse,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(&receive_context.dialog_key, &outgoing.envelope)?;
    if contact
        .as_ref()
        .is_some_and(|value| value.relationship_state == ContactRelationshipState::Approved)
    {
        let _ = force_send_current_public_profile_sync_for_session(
            storage,
            &transport,
            &outgoing.updated_session,
            Some(&receive_context.dialog_key),
        );
    }
    if let Some(contact) = contact.or_else(|| {
        storage
            .resolve_contact_for_dialog(&receive_context.dialog_key)
            .ok()
            .flatten()
    }) {
        if contact.relationship_state == ContactRelationshipState::Approved {
            let _ =
                storage.record_contact_presence_activity(&contact.contact_id, current_unix_ms());
        }
    }
    let _ = touch_runtime_chat_session_interaction(
        &storage.db_path().to_string_lossy(),
        &receive_context.dialog_key,
        omni_types::ChatSessionInteractionKind::Verification,
        current_unix_ms(),
    );

    Ok(())
}

fn handle_incoming_verification_challenge_response(
    storage: &Storage,
    transport_handle: &Arc<Mutex<Option<TransportManager>>>,
    _session_lock: &Arc<Mutex<()>>,
    response: &omni_types::VerificationChallengeResponseV1,
    _received_encrypted: bool,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }

    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(pending) = take_pending_verification_challenge(&db_path, &response.challenge_id)
    else {
        return Ok(());
    };
    let now = current_unix_ms();
    if now > pending.expires_at_unix_ms {
        return fail_closed_verification_challenge_response(
            storage,
            transport_handle,
            &db_path,
            &pending,
            receive_context,
            "verification response arrived after the challenge expired",
        );
    }
    if pending.peer_id != receive_context.peer_id {
        return fail_closed_verification_challenge_response(
            storage,
            transport_handle,
            &db_path,
            &pending,
            receive_context,
            "verification response came from a different session peer",
        );
    }

    let payload: MyVerificationPayloadV1 =
        match serde_json::from_str(&response.verification_payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                return fail_closed_verification_challenge_response(
                    storage,
                    transport_handle,
                    &db_path,
                    &pending,
                    receive_context,
                    format!("failed to parse verification response payload: {error}"),
                );
            }
        };
    if let Err(error) = omni_protocol::validate_my_verification_payload_v1(&payload) {
        return fail_closed_verification_challenge_response(
            storage,
            transport_handle,
            &db_path,
            &pending,
            receive_context,
            format!("verification response payload was invalid: {error}"),
        );
    }

    if let Some(expected_remote_account_id) = pending.remote_account_id.as_deref() {
        let contact = match storage
            .find_contact_by_remote_account_id(&AccountId(expected_remote_account_id.to_string()))
        {
            Ok(Some(contact)) => contact,
            Ok(None) => {
                return fail_closed_verification_challenge_response(
                    storage,
                    transport_handle,
                    &db_path,
                    &pending,
                    receive_context,
                    "verification response referenced an unknown remote account",
                );
            }
            Err(error) => {
                return fail_closed_verification_challenge_response(
                    storage,
                    transport_handle,
                    &db_path,
                    &pending,
                    receive_context,
                    format!("verification response could not load the remote contact: {error}"),
                );
            }
        };
        if contact.relationship_state != ContactRelationshipState::Approved {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                "verification response arrived for a non-approved contact",
            );
        }
    }

    if let Some(expected_remote_account_id) = pending.remote_account_id.as_deref() {
        if payload.account.account_id != expected_remote_account_id {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                "verification response account_id did not match the pending challenge",
            );
        }
    }
    if let Some(expected_remote_instance_id) = pending.remote_instance_id.as_deref() {
        if expected_remote_instance_id != receive_context.remote_instance_id {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                "verification response instance_id did not match the active session",
            );
        }
    }

    let remote_bundle = match receive_context.remote_prekey_bundle.as_ref() {
        Some(bundle) => bundle,
        None => {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                "received verification response without a remote prekey bundle",
            );
        }
    };
    let remote_signing_public_key = match identity_signing_public_key_from_wire(
        &remote_bundle.account.identity_signing_public_key,
    ) {
        Ok(value) => value,
        Err(error) => {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                format!(
                    "received verification response with an invalid remote signing key: {error}"
                ),
            );
        }
    };
    let signature = match signature_from_wire(&response.signature_r, &response.signature_s) {
        Ok(value) => value,
        Err(error) => {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                format!("verification response signature could not be parsed: {error}"),
            );
        }
    };
    if let Err(error) = verify_signature(
        &remote_signing_public_key,
        &pending.challenge_bytes,
        &signature,
    ) {
        return fail_closed_verification_challenge_response(
            storage,
            transport_handle,
            &db_path,
            &pending,
            receive_context,
            format!("verification response signature could not be validated: {error}"),
        );
    }

    let contact_payload = match omni_protocol::contact_payload_from_my_payload(&payload) {
        Ok(value) => value,
        Err(error) => {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                format!("verification response payload could not be converted: {error}"),
            );
        }
    };
    let observation = observation_from_verification_payload(&contact_payload);
    let contact = match storage.add_contact(
        &AddContactInput {
            petname: payload.account.display_name.clone(),
            remote_account_id: Some(AccountId(payload.account.account_id.clone())),
            public_display_name: Some(payload.account.display_name.clone()),
            remote_instance_id: Some(receive_context.remote_instance_id.clone()),
            primary_dialog_key: Some(receive_context.dialog_key.clone()),
            initial_observation: Some(observation),
        },
        now,
    ) {
        Ok(value) => value,
        Err(error) => {
            return fail_closed_verification_challenge_response(
                storage,
                transport_handle,
                &db_path,
                &pending,
                receive_context,
                format!("verification response could not update the contact: {error}"),
            );
        }
    };
    if let Err(error) = storage.verify_contact(
        &contact.contact_id,
        VerificationMethod::ChallengeResponse,
        now,
    ) {
        return fail_closed_verification_challenge_response(
            storage,
            transport_handle,
            &db_path,
            &pending,
            receive_context,
            format!("verification response could not verify the contact: {error}"),
        );
    }
    let _ = clear_verification_challenges_for_contact(storage, &contact);
    let _ = storage.mark_contact_met(&contact.contact_id, now);

    let Some(transport) = clone_active_transport(transport_handle)? else {
        return Ok(());
    };
    process_pending_for_active_dialog(storage, &transport, &receive_context.dialog_key)?;
    let _ = touch_runtime_chat_session_interaction(
        &db_path,
        &receive_context.dialog_key,
        omni_types::ChatSessionInteractionKind::Verification,
        now,
    );

    Ok(())
}

fn mark_contact_activity_for_dialog(
    storage: &Storage,
    dialog_key: &str,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    if runtime_chat_session_auto_reconnect_paused(&db_path, dialog_key)? {
        let _ = mark_contact_offline_for_dialog(storage, dialog_key, now_unix_ms);
        return Ok(());
    }

    let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? else {
        return Ok(());
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }

    let _ = storage.record_contact_presence_activity(&contact.contact_id, now_unix_ms);
    Ok(())
}

fn mark_contact_offline_for_dialog(
    storage: &Storage,
    dialog_key: &str,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? else {
        return Ok(());
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }

    let _ = storage.record_contact_presence_offline(&contact.contact_id, now_unix_ms);
    Ok(())
}


/// How long a Complete frame waits for the other streams of the same transfer to finish delivering.
/// Generous, because the alternative to waiting is finalising a half-written file; and bounded,
/// because a sender that closed its streams early must not park this thread forever.
const ATTACHMENT_COMPLETE_DRAIN_TIMEOUT_MS: u64 = 30_000;

/// Block until this transfer's contiguous prefix covers the whole file, or the drain timeout runs
/// out. Returns either way — a timeout is NOT silently fatal here, because the completion path
/// downstream already refuses to mark a download verified unless the running hash covered every
/// byte, so an incomplete transfer fails there with a real reason instead of here with a guess.
fn wait_for_attachment_receive_completion(db_path: &str, transfer_id: &str) {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(ATTACHMENT_COMPLETE_DRAIN_TIMEOUT_MS);
    loop {
        // Not complete (`None | Some(false)`) -> keep waiting.
        // No cached writer yet means NOT ONE CHUNK has been persisted, which is the strongest
            // possible reason to keep waiting -- not, as this first read it, a reason to proceed.
            // Getting that backwards cost a live run: with the chunks on their own streams, Complete
            // was handled before any chunk, this returned immediately, completion finalised the
            // transfer, and the chunks that arrived a moment later were rejected with "transfer not
            // found". Sender-side ordering (all writers joined before Complete) does NOT give
            // receiver-side ordering across independent streams -- that independence is the whole
        // point of using them.
        if let Some(true) = attachment_receive_is_complete(db_path, transfer_id) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            omni_app_trace_log(
                "attachment/complete",
                &format!(
                    "transfer={transfer_id} still incomplete after {ATTACHMENT_COMPLETE_DRAIN_TIMEOUT_MS}ms of drain; finalising anyway (verification decides)"
                ),
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Что открылось — для `recv/accepted`: вид события и id сообщения, если он есть. Только имена
/// и id, без содержимого.
fn decrypted_payload_trace_label(payload: &omni_messaging::DecryptedWirePayload) -> String {
    use omni_types::LogicalEventPayloadV1 as Event;
    let omni_messaging::DecryptedWirePayload::Payload(payload) = payload else {
        return "kind=unrecognized".to_string();
    };
    let event = match payload {
        omni_types::WirePayloadV1::LogicalEvent(event) => &event.payload,
        omni_types::WirePayloadV1::ChatText(text) => {
            return format!("kind=chat_text message={}", text.message_id);
        }
        _ => return "kind=wire".to_string(),
    };
    match event {
        Event::ChatText(text) => format!("kind=chat_text message={}", text.message_id),
        Event::DeliveryReceipt(receipt) => format!("kind=receipt message={}", receipt.message_id),
        Event::AttachmentOffer(offer) => format!("kind=attachment_offer message={}", offer.message_id),
        Event::SentTranscript(sent) => format!("kind=sent_transcript message={}", sent.message_id),
        Event::ContactPresencePing(_) => "kind=presence_ping".to_string(),
        Event::ContactPresencePong(_) => "kind=presence_pong".to_string(),
        Event::ContactPresence(_) => "kind=presence".to_string(),
        Event::GroupControl(_) => "kind=group_control".to_string(),
        _ => "kind=event".to_string(),
    }
}
