const CALL_RECONNECT_GRACE_MS: i64 = 10_000;

/// How long the peer may say nothing before a live call is treated as reconnecting.
///
/// Presence ping/pong runs on the dialog roughly every 10 s for the whole call (visible
/// as `ping/entry` / `pong/entry` in the trace) and every inbound logical event refreshes
/// the contact's activity stamp, so two missed rounds really is silence.
const CALL_PEER_SILENCE_MS: i64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallLinkHealth {
    /// We have heard from the peer recently enough to believe the call is carrying.
    Responsive,
    /// Nothing from the peer for [`CALL_PEER_SILENCE_MS`].
    Silent,
}

/// What a tick should do to a call, given how the link looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallLinkTransition {
    Keep,
    /// Peer went quiet: hold the call open until `deadline_unix_ms`, then give up.
    StartReconnecting { deadline_unix_ms: i64 },
    /// Peer came back.
    Recovered,
    /// The reconnect window expired.
    EndConnectionLost,
}

/// Is the peer still answering?
///
/// Deliberately based on OBSERVED PEER ACTIVITY rather than on a transport record. The
/// previous signal was `TransportManager::connection_snapshot_for_dialog`, which only
/// knows about the LEGACY transport's verified connections — and the traffic has ridden
/// the mesh overlay since the legacy path was removed. So it answered "no connection" for
/// a perfectly healthy mesh call (the caller sat in `reconnecting` while audio flowed) and
/// "connection fine" for a peer whose app had exited (the callee kept an `active` call
/// forever). Both were seen in one live pass, 2026-08-11, on the two ends of the SAME
/// call. Activity is transport-agnostic: mesh, legacy or relay all refresh it.
///
/// Unknown counts as responsive: a missing presence row must never be able to kill a call
/// that is working.
fn call_link_health(last_activity_at_unix_ms: Option<i64>, now_unix_ms: i64) -> CallLinkHealth {
    match last_activity_at_unix_ms {
        Some(at) if now_unix_ms.saturating_sub(at) > CALL_PEER_SILENCE_MS => CallLinkHealth::Silent,
        _ => CallLinkHealth::Responsive,
    }
}

fn call_link_transition(
    state: CallState,
    health: CallLinkHealth,
    reconnect_deadline_unix_ms: Option<i64>,
    now_unix_ms: i64,
) -> CallLinkTransition {
    match (state, health) {
        (CallState::Active | CallState::Connecting, CallLinkHealth::Silent) => {
            CallLinkTransition::StartReconnecting {
                deadline_unix_ms: now_unix_ms + CALL_RECONNECT_GRACE_MS,
            }
        }
        (CallState::Reconnecting, CallLinkHealth::Responsive) => CallLinkTransition::Recovered,
        (CallState::Reconnecting, CallLinkHealth::Silent)
            if reconnect_deadline_unix_ms.is_some_and(|deadline| now_unix_ms >= deadline) =>
        {
            CallLinkTransition::EndConnectionLost
        }
        _ => CallLinkTransition::Keep,
    }
}

fn call_peer_last_activity(storage: &Storage, dialog_key: &str) -> Option<i64> {
    storage
        .resolve_contact_for_dialog(dialog_key)
        .ok()
        .flatten()
        .and_then(|contact| contact.presence_last_activity_at_unix_ms)
}

/// Why every public entry point below takes the app session guard.
///
/// Call signaling rides the same Double Ratchet session as chat, presence and the
/// media capability publishes — and `send_call_event_for_session_address` is a
/// read-modify-write of that session (load record → encrypt → store advanced
/// record). Every other sender in the crate brackets that with
/// `lock_app_session_guard`; this file did not. Two senders racing therefore both
/// encrypted at the SAME message number and the loser's envelope was dropped by the
/// peer's replay window — silently, and the sender saw a successful send. A hangup
/// lost that way leaves the other side sitting in a call that has already ended.
///
/// The guard belongs HERE and not in `send_call_event_for_session_address` or
/// `tick_call_runtime`: those are also reached from the inbound dispatch, which
/// already holds the guard, and `std::sync::Mutex` is not reentrant.
/// Состоялся ли звонок, судя по исходу отправки приглашения.
///
/// Для СООБЩЕНИЯ `DurableOnly` («положили в durable до реконсиляции») — правильный успех: текст
/// обязан долежать до появления собеседника. Для ЗВОНКА — нет. Звонок это realtime: приглашение,
/// доставленное через минуту, звонить уже не должно, а до тех пор звонящий смотрел бы на
/// исходящий вызов, которого на другом конце не существует.
///
/// Живьём 2026-08-31: `call/out ... queued: no live path, held for later`, и больше про этот
/// call_id в трассе НЕТ НИ ОДНОЙ строки — линк встал через 84 секунды, приглашение так и не
/// уехало. Разбор: docs/bugs/a-call-nobody-could-hear-was-reported-as-placed.md.
///
/// Отдельной функцией, потому что это ПРАВИЛО, а не деталь отправки: его надо было уметь
/// проверить, а два исхода отправки до этого показывались как один.
fn call_was_placed(delivery: WireEnvelopeDelivery) -> bool {
    match delivery {
        WireEnvelopeDelivery::Live => true,
        // «Труба есть, но плечо доказано мёртвым» — для звонка это НЕ доставка. Первая версия
        // этого правила ловила только `DurableOnly`, то есть случай «пути нет вовсе», и на живой
        // паре 2026-08-31 пропустила ровно тот исход, ради которого писалась: контакт показан
        // как `will_deliver_later`, `call.place` вернул `ok`, второй конец не зазвонил. Оба
        // состояния приходили сюда одним значением, различить их было нечем.
        WireEnvelopeDelivery::LiveUnconfirmed => false,
        WireEnvelopeDelivery::DurableOnly => false,
    }
}

/// Почему звонок не состоялся — словами, различающими два разных ремонта: «пути нет вовсе» и
/// «труба есть, но пир перестал отвечать». Отдельной функцией по той же причине, что и правило
/// выше: чтобы это можно было проверить, а не только прочитать.
fn refusal_for_undelivered_invite(delivery: WireEnvelopeDelivery) -> String {
    match delivery {
        WireEnvelopeDelivery::LiveUnconfirmed => {
            "the peer stopped answering: the call was not placed"
        }
        _ => "no live path to the peer: the call was not placed",
    }
    .to_string()
}

fn trace_call_out(
    call_id: &str,
    payload_kind: SecurityPayloadKind,
    dialog_key: &str,
    result: &OmniResult<WireEnvelopeDelivery>,
) {
    // «sent» тут означало и «байты ушли», и «положено в durable до реконсиляции». Живой разбор
    // 2026-08-29 читал трассу и видел `CallAccept ... sent` там, где согласие никуда не уехало.
    let outcome = match result {
        Ok(WireEnvelopeDelivery::Live) => "sent".to_string(),
        Ok(WireEnvelopeDelivery::LiveUnconfirmed) => {
            "sent into a live pipe, but the outbound leg is proven dead".to_string()
        }
        Ok(WireEnvelopeDelivery::DurableOnly) => "queued: no live path, held for later".to_string(),
        Err(error) => format!("FAILED: {error}"),
    };
    omni_app_trace_log(
        "call/out",
        &format!("{payload_kind:?} call_id={call_id} dialog_key={dialog_key} {outcome}"),
    );
}

/// What an inbound call event actually did. `applied` vs `ignored` is the difference
/// between "the peer's hangup ended our call" and "it arrived and nothing happened",
/// and until this existed the trace could not tell those apart — nor either of them
/// from "it never arrived".
fn trace_call_in(event: &str, call_id: &str, decision: &str) {
    omni_app_trace_log(
        "call/in",
        &format!("{event} call_id={call_id} {decision}"),
    );
}

#[derive(Debug, Clone)]
struct ActiveCallRuntime {
    summary: CallSummary,
    remote_account_id: AccountId,
    remote_device_id: DeviceId,
    invite_expires_at_unix_ms: i64,
    /// An outgoing call rings every device of the contact (docs/decisions/multi-device-equal-nodes.md,
    /// 2.5): these are the devices invited. The first to accept becomes the call's device —
    /// `remote_device_id` and `summary.dialog_key` move to it — and the rest are cancelled. Empty for
    /// an incoming call, and for a contact whose devices are not known yet (one device, as before).
    invited_devices: Vec<CallDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallDevice {
    device_id: DeviceId,
    /// The device's own dialog — where its call events and media go.
    dialog_key: String,
}

/// The devices of the contact behind `dialog_key` that a call can ring: each with an established
/// session, on its own dialog.
fn callable_devices_of_contact(
    storage: &Storage,
    contact_id: &ContactId,
    remote_account_id: &AccountId,
) -> OmniResult<Vec<CallDevice>> {
    let mut devices = Vec::new();
    for link in storage.list_contact_device_dialogs(contact_id)? {
        let Some(device_id) = link.remote_device_id else {
            continue;
        };
        if devices.iter().any(|device: &CallDevice| device.device_id == device_id) {
            continue;
        }
        let established = storage
            .get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: remote_account_id.clone(),
                remote_device_id: device_id.clone(),
            })?
            .is_some_and(|session| {
                session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
            });
        if established {
            devices.push(CallDevice {
                device_id,
                dialog_key: link.dialog_key,
            });
        }
    }
    Ok(devices)
}

#[derive(Default, Debug, Clone)]
struct CallRuntimeState {
    current_call: Option<ActiveCallRuntime>,
}

/// Revision of "something about a call changed", for waking the shell (`wait_for_call_change`).
///
/// The shell used to learn about calls only by polling: every 2 s with no call, every 3 s with the
/// window hidden. An unattended remote-control session is accepted by THIS core in ~0.1 s, and then
/// sat up to 3 s until the shell noticed it had media to start (live 2026-10-06: ~1.1 s). Process-
/// wide on purpose: it carries no profile state, only "look now", and a spurious wake is harmless.
static CALL_CHANGES: (Mutex<u64>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

fn upsert_call_and_wake_shell(storage: &Storage, summary: &CallSummary) -> OmniResult<()> {
    storage.upsert_call_session(summary)?;
    if let Ok(mut revision) = CALL_CHANGES.0.lock() {
        *revision = revision.wrapping_add(1);
        CALL_CHANGES.1.notify_all();
    }
    Ok(())
}

/// Block until a call changed after revision `since`, or `timeout` passes. Returns the current
/// revision (pass it back as `since` next time).
pub fn wait_for_call_change(since: u64, timeout: std::time::Duration) -> u64 {
    let Ok(guard) = CALL_CHANGES.0.lock() else {
        return since;
    };
    match CALL_CHANGES
        .1
        .wait_timeout_while(guard, timeout, |revision| *revision == since)
    {
        Ok((revision, _)) => *revision,
        Err(_) => since,
    }
}

static CALL_RUNTIME_REGISTRY: std::sync::OnceLock<Mutex<HashMap<String, CallRuntimeState>>> =
    std::sync::OnceLock::new();

impl OmniApp {
    pub fn decline_call(
        &self,
        call_id: impl AsRef<str>,
        reason: CallDeclineReason,
    ) -> OmniResult<CallSummary> {
        let _ = self.ensure_runtime_profile_active("decline_call")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        tick_call_runtime(&self.storage, Some(&self.transport))?;
        let now_unix_ms = current_unix_ms();
        let db_path = self.storage.db_path().to_string_lossy().to_string();

        let runtime = current_call_runtime(&db_path)?.ok_or_else(|| {
            OmniError::InvalidInput("there is no active call to decline".to_string())
        })?;
        if runtime.summary.call_id != call_id.as_ref() {
            return Err(OmniError::InvalidInput(
                "requested call does not match the current call".to_string(),
            ));
        }

        let ended = end_runtime_call(
            &self.storage,
            &db_path,
            CallOutcome::Declined,
            Some(reason),
            now_unix_ms,
        )?;
        let session_address = omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id,
            remote_device_id: runtime.remote_device_id,
        };
        let decline = omni_types::LogicalEventEnvelopeV1::call_decline(
            ended.call_id.clone(),
            reason,
            now_unix_ms,
        );
        send_call_event_for_session_address(
            &self.storage,
            &self.transport,
            &ended.dialog_key,
            &session_address,
            &decline,
            SecurityPayloadKind::CallDecline,
        )?;
        Ok(ended)
    }

    pub fn hang_up_call(&self, call_id: impl AsRef<str>) -> OmniResult<CallSummary> {
        let _ = self.ensure_runtime_profile_active("hang_up_call")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        tick_call_runtime(&self.storage, Some(&self.transport))?;
        let now_unix_ms = current_unix_ms();
        let db_path = self.storage.db_path().to_string_lossy().to_string();

        let runtime = current_call_runtime(&db_path)?.ok_or_else(|| {
            OmniError::InvalidInput("there is no active call to hang up".to_string())
        })?;
        if runtime.summary.call_id != call_id.as_ref() {
            return Err(OmniError::InvalidInput(
                "requested call does not match the current call".to_string(),
            ));
        }

        let (outcome, event, payload_kind) = if runtime.summary.connected_at_unix_ms.is_some() {
            (
                CallOutcome::Completed,
                omni_types::LogicalEventEnvelopeV1::call_hangup(
                    runtime.summary.call_id.clone(),
                    now_unix_ms,
                ),
                SecurityPayloadKind::CallHangup,
            )
        } else {
            (
                CallOutcome::Cancelled,
                omni_types::LogicalEventEnvelopeV1::call_cancel(
                    runtime.summary.call_id.clone(),
                    now_unix_ms,
                ),
                SecurityPayloadKind::CallCancel,
            )
        };
        let ended = end_runtime_call(&self.storage, &db_path, outcome, None, now_unix_ms)?;
        let session_address = omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id.clone(),
            remote_device_id: runtime.remote_device_id.clone(),
        };
        send_call_event_for_session_address(
            &self.storage,
            &self.transport,
            &ended.dialog_key,
            &session_address,
            &event,
            payload_kind,
        )?;
        // Nobody answered yet: the other devices are still ringing.
        if runtime.summary.connected_at_unix_ms.is_none() {
            cancel_other_invited_devices(
                &self.storage,
                &self.transport,
                &runtime,
                &runtime.remote_device_id,
                now_unix_ms,
            );
        }
        Ok(ended)
    }

    /// Place an outgoing call on a dialog: create the local outgoing call-lifecycle
    /// state and send a `CallInvite` to the peer (over the active mesh transport). The
    /// realtime-media pipeline is started separately by the shell; this is the call
    /// *signaling* so both peers surface a `CallSummary` (the callee rings).
    pub fn place_call(&self, dialog_key: impl AsRef<str>) -> OmniResult<CallSummary> {
        self.place_call_of_kind(dialog_key.as_ref(), false)
    }

    /// `remote_control`: the call is a remote-control session (no microphone, no camera) asking
    /// the callee's core for control of its computer — see app_remote_access.rs.
    fn place_call_of_kind(&self, dialog_key: &str, remote_control: bool) -> OmniResult<CallSummary> {
        let _ = self.ensure_runtime_profile_active("place_call")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        tick_call_runtime(&self.storage, Some(&self.transport))?;
        let now_unix_ms = current_unix_ms();
        let db_path = self.storage.db_path().to_string_lossy().to_string();

        if let Some(existing) = current_call_runtime(&db_path)? {
            // One call per profile. Re-placing the same dialog is idempotent.
            if existing.summary.dialog_key == dialog_key
                && existing.summary.remote_control.is_some() == remote_control
            {
                return Ok(existing.summary);
            }
            if remote_control && existing.summary.dialog_key == dialog_key {
                // A call with this contact is already running: control goes through it (the peer
                // shows its screen and allows control in the call), never through a second call.
                return Err(OmniError::InvalidInput(
                    "a call with this contact is already running: ask them to share the screen and allow control in the call".to_string(),
                ));
            }
            return Err(OmniError::InvalidInput(
                "another call is already in progress".to_string(),
            ));
        }

        let call_id = format!("call-out-{now_unix_ms}");
        let runtime = build_call_runtime_for_dialog(
            &self.storage,
            dialog_key,
            call_id.clone(),
            CallDirection::Outgoing,
            CallState::Connecting,
            now_unix_ms,
        )?;
        let mut runtime = runtime;
        if remote_control {
            runtime.summary.remote_control = Some(omni_types::RemoteControlSession {
                role: omni_types::RemoteControlRole::Controller,
                phase: omni_types::RemoteControlPhase::AwaitingOwner,
                answer_deadline_unix_ms: None,
            });
            runtime.invite_expires_at_unix_ms = now_unix_ms + REMOTE_CONTROL_INVITE_TTL_MS;
        }
        upsert_call_and_wake_shell(&self.storage, &runtime.summary)?;

        let invite_participants = vec![omni_types::CallParticipantDescriptorV1 {
            participant_id: runtime.remote_account_id.0.clone(),
            account_id: runtime.remote_account_id.0.clone(),
            device_id: runtime.remote_device_id.0.clone(),
        }];
        let invite = if remote_control {
            omni_types::LogicalEventEnvelopeV1::remote_control_invite(
                call_id,
                dialog_key,
                invite_participants,
                now_unix_ms,
                now_unix_ms + REMOTE_CONTROL_INVITE_TTL_MS,
            )
        } else {
            omni_types::LogicalEventEnvelopeV1::call_invite(
                call_id,
                dialog_key,
                invite_participants,
                now_unix_ms,
                now_unix_ms + 60_000,
            )
        };
        let devices = match runtime.summary.contact_id.as_deref() {
            Some(contact_id) => callable_devices_of_contact(
                &self.storage,
                &ContactId(contact_id.to_string()),
                &runtime.remote_account_id,
            )?,
            None => Vec::new(),
        };
        let targets: Vec<CallDevice> = if devices.len() > 1 {
            runtime.invited_devices = devices.clone();
            devices
        } else {
            vec![CallDevice {
                device_id: runtime.remote_device_id.clone(),
                dialog_key: dialog_key.to_string(),
            }]
        };
        let summary = runtime.summary.clone();
        let remote_account_id = runtime.remote_account_id.clone();
        set_current_call_runtime(&db_path, Some(runtime))?;
        // Every device rings; the call is placed if the invite reached any of them.
        let mut delivery = None;
        for target in &targets {
            let sent = send_call_event_for_session_address(
                &self.storage,
                &self.transport,
                &target.dialog_key,
                &omni_crypto::SessionAddress {
                    remote_account_id: remote_account_id.clone(),
                    remote_device_id: target.device_id.clone(),
                },
                &invite,
                SecurityPayloadKind::CallInvite,
            );
            match sent {
                Ok(this) if call_was_placed(this) => delivery = Some(this),
                Ok(this) => {
                    delivery.get_or_insert(this);
                }
                Err(error) if targets.len() == 1 => return Err(error),
                Err(_) => {}
            }
        }
        let delivery = delivery.ok_or_else(|| {
            OmniError::Transport("the call invite reached none of the contact's devices".to_string())
        })?;
        if !call_was_placed(delivery) {
            // Для СООБЩЕНИЯ «положили в durable до реконсиляции» — правильный успех: текст обязан
            // долежать. Для ЗВОНКА — нет. Звонок это realtime: приглашение, доставленное через
            // минуту, звонить уже не должно, а до тех пор звонящий смотрел бы на исходящий вызов,
            // которого на другом конце не существует.
            //
            // Живьём 2026-08-31 это выглядело так: `call/out ... queued: no live path, held for
            // later`, и больше про этот call_id в трассе НЕТ НИ ОДНОЙ строки — линк встал через
            // 84 секунды, приглашение так и не уехало, а человек всё это время видел «звоню».
            // Разбор: docs/bugs/a-call-nobody-could-hear-was-reported-as-placed.md.
            set_current_call_runtime(&db_path, None)?;
            // Причина названа, а не свёрнута в одну фразу: «пути нет вовсе» и «труба есть, но
            // пир перестал отвечать» человек чинит по-разному, а до этого оба показывались
            // одинаково — и, пока различия не было, второй случай вообще проходил как успех.
            return Err(OmniError::Transport(refusal_for_undelivered_invite(delivery)));
        }
        Ok(summary)
    }

    /// Accept the current incoming (ringing) call: promote it to `Active` and send a
    /// `CallAccept` to the caller so their side goes active too.
    pub fn accept_call(&self, call_id: impl AsRef<str>) -> OmniResult<CallSummary> {
        let _ = self.ensure_runtime_profile_active("accept_call")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        tick_call_runtime(&self.storage, Some(&self.transport))?;
        let now_unix_ms = current_unix_ms();
        let db_path = self.storage.db_path().to_string_lossy().to_string();

        let mut runtime = current_call_runtime(&db_path)?.ok_or_else(|| {
            OmniError::InvalidInput("there is no incoming call to accept".to_string())
        })?;
        if runtime.summary.call_id != call_id.as_ref() {
            return Err(OmniError::InvalidInput(
                "requested call does not match the current call".to_string(),
            ));
        }
        if runtime.summary.direction != CallDirection::Incoming {
            return Err(OmniError::InvalidInput(
                "only an incoming call can be accepted".to_string(),
            ));
        }

        let session_address = omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id.clone(),
            remote_device_id: runtime.remote_device_id.clone(),
        };
        let accept = omni_types::LogicalEventEnvelopeV1::call_accept(
            runtime.summary.call_id.clone(),
            now_unix_ms,
        );
        let dialog_key = runtime.summary.dialog_key.clone();

        // СНАЧАЛА ДОСТАВИТЬ СОГЛАСИЕ, ПОТОМ ОБЪЯВЛЯТЬ ЗВОНОК АКТИВНЫМ.
        //
        // Раньше состояние переводилось в `Active` и записывалось ДО отправки, а отправка
        // возвращала `Ok(())` и тогда, когда живого пути не было и конверт лёг в durable
        // «доставлю позже». Для звонка «позже» не бывает: вызывающий отваливается по таймауту, а
        // принимающий остаётся в активном звонке С ОТКРЫТЫМ МИКРОФОНОМ и собеседником, которого
        // там уже нет. Ровно это снято живьём 2026-08-29, трижды подряд —
        // docs/bugs/callee-stays-in-a-call-that-ended.md.
        //
        // Отказ оставляет звонок ЗВОНЯЩИМ: состояние мы не трогали, человек может нажать ещё раз,
        // а если пути так и нет — звонок закончится честным таймаутом у обоих, а не фантомом у
        // одного.
        let delivery = send_call_event_for_session_address(
            &self.storage,
            &self.transport,
            &dialog_key,
            &session_address,
            &accept,
            SecurityPayloadKind::CallAccept,
        )?;
        if delivery != WireEnvelopeDelivery::Live {
            return Err(OmniError::Transport(
                "the caller is not reachable right now: the acceptance could not be delivered"
                    .to_string(),
            ));
        }

        runtime.summary.state = CallState::Active;
        if runtime.summary.connected_at_unix_ms.is_none() {
            runtime.summary.connected_at_unix_ms = Some(now_unix_ms);
        }
        // Answering a remote-control request IS the owner's "allow this time".
        approve_remote_control(&self.storage, &mut runtime.summary, now_unix_ms);
        runtime.summary.network_quality = Some(CallNetworkQuality::Good);
        runtime.summary.reconnect_deadline_unix_ms = None;
        runtime.summary.updated_at_unix_ms = now_unix_ms;
        upsert_call_and_wake_shell(&self.storage, &runtime.summary)?;
        let summary = runtime.summary.clone();
        set_current_call_runtime(&db_path, Some(runtime))?;
        Ok(summary)
    }

    pub fn set_local_mic_muted(
        &self,
        call_id: impl AsRef<str>,
        is_muted: bool,
    ) -> OmniResult<CallSummary> {
        let _ = self.ensure_runtime_profile_active("set_local_mic_muted")?;
        let now_unix_ms = current_unix_ms();
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mut runtime = current_call_runtime(&db_path)?.ok_or_else(|| {
            OmniError::InvalidInput("there is no active call to update".to_string())
        })?;
        if runtime.summary.call_id != call_id.as_ref() {
            return Err(OmniError::InvalidInput(
                "requested call does not match the current call".to_string(),
            ));
        }

        runtime.summary.local_mic_muted = is_muted;
        runtime.summary.updated_at_unix_ms = now_unix_ms;
        if let Some(local_participant) = runtime
            .summary
            .participants
            .iter_mut()
            .find(|participant| participant.is_local)
        {
            local_participant.is_muted = is_muted;
        }
        upsert_call_and_wake_shell(&self.storage, &runtime.summary)?;
        set_current_call_runtime(&db_path, Some(runtime.clone()))?;
        Ok(runtime.summary)
    }

    pub fn get_call_inbox_snapshot(&self) -> OmniResult<CallInboxSnapshot> {
        let _ = self.ensure_runtime_profile_active("get_call_inbox_snapshot")?;
        // The UI polls this, and the tick below can send a CallTimeout — so it is a
        // sender like the others and takes the guard too.
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        tick_call_runtime(&self.storage, Some(&self.transport))?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if let Some(runtime) = current_call_runtime(&db_path)? {
            return Ok(CallInboxSnapshot {
                current_call: Some(runtime.summary),
                incoming_calls: Vec::new(),
                updated_at_unix_ms: current_unix_ms(),
            });
        }
        // This process's live registry has nothing for this profile: any non-`ended` row left
        // in storage is an ORPHAN from a process that was killed/crashed mid-call before it could
        // persist `state='ended'` (the only writer of that column). Left alone, the query below
        // would resurrect it as an apparently-live call every time — one the user can never end,
        // since hang_up_call/decline_call only match against the live registry we just found
        // empty. Finalize it here (storage-only, no wire event) instead of showing it.
        self.storage.finalize_orphaned_call_sessions(current_unix_ms())?;
        self.storage.get_call_inbox_snapshot()
    }

}

fn lock_call_runtime_registry(
) -> OmniResult<std::sync::MutexGuard<'static, HashMap<String, CallRuntimeState>>> {
    CALL_RUNTIME_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| OmniError::Internal("call runtime registry lock is poisoned".to_string()))
}

fn current_call_runtime(db_path: &str) -> OmniResult<Option<ActiveCallRuntime>> {
    Ok(lock_call_runtime_registry()?
        .get(db_path)
        .and_then(|entry| entry.current_call.clone()))
}

fn set_current_call_runtime(
    db_path: &str,
    runtime: Option<ActiveCallRuntime>,
) -> OmniResult<()> {
    let mut registry = lock_call_runtime_registry()?;
    match runtime {
        Some(runtime) => {
            registry
                .entry(db_path.to_string())
                .or_insert_with(CallRuntimeState::default)
                .current_call = Some(runtime);
        }
        None => {
            registry.remove(db_path);
        }
    }
    Ok(())
}

pub(crate) fn clear_call_runtime_for_profile(db_path: &str) {
    if let Ok(mut registry) = lock_call_runtime_registry() {
        registry.remove(db_path);
    }
}

/// Whether this profile has a call occupying the link right now.
///
/// Asked by the attachment sender, which yields to it. Voice and video ride QUIC datagrams on the
/// SAME connection as a bulk transfer, under the same congestion controller, so a file we are
/// sending queues our own call behind it — measured on the mobile stand as RTT under load going
/// from tens of milliseconds to over a second.
///
/// Every non-`Ended` state counts, ringing included. A call that is still ringing has no media yet,
/// but its invite and answer are exactly the small messages that must not wait behind a file, and
/// ringing is seconds long — there is nothing to gain by being clever here.
///
/// A poisoned registry reads as "no call": the sender must never be blocked by the state of a lock
/// it only consults as a hint.
pub(crate) fn call_occupies_link_for_profile(db_path: &str) -> bool {
    lock_call_runtime_registry()
        .ok()
        .and_then(|registry| {
            registry
                .get(db_path)
                .map(|entry| match entry.current_call.as_ref() {
                    Some(call) => call.summary.state != omni_types::CallState::Ended,
                    None => false,
                })
        })
        .unwrap_or(false)
}

/// Build a fresh call runtime for a dialog by resolving the peer (contact card +
/// latest session) the same way the realtime-media send path does. Used by both the
/// outgoing `place_call` and the incoming-invite handler.
/// Сколько звонок обязан простоять активным, прежде чем чужой пинг получает право его закрыть.
///
/// Окно нужно против гонки установления: собеседник мог ещё не узнать о звонке, который у нас уже
/// активен, и его пинг честно скажет «звонка нет». Presence ходит примерно раз в 10 с, поэтому 15 с
/// — это гарантированно ПОСЛЕ того, как он должен был узнать.
const CALL_PRESENCE_RECONCILE_GRACE_MS: i64 = 15_000;

/// Надо ли завершить СВОЙ звонок, узнав из пинга, что думает о нём собеседник.
///
/// Зачем это вообще: управляющее сообщение звонка может пропасть, и тогда одна сторона остаётся в
/// звонке, которого нет, С ОТКРЫТЫМ МИКРОФОНОМ. Живой прогон 2026-08-30 это снял: отбой ушёл по
/// ЖИВОМУ пути, `call/in end` у собеседника не появилось, и он висел активным, пока звонок не
/// завершили руками. Механизм тишины там не срабатывает по построению — линк живой, presence ходит,
/// собеседник «на месте», просто молчит. См. docs/bugs/a-lost-hangup-leaves-a-phantom-call.md.
///
/// Функция намеренно ЧИСТАЯ: порвать живой звонок здесь легче, чем починить фантомный, поэтому
/// каждое условие проверяется тестом, а не рассуждением.
///
/// Правила, и все — запрещающие:
///   * собеседник НЕ СКАЗАЛ (старая сборка) — не трогаем ничего. Отсутствие поля обязано означать
///     «не знаю», иначе новая сторона рвала бы живые звонки со старой по каждому пингу;
///   * звонок не наш по диалогу — не трогаем: пинг от одного контакта не смеет закрывать звонок с
///     другим;
///   * звонок не `Active` — не трогаем: звонящий/устанавливающийся ещё имеет право быть неизвестным;
///   * звонок активен меньше льготного окна — не трогаем;
///   * собеседник назвал ТОТ ЖЕ звонок — не трогаем.
///
/// И только когда всё это пройдено: он говорит «нет звонка» или называет ДРУГОЙ — наш кончился.
fn should_end_call_after_peer_presence(
    my_call: Option<&CallSummary>,
    from_dialog_key: &str,
    peer_call_state: Option<&omni_types::PresenceCallStateV1>,
    now_unix_ms: i64,
) -> bool {
    let Some(peer) = peer_call_state else {
        return false;
    };
    let Some(call) = my_call else {
        return false;
    };
    if normalize_dialog_key(&call.dialog_key) != normalize_dialog_key(from_dialog_key) {
        return false;
    }
    if call.state != CallState::Active {
        return false;
    }
    let Some(connected_at_unix_ms) = call.connected_at_unix_ms else {
        return false;
    };
    if now_unix_ms.saturating_sub(connected_at_unix_ms) < CALL_PRESENCE_RECONCILE_GRACE_MS {
        return false;
    }
    peer.active_call_id.as_deref() != Some(call.call_id.as_str())
}

/// Состояние СВОЕГО звонка с этим диалогом — то, что уезжает в presence-пинге.
fn presence_call_state_for_dialog(
    db_path: &str,
    dialog_key: &str,
) -> Option<omni_types::PresenceCallStateV1> {
    let active_call_id = current_call_runtime(db_path)
        .ok()
        .flatten()
        .filter(|runtime| {
            normalize_dialog_key(&runtime.summary.dialog_key) == normalize_dialog_key(dialog_key)
                && runtime.summary.state == CallState::Active
        })
        .map(|runtime| runtime.summary.call_id);
    Some(omni_types::PresenceCallStateV1 { active_call_id })
}

/// Свести своё состояние звонка с тем, что сказал собеседник в пинге. Завершает СВОЙ звонок, если
/// собеседник о нём не знает — см. [`should_end_call_after_peer_presence`].
fn reconcile_call_with_peer_presence(
    storage: &Storage,
    dialog_key: &str,
    peer_call_state: Option<&omni_types::PresenceCallStateV1>,
) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Ok(Some(runtime)) = current_call_runtime(&db_path) else {
        return;
    };
    let now_unix_ms = current_unix_ms();
    if !should_end_call_after_peer_presence(
        Some(&runtime.summary),
        dialog_key,
        peer_call_state,
        now_unix_ms,
    ) {
        return;
    }
    omni_app_trace_log(
        "call/reconcile",
        &format!(
            "call_id={} dialog_key={dialog_key} peer_says={:?} — собеседник не знает о звонке, завершаем",
            runtime.summary.call_id,
            peer_call_state.and_then(|state| state.active_call_id.clone()),
        ),
    );
    let _ = end_runtime_call(
        storage,
        &db_path,
        CallOutcome::Completed,
        None,
        now_unix_ms,
    );
}

fn build_call_runtime_for_dialog(
    storage: &Storage,
    dialog_key: &str,
    call_id: String,
    direction: CallDirection,
    state: CallState,
    now_unix_ms: i64,
) -> OmniResult<ActiveCallRuntime> {
    let card = storage
        .resolve_contact_card_for_dialog(dialog_key)?
        .ok_or_else(|| {
            OmniError::InvalidInput("a call requires a known contact for the dialog".to_string())
        })?;
    let remote_account_id = card.contact.remote_account_id.clone().ok_or_else(|| {
        OmniError::InvalidInput("a call requires a contact with a remote account id".to_string())
    })?;
    // The device behind this very dialog when we know it — a contact can have several, and the call
    // talks to one of them; otherwise the account's latest session, as with one device.
    let dialog_device = storage
        .list_contact_device_dialogs(&card.contact.contact_id)?
        .into_iter()
        .find(|link| normalize_dialog_key(&link.dialog_key) == normalize_dialog_key(dialog_key))
        .and_then(|link| link.remote_device_id);
    let remote_device_id = match dialog_device {
        Some(device_id) => device_id,
        None => storage
            .get_latest_session_for_remote_account(&remote_account_id)?
            .ok_or_else(|| {
                OmniError::Transport(
                    "no established session is available for the call peer".to_string(),
                )
            })?
            .session_address
            .remote_device_id,
    };
    let display_name = {
        let trimmed = card.contact.petname.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    let participants = vec![omni_types::CallParticipantSummary {
        participant_id: remote_account_id.0.clone(),
        account_id: remote_account_id.0.clone(),
        device_id: remote_device_id.0.clone(),
        display_name,
        is_local: false,
        is_muted: false,
    }];
    let ringing = matches!(state, CallState::Ringing);
    let active = matches!(state, CallState::Active);
    Ok(ActiveCallRuntime {
        summary: CallSummary {
            call_id,
            dialog_key: dialog_key.to_string(),
            contact_id: Some(card.contact.contact_id.0.clone()),
            direction,
            state,
            outcome: None,
            decline_reason: None,
            participants,
            local_mic_muted: false,
            network_quality: Some(CallNetworkQuality::Good),
            started_at_unix_ms: now_unix_ms,
            ringing_started_at_unix_ms: ringing.then_some(now_unix_ms),
            connected_at_unix_ms: active.then_some(now_unix_ms),
            ended_at_unix_ms: None,
            reconnect_deadline_unix_ms: None,
            updated_at_unix_ms: now_unix_ms,
            remote_control: None,
        },
        remote_account_id,
        remote_device_id,
        invite_expires_at_unix_ms: now_unix_ms + 60_000,
        invited_devices: Vec::new(),
    })
}

fn end_runtime_call(
    storage: &Storage,
    db_path: &str,
    outcome: CallOutcome,
    decline_reason: Option<CallDeclineReason>,
    ended_at_unix_ms: i64,
) -> OmniResult<CallSummary> {
    let mut runtime = current_call_runtime(db_path)?.ok_or_else(|| {
        OmniError::InvalidInput("there is no active call to end".to_string())
    })?;
    runtime.summary.state = CallState::Ended;
    runtime.summary.outcome = Some(outcome);
    runtime.summary.decline_reason = decline_reason;
    runtime.summary.ended_at_unix_ms = Some(ended_at_unix_ms);
    runtime.summary.reconnect_deadline_unix_ms = None;
    runtime.summary.updated_at_unix_ms = ended_at_unix_ms;
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(db_path, None)?;
    // Tear down media negotiation state for this dialog so the dialog
    // immediately stops being treated as having an active media session.
    // Without this, `dialog_has_active_media_negotiation` (used by the
    // datagram hot-path approval gate) would keep accepting RTP packets
    // until the QUIC SessionClosed event eventually fires. Datagram /
    // control queues are still cleared on SessionClosed — see
    // docs/decisions/call-end-cleanup.md for the remaining gaps.
    let _ = clear_realtime_media_negotiation_for_dialog(
        db_path,
        &runtime.summary.dialog_key,
    );
    Ok(runtime.summary)
}

fn derive_call_network_quality(
    runtime: &ActiveCallRuntime,
    now_unix_ms: i64,
) -> Option<CallNetworkQuality> {
    if runtime.summary.state == CallState::Reconnecting {
        return Some(CallNetworkQuality::Recovering);
    }
    if !matches!(runtime.summary.state, CallState::Connecting | CallState::Active) {
        return runtime.summary.network_quality;
    }

    let _ = now_unix_ms;
    Some(CallNetworkQuality::Good)
}

/// Encrypt and send one call event. The caller must already hold the app session
/// guard — see the note at the top of this file.
fn send_call_event_for_session_address(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
    session_address: &omni_crypto::SessionAddress,
    event: &omni_types::LogicalEventEnvelopeV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<WireEnvelopeDelivery> {
    let result = send_call_event_inner(
        storage,
        transport,
        dialog_key,
        session_address,
        event,
        payload_kind,
    );
    trace_call_out(
        call_id_of_logical_event(event),
        payload_kind,
        dialog_key,
        &result,
    );
    result
}

fn send_call_event_inner(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
    session_address: &omni_crypto::SessionAddress,
    event: &omni_types::LogicalEventEnvelopeV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<WireEnvelopeDelivery> {
    let mut outgoing = build_encrypted_logical_event_envelope(
        &storage
            .get_session_record(session_address)?
            .ok_or_else(|| {
                OmniError::Transport(
                    "no established call signaling session is available".to_string(),
                )
            })?,
        event,
        payload_kind,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key_reporting(dialog_key, &outgoing.envelope)
}

fn call_id_of_logical_event(event: &omni_types::LogicalEventEnvelopeV1) -> &str {
    match &event.payload {
        omni_types::LogicalEventPayloadV1::CallInvite(invite) => &invite.call_id,
        omni_types::LogicalEventPayloadV1::CallRinging(ringing) => &ringing.call_id,
        omni_types::LogicalEventPayloadV1::CallAccept(accept) => &accept.call_id,
        omni_types::LogicalEventPayloadV1::CallDecline(decline) => &decline.call_id,
        omni_types::LogicalEventPayloadV1::CallBusy(busy) => &busy.call_id,
        omni_types::LogicalEventPayloadV1::CallCancel(cancel) => &cancel.call_id,
        omni_types::LogicalEventPayloadV1::CallHangup(hangup) => &hangup.call_id,
        omni_types::LogicalEventPayloadV1::CallTimeout(timeout) => &timeout.call_id,
        omni_types::LogicalEventPayloadV1::CallReconnect(reconnect) => &reconnect.call_id,
        omni_types::LogicalEventPayloadV1::CallScreenControl(control) => &control.call_id,
        _ => "",
    }
}

fn tick_call_runtime(storage: &Storage, transport: Option<&TransportManager>) -> OmniResult<()> {
    let now_unix_ms = current_unix_ms();
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        return Ok(());
    };

    let mut changed = false;
    let health = call_link_health(
        call_peer_last_activity(storage, &runtime.summary.dialog_key),
        now_unix_ms,
    );

    // Not answered yet: there is no link of the call to lose. The dialog it is keyed by may be any one
    // of the contact's devices — a dead one too — so judging its silence before an answer put a call
    // still ringing on the other devices to «reconnecting» and ended it ten seconds later as «the peer
    // went silent» (stand 2026-10-15). No answer is the invite's expiry to decide.
    let transition = if runtime.summary.connected_at_unix_ms.is_none() {
        CallLinkTransition::Keep
    } else {
        call_link_transition(
            runtime.summary.state,
            health,
            runtime.summary.reconnect_deadline_unix_ms,
            now_unix_ms,
        )
    };
    match transition {
        CallLinkTransition::StartReconnecting { deadline_unix_ms } => {
            runtime.summary.state = CallState::Reconnecting;
            runtime.summary.network_quality = Some(CallNetworkQuality::Recovering);
            runtime.summary.reconnect_deadline_unix_ms = Some(deadline_unix_ms);
            runtime.summary.updated_at_unix_ms = now_unix_ms;
            changed = true;
        }
        CallLinkTransition::Recovered => {
            runtime.summary.state = CallState::Active;
            runtime.summary.network_quality = Some(CallNetworkQuality::Recovering);
            runtime.summary.reconnect_deadline_unix_ms = None;
            runtime.summary.updated_at_unix_ms = now_unix_ms;
            changed = true;
        }
        CallLinkTransition::EndConnectionLost => {
            trace_call_in(
                "link",
                &runtime.summary.call_id,
                "ended: the peer went silent for the whole reconnect window",
            );
            let _ = end_runtime_call(
                storage,
                &db_path,
                CallOutcome::ConnectionLost,
                None,
                now_unix_ms,
            )?;
            return Ok(());
        }
        CallLinkTransition::Keep => {}
    }

    if runtime.summary.connected_at_unix_ms.is_none() && now_unix_ms >= runtime.invite_expires_at_unix_ms {
        let ended = end_runtime_call(
            storage,
            &db_path,
            CallOutcome::Missed,
            None,
            now_unix_ms,
        )?;
        let session_address = omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id,
            remote_device_id: runtime.remote_device_id,
        };
        if let Some(transport) = transport {
            let timeout = omni_types::LogicalEventEnvelopeV1::call_timeout(
                ended.call_id,
                now_unix_ms,
            );
            let _ = send_call_event_for_session_address(
                storage,
                transport,
                &ended.dialog_key,
                &session_address,
                &timeout,
                SecurityPayloadKind::CallTimeout,
            );
        }
        return Ok(());
    }

    // The reconnect-window expiry lives in `call_link_transition` above — it is part of
    // the same decision as entering the window, and having it twice meant one copy could
    // (and did) disagree with the other about what "still connected" means.
    let next_quality = derive_call_network_quality(&runtime, now_unix_ms);
    if runtime.summary.network_quality != next_quality {
        runtime.summary.network_quality = next_quality;
        runtime.summary.updated_at_unix_ms = now_unix_ms;
        changed = true;
    }

    if changed {
        upsert_call_and_wake_shell(storage, &runtime.summary)?;
        set_current_call_runtime(&db_path, Some(runtime))?;
    }

    Ok(())
}

fn current_or_persisted_call_summary_for_dialog(
    storage: &Storage,
    dialog_key: &str,
) -> OmniResult<Option<CallSummary>> {
    tick_call_runtime(storage, None)?;
    let db_path = storage.db_path().to_string_lossy().to_string();
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    if let Some(runtime) = current_call_runtime(&db_path)? {
        // The call may be with another device of the chat's contact.
        if storage.chat_dialog_key_for(&runtime.summary.dialog_key)? == chat_dialog_key {
            return Ok(Some(runtime.summary));
        }
    }
    storage.get_latest_call_for_dialog(&chat_dialog_key)
}

fn handle_incoming_call_invite(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    invite: &omni_types::CallInviteV1,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let now_unix_ms = current_unix_ms();
    tick_call_runtime(storage, Some(transport))?;

    if storage.get_call_summary(&invite.call_id)?.is_some() {
        trace_call_in("invite", &invite.call_id, "ignored: already known here");
        return Ok(());
    }

    let contact_card = match storage.resolve_contact_card_for_dialog(&receive_context.dialog_key)? {
        Some(contact_card)
            if contact_card.contact.relationship_state == ContactRelationshipState::Approved =>
        {
            contact_card
        }
        _ => {
            trace_call_in(
                "invite",
                &invite.call_id,
                "declined: the dialog is not an approved contact",
            );
            let decline = omni_types::LogicalEventEnvelopeV1::call_decline(
                invite.call_id.clone(),
                CallDeclineReason::SecurityBlocked,
                now_unix_ms,
            );
            let _ = send_call_event_for_session_address(
                storage,
                transport,
                &receive_context.dialog_key,
                &session.session_address,
                &decline,
                SecurityPayloadKind::CallDecline,
            );
            return Ok(());
        }
    };

    // Busy: one call per profile. If we are already on a different call, decline.
    let db_path = storage.db_path().to_string_lossy().to_string();
    if let Some(existing) = current_call_runtime(&db_path)? {
        if existing.summary.call_id == invite.call_id {
            // Already ringing this exact call — idempotent.
            trace_call_in("invite", &invite.call_id, "ignored: already ringing");
            return Ok(());
        }
        trace_call_in(
            "invite",
            &invite.call_id,
            &format!("declined: busy on {}", existing.summary.call_id),
        );
        let decline = omni_types::LogicalEventEnvelopeV1::call_decline(
            invite.call_id.clone(),
            CallDeclineReason::Busy,
            now_unix_ms,
        );
        // Отказ по занятости — обычное сообщение: если живого пути нет, durable довезёт его
        // позже, и это лучше, чем ничего. Режим доставки здесь не важен.
        return send_call_event_for_session_address(
            storage,
            transport,
            &receive_context.dialog_key,
            &session.session_address,
            &decline,
            SecurityPayloadKind::CallDecline,
        )
        .map(|_delivery| ());
    }

    // A remote-control request is decided HERE, by the owner's core, against the owner's own grant —
    // whatever the caller believed. No grant (never given, revoked, stale on the caller's side) is a
    // refusal, not a question.
    let remote_control_mode = if invite.remote_control {
        match storage.get_contact_remote_access(&contact_card.contact.contact_id)?.granted_mode {
            Some(mode) => Some(mode),
            None => {
                trace_call_in(
                    "invite",
                    &invite.call_id,
                    "declined: remote control requested, but this contact has no standing access",
                );
                let decline = omni_types::LogicalEventEnvelopeV1::call_decline(
                    invite.call_id.clone(),
                    CallDeclineReason::SecurityBlocked,
                    now_unix_ms,
                );
                return send_call_event_for_session_address(
                    storage,
                    transport,
                    &receive_context.dialog_key,
                    &session.session_address,
                    &decline,
                    SecurityPayloadKind::CallDecline,
                )
                .map(|_delivery| ());
            }
        }
    } else {
        None
    };
    if remote_control_mode == Some(omni_types::RemoteAccessMode::Unattended) {
        return start_unattended_remote_control(storage, transport, receive_context, invite, session, now_unix_ms);
    }

    // Approved + free: create an incoming ringing call and ring back, so the caller
    // moves to `ringing` and our shell shows the ringing card with Accept/Decline.
    let mut runtime = build_call_runtime_for_dialog(
        storage,
        &receive_context.dialog_key,
        invite.call_id.clone(),
        CallDirection::Incoming,
        CallState::Ringing,
        now_unix_ms,
    )?;
    if remote_control_mode.is_some() {
        // "Ask": the owner has REMOTE_CONTROL_ANSWER_MS to answer; silence is a refusal.
        let deadline = now_unix_ms + REMOTE_CONTROL_ANSWER_MS;
        runtime.summary.remote_control = Some(omni_types::RemoteControlSession {
            role: omni_types::RemoteControlRole::Owner,
            phase: omni_types::RemoteControlPhase::AwaitingOwner,
            answer_deadline_unix_ms: Some(deadline),
        });
        runtime.invite_expires_at_unix_ms = deadline;
    }
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))?;
    trace_call_in("invite", &invite.call_id, "applied: ringing");
    // Presence/call event for the daemon's event plane: an incoming call is ringing.
    record_incoming_call_event(&db_path, &receive_context.dialog_key, &invite.call_id);
    let ringing =
        omni_types::LogicalEventEnvelopeV1::call_ringing(invite.call_id.clone(), now_unix_ms);
    // «Звоню» — уведомление, а не решение: живого пути может ещё не быть, а звонок у нас уже
    // звонит. Режим доставки здесь ни на что не влияет.
    send_call_event_for_session_address(
        storage,
        transport,
        &receive_context.dialog_key,
        &session.session_address,
        &ringing,
        SecurityPayloadKind::CallRinging,
    )
    .map(|_delivery| ())
}

fn handle_incoming_call_ringing(
    storage: &Storage,
    ringing: &omni_types::CallRingingV1,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        return Ok(());
    };
    if runtime.summary.call_id != ringing.call_id || runtime.summary.direction != CallDirection::Outgoing {
        return Ok(());
    }
    // «Ringing» only moves a call that is still calling. A contact with several devices rings on each,
    // and one device's «ringing» can arrive after another one answered (through a relay it routinely
    // does): it used to put the live call back to «ringing» — the caller's window showed the call
    // ringing with the talk going on (stand 2026-10-14, Alice on a PC and a laptop).
    // Before an answer the call may sit in `Calling`, `Connecting` or even `Reconnecting` (the link
    // check runs before anyone answered); what tells «not answered» is that nothing connected yet.
    let not_answered_yet = runtime.summary.connected_at_unix_ms.is_none() && runtime.summary.state != CallState::Ended;
    if !not_answered_yet {
        trace_call_in(
            "ringing",
            &ringing.call_id,
            &format!("ignored: the call is already {:?}", runtime.summary.state),
        );
        return Ok(());
    }
    runtime.summary.state = CallState::Ringing;
    runtime.summary.ringing_started_at_unix_ms = Some(ringing.ringing_at_unix_ms);
    runtime.summary.updated_at_unix_ms = current_unix_ms();
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))
}

/// Stop the devices that rang but did not take the call ("answered on another device").
fn cancel_other_invited_devices(
    storage: &Storage,
    transport: &TransportManager,
    runtime: &ActiveCallRuntime,
    answering_device: &DeviceId,
    now_unix_ms: i64,
) {
    let cancel = omni_types::LogicalEventEnvelopeV1::call_cancel(
        runtime.summary.call_id.clone(),
        now_unix_ms,
    );
    for device in &runtime.invited_devices {
        if &device.device_id == answering_device {
            continue;
        }
        let _ = send_call_event_for_session_address(
            storage,
            transport,
            &device.dialog_key,
            &omni_crypto::SessionAddress {
                remote_account_id: runtime.remote_account_id.clone(),
                remote_device_id: device.device_id.clone(),
            },
            &cancel,
            SecurityPayloadKind::CallCancel,
        );
    }
}

fn handle_incoming_call_accept(
    storage: &Storage,
    transport: &TransportManager,
    accept: &omni_types::CallAcceptV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        trace_call_in("accept", &accept.call_id, "ignored: no call is running here");
        return Ok(());
    };
    if runtime.summary.call_id != accept.call_id || runtime.summary.direction != CallDirection::Outgoing {
        trace_call_in(
            "accept",
            &accept.call_id,
            &format!(
                "ignored: current call is {} ({:?})",
                runtime.summary.call_id, runtime.summary.direction
            ),
        );
        return Ok(());
    }
    trace_call_in("accept", &accept.call_id, "applied: call is active");
    if !runtime.invited_devices.is_empty() {
        // The device that answered is the call's device: its dialog carries the media from now on.
        let answering_device = session.session_address.remote_device_id.clone();
        runtime.remote_device_id = answering_device.clone();
        runtime.summary.dialog_key = normalize_dialog_key(&receive_context.dialog_key);
        for participant in &mut runtime.summary.participants {
            if !participant.is_local {
                participant.device_id = answering_device.0.clone();
            }
        }
        cancel_other_invited_devices(storage, transport, &runtime, &answering_device, current_unix_ms());
        runtime.invited_devices.clear();
    }
    runtime.summary.state = CallState::Active;
    approve_remote_control(storage, &mut runtime.summary, current_unix_ms());
    runtime.summary.connected_at_unix_ms = Some(accept.accepted_at_unix_ms);
    runtime.summary.network_quality = Some(CallNetworkQuality::Good);
    runtime.summary.reconnect_deadline_unix_ms = None;
    runtime.summary.updated_at_unix_ms = current_unix_ms();
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))
}

fn handle_incoming_call_decline_or_busy(
    storage: &Storage,
    call_id: &str,
    outcome: CallOutcome,
    decline_reason: Option<CallDeclineReason>,
    ended_at_unix_ms: i64,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(runtime) = current_call_runtime(&db_path)? else {
        trace_call_in("end", call_id, "ignored: no call is running here");
        return Ok(());
    };
    if runtime.summary.call_id != call_id {
        trace_call_in(
            "end",
            call_id,
            &format!("ignored: we are on a different call ({})", runtime.summary.call_id),
        );
        return Ok(());
    }
    let _ = end_runtime_call(storage, &db_path, outcome, decline_reason, ended_at_unix_ms)?;
    trace_call_in("end", call_id, &format!("applied: {outcome:?}"));
    Ok(())
}

fn handle_incoming_call_cancel_or_hangup_or_timeout(
    storage: &Storage,
    call_id: &str,
    outcome: CallOutcome,
    ended_at_unix_ms: i64,
) -> OmniResult<()> {
    handle_incoming_call_decline_or_busy(storage, call_id, outcome, None, ended_at_unix_ms)
}

fn handle_incoming_call_reconnect(
    storage: &Storage,
    reconnect: &omni_types::CallReconnectV1,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        return Ok(());
    };
    if runtime.summary.call_id != reconnect.call_id {
        return Ok(());
    }
    runtime.summary.state = CallState::Reconnecting;
    runtime.summary.network_quality = Some(CallNetworkQuality::Recovering);
    runtime.summary.reconnect_deadline_unix_ms =
        Some(reconnect.reconnecting_at_unix_ms + CALL_RECONNECT_GRACE_MS);
    runtime.summary.updated_at_unix_ms = current_unix_ms();
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))
}

fn handle_call_session_activated(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        return Ok(());
    };
    if normalize_dialog_key(&runtime.summary.dialog_key) != normalize_dialog_key(&receive_context.dialog_key) {
        return Ok(());
    }
    if runtime.summary.state != CallState::Reconnecting {
        return Ok(());
    }
    runtime.summary.state = CallState::Active;
    runtime.summary.network_quality = Some(CallNetworkQuality::Recovering);
    runtime.summary.reconnect_deadline_unix_ms = None;
    runtime.summary.updated_at_unix_ms = current_unix_ms();
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))
}

fn handle_call_session_closed(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(mut runtime) = current_call_runtime(&db_path)? else {
        return Ok(());
    };
    if normalize_dialog_key(&runtime.summary.dialog_key) != normalize_dialog_key(&receive_context.dialog_key) {
        return Ok(());
    }
    // Audit P1: extend reconnect grace on every SessionClosed during the
    // reconnect window. Previously this gate only fired for Active|Connecting,
    // so a second flap during Reconnecting was ignored and the call died at
    // the original deadline even if the peer was mid-reconnect. We pick the
    // later of the current deadline and now+grace, so the deadline never
    // moves backwards and a steady flap stream keeps the call alive while
    // genuine inactivity still expires it.
    if matches!(
        runtime.summary.state,
        CallState::Active | CallState::Connecting | CallState::Reconnecting
    ) {
        let now_unix_ms = current_unix_ms();
        let extended_deadline = now_unix_ms + CALL_RECONNECT_GRACE_MS;
        let new_deadline = runtime
            .summary
            .reconnect_deadline_unix_ms
            .map(|existing| existing.max(extended_deadline))
            .unwrap_or(extended_deadline);
        runtime.summary.state = CallState::Reconnecting;
        runtime.summary.network_quality = Some(CallNetworkQuality::Recovering);
        runtime.summary.reconnect_deadline_unix_ms = Some(new_deadline);
        runtime.summary.updated_at_unix_ms = now_unix_ms;
        upsert_call_and_wake_shell(storage, &runtime.summary)?;
        set_current_call_runtime(&db_path, Some(runtime))?;
    }
    Ok(())
}

/// How long the owner has to answer a remote-control request in "ask" mode.
const REMOTE_CONTROL_ANSWER_MS: i64 = 30_000;
/// The controller gives up a little after the owner's deadline, so the owner's own timeout (sent as
/// CallTimeout) normally decides how it ended.
const REMOTE_CONTROL_INVITE_TTL_MS: i64 = REMOTE_CONTROL_ANSWER_MS + 5_000;

/// The call went live with the owner's consent: the session is approved on this side. Records the
/// session time for the contact card («Последний сеанс»). An ordinary call is left alone.
fn approve_remote_control(storage: &Storage, summary: &mut CallSummary, now_unix_ms: i64) {
    let Some(session) = summary.remote_control.as_mut() else {
        return;
    };
    session.phase = omni_types::RemoteControlPhase::Approved;
    session.answer_deadline_unix_ms = None;
    if let Some(contact_id) = summary.contact_id.as_ref() {
        let _ = storage.note_contact_remote_access_session(&ContactId(contact_id.clone()), now_unix_ms);
    }
}

/// Unattended access: the owner's core answers for the owner — the session goes live at once and
/// the caller is told so with an ordinary CallAccept.
fn start_unattended_remote_control(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    invite: &omni_types::CallInviteV1,
    session: &omni_crypto::SessionRecord,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let mut runtime = build_call_runtime_for_dialog(
        storage,
        &receive_context.dialog_key,
        invite.call_id.clone(),
        CallDirection::Incoming,
        CallState::Active,
        now_unix_ms,
    )?;
    runtime.summary.remote_control = Some(omni_types::RemoteControlSession {
        role: omni_types::RemoteControlRole::Owner,
        phase: omni_types::RemoteControlPhase::AwaitingOwner,
        answer_deadline_unix_ms: None,
    });
    let accept = omni_types::LogicalEventEnvelopeV1::call_accept(invite.call_id.clone(), now_unix_ms);
    let delivery = send_call_event_for_session_address(
        storage,
        transport,
        &receive_context.dialog_key,
        &session.session_address,
        &accept,
        SecurityPayloadKind::CallAccept,
    )?;
    if delivery != WireEnvelopeDelivery::Live {
        // Same rule as accept_call: never sit in a live session the controller never heard about.
        trace_call_in("invite", &invite.call_id, "remote control: acceptance not delivered, dropped");
        return Ok(());
    }
    approve_remote_control(storage, &mut runtime.summary, now_unix_ms);
    upsert_call_and_wake_shell(storage, &runtime.summary)?;
    set_current_call_runtime(&db_path, Some(runtime))?;
    trace_call_in("invite", &invite.call_id, "applied: unattended remote control, session live");
    record_incoming_call_event(&db_path, &receive_context.dialog_key, &invite.call_id);
    Ok(())
}

#[cfg(test)]
mod call_placement_tests {
    use super::*;

    /// Обе ветки, потому что до этого оба исхода отправки показывались как один — и именно на
    /// этом человек полторы минуты смотрел на звонок, которого у собеседника не было.
    #[test]
    fn a_call_counts_as_placed_only_when_the_invite_actually_left() {
        assert!(
            call_was_placed(WireEnvelopeDelivery::Live),
            "живая доставка — единственный исход, при котором собеседник звонит"
        );
        assert!(
            !call_was_placed(WireEnvelopeDelivery::DurableOnly),
            "очередь до реконсиляции — правильный успех для сообщения и НЕ звонок"
        );
        assert!(
            !call_was_placed(WireEnvelopeDelivery::LiveUnconfirmed),
            "плечо доказано мёртвым: труба есть, звонить в неё некому"
        );
    }

    /// Отрицательная половина правила о ПРИЧИНЕ: до неё две разные причины отказа сворачивались
    /// в одну фразу, и по ней нельзя было понять, чинить путь или ждать пира.
    #[test]
    fn the_refusal_names_which_of_the_two_failures_happened() {
        let dead_leg = refusal_for_undelivered_invite(WireEnvelopeDelivery::LiveUnconfirmed);
        let no_path = refusal_for_undelivered_invite(WireEnvelopeDelivery::DurableOnly);
        assert_ne!(
            dead_leg, no_path,
            "две разные причины обязаны читаться по-разному"
        );
        assert!(dead_leg.contains("stopped answering"), "{dead_leg}");
        assert!(no_path.contains("no live path"), "{no_path}");
    }
}
