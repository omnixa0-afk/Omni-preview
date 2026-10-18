const CONTACT_PRESENCE_RECENTLY_AFTER_MS: i64 = 40_000;
const CONTACT_PRESENCE_OFFLINE_AFTER_MS: i64 = 90_000;
const CONTACT_PRESENCE_SKIP_WHEN_RECENT_ACTIVITY_MS: i64 = 10_000;
/// Пол по частоте проб, когда мы НЕ ЗНАЕМ, слышат ли нас. Такт очереди 250 мс, так что без пола
/// «пробуй, пока не знаешь» превратилось бы в поток. Совпадает с тактом keepalive'а открытого
/// диалога — быстрее пробовать нечего, круг всё равно длится десятки миллисекунд.
const CONTACT_PRESENCE_PROBE_MIN_INTERVAL_MS: i64 = OPEN_CHAT_KEEPALIVE_INTERVAL_MS;
const OPEN_CHAT_KEEPALIVE_INTERVAL_MS: i64 = 10_000;
#[cfg(not(test))]
const ACTIVE_SESSION_ACK_GRACE_MS: i64 = 4_000;
#[cfg(test)]
const ACTIVE_SESSION_ACK_GRACE_MS: i64 = 600;
const ATTACHMENT_SERVING_RECHECK_BYTES: i64 = 1024 * 1024;

// TM-005 (docs/decisions/attachment-transfer-quotas.md): how many downloads of one dialog a
// contact's photos may start on their own. Past it a photo waits for a tap; the offer is never refused.
const MAX_INBOUND_ATTACHMENTS_PER_DIALOG: i64 = 8;
// How often the tick looks for outgoing files left `Preparing` with no pass reading them (a restart
// cut it short). The first tick looks at once.
const ATTACHMENT_PREPARATION_SCAN_EVERY_MS: i64 = 10_000;
// How long a download may sit without progress before the request is sent again. Progress
// checkpoints are written every 250 ms while chunks arrive (ATTACHMENT_PROGRESS_CHECKPOINT_INTERVAL_MS),
// so nothing moving for this long on a connected dialog is a real stall and not pacing. See
// docs/bugs/attachment-transfer-never-starts.md.
const ATTACHMENT_DOWNLOAD_REQUEST_RETRY_AFTER_MS: i64 = 20_000;
// Each further retry of the same stall waits twice as long, up to this. Never «give up»: it used to
// after five retries (two minutes), and a link down for ten minutes left a download standing until
// the app was restarted. At the cap a stuck download costs one small request per ten minutes.
const ATTACHMENT_DOWNLOAD_REQUEST_MAX_PAUSE_MS: i64 = 10 * 60_000;

#[derive(Default)]
struct QueueWorkerState {
    // Last look for outgoing files whose preparation a restart cut short.
    last_preparation_scan_at_unix_ms: i64,
    // Last pass that handed our other devices the copies they still lack.
    last_own_device_reconcile_at_unix_ms: i64,
    // Per device of ours: how many reconcile passes in a row had to send copies, and when the next
    // one may. Keeps an unresponsive device from being fed a copy every pass.
    own_device_backoff: std::collections::HashMap<String, OwnDeviceBackoff>,
    // "dialog|device" of the revocations already handed to a contact's device on its current link.
    revocations_told_to_contacts: std::collections::HashSet<String>,
    // Per dialog of a device we talk to: the courier set we told it on its current link.
    courier_sets_told: std::collections::HashMap<String, u64>,
    // The live links and our courier generation at the last courier pass: a new link or a changed
    // set is told at once, not at the next reconcile.
    courier_pass_key: u64,
    // A device on a live link still owed our courier set (its session was not ready): when to try
    // again, and the pause after that (1 s doubling; the reconcile pass covers the rest).
    couriers_owed_retry: Option<(i64, i64)>,
    // Our other devices that were ready (live link, established session) at the last tick, as
    // "dialog#session created at": one that becomes ready — or whose session was replaced, which
    // loses what went out on the old one — gets the reconcile pass (copies, contacts, groups,
    // couriers) at once, not up to 15 s later.
    own_devices_ready: std::collections::HashSet<String>,
    // `RECONCILE_REQUESTS` at the last pass this worker made for a request.
    reconcile_requests_seen: u64,
    // Downloads that were requested and have not moved a byte since, keyed by message_id.
    stalled_downloads: std::collections::HashMap<String, StalledDownloadWatch>,
}

fn run_queue_worker(
    db_path: String,
    transport: TransportManager,
    stop_rx: mpsc::Receiver<()>,
    session_lock: Arc<Mutex<()>>,
) {
    let storage = match Storage::open(&db_path).and_then(|storage| {
        storage.healthcheck()?;
        Ok(storage)
    }) {
        Ok(storage) => storage,
        Err(_error) => {
            return;
        }
    };
    let mut worker_state = QueueWorkerState::default();
    // A tick that fails cuts short everything after the failing step — every tick, if the step
    // keeps failing — and used to do so in silence. Said once per change.
    let mut last_tick_error: Option<String> = None;
    loop {
        match stop_rx.recv_timeout(QUEUE_WORKER_POLL_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let error = process_queue_tick(&storage, &transport, &session_lock, &mut worker_state)
            .err()
            .map(|error| error.to_string());
        if error != last_tick_error {
            if let Some(error) = error.as_deref() {
                omni_app_trace_log("queue/tick", &format!("tick failed: {error}"));
            }
            last_tick_error = error;
        }
    }
}

fn process_queue_tick(
    storage: &Storage,
    transport: &TransportManager,
    session_lock: &Arc<Mutex<()>>,
    worker_state: &mut QueueWorkerState,
) -> OmniResult<()> {
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }

    let now = current_unix_ms();
    let snapshot = transport.snapshot();
    {
        let _session_guard = lock_app_session_guard(session_lock)?;
        promote_overdue_sent_messages(storage, &snapshot, now)?;

        if !matches!(
            snapshot.state,
            omni_types::TransportState::Connected | omni_types::TransportState::ListeningAndConnected
        ) {
            let db_path = storage.db_path().to_string_lossy().into_owned();
            // The active dialog is down, not every source: a download whose stream runs, or whose
            // request went to a source we still have a link to, keeps waiting for it — paused, it
            // was asked for again every tick until its first chunk came.
            storage.pause_inflight_attachment_downloads(now, &|runtime| {
                runtime
                    .active_transfer_id
                    .as_deref()
                    .is_some_and(|transfer_id| attachment_receive_writer(&db_path, transfer_id).is_some())
                    || download_source_is_linked(storage, transport, runtime)
            })?;
        }

        let mut download_dialog_keys = Vec::new();
        if let Some(dialog_key) = snapshot.dialog_key.as_deref() {
            process_pending_for_active_dialog(storage, transport, dialog_key)?;
            process_pending_attachment_downloads_for_active_dialog(storage, transport, dialog_key)?;
            download_dialog_keys.push(dialog_key.to_string());
        }
        // Copies of our own messages download their files from our other devices, whichever dialog
        // is active. Best effort: a device that is away must not stall the rest of the tick.
        for (_device_id, device_dialog_key) in storage.list_own_device_dialogs()? {
            if download_dialog_keys.contains(&device_dialog_key) {
                continue;
            }
            let _ = process_pending_attachment_downloads_for_active_dialog(
                storage,
                transport,
                &device_dialog_key,
            );
            download_dialog_keys.push(device_dialog_key);
        }
        // A download requested in a chat that is not the active transport dialog (several linked
        // devices: the active one is whichever connected last) would otherwise never go out.
        for pending_dialog_key in storage.dialogs_with_pending_attachment_downloads()? {
            if download_dialog_keys.contains(&pending_dialog_key) {
                continue;
            }
            let _ = process_pending_attachment_downloads_for_active_dialog(
                storage,
                transport,
                &pending_dialog_key,
            );
            download_dialog_keys.push(pending_dialog_key);
        }
        // A download under way in a chat that is not the active transport dialog: its stall must be
        // watched too. After a restart the active dialog is whichever linked last, and the download
        // from another contact sat without a single retry until its link happened to come up again
        // (live 2026-10-16: 110 s).
        for downloading_dialog_key in storage.dialogs_with_downloading_attachments()? {
            if !download_dialog_keys.contains(&downloading_dialog_key) {
                download_dialog_keys.push(downloading_dialog_key);
            }
        }
        // A chat that is not the active transport dialog keeps its own queue: without this walk a
        // message parked there (no route at send time) waited until that chat became active.
        // Best effort: one unreachable chat must not stall the others.
        let active_chat_key = match snapshot.dialog_key.as_deref() {
            Some(dialog_key) => Some(storage.chat_dialog_key_for(dialog_key)?),
            None => None,
        };
        for pending_dialog_key in storage.dialogs_with_due_pending_queue(now)? {
            if active_chat_key.as_deref() == Some(pending_dialog_key.as_str()) {
                continue;
            }
            let _ = process_pending_for_active_dialog(storage, transport, &pending_dialog_key);
        }
        retry_stalled_attachment_downloads(
            storage,
            transport,
            worker_state,
            &download_dialog_keys,
            now,
        )?;

        if matches!(
            snapshot.state,
            omni_types::TransportState::Connected | omni_types::TransportState::ListeningAndConnected
        ) {
            if let Some(session) = active_established_session_for_profile_sync(storage, transport)? {
                let _ = send_current_public_profile_sync_for_session(
                    storage,
                    transport,
                    &session,
                    snapshot.dialog_key.as_deref(),
                );
                // R1c: push my current relay set to the active linked friend (covers a relink of an
                // already-approved contact, which the approval-gated on-link force misses). Rate-limited
                // by version, so an unchanged set is a no-op. See r1c-rotatable-white-relay.md §B.
                let _ = send_relay_set_update_for_session(
                    storage,
                    transport,
                    &session,
                    snapshot.dialog_key.as_deref(),
                );
            }
        }

        // A link that just came up, or a courier set the user just changed, is told now. The check
        // is one lock and a hash per tick; the pass itself walks the contacts, so it runs only when
        // something changed (and on every reconcile below).
        let courier_pass_key = courier_pass_key(transport);
        let courier_retry_due = worker_state
            .couriers_owed_retry
            .is_some_and(|(at, _)| now >= at);
        if courier_pass_key != worker_state.courier_pass_key || courier_retry_due {
            let owed = tell_devices_our_couriers(storage, transport, &mut worker_state.courier_sets_told);
            worker_state.courier_pass_key = courier_pass_key;
            worker_state.couriers_owed_retry = match (owed, worker_state.couriers_owed_retry) {
                (false, _) => None,
                (true, Some((_, pause))) if pause * 2 > OWN_DEVICE_RECONCILE_INTERVAL_MS => None,
                (true, Some((_, pause))) => Some((now + pause * 2, pause * 2)),
                (true, None) => Some((now + 1_000, 1_000)),
            };
        }
        // Our own few devices only (a contact coming and going does not buy a reconcile pass): a
        // session read per device per tick. The link alone is not enough — it is up before the
        // session, and a pass then only sends the bootstrap.
        let ready = ready_own_device_dialogs(storage, transport);
        let own_device_came_online =
            ready.iter().any(|dialog| !worker_state.own_devices_ready.contains(dialog));
        worker_state.own_devices_ready = ready;
        let reconcile_due = now.saturating_sub(worker_state.last_own_device_reconcile_at_unix_ms)
            >= OWN_DEVICE_RECONCILE_INTERVAL_MS;
        let reconcile_requests = RECONCILE_REQUESTS.load(std::sync::atomic::Ordering::Relaxed);
        let reconcile_requested = reconcile_requests != worker_state.reconcile_requests_seen;
        worker_state.reconcile_requests_seen = reconcile_requests;
        if own_device_came_online || reconcile_requested || reconcile_due {
            reconcile_own_devices(
                storage,
                transport,
                &mut worker_state.own_device_backoff,
                &mut worker_state.revocations_told_to_contacts,
                now,
            );
            let _ = tell_devices_our_couriers(storage, transport, &mut worker_state.courier_sets_told);
            account_passport_round(storage, transport, now);
            history_sync_round(storage, transport, now);
            history_recovery_round(storage, transport, now);
            crate::app_group_rejoin::group_rejoin_round(storage, transport, now);
            crate::app_group_rejoin::offer_groups_to_own_devices(storage, transport, now);
            // An extra pass for a device that just came up does not move the schedule: the link is
            // up before its session is, so the pass that finds the session ready is the next one.
            if reconcile_due {
                worker_state.last_own_device_reconcile_at_unix_ms = now;
            }
        }
        if now.saturating_sub(worker_state.last_preparation_scan_at_unix_ms) >= ATTACHMENT_PREPARATION_SCAN_EVERY_MS {
            worker_state.last_preparation_scan_at_unix_ms = now;
            // Best effort: a failed look must not cut the rest of the tick short.
            if let Err(error) = crate::app_attachment_preparation::resume_attachment_preparations(storage) {
                omni_app_trace_log("attachment/prepare", &format!("resume scan failed: {error}"));
            }
        }
        process_contact_presence_tick(storage, transport, &snapshot, now)?;
        maintain_open_chat_session_leases(storage, transport, now)?;
        expire_stale_contact_presence(storage, now)?;
    }

    Ok(())
}

/// Bumped by a user action whose effect the reconcile pass carries (a device taken back must hear it
/// now, not at the next pass). Any profile's worker answers with one extra pass — harmless.
static RECONCILE_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn request_reconcile_pass() {
    RECONCILE_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Our other devices with a live link and an established session right now, as "dialog#session
/// created at" (a replaced session reads as new) — revoked ones included: the pass is what tells
/// such a device it was taken back.
fn ready_own_device_dialogs(
    storage: &Storage,
    transport: &TransportManager,
) -> std::collections::HashSet<String> {
    let Ok(Some(account)) = storage.get_local_account() else {
        return std::collections::HashSet::new();
    };
    let mut devices = storage.list_own_device_dialogs().unwrap_or_default();
    devices.extend(storage.list_revoked_own_device_dialogs().unwrap_or_default());
    devices
        .into_iter()
        .filter(|(_, dialog_key)| transport.connection_snapshot_for_dialog(dialog_key).is_some())
        .filter_map(|(device_id, dialog_key)| {
            let session = storage
                .get_session_record(&omni_crypto::SessionAddress {
                    remote_account_id: account.descriptor.account_id.clone(),
                    remote_device_id: device_id,
                })
                .ok()
                .flatten()?;
            (session.bootstrap_state == omni_crypto::SessionBootstrapState::Established)
                .then(|| format!("{dialog_key}#{}", session.created_at_unix_ms))
        })
        .collect()
}

fn process_pending_for_active_dialog(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
) -> OmniResult<()> {
    // A device's dialog flushes its contact's chat: the queue lives under the chat. And under the
    // key it was queued with: the chat's key moves when the contact's primary dialog does (an
    // account dialog gives way to a device's once its link is up), and a message queued a moment
    // before stayed under the old key — the tick named that key as due every time, then looked
    // under the new one and found nothing; the message sat queued for good, never once attempted
    // (core gate 2026-10-15, `a_device_that_got_a_contact_from_another_device_of_ours_can_write_to_it`).
    let now = current_unix_ms();
    let chat_dialog_key = storage.chat_dialog_key_for(dialog_key)?;
    let mut pending = storage.pending_queue_for_dialog(&chat_dialog_key, now)?;
    let queued_under = omni_types::normalize_dialog_key(dialog_key);
    if queued_under != chat_dialog_key {
        for item in storage.pending_queue_for_dialog(&queued_under, now)? {
            if !pending.iter().any(|known| known.message_id == item.message_id) {
                pending.push(item);
            }
        }
    }

    // One message that cannot be attempted (a storage error) must not hold back the ones behind it.
    for item in pending {
        if let Err(error) = attempt_send_existing_message(storage, transport, &item.message_id) {
            omni_app_trace_log(
                "queue/pending",
                &format!("message={} attempt failed: {error}", item.message_id),
            );
        }
    }

    Ok(())
}

fn process_pending_attachment_downloads_for_active_dialog(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
) -> OmniResult<()> {
    let pending = storage.list_pending_attachment_downloads_for_dialog(dialog_key)?;
    // A request that fails (no route to the holder yet) stays `requested` for the next tick and
    // must not stop the downloads queued behind it, nor the rest of the tick.
    for item in pending {
        // Paused for a source we have no link to: it is asked for again when the source is back
        // (`request_downloads_on_session_up`). Asked every tick instead, a request that got out by
        // a route went `downloading`, the next tick paused it again, and round it went every
        // ~300 ms (live 2026-10-16).
        if item.attachment.state == omni_types::AttachmentTransferState::WaitingForReconnect
            && !download_source_is_linked(storage, transport, &item)
        {
            continue;
        }
        if let Err(error) = attempt_send_attachment_download_request(
            storage,
            transport,
            &item.attachment.message_id,
        ) {
            omni_app_trace_log(
                "queue/pending_download",
                &format!("message={} request failed: {error}", item.attachment.message_id),
            );
        }
    }

    Ok(())
}

fn synchronize_active_contact_session(
    storage: &Storage,
    snapshot: &TransportSnapshot,
    now_unix_ms: i64,
) -> OmniResult<Option<ContactSnapshot>> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    if !matches!(
        snapshot.state,
        omni_types::TransportState::Connected | omni_types::TransportState::ListeningAndConnected
    ) {
        return Ok(None);
    }

    if let Some(dialog_key) = snapshot.dialog_key.as_deref() {
        if let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? {
            return Ok(Some(contact));
        }
    }

    let contact = if let Some(remote_account_id) = snapshot.remote_account_id.as_deref() {
        storage.find_contact_by_remote_account_id(&AccountId(remote_account_id.to_string()))?
    } else if let Some(remote_instance_id) = snapshot.remote_instance_id.as_deref() {
        storage.find_contact_by_remote_instance_id(remote_instance_id)?
    } else {
        None
    };
    let Some(contact) = contact else {
        return Ok(None);
    };
    let previous_dialog_key = contact.primary_dialog_key.clone();
    let synchronized_dialog_key = snapshot
        .dialog_key
        .clone()
        .or_else(|| contact.primary_dialog_key.clone());

    let mut synchronized = match contact.relationship_state {
        omni_identity::ContactRelationshipState::Approved => {
            if contact.primary_dialog_key == synchronized_dialog_key {
                contact.clone()
            } else {
                storage.add_contact(
                    &AddContactInput {
                        petname: contact.petname.clone(),
                        remote_account_id: contact.remote_account_id.clone().or_else(|| {
                            snapshot
                                .remote_account_id
                                .as_deref()
                                .map(|value| AccountId(value.to_string()))
                        }),
                        public_display_name: contact
                            .public_display_name
                            .clone()
                            .or_else(|| snapshot.remote_account_id.clone()),
                        remote_instance_id: contact
                            .remote_instance_id
                            .clone()
                            .or_else(|| snapshot.remote_instance_id.clone()),
                        primary_dialog_key: synchronized_dialog_key.clone(),
                        initial_observation: None,
                    },
                    now_unix_ms,
                )?
            }
        }
        omni_identity::ContactRelationshipState::Pending => {
            if contact.primary_dialog_key == synchronized_dialog_key {
                contact.clone()
            } else {
                storage.add_pending_contact_with_direction(
                    &AddContactInput {
                        petname: contact.petname.clone(),
                        remote_account_id: contact.remote_account_id.clone().or_else(|| {
                            snapshot
                                .remote_account_id
                                .as_deref()
                                .map(|value| AccountId(value.to_string()))
                        }),
                        public_display_name: contact
                            .public_display_name
                            .clone()
                            .or_else(|| snapshot.remote_account_id.clone()),
                        remote_instance_id: contact
                            .remote_instance_id
                            .clone()
                            .or_else(|| snapshot.remote_instance_id.clone()),
                        primary_dialog_key: synchronized_dialog_key.clone(),
                        initial_observation: None,
                    },
                    now_unix_ms,
                    contact.pending_request_direction,
                )?
            }
        }
        _ => contact,
    };

    if let Some(active_dialog_key) = snapshot.dialog_key.as_deref() {
        if synchronized.primary_dialog_key.as_deref() != Some(active_dialog_key) {
            // The contact's chat does not follow whichever device is connected: a dialog becomes
            // the chat only for a contact that has none yet (multi-device-equal-nodes.md).
            let make_primary = synchronized.primary_dialog_key.is_none();
            synchronized = storage.link_dialog_to_contact(
                &synchronized.contact_id,
                active_dialog_key,
                make_primary,
                now_unix_ms,
            )?;
        }

        if let Some(previous_dialog_key) = previous_dialog_key.as_deref() {
            if previous_dialog_key != active_dialog_key {
                storage.rebind_pending_dialog_scope(
                    previous_dialog_key,
                    active_dialog_key,
                    now_unix_ms,
                )?;
                migrate_runtime_chat_session_dialog_key(
                    &db_path,
                    previous_dialog_key,
                    active_dialog_key,
                )?;
            }
        }

        if let Some(remote_account_id) = synchronized.remote_account_id.as_ref() {
            let account_dialog_key = omni_types::dialog_key_from_account_id(&remote_account_id.0);
            if account_dialog_key != active_dialog_key {
                storage.rebind_pending_dialog_scope(
                    &account_dialog_key,
                    active_dialog_key,
                    now_unix_ms,
                )?;
                migrate_runtime_chat_session_dialog_key(
                    &db_path,
                    &account_dialog_key,
                    active_dialog_key,
                )?;
            }
        }
    }

    Ok(Some(synchronized))
}

fn send_presence_ping_to_session(
    storage: &Storage,
    transport: &TransportManager,
    contact: &ContactSnapshot,
    dialog_key: &str,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let Some(remote_account_id) = contact.remote_account_id.clone() else {
        return Ok(());
    };
    let ping_id = format!(
        "presence-{}-{}",
        now_unix_ms,
        hex::encode(random_bytes(4)?)
    );
    let ping_id_for_probe = ping_id.clone();
    // Кладём в пинг СВОЁ состояние звонка: сверка на случай потерянного управляющего сообщения.
    // Presence уже ходит во время звонка каждые ~10 с, поэтому ни нового канала, ни нового таймера
    // не нужно. См. docs/bugs/a-lost-hangup-leaves-a-phantom-call.md.
    let call_state = presence_call_state_for_dialog(
        storage.db_path().to_string_lossy().as_ref(),
        dialog_key,
    );
    let payload = omni_types::WirePayloadV1::LogicalEvent(
        omni_types::LogicalEventEnvelopeV1::contact_presence_ping_with_call_state(
            ping_id,
            now_unix_ms,
            call_state,
        ),
    );
    let Ok(mut outgoing) = prepare_encrypted_payload_for_established_session(
        storage,
        &remote_account_id,
        dialog_key,
        &payload,
        SecurityPayloadKind::ContactPresencePing,
    ) else {
        return Ok(());
    };
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    storage.record_contact_presence_sent(&contact.contact_id, now_unix_ms)?;
    // Проба заводится в СЕССИИ, а не в диалоге: ответ на неё — единственное доказательство, что
    // наше исходящее плечо живо. См. docs/decisions/reachability-belongs-to-the-session.md.
    let db_path = storage.db_path().to_string_lossy().to_string();
    let session_key = outgoing.updated_session.session_address.storage_key();
    link_session_to_dialog(&db_path, dialog_key, &session_key);
    let _ = note_session_probe_sent(&db_path, &session_key, &ping_id_for_probe, now_unix_ms);
    Ok(())
}

fn process_contact_presence_tick(
    storage: &Storage,
    transport: &TransportManager,
    snapshot: &TransportSnapshot,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let Some(contact) = synchronize_active_contact_session(storage, snapshot, now_unix_ms)? else {
        return Ok(());
    };
    if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
        return Ok(());
    }
    let db_path = storage.db_path().to_string_lossy().to_string();
    for dialog_key in contact_dialog_keys_for_peer_reset(&contact) {
        if runtime_chat_session_auto_reconnect_paused(&db_path, &dialog_key)? {
            storage.record_contact_presence_offline(&contact.contact_id, now_unix_ms)?;
            let _ = transport.disconnect();
            return Ok(());
        }
    }

    let has_live_approved_session = matches!(
        snapshot.state,
        omni_types::TransportState::Connected | omni_types::TransportState::ListeningAndConnected
    );
    let has_recent_confirmed_activity = contact
        .presence_last_activity_at_unix_ms
        .is_some_and(|last_activity_at_unix_ms| {
            now_unix_ms.saturating_sub(last_activity_at_unix_ms) < CONTACT_PRESENCE_RECENTLY_AFTER_MS
        });
    if has_live_approved_session
        && contact.presence_state != omni_identity::ContactPresenceState::Online
        && has_recent_confirmed_activity
    {
        storage.record_contact_presence_activity(&contact.contact_id, now_unix_ms)?;
    }

    let Some(dialog_key) = snapshot
        .dialog_key
        .as_deref()
        .or(contact.primary_dialog_key.as_deref())
    else {
        return Ok(());
    };

    // ИЗМЕРЕНИЕ. Отметить состояние — да. Рвать связь — нет: раньше здесь стоял
    // `transport.disconnect()`, который к тому же бил по АКТИВНОМУ соединению, а не обязательно по
    // тому контакту, чей presence протух. Ошибка измерения немедленно наносила ущерб, ущерб менял
    // измерение — петля, из которой и рождался флаппинг.
    // См. docs/decisions/reachability-belongs-to-the-session.md, риск 1.
    // ВОССТАНОВЛЕНИЕ висит на ДОКАЗАТЕЛЬСТВЕ, а не на состоянии presence. Сначала оно стояло под
    // переходом в offline — и оказалось почти недостижимым: «активность» освежается входящим
    // трафиком, поэтому пир, который нас не слышит, но сам шлёт, держал бы нас «в сети» вечно. Та
    // же подмена, что и в остальных находках этих суток, только этажом выше.
    maybe_recover_dead_outbound_leg(transport, &db_path, dialog_key, now_unix_ms);

    if contact_should_transition_offline(&contact, now_unix_ms) {
        storage.record_contact_presence_offline(&contact.contact_id, now_unix_ms)?;
        return Ok(());
    }

    // Пробовать нужно не тогда, когда на канале тихо, а тогда, когда мы не знаем, слышат ли НАС.
    // Приём чужого ping'а освежает «активность» и двигает срок следующей пробы, поэтому пассивная
    // сторона переставала пробовать вовсе: замер 2026-08-13 (75 с живого линка) — 8 проб у одной
    // ноды и НОЛЬ у другой, то есть у второй не появлялось доказательств о собственном плече.
    // Устаревшее подтверждение перебивает и срок, и пропуск по активности; пол по частоте держит
    // `presence_last_sent_at`, иначе такт очереди (250 мс) превратил бы это в поток проб.
    let last_activity_at_unix_ms = contact.presence_last_activity_at_unix_ms.unwrap_or_default();
    let last_sent_at_unix_ms = contact.presence_last_sent_at_unix_ms.unwrap_or_default();
    //
    // Порог тут — такт пробы, а не срок годности вердикта: обе стороны должны узнавать о себе
    // одинаково часто. С более мягким порогом пассивная сторона обновляла подтверждение втрое
    // реже активной (замер: 12 проб против 2 за 90 с), то есть её горизонт знания был втрое хуже
    // без всякой на то причины.
    let blind_about_our_own_leg =
        session_acknowledgement_age_for_dialog(&db_path, dialog_key, now_unix_ms)
            .is_none_or(|age_ms| age_ms >= CONTACT_PRESENCE_PROBE_MIN_INTERVAL_MS);
    let probe_floor_elapsed = now_unix_ms.saturating_sub(last_sent_at_unix_ms)
        >= CONTACT_PRESENCE_PROBE_MIN_INTERVAL_MS;
    let probe_because_blind = blind_about_our_own_leg && probe_floor_elapsed;

    if !probe_because_blind {
        let Some(next_due_at_unix_ms) = storage.next_contact_heartbeat_due_at(&contact.contact_id)?
        else {
            return Ok(());
        };
        if now_unix_ms < next_due_at_unix_ms {
            return Ok(());
        }
        if now_unix_ms.saturating_sub(last_activity_at_unix_ms)
            < CONTACT_PRESENCE_SKIP_WHEN_RECENT_ACTIVITY_MS
        {
            return Ok(());
        }
    }

    if last_sent_at_unix_ms > last_activity_at_unix_ms {
        storage.record_contact_presence_miss(&contact.contact_id, now_unix_ms)?;
    }

    send_presence_ping_to_session(storage, transport, &contact, dialog_key, now_unix_ms)?;

    Ok(())
}

/// ВОССТАНОВЛЕНИЕ — отдельное именованное решение, а не побочный эффект измерения.
///
/// Срабатывает только по доказательству (наши пробы не отвечены дольше порога), бьёт ТОЧНО в путь к
/// этому пиру и не чаще, чем раз в окно проб. Сброс фантомного линка заставляет передозвониться:
/// пока мёртвая-но-зарегистрированная запись жива, и живая отправка, и durable-сверка льются в
/// пустоту, а рандеву не дозванивается, считая, что мы уже слинкованы. Тот же механизм уже
/// применяет очередь отправки (`DROP_PHANTOM_LINK_AFTER_ATTEMPTS`) — здесь он получает второй
/// вход, для случая, когда очереди нет, а диалог всё равно надо чинить.
fn maybe_recover_dead_outbound_leg(
    transport: &TransportManager,
    db_path: &str,
    dialog_key: &str,
    now_unix_ms: i64,
) {
    if !session_outbound_leg_is_proven_dead(db_path, dialog_key, now_unix_ms) {
        return;
    }
    if !allow_session_recovery_attempt(db_path, dialog_key, now_unix_ms) {
        return;
    }
    let dropped = transport.drop_phantom_mesh_link_for_dialog_key(dialog_key);
    omni_app_trace_log(
        "presence/recovery",
        &format!("dialog_key={dialog_key} action=drop_phantom_link dropped={dropped}"),
    );
}

fn maintain_open_chat_session_leases(
    storage: &Storage,
    transport: &TransportManager,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let leases = list_open_chat_session_leases(storage)?;
    if leases.is_empty() {
        return Ok(());
    }

    for lease in leases
        .iter()
        .filter(|lease| lease.is_open_lease && !lease.auto_reconnect_paused)
    {
        if let Some(connection) = transport.connection_snapshot_for_dialog(&lease.dialog_key) {
            process_keepalive_for_dialog_connection(
                storage,
                transport,
                &db_path,
                &lease.dialog_key,
                &connection,
                now_unix_ms,
            )?;
            mark_runtime_chat_session_lease_triggered(&db_path, &lease.dialog_key)?;
        }
    }
    // No link for an open chat: nothing to do here. The mesh autoconnect worker keeps EVERY approved
    // contact dialed (LAN, punch, door, relay, rendezvous). This used to fall back to a direct QUIC
    // `connect` by IP hints on the pre-mesh transport, whose listener no longer runs — a dial
    // to nowhere every few seconds. docs/decisions/no-friend-tier.md (part 2).
    Ok(())
}

fn process_keepalive_for_dialog_connection(
    storage: &Storage,
    transport: &TransportManager,
    db_path: &str,
    dialog_key: &str,
    connection: &DialogConnectionSnapshot,
    now_unix_ms: i64,
) -> OmniResult<()> {
    let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? else {
        return Ok(());
    };
    if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
        return Ok(());
    }

    set_runtime_chat_session_endpoints(
        db_path,
        dialog_key,
        connection.local_endpoint.as_deref(),
        connection.peer_endpoint.as_deref(),
    )?;

    let last_sent_at_unix_ms = contact.presence_last_sent_at_unix_ms.unwrap_or_default();
    let should_bypass_keepalive_throttle = runtime_chat_session_should_bypass_keepalive_throttle(
        db_path,
        dialog_key,
        now_unix_ms,
        last_sent_at_unix_ms,
    )?;
    if last_sent_at_unix_ms > 0
        && now_unix_ms.saturating_sub(last_sent_at_unix_ms) < OPEN_CHAT_KEEPALIVE_INTERVAL_MS
        && !should_bypass_keepalive_throttle
    {
        return Ok(());
    }
    let last_activity_at_unix_ms = contact.presence_last_activity_at_unix_ms.unwrap_or_default();
    if last_sent_at_unix_ms > last_activity_at_unix_ms {
        // Промах — это ИЗМЕРЕНИЕ. Раньше он же рвал диалог (`disconnect_dialog`), то есть тишина
        // сама себя подтверждала: рвём → нечему отвечать → снова тишина. Теперь снос отдан
        // отдельному решению, которое требует доказательства и бьёт точно в путь.
        storage.record_contact_presence_miss(&contact.contact_id, now_unix_ms)?;
        if contact_should_transition_recently(&contact, now_unix_ms) {
            maybe_recover_dead_outbound_leg(transport, db_path, dialog_key, now_unix_ms);
        }
    }

    if connection.remote_account_id.is_none() || connection.remote_device_id.is_none() {
        return Ok(());
    }
    send_presence_ping_to_session(storage, transport, &contact, dialog_key, now_unix_ms)?;
    touch_runtime_chat_session_interaction(
        db_path,
        dialog_key,
        omni_types::ChatSessionInteractionKind::Keepalive,
        now_unix_ms,
    )?;

    Ok(())
}

fn expire_stale_contact_presence(storage: &Storage, now_unix_ms: i64) -> OmniResult<()> {
    let max_last_activity_at_unix_ms =
        now_unix_ms.saturating_sub(CONTACT_PRESENCE_RECENTLY_AFTER_MS);
    for contact in storage.list_contacts_due_for_presence_transition(max_last_activity_at_unix_ms)? {
        if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
            continue;
        }
        if contact.presence_state == omni_identity::ContactPresenceState::Offline {
            continue;
        }
        if contact_should_transition_offline(&contact, now_unix_ms) {
            storage.record_contact_presence_offline(&contact.contact_id, now_unix_ms)?;
            continue;
        }

        if contact_should_transition_recently(&contact, now_unix_ms)
            && contact.presence_state != omni_identity::ContactPresenceState::Recently
        {
            storage.record_contact_presence_recently(&contact.contact_id, now_unix_ms)?;
        }
    }

    Ok(())
}

fn contact_should_transition_recently(contact: &ContactSnapshot, now_unix_ms: i64) -> bool {
    contact
        .presence_last_activity_at_unix_ms
        .map(|last_activity_at_unix_ms| {
            now_unix_ms.saturating_sub(last_activity_at_unix_ms) >= CONTACT_PRESENCE_RECENTLY_AFTER_MS
        })
        .unwrap_or(false)
}

fn contact_should_transition_offline(contact: &ContactSnapshot, now_unix_ms: i64) -> bool {
    contact
        .presence_last_activity_at_unix_ms
        .map(|last_activity_at_unix_ms| {
            now_unix_ms.saturating_sub(last_activity_at_unix_ms) >= CONTACT_PRESENCE_OFFLINE_AFTER_MS
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendExistingMessageOutcome {
    Sent,
    DeferredUntilVerification,
    DeferredUntilSessionEstablished,
    DeferredUntilLiveSessionConfirmed,
    /// Придержано окном приёма: неподтверждённых конвертов на сессии уже столько, что следующий
    /// вынес бы окно и заклинил её. Это ШТАТНОЕ противодавление, а не признак поломки, поэтому у
    /// него отдельный исход: эскалации `DeferredUntilLiveSessionConfirmed` (снос «фантомного» линка
    /// по числу попыток) здесь были бы вредны — линк цел, просто пир ещё не прислал расписки.
    HeldByReceiveWindow,
    /// Придержано до прочтения отзывов аккаунта получателя: устройство могли отозвать, пока нас не
    /// было. Тоже ожидание, а не неудача доставки — попыткой не считается. Считалось (как
    /// `DeferredUntilSessionEstablished`) — и шесть таких ожиданий подряд доводили ПЕРВУЮ настоящую
    /// отправку до порога «фантомного линка», живая прямая связь сносилась на TCP-дверь (живьём
    /// 2026-10-10, файл ноутбук → ПК).
    HeldUntilRevocationsRead,
}

impl SendExistingMessageOutcome {
    /// Of two devices' deferrals, which one the message as a whole reports: the one that asks the
    /// queue for the most attention (a stuck live session beats a full window beats a pending
    /// bootstrap beats a pending verification).
    fn outranks(&self, other: &Self) -> bool {
        fn rank(outcome: &SendExistingMessageOutcome) -> u8 {
            match outcome {
                SendExistingMessageOutcome::Sent => 4,
                SendExistingMessageOutcome::DeferredUntilLiveSessionConfirmed => 3,
                SendExistingMessageOutcome::HeldByReceiveWindow => 2,
                SendExistingMessageOutcome::HeldUntilRevocationsRead => 1,
                SendExistingMessageOutcome::DeferredUntilSessionEstablished => 1,
                SendExistingMessageOutcome::DeferredUntilVerification => 0,
            }
        }
        rank(self) > rank(other)
    }
}

fn active_transport_matches_dialog(snapshot: &TransportSnapshot, dialog_key: &str) -> bool {
    matches!(
        snapshot.state,
        omni_types::TransportState::Connected | omni_types::TransportState::ListeningAndConnected
    ) && snapshot.dialog_key.as_deref() == Some(dialog_key)
}

fn effective_receipt_wait_before_retry_ms(
    snapshot: &TransportSnapshot,
    item: &omni_types::OutboundQueueRecord,
) -> i64 {
    if active_transport_matches_dialog(snapshot, &item.dialog_key) {
        RECEIPT_WAIT_BEFORE_RETRY_MS + ACTIVE_SESSION_ACK_GRACE_MS
    } else {
        RECEIPT_WAIT_BEFORE_RETRY_MS
    }
}

/// Delay before the next delivery retry. The first `MAX_RETRY_ATTEMPTS` use the fast
/// `RETRY_DELAY_MS`; beyond that we keep retrying (never bury an undelivered message to a
/// contact) with exponential backoff capped at `MAX_RETRY_BACKOFF_MS`. Durable
/// store-and-forward backs eventual delivery across non-overlapping online windows, so the
/// slow retry is just an opportunistic accelerator, not a busy-loop. See
/// docs/decisions/delivery-reliability-model.md.
fn delivery_retry_backoff_ms(attempt_count: i64) -> i64 {
    if attempt_count < MAX_RETRY_ATTEMPTS {
        return RETRY_DELAY_MS;
    }
    let over = (attempt_count - MAX_RETRY_ATTEMPTS).clamp(0, 6) as u32;
    (RETRY_DELAY_MS.saturating_mul(1i64 << over)).min(MAX_RETRY_BACKOFF_MS)
}

#[cfg(test)]
mod stalled_download_tests {
    use super::*;

    const START: i64 = 1_000_000;
    const PAST_THE_THRESHOLD: i64 = ATTACHMENT_DOWNLOAD_REQUEST_RETRY_AFTER_MS + 1;

    #[test]
    fn a_download_that_is_moving_is_never_asked_for_again() {
        let mut watch = StalledDownloadWatch::started(0, START);
        // Even far past the stall threshold: what matters is that bytes arrived, not how fast.
        let mut now = START;
        for chunk in 1..=4_i64 {
            now += PAST_THE_THRESHOLD;
            assert_eq!(
                watch.observe(chunk * 1024, now),
                StalledDownloadAction::Progressing
            );
        }
        assert_eq!(watch.retries_sent, 0);
    }

    #[test]
    fn a_pause_shorter_than_the_threshold_is_not_a_stall() {
        let mut watch = StalledDownloadWatch::started(4096, START);

        assert_eq!(
            watch.observe(4096, START + ATTACHMENT_DOWNLOAD_REQUEST_RETRY_AFTER_MS - 1),
            StalledDownloadAction::KeepWaiting
        );
        assert_eq!(watch.retries_sent, 0);
    }

    #[test]
    fn a_stall_is_asked_about_less_and_less_often_but_never_given_up() {
        let mut watch = StalledDownloadWatch::started(0, START);
        let mut now = START;
        let mut pause = ATTACHMENT_DOWNLOAD_REQUEST_RETRY_AFTER_MS;
        // An hour and more of a dead link: still asked, at most every ten minutes.
        for attempt in 1..=12_u32 {
            assert_eq!(watch.observe(0, now + pause - 1), StalledDownloadAction::KeepWaiting, "attempt {attempt}");
            now += pause;
            assert_eq!(watch.observe(0, now), StalledDownloadAction::Resend, "attempt {attempt}");
            assert_eq!(watch.retries_sent, attempt);
            pause = (pause * 2).min(ATTACHMENT_DOWNLOAD_REQUEST_MAX_PAUSE_MS);
        }
        assert_eq!(stalled_download_pause_ms(12), ATTACHMENT_DOWNLOAD_REQUEST_MAX_PAUSE_MS);
    }

    #[test]
    fn a_byte_arriving_after_a_retry_restores_the_full_budget() {
        let mut watch = StalledDownloadWatch::started(0, START);
        let mut now = START + PAST_THE_THRESHOLD;
        assert_eq!(watch.observe(0, now), StalledDownloadAction::Resend);

        // The retry worked: the sender woke up and chunks are arriving again.
        now += 100;
        assert_eq!(watch.observe(65_536, now), StalledDownloadAction::Progressing);
        assert_eq!(watch.retries_sent, 0);

        // A later stall is judged on its own, not on the earlier one's budget.
        now += PAST_THE_THRESHOLD;
        assert_eq!(watch.observe(65_536, now), StalledDownloadAction::Resend);
    }
}

#[cfg(test)]
mod runtime_queue_unit_tests {
    use super::*;

    fn sent_queue_item(dialog_key: &str) -> omni_types::OutboundQueueRecord {
        omni_types::OutboundQueueRecord {
            message_id: "msg-1".to_string(),
            dialog_key: dialog_key.to_string(),
            status: MessageStatus::Sent,
            attempt_count: 1,
            next_attempt_unix_ms: None,
            last_attempt_unix_ms: Some(10),
            last_failure_kind: None,
            last_failure_message: None,
            remote_delivered_at_unix_ms: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    #[test]
    fn active_dialog_receipt_timeout_gets_extra_grace() {
        let snapshot = TransportSnapshot {
            state: omni_types::TransportState::Connected,
            transport_mode: omni_types::TransportMode::Quic,
            peer_id: Some("127.0.0.1:40123".to_string()),
            dialog_key: Some("dialog:instance:peer-1".to_string()),
            remote_instance_id: Some("peer-1".to_string()),
            remote_device_name: None,
            remote_account_id: Some("acct-peer-1".to_string()),
            initiated_locally: true,
            duplicate_resolved: false,
            last_error: None,
        };
        let item = sent_queue_item("dialog:instance:peer-1");

        assert_eq!(
            effective_receipt_wait_before_retry_ms(&snapshot, &item),
            RECEIPT_WAIT_BEFORE_RETRY_MS + ACTIVE_SESSION_ACK_GRACE_MS
        );
    }

    #[test]
    fn inactive_dialog_receipt_timeout_uses_base_window() {
        let snapshot = TransportSnapshot {
            state: omni_types::TransportState::Connected,
            transport_mode: omni_types::TransportMode::Quic,
            peer_id: Some("127.0.0.1:40123".to_string()),
            dialog_key: Some("dialog:instance:peer-2".to_string()),
            remote_instance_id: Some("peer-2".to_string()),
            remote_device_name: None,
            remote_account_id: Some("acct-peer-2".to_string()),
            initiated_locally: true,
            duplicate_resolved: false,
            last_error: None,
        };
        let item = sent_queue_item("dialog:instance:peer-1");

        assert_eq!(
            effective_receipt_wait_before_retry_ms(&snapshot, &item),
            RECEIPT_WAIT_BEFORE_RETRY_MS
        );
    }

    /// docs/bugs/message-stuck-sending-after-restart.md: a row left in «sending» is retried like an
    /// unconfirmed «sent» one, once the receipt wait has passed — not before.
    #[test]
    fn a_row_left_sending_is_retried_after_the_receipt_wait() {
        let snapshot = TransportSnapshot {
            state: omni_types::TransportState::Connected,
            transport_mode: omni_types::TransportMode::Quic,
            peer_id: None,
            dialog_key: None,
            remote_instance_id: None,
            remote_device_name: None,
            remote_account_id: None,
            initiated_locally: true,
            duplicate_resolved: false,
            last_error: None,
        };
        let mut item = sent_queue_item("dialog:instance:peer-1");
        item.status = MessageStatus::Sending;
        assert!(!overdue_for_retry(&snapshot, &item, 10 + RECEIPT_WAIT_BEFORE_RETRY_MS - 1));
        assert!(overdue_for_retry(&snapshot, &item, 10 + RECEIPT_WAIT_BEFORE_RETRY_MS));
        item.status = MessageStatus::Delivered;
        assert!(!overdue_for_retry(&snapshot, &item, i64::MAX / 2));
    }

    #[test]
    fn delivery_retry_backoff_grows_then_caps_and_never_overflows() {
        // First MAX_RETRY_ATTEMPTS retry fast; beyond that we KEEP retrying (never bury) with
        // exponential backoff capped at MAX_RETRY_BACKOFF_MS. Guards the no-bury contract —
        // see docs/decisions/delivery-reliability-model.md.
        assert_eq!(delivery_retry_backoff_ms(0), RETRY_DELAY_MS);
        assert_eq!(delivery_retry_backoff_ms(MAX_RETRY_ATTEMPTS - 1), RETRY_DELAY_MS);
        assert_eq!(delivery_retry_backoff_ms(MAX_RETRY_ATTEMPTS), RETRY_DELAY_MS);
        assert!(delivery_retry_backoff_ms(MAX_RETRY_ATTEMPTS + 1) > RETRY_DELAY_MS);
        assert!(
            delivery_retry_backoff_ms(MAX_RETRY_ATTEMPTS + 2)
                > delivery_retry_backoff_ms(MAX_RETRY_ATTEMPTS + 1)
        );
        assert_eq!(delivery_retry_backoff_ms(1_000_000), MAX_RETRY_BACKOFF_MS);
        assert!(delivery_retry_backoff_ms(i64::MAX) <= MAX_RETRY_BACKOFF_MS);
    }
}

/// A row whose delivery receipt is overdue. «Sending» past the same wait is an attempt that never
/// finished (the process went down mid-send, or a side path left it there): it is as overdue as a
/// «sent» one — the retry pass looked at «sent» only, and such a row hung for good.
fn overdue_for_retry(snapshot: &TransportSnapshot, item: &omni_types::OutboundQueueRecord, now: i64) -> bool {
    if item.status != MessageStatus::Sent && item.status != MessageStatus::Sending {
        return false;
    }
    let last_attempt_unix_ms = item.last_attempt_unix_ms.unwrap_or(item.updated_at_unix_ms);
    last_attempt_unix_ms <= now - effective_receipt_wait_before_retry_ms(snapshot, item)
}

fn promote_overdue_sent_messages(
    storage: &Storage,
    snapshot: &TransportSnapshot,
    now: i64,
) -> OmniResult<()> {
    // Filter outside the transaction so we only enter SQLite write-mode when
    // there is actually work to do. Each survivor will run ≥4 SQL operations
    // inside mark_queue_retry / mark_queue_failed; batching them into a single
    // BEGIN IMMEDIATE / COMMIT eliminates per-statement fsync overhead.
    // See docs/decisions/storage-batching.md.
    let overdue: Vec<_> = storage
        .outbound_queue_snapshot()?
        .items
        .into_iter()
        .filter(|item| overdue_for_retry(snapshot, item, now))
        .collect();

    if overdue.is_empty() {
        return Ok(());
    }

    storage.with_immediate_transaction("promote_overdue_sent_messages", |storage| {
        for item in &overdue {
            // Never bury a message because an ack was slow. Earlier this called
            // `mark_queue_failed` after MAX_RETRY_ATTEMPTS, so any outage longer than a few
            // seconds permanently lost the message — the opposite of how a messenger must
            // behave. Keep it in the retry queue with a capped exponential backoff: it
            // delivers when the link/peer recovers, and the retry path falls through to
            // durable store-and-forward (`hold_durable`) on a link-down attempt, so it is
            // delivered even across non-overlapping online windows (Signal retries to
            // delivery; Reticulum/LXMF propagation holds until fetched). The recipient's
            // delivery receipt is what finally marks it Delivered. A truly terminal state
            // (contact removed) is handled when the contact is forgotten, not here. See
            // docs/decisions/delivery-reliability-model.md.
            storage.mark_queue_retry(
                &item.message_id,
                QueueFailureKind::AckTimeout,
                "delivery receipt not yet received; keeping the message queued (durable S&F backs delivery)",
                now + delivery_retry_backoff_ms(item.attempt_count),
                now,
            )?;
        }
        Ok(())
    })
}

fn attempt_send_existing_message(
    storage: &Storage,
    transport: &TransportManager,
    message_id: &str,
) -> OmniResult<ChatMessageRecord> {
    let existing = storage.get_message(message_id)?;
    // Its file is still being prepared: nothing to offer yet. It stays queued, untouched — no
    // attempt counted — and goes on the first tick after the preparation ends.
    if crate::app_attachment_preparation::attachment_is_preparing(storage, message_id) {
        return Ok(existing);
    }

    match send_existing_message_encrypted(storage, transport, &existing) {
        Ok(SendExistingMessageOutcome::Sent) => {
            let sent = storage.update_message_status(message_id, MessageStatus::Sent, current_unix_ms());
            // First time out — including after attempts that were only deferred (a session still
            // being set up: the usual first message right after a link comes up). Those leave the
            // row `retrying`, and copying from `queued` alone sent no copy at all; the device then
            // waited for the reconcile pass. A retry after a sent attempt copies only to devices
            // that have not confirmed it.
            if matches!(existing.status, MessageStatus::Queued | MessageStatus::Retrying) {
                copy_to_own_devices(storage, transport, &existing);
            }
            sent
        }
        Ok(SendExistingMessageOutcome::DeferredUntilVerification) => Ok(existing),
        Ok(SendExistingMessageOutcome::HeldByReceiveWindow) => {
            // Ждём расписок, а не чинки: линк цел, сессия цела, просто окно приёма заполнено.
            // Счётчик попыток НЕ трогаем — иначе штатное противодавление копило бы «неудачи» и
            // дотягивало до эскалаций, которые сносят живой линк.
            let now = current_unix_ms();
            storage.mark_queue_retry(
                message_id,
                QueueFailureKind::AckTimeout,
                "receive window is full; waiting for delivery receipts before sending more",
                now + delivery_retry_backoff_ms(1),
                now,
            )?;
            Ok(existing)
        }
        Ok(SendExistingMessageOutcome::HeldUntilRevocationsRead) => {
            // Ждём прочтения отзывов (см. вариант) — счётчик попыток не трогаем.
            let now = current_unix_ms();
            storage.mark_queue_retry(
                message_id,
                QueueFailureKind::NoActiveSession,
                "waiting to read the recipient account's revocations",
                now + delivery_retry_backoff_ms(1),
                now,
            )?;
            Ok(existing)
        }
        Ok(SendExistingMessageOutcome::DeferredUntilSessionEstablished) => {
            let updated_at = current_unix_ms();
            let _ = storage.mark_queue_attempt_started(message_id, updated_at)?;
            // Don't bury — the crypto session establishes when the (possibly offline) peer
            // comes back; keep retrying with capped backoff instead of failing after a fixed
            // timeout. See docs/decisions/delivery-reliability-model.md.
            let attempt_count = storage.queue_attempt_count_for_message(message_id)?;
            storage.mark_queue_retry(
                message_id,
                QueueFailureKind::NoActiveSession,
                "waiting for encrypted session bootstrap acknowledgement",
                updated_at + delivery_retry_backoff_ms(attempt_count),
                updated_at,
            )
        }
        Ok(SendExistingMessageOutcome::DeferredUntilLiveSessionConfirmed) => {
            let updated_at = current_unix_ms();
            let _ = storage.mark_queue_attempt_started(message_id, updated_at)?;
            let attempt_count = storage.queue_attempt_count_for_message(message_id)?;
            // Phantom-link escalation from the DEFER path. The keepalive-wait gate in
            // `send_existing_message` holds the message here (before the real send) while the
            // open lease has a live carrier snapshot but no confirmed keepalive round-trip. On an
            // asymmetric link (mobile UDP flow killed one-way) that keepalive can NEVER confirm, so
            // the message would defer forever and never reach the send-path phantom-drop. The
            // attempt counter still advances one-per-tick across both paths, so dropping the
            // dead-but-registered Link exactly once at the same threshold clears the phantom
            // carrier snapshot: the next tick no longer defers (no snapshot ⇒ gate falls through to
            // the real send + durable backstop), and the auto-connect / rendezvous re-dial rebuilds
            // a FRESH path over which the durable-held frame reconciles. See
            // docs/decisions/delivery-reliability-model.md.
            if attempt_count == DROP_PHANTOM_LINK_AFTER_ATTEMPTS {
                let existing = storage.get_message(message_id)?;
                let dropped =
                    transport.drop_phantom_mesh_link_for_dialog_key(&existing.dialog_key);
                omni_app_trace_log(
                    "queue/phantom_drop",
                    &format!(
                        "message={message_id} dialog_key={} attempts={attempt_count} \
                         dropped={dropped} path=deferred_keepalive",
                        existing.dialog_key
                    ),
                );
            }
            storage.mark_queue_retry(
                message_id,
                QueueFailureKind::NoActiveSession,
                "waiting for confirmed chat-session keepalive",
                updated_at + delivery_retry_backoff_ms(attempt_count),
                updated_at,
            )
        }
        Err(error) => {
            let updated_at = current_unix_ms();
            let attempt_count = storage.queue_attempt_count_for_message(message_id)?;
            // Don't bury on a transport write error — it's almost always transient (link
            // churn / momentary unreachability). Keep retrying with capped backoff; a
            // genuinely malformed envelope fails at build time, before here. See
            // docs/decisions/delivery-reliability-model.md.
            storage.mark_queue_retry(
                message_id,
                QueueFailureKind::TransportWriteFailed,
                &error.to_string(),
                updated_at + delivery_retry_backoff_ms(attempt_count),
                updated_at,
            )?;
            Ok(storage.get_message(message_id)?)
        }
    }
}

fn attempt_send_attachment_download_request(
    storage: &Storage,
    transport: &TransportManager,
    message_id: &str,
) -> OmniResult<AttachmentRuntimeInfo> {
    let requested_at_unix_ms = current_unix_ms();
    let runtime = storage
        .get_attachment_runtime_info(message_id)?
        .ok_or_else(|| OmniError::Storage(format!("attachment '{}' was not found", message_id)))?;
    settle_live_attachment_receive_writer(storage, &runtime);
    let (runtime, resume_offset_bytes) =
        storage.attachment_download_resume(&runtime.attachment.message_id, requested_at_unix_ms)?;
    // A group's file is pulled from its holder by the mesh tick (`app_group_attachments`); there is
    // no 1:1 session to ask over.
    if runtime.dialog_key.starts_with("group:") {
        return Ok(runtime);
    }
    let transfer_id = runtime.active_transfer_id.clone().ok_or_else(|| {
        OmniError::Storage("attachment download request is missing its active transfer id".to_string())
    })?;
    let payload = omni_types::WirePayloadV1::LogicalEvent(
        omni_types::LogicalEventEnvelopeV1::attachment_download_request(
            transfer_id,
            runtime.attachment.message_id.clone(),
            resume_offset_bytes,
            requested_at_unix_ms,
        ),
    );
    // Where the request goes: a device of ours, the dialog the file came on, or another device of
    // the contact (below).
    let source_dialog: String;
    // A copy of our own message: the file is on the device of ours that sent it. A contact's file:
    // first another device of ours that may have downloaded it already (near, and there when the
    // contact is not), then the contact.
    let own_source = match own_device_for_dialog(storage, &runtime.dialog_key)? {
        Some(device_id) => Some((device_id, runtime.dialog_key.clone())),
        None => own_device_download_source(storage, transport, &runtime.attachment.message_id),
    };
    let mut outgoing = if let Some((device_id, device_dialog)) = own_source {
        source_dialog = device_dialog;
        let (_profile, local_account, _device) = active_profile_context(storage)?;
        prepare_encrypted_payload_for_session_address(
            storage,
            &omni_crypto::SessionAddress {
                remote_account_id: local_account.descriptor.account_id,
                remote_device_id: device_id,
            },
            &payload,
            SecurityPayloadKind::AttachmentDownloadRequest,
        )?
    } else {
        let contact = storage
            .resolve_contact_for_dialog(&runtime.dialog_key)?
            .ok_or_else(|| {
                OmniError::Transport("attachment contact is not available for the current dialog".to_string())
            })?;
        let remote_account_id = contact.remote_account_id.clone().ok_or_else(|| {
            OmniError::Transport("attachment contact does not have a remote_account_id".to_string())
        })?;
        source_dialog = download_source_dialog(storage, transport, &contact, &remote_account_id, &runtime.dialog_key);
        prepare_encrypted_payload_for_remote_account(
            storage,
            transport,
            &remote_account_id,
            Some(&source_dialog),
            &payload,
            SecurityPayloadKind::AttachmentDownloadRequest,
        )?
        .0
    };
    storage.store_session_record(&mut outgoing.updated_session)?;
    // Both outcomes are traced because the failure mode this exists for is silence: the request
    // going out and nothing coming back looks exactly like the request never going out.
    // See docs/bugs/attachment-transfer-never-starts.md.
    if let Err(error) = transport.send_wire_envelope_for_dialog_key(&source_dialog, &outgoing.envelope) {
        omni_app_trace_log(
            "attachment/download-request",
            &format!(
                "dialog_key={} transfer={} message={} send FAILED: {error}",
                runtime.dialog_key,
                runtime.active_transfer_id.as_deref().unwrap_or("?"),
                runtime.attachment.message_id,
            ),
        );
        return Err(error);
    }
    #[cfg(test)]
    count_download_request_sent(storage, &runtime.attachment.message_id);
    omni_app_trace_log(
        "attachment/download-request",
        &format!(
            "dialog_key={} transfer={} message={} sent resume_offset={resume_offset_bytes} to={source_dialog}",
            runtime.dialog_key,
            runtime.active_transfer_id.as_deref().unwrap_or("?"),
            runtime.attachment.message_id,
        ),
    );
    storage.mark_attachment_downloading(
        &runtime.attachment.message_id,
        runtime.attachment.bytes_transferred,
        requested_at_unix_ms,
    )
}

#[cfg(test)]
fn download_requests_sent() -> &'static Mutex<HashMap<(String, String), u32>> {
    static SENT: OnceLock<Mutex<HashMap<(String, String), u32>>> = OnceLock::new();
    SENT.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn count_download_request_sent(storage: &Storage, message_id: &str) {
    let key = (storage.db_path().to_string_lossy().into_owned(), message_id.to_string());
    *download_requests_sent().lock().unwrap().entry(key).or_default() += 1;
}

/// How many download requests for `message_id` the profile at `db_path` has sent.
#[cfg(test)]
pub(crate) fn download_requests_sent_for(db_path: &str, message_id: &str) -> u32 {
    let key = (db_path.to_string(), message_id.to_string());
    download_requests_sent().lock().unwrap().get(&key).copied().unwrap_or(0)
}

/// A download that has been requested but has not moved a single byte, and for how long.
///
/// Tracked in memory rather than in the schema on purpose: this is a retry budget for the current
/// process, not a fact about the transfer. A restart resuming from scratch is correct — after a
/// restart the request genuinely has not been sent by anybody.
#[derive(Debug, Clone)]
struct StalledDownloadWatch {
    bytes_transferred: i64,
    unchanged_since_unix_ms: i64,
    retries_sent: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StalledDownloadAction {
    /// Bytes moved since the last look. Whatever it is doing, it is not stuck.
    Progressing,
    /// No movement, but not for long enough to call it a stall.
    KeepWaiting,
    /// Ask for the file again.
    Resend,
}

/// How long the stall must last before retry number `retries_sent + 1`.
fn stalled_download_pause_ms(retries_sent: u32) -> i64 {
    ATTACHMENT_DOWNLOAD_REQUEST_RETRY_AFTER_MS
        .saturating_mul(1_i64 << retries_sent.min(16))
        .min(ATTACHMENT_DOWNLOAD_REQUEST_MAX_PAUSE_MS)
}

impl StalledDownloadWatch {
    fn started(bytes_transferred: i64, now_unix_ms: i64) -> Self {
        Self {
            bytes_transferred,
            unchanged_since_unix_ms: now_unix_ms,
            retries_sent: 0,
        }
    }

    /// Fold one observation of the transfer's progress in, and say what to do about it. Split from
    /// the pass so the policy is testable without a peer that can be made to lose a request.
    fn observe(&mut self, bytes_transferred: i64, now_unix_ms: i64) -> StalledDownloadAction {
        // Any progress at all clears the budget: a slow transfer is not a stalled one.
        if bytes_transferred != self.bytes_transferred {
            self.bytes_transferred = bytes_transferred;
            self.unchanged_since_unix_ms = now_unix_ms;
            self.retries_sent = 0;
            return StalledDownloadAction::Progressing;
        }
        if now_unix_ms.saturating_sub(self.unchanged_since_unix_ms)
            < stalled_download_pause_ms(self.retries_sent)
        {
            return StalledDownloadAction::KeepWaiting;
        }
        self.retries_sent = self.retries_sent.saturating_add(1);
        self.unchanged_since_unix_ms = now_unix_ms;
        StalledDownloadAction::Resend
    }
}

/// Re-send the download request for transfers that stopped moving (never started, or a link that
/// broke mid-file: the request carries the resume offset, so it continues at the first block the
/// download's map lacks).
///
/// The gap this closes: `attempt_send_attachment_download_request` moves the row to `downloading`,
/// which takes it out of the pending set forever, so the request was sent exactly once. If it was
/// lost, or the peer dropped it, the transfer sat at zero with no progress, no error and no retry —
/// observed once on a live mobile link, and from the user's side it is simply a file that never
/// moves. The sender coming back does not wait for this pass: `request_downloads_on_session_up`.
///
/// Safe to repeat: the sender reserves a worker per transfer id and ignores a duplicate request for
/// a transfer already in flight (`spawn_attachment_transfer_sender`), and the resume offset is
/// recomputed from the block map on every attempt, so a retry that races a transfer which just
/// started costs one small control frame.
///
/// One pass over every dialog a download can come from (the active one and our other devices): the
/// budgets of downloads outside the pass would be dropped as finished.
fn retry_stalled_attachment_downloads(
    storage: &Storage,
    transport: &TransportManager,
    worker_state: &mut QueueWorkerState,
    dialog_keys: &[String],
    now_unix_ms: i64,
) -> OmniResult<()> {
    let mut downloading = Vec::new();
    for dialog_key in dialog_keys {
        downloading.extend(storage.list_downloading_attachments_for_dialog(dialog_key)?);
    }
    // Anything no longer downloading is finished, failed or paused; its budget goes with it.
    worker_state.stalled_downloads.retain(|message_id, _| {
        downloading
            .iter()
            .any(|runtime| &runtime.attachment.message_id == message_id)
    });

    for runtime in downloading {
        let dialog_key = &runtime.dialog_key;
        let message_id = runtime.attachment.message_id.clone();
        let bytes = runtime.attachment.bytes_transferred.max(0);
        let watch = worker_state
            .stalled_downloads
            .entry(message_id.clone())
            .or_insert_with(|| StalledDownloadWatch::started(bytes, now_unix_ms));
        let stalled_for = now_unix_ms.saturating_sub(watch.unchanged_since_unix_ms);
        match watch.observe(bytes, now_unix_ms) {
            StalledDownloadAction::Progressing | StalledDownloadAction::KeepWaiting => continue,
            StalledDownloadAction::Resend => {}
        }
        let attempt = watch.retries_sent;
        omni_app_trace_log(
            "attachment/download-request",
            &format!(
                "dialog_key={dialog_key} message={message_id} stalled at {bytes}B for \
                 {stalled_for}ms, re-sending request (retry {attempt}, next after {}ms)",
                stalled_download_pause_ms(attempt)
            ),
        );
        // A failed retry is already traced by the send itself and must not abort the pass: the
        // other stalled downloads still deserve their attempt.
        let _ = attempt_send_attachment_download_request(storage, transport, &message_id);
    }

    Ok(())
}

/// How long a device of ours that said it does not have a file is not asked for it again.
const OWN_DEVICE_NOT_HERE_FOR_MS: i64 = 10 * 60_000;

/// (db path, message id) → devices of ours that said they do not have its file, and when.
type OwnNotHere = HashMap<(String, String), Vec<(String, i64)>>;

fn own_not_here() -> &'static Mutex<OwnNotHere> {
    static OWN_NOT_HERE: OnceLock<Mutex<OwnNotHere>> = OnceLock::new();
    OWN_NOT_HERE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A device of ours said it does not have the file of `message_id`: the next request goes on.
pub(crate) fn note_own_device_lacks_file(db_path: &str, message_id: &str, device_id: &DeviceId) {
    if let Ok(mut not_here) = own_not_here().lock() {
        let devices = not_here.entry((db_path.to_string(), message_id.to_string())).or_default();
        devices.retain(|(device, _)| device != &device_id.0);
        devices.push((device_id.0.clone(), current_unix_ms()));
    }
}

/// Forget what our devices said they lack (`stop_runtime_context`).
pub(crate) fn clear_own_device_not_here_for_db(db_path: &str) {
    if let Ok(mut not_here) = own_not_here().lock() {
        not_here.retain(|(db, _), _| db != db_path);
    }
}

/// A device of ours to ask for a contact's file first (docs/decisions/attachment-content-tree.md,
/// «свои устройства получателя»): one we have a link and a session with, that has not just said
/// it lacks the file. It hands on its downloaded copy, over the LAN as often as not, and when the
/// contact is away; one without the file says so at once and the request goes to the contact.
fn own_device_download_source(storage: &Storage, transport: &TransportManager, message_id: &str) -> Option<(DeviceId, String)> {
    let db_path = storage.db_path().to_string_lossy().into_owned();
    let now = current_unix_ms();
    let lacking: Vec<String> = own_not_here()
        .lock()
        .ok()
        .and_then(|not_here| not_here.get(&(db_path, message_id.to_string())).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, at)| now - at < OWN_DEVICE_NOT_HERE_FOR_MS)
        .map(|(device, _)| device)
        .collect();
    let (_profile, local_account, _device) = active_profile_context(storage).ok()?;
    storage.list_own_device_dialogs().ok()?.into_iter().find(|(device_id, dialog_key)| {
        !lacking.contains(&device_id.0)
            && transport.connection_snapshot_for_dialog(dialog_key).is_some()
            && storage
                .get_session_record(&omni_crypto::SessionAddress {
                    remote_account_id: local_account.descriptor.account_id.clone(),
                    remote_device_id: device_id.clone(),
                })
                .ok()
                .flatten()
                .is_some_and(|session| session.bootstrap_state == omni_crypto::SessionBootstrapState::Established)
    })
}

/// Which of the contact's devices a download request goes to (docs/decisions/attachment-content-tree.md,
/// 3.3): the one the file came from while we have a link to it; else another device of the contact
/// we have a link and a session with — the sender's other device that holds the file hands it on,
/// and one that does not stays silent. None linked: the file's own dialog, where the request waits.
fn download_source_dialog(
    storage: &Storage,
    transport: &TransportManager,
    contact: &ContactSnapshot,
    remote_account_id: &AccountId,
    file_dialog: &str,
) -> String {
    if transport.connection_snapshot_for_dialog(file_dialog).is_some() {
        return file_dialog.to_string();
    }
    let established = |dialog_key: &str| {
        storage
            .get_session_for_dialog(remote_account_id, dialog_key)
            .ok()
            .flatten()
            .is_some_and(|session| session.bootstrap_state == omni_crypto::SessionBootstrapState::Established)
    };
    storage
        .list_contact_device_dialogs(&contact.contact_id)
        .unwrap_or_default()
        .into_iter()
        .map(|link| link.dialog_key)
        .find(|dialog_key| {
            dialog_key != file_dialog
                && transport.connection_snapshot_for_dialog(dialog_key).is_some()
                && established(dialog_key)
        })
        .unwrap_or_else(|| file_dialog.to_string())
}

/// The source a 1:1 download asks (as `attempt_send_attachment_download_request` picks it) is one
/// we have a link to. A group's file is pulled by its coordinator, which keeps its own writer.
fn download_source_is_linked(storage: &Storage, transport: &TransportManager, runtime: &AttachmentRuntimeInfo) -> bool {
    if runtime.dialog_key.starts_with("group:") {
        return false;
    }
    let linked = |dialog_key: &str| transport.connection_snapshot_for_dialog(dialog_key).is_some();
    if own_device_for_dialog(storage, &runtime.dialog_key).ok().flatten().is_some() {
        return linked(&runtime.dialog_key);
    }
    if own_device_download_source(storage, transport, &runtime.attachment.message_id).is_some() {
        return true;
    }
    let Some(contact) = storage.resolve_contact_for_dialog(&runtime.dialog_key).ok().flatten() else {
        return false;
    };
    let Some(remote_account_id) = contact.remote_account_id.clone() else {
        return false;
    };
    linked(&download_source_dialog(storage, transport, &contact, &remote_account_id, &runtime.dialog_key))
}

/// A session with a contact (or a device of ours) came up: the downloads from it do not wait for
/// the stall retry — after a long break that is up to `ATTACHMENT_DOWNLOAD_REQUEST_MAX_PAUSE_MS` —
/// but are asked for again now, each from the first block its map lacks
/// (docs/decisions/attachment-content-tree.md, «Устойчивость к сети»). A stream that still runs
/// drops the duplicate request (`spawn_attachment_transfer_sender`); the next progress resets the
/// stall budget.
pub(crate) fn request_downloads_on_session_up(storage: &Storage, transport: &TransportManager, dialog_key: &str) {
    let mut dialog_keys = vec![omni_types::normalize_dialog_key(dialog_key)];
    if let Ok(chat_dialog_key) = storage.chat_dialog_key_for(dialog_key) {
        if !dialog_keys.contains(&chat_dialog_key) {
            dialog_keys.push(chat_dialog_key);
        }
    }
    for dialog_key in dialog_keys {
        let _ = process_pending_attachment_downloads_for_active_dialog(storage, transport, &dialog_key);
        for runtime in storage.list_downloading_attachments_for_dialog(&dialog_key).unwrap_or_default() {
            omni_app_trace_log(
                "attachment/download-request",
                &format!(
                    "dialog_key={dialog_key} message={} session up at {}B, asking again",
                    runtime.attachment.message_id, runtime.attachment.bytes_transferred
                ),
            );
            let _ = attempt_send_attachment_download_request(storage, transport, &runtime.attachment.message_id);
        }
    }
}

fn choose_attachment_chunk_size_bytes(
    file_size_bytes: i64,
    resume_offset_bytes: i64,
) -> usize {
    let remaining_bytes =
        file_size_bytes.saturating_sub(resume_offset_bytes.max(0)) as usize;
    remaining_bytes.clamp(16 * 1024, 1024 * 1024)
}

/// How often the queue checks that our other devices have the copies of what this one sent — the
/// path that catches what the immediate copy and the on-link catch-up missed (a session that was
/// still being set up, a copy lost in flight).
const OWN_DEVICE_RECONCILE_INTERVAL_MS: i64 = 15_000;
/// At most this many copies to one device per pass: each spends the session's send counter, and the
/// receiver's window is `MAX_RECEIVE_GAP` wide.
const OWN_DEVICE_COPIES_PER_PASS: usize = (omni_crypto::MAX_RECEIVE_GAP / 4) as usize;

/// Hand our other devices copies of `message`, right after its first send
/// (docs/decisions/multi-device-equal-nodes.md, 2.4). Best effort: reconciliation catches misses.
fn copy_to_own_devices(storage: &Storage, transport: &TransportManager, message: &ChatMessageRecord) {
    let Ok(Some(contact)) = storage.resolve_contact_for_dialog(&message.dialog_key) else {
        return;
    };
    let Some(contact_account_id) = contact.remote_account_id else {
        return;
    };
    let has_it = storage.message_delivered_devices(&message.id).unwrap_or_default();
    for (device_id, device_dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
        if has_it.contains(&device_id) {
            continue;
        }
        if let Err(error) = send_sent_transcript_to_own_device(
            storage,
            transport,
            message,
            &contact_account_id,
            &device_id,
            &device_dialog_key,
        ) {
            omni_app_trace_log(
                "own_devices/copy",
                &format!("message={} device={} failed: {error}", message.id, device_id.0),
            );
        }
    }
}

/// The device of ours behind `dialog_key`, when the dialog is one of our other devices.
fn own_device_for_dialog(storage: &Storage, dialog_key: &str) -> OmniResult<Option<DeviceId>> {
    Ok(storage
        .list_own_device_dialogs()?
        .into_iter()
        .find(|(_, device_dialog_key)| device_dialog_key == dialog_key)
        .map(|(device_id, _)| device_id))
}

/// Backoff for the periodic reconcile of one device of ours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OwnDeviceBackoff {
    /// Passes in a row that sent at least one copy (the device had not confirmed the last ones).
    streak: u32,
    next_pass_at_unix_ms: i64,
}

const OWN_DEVICE_RECONCILE_MAX_BACKOFF_MS: i64 = 10 * 60 * 1000;

impl OwnDeviceBackoff {
    /// Fold the outcome of a pass in: a pass that had nothing to send means the device is caught
    /// up; one that sent copies waits twice as long as the one before, up to the cap.
    fn after_pass(self, copies_sent: usize, now_unix_ms: i64) -> Self {
        if copies_sent == 0 {
            return Self::default();
        }
        let streak = self.streak.saturating_add(1);
        let wait = OWN_DEVICE_RECONCILE_INTERVAL_MS
            .saturating_mul(1i64 << streak.min(16))
            .min(OWN_DEVICE_RECONCILE_MAX_BACKOFF_MS);
        Self {
            streak,
            next_pass_at_unix_ms: now_unix_ms.saturating_add(wait),
        }
    }
}

/// Every reachable device of ours: send it the copies it still lacks.
///
/// Each copy spends the session's send counter, and the receiving device only accepts a bounded gap
/// of them (`MAX_RECEIVE_GAP`), so a device that does not answer must not be fed one every pass:
/// a device with no live link is skipped (the on-link catch-up covers its return) and one that
/// keeps not confirming is retried at growing intervals.
fn reconcile_own_devices(
    storage: &Storage,
    transport: &TransportManager,
    backoff: &mut std::collections::HashMap<String, OwnDeviceBackoff>,
    revocations_told_to_contacts: &mut std::collections::HashSet<String>,
    now_unix_ms: i64,
) {
    tell_revoked_own_devices(storage, transport, backoff, now_unix_ms);
    let _ = tell_contacts_about_revocations(storage, transport, revocations_told_to_contacts);
    tell_kept_own_devices_of_key_move(storage, transport, backoff, now_unix_ms);
    let _ = tell_contacts_about_key_move(storage, transport, revocations_told_to_contacts, None);
    apply_account_mailbox_inbox(storage, transport);
    apply_revocation_notice_inbox(storage, transport);
    run_held_contact_catch_ups(storage, transport);
    sync_contacts_with_own_devices(storage, transport, now_unix_ms);
    sync_profile_with_own_devices(storage, transport);
    for (device_id, device_dialog_key) in storage.list_own_device_dialogs().unwrap_or_default() {
        if transport.connection_snapshot_for_dialog(&device_dialog_key).is_none() {
            backoff.remove(&device_id.0);
            continue;
        }
        let state = backoff.get(&device_id.0).copied().unwrap_or_default();
        if now_unix_ms < state.next_pass_at_unix_ms {
            continue;
        }
        match catch_up_own_device_counted(storage, transport, &device_id, &device_dialog_key) {
            Ok(copies_sent) => {
                backoff.insert(device_id.0.clone(), state.after_pass(copies_sent, now_unix_ms));
            }
            Err(error) => omni_app_trace_log(
                "own_devices/catch_up",
                &format!("device={} failed: {error}", device_id.0),
            ),
        }
    }
}

/// Copies of what this device sent in the catch-up window that `device_id` has not confirmed.
fn catch_up_own_device(
    storage: &Storage,
    transport: &TransportManager,
    device_id: &DeviceId,
    device_dialog_key: &str,
) -> OmniResult<()> {
    catch_up_own_device_counted(storage, transport, device_id, device_dialog_key).map(|_| ())
}

/// [`catch_up_own_device`], reporting how many copies went out.
fn catch_up_own_device_counted(
    storage: &Storage,
    transport: &TransportManager,
    device_id: &DeviceId,
    device_dialog_key: &str,
) -> OmniResult<usize> {
    let since = current_unix_ms() - DEVICE_CATCH_UP_WINDOW_MS;
    let mut sent = 0;
    for message in storage.list_own_sent_messages_since(since)? {
        if sent >= OWN_DEVICE_COPIES_PER_PASS {
            break;
        }
        if storage.message_delivered_devices(&message.id)?.contains(device_id) {
            continue;
        }
        let Some(contact) = storage.resolve_contact_for_dialog(&message.dialog_key)? else {
            continue;
        };
        let Some(contact_account_id) = contact.remote_account_id else {
            continue;
        };
        if !send_sent_transcript_to_own_device(
            storage,
            transport,
            &message,
            &contact_account_id,
            device_id,
            device_dialog_key,
        )? {
            break; // no session yet: its bootstrap is on the way, the next pass continues
        }
        sent += 1;
    }
    Ok(sent)
}

/// The established session with one of our own devices. When there is none yet, this starts one —
/// its bootstrap goes out now (at most once per reissue window) — and returns `None`: the caller
/// tries again on a later pass. `waits` names, for the trace, what is waiting for the session.
fn established_session_with_own_device(
    storage: &Storage,
    transport: &TransportManager,
    own_account_id: &AccountId,
    device_id: &DeviceId,
    device_dialog_key: &str,
    waits: &str,
) -> OmniResult<Option<omni_crypto::SessionRecord>> {
    let now_unix_ms = current_unix_ms();
    let session = storage.get_session_record(&omni_crypto::SessionAddress {
        remote_account_id: own_account_id.clone(),
        remote_device_id: device_id.clone(),
    })?;
    match session {
        Some(session) if session.bootstrap_state == omni_crypto::SessionBootstrapState::Established => {
            Ok(Some(session))
        }
        Some(session) if now_unix_ms - session.updated_at_unix_ms < SESSION_BOOTSTRAP_REISSUE_AFTER_MS => {
            Ok(None)
        }
        _ => {
            let Some(bundle) = storage.get_remote_prekey_bundle(own_account_id, device_id)? else {
                return Ok(None);
            };
            let (_session, bootstrap, _observed_at) =
                bootstrap_session_for_remote_bundle(storage, &bundle, now_unix_ms)?;
            let envelope = omni_messaging::wrap_session_bootstrap(bootstrap)?;
            transport.send_wire_envelope_for_dialog_key(device_dialog_key, &envelope)?;
            omni_app_trace_log(
                "own_devices/copy",
                &format!("device={} session bootstrap sent ({waits})", device_id.0),
            );
            Ok(None)
        }
    }
}

/// One copy to one of our devices. `false` when there is no session with it yet — then this starts
/// one (and sends nothing else), the way a message to a contact's new device does.
fn send_sent_transcript_to_own_device(
    storage: &Storage,
    transport: &TransportManager,
    message: &ChatMessageRecord,
    contact_account_id: &AccountId,
    device_id: &DeviceId,
    device_dialog_key: &str,
) -> OmniResult<bool> {
    let (_profile, local_account, _device) = active_profile_context(storage)?;
    let own_account_id = local_account.descriptor.account_id;
    let now_unix_ms = current_unix_ms();
    let Some(session) = established_session_with_own_device(
        storage,
        transport,
        &own_account_id,
        device_id,
        device_dialog_key,
        &format!("message={} waits", message.id),
    )?
    else {
        return Ok(false);
    };
    // The same offer the contact got: the copy downloads the file from this device. A file still
    // being prepared has none yet — the copy waits for it.
    if crate::app_attachment_preparation::attachment_is_preparing(storage, &message.id) {
        return Ok(false);
    }
    let attachment = storage
        .get_attachment_runtime_info(&message.id)?
        .map(|runtime| runtime.offer_for(&message.id, &message.body, message.created_at_unix_ms));
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        &session,
        &omni_types::LogicalEventEnvelopeV1::sent_transcript(omni_types::SentTranscriptV1 {
            contact_account_id: contact_account_id.0.clone(),
            message_id: message.id.clone(),
            body: message.body.clone(),
            created_at_unix_ms: message.created_at_unix_ms,
            attachment,
        }),
        SecurityPayloadKind::SentTranscript,
        now_unix_ms,
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(device_dialog_key, &outgoing.envelope)?;
    omni_app_trace_log(
        "own_devices/copy",
        &format!("device={} message={} sent", device_id.0, message.id),
    );
    Ok(true)
}

/// How long the sender keeps sending a device of a contact what it missed while away: after that
/// the device catches up from its own account's devices (docs/decisions/multi-device-equal-nodes.md).
const DEVICE_CATCH_UP_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A contact's device came online: send it what went out while it was away — messages another of
/// the contact's devices may already have confirmed, so the queue no longer holds them. The
/// message's own status is left alone.
fn catch_up_contact_device(
    storage: &Storage,
    transport: &TransportManager,
    device_dialog_key: &str,
    device_id: &DeviceId,
) -> OmniResult<()> {
    catch_up_contact_device_since(storage, transport, device_dialog_key, device_id, i64::MIN)
}

/// `catch_up_contact_device` with nothing older than `not_before_unix_ms`: a session that just came
/// up owes its device what went out while it was being built — not what was written before we knew
/// of the device (a device restored or added later gets that from the history, «со слов»).
fn catch_up_contact_device_since(
    storage: &Storage,
    transport: &TransportManager,
    device_dialog_key: &str,
    device_id: &DeviceId,
    not_before_unix_ms: i64,
) -> OmniResult<()> {
    let Some(contact) = storage.resolve_contact_for_dialog(device_dialog_key)? else {
        return Ok(());
    };
    let Some(remote_account_id) = contact.remote_account_id.clone() else {
        return Ok(());
    };
    if !contact_revocations_settled(storage, &remote_account_id, current_unix_ms()) {
        omni_app_trace_log(
            "queue/catch_up",
            &format!("device={} held: the account's revocations are not read yet", device_id.0),
        );
        hold_contact_catch_up(&storage.db_path().to_string_lossy(), device_dialog_key, device_id);
        return Ok(());
    }
    let chat_dialog_key = storage.chat_dialog_key_for(device_dialog_key)?;
    let since = (current_unix_ms() - DEVICE_CATCH_UP_WINDOW_MS).max(not_before_unix_ms);
    let target = DeliveryTarget {
        dialog_key: device_dialog_key.to_string(),
        device_id: Some(device_id.clone()),
    };
    for message in storage.list_sent_messages_since(&chat_dialog_key, since)? {
        if storage.message_delivered_devices(&message.id)?.contains(device_id) {
            continue;
        }
        if let Err(error) = send_existing_message_to_device(
            storage,
            transport,
            &message,
            &contact,
            &remote_account_id,
            &target,
            false,
        ) {
            omni_app_trace_log(
                "queue/catch_up",
                &format!("message={} device={} failed: {error}", message.id, device_id.0),
            );
        }
    }
    Ok(())
}

/// One device of the chat's contact to deliver to (docs/decisions/multi-device-equal-nodes.md).
struct DeliveryTarget {
    /// The device's own dialog — what the transport routes on.
    dialog_key: String,
    /// `None` while no session has told us which device answers there: the account's latest
    /// session and prekey bundle serve it, as before multi-device.
    device_id: Option<DeviceId>,
}

/// Every device of the contact that has not confirmed the message yet. A contact whose devices are
/// not known yet has one target: the chat's own dialog.
fn delivery_targets(
    storage: &Storage,
    contact: &ContactSnapshot,
    existing: &ChatMessageRecord,
) -> OmniResult<Vec<DeliveryTarget>> {
    let delivered = storage.message_delivered_devices(&existing.id)?;
    let mut targets: Vec<DeliveryTarget> = Vec::new();
    for link in storage.list_contact_device_dialogs(&contact.contact_id)? {
        let Some(device_id) = link.remote_device_id else {
            continue;
        };
        if delivered.contains(&device_id)
            || targets.iter().any(|target| target.device_id.as_ref() == Some(&device_id))
        {
            continue;
        }
        targets.push(DeliveryTarget {
            dialog_key: link.dialog_key,
            device_id: Some(device_id),
        });
    }
    // Our own devices confirm copies in the same table; only a confirmation from the contact means
    // the chat's dialog has been served.
    let own_account_id = storage.get_local_account()?.map(|account| account.descriptor.account_id);
    let mut contact_confirmed = false;
    for device_id in &delivered {
        let own = match &own_account_id {
            Some(account_id) => storage.is_own_other_device(account_id, device_id)?,
            None => false,
        };
        if !own {
            contact_confirmed = true;
            break;
        }
    }
    if targets.is_empty() && !contact_confirmed {
        targets.push(DeliveryTarget {
            dialog_key: existing.dialog_key.clone(),
            device_id: None,
        });
    }
    Ok(targets)
}

/// Send `existing` to every device of its contact. Sent once any device took it; otherwise the
/// most telling deferral, so the queue backs off the way it would for a single device.
fn send_existing_message_encrypted(
    storage: &Storage,
    transport: &TransportManager,
    existing: &ChatMessageRecord,
) -> OmniResult<SendExistingMessageOutcome> {
    let _ = active_profile_context(storage)?;
    let contact_card = storage.resolve_contact_card_for_dialog(&existing.dialog_key)?;
    let Some(contact_card) = contact_card else {
        return Ok(SendExistingMessageOutcome::DeferredUntilVerification);
    };

    let contact = contact_card.contact;
    if contact.is_blocked {
        return Err(OmniError::Transport(
            "cannot send encrypted message to a blocked contact".to_string(),
        ));
    }
    let remote_account_id = contact.remote_account_id.clone().ok_or_else(|| {
        OmniError::Transport("contact does not have a remote_account_id".to_string())
    })?;
    let targets = delivery_targets(storage, &contact, existing)?;
    if targets.is_empty() {
        // Every known device confirmed it already.
        return Ok(SendExistingMessageOutcome::Sent);
    }
    let mut first_error = None;
    let mut deferral: Option<SendExistingMessageOutcome> = None;
    for target in &targets {
        match send_existing_message_to_device(
            storage,
            transport,
            existing,
            &contact,
            &remote_account_id,
            target,
            true,
        ) {
            Ok(SendExistingMessageOutcome::Sent) => {
                deferral = Some(SendExistingMessageOutcome::Sent);
            }
            Ok(outcome) => {
                if deferral.as_ref().is_none_or(|current| outcome.outranks(current)) {
                    deferral = Some(outcome);
                }
            }
            Err(error) => {
                omni_app_trace_log(
                    "queue/device",
                    &format!(
                        "message={} device={:?} dialog_key={} failed: {error}",
                        existing.id, target.device_id, target.dialog_key
                    ),
                );
                first_error.get_or_insert(error);
            }
        }
    }
    match (deferral, first_error) {
        (Some(outcome), _) => Ok(outcome),
        (None, Some(error)) => Err(error),
        (None, None) => Ok(SendExistingMessageOutcome::Sent),
    }
}

fn send_existing_message_to_device(
    storage: &Storage,
    transport: &TransportManager,
    existing: &ChatMessageRecord,
    contact: &ContactSnapshot,
    remote_account_id: &AccountId,
    target: &DeliveryTarget,
    // The queue's own attempt: only then does the queue row record it. A catch-up of one device is
    // a side send — it marked the row «sending», nothing put it back to «sent», and the retry pass
    // looks at «sent» rows only: the message hung in «sending» for good (stand 2026-10-13,
    // docs/bugs/message-stuck-sending-after-restart.md).
    queue_driven: bool,
) -> OmniResult<SendExistingMessageOutcome> {
    let now_unix_ms = current_unix_ms();
    // A device its account may have taken back while we were off: not before we have heard.
    if target.device_id.is_some() && !contact_revocations_settled(storage, remote_account_id, now_unix_ms) {
        omni_app_trace_log(
            "queue/device",
            &format!(
                "message={} device={:?} held: the account's revocations are not read yet",
                existing.id, target.device_id
            ),
        );
        return Ok(SendExistingMessageOutcome::HeldUntilRevocationsRead);
    }
    let current_session = match &target.device_id {
        Some(device_id) => storage.get_session_record(&omni_crypto::SessionAddress {
            remote_account_id: remote_account_id.clone(),
            remote_device_id: device_id.clone(),
        })?,
        None => storage.get_latest_session_for_remote_account(remote_account_id)?,
    };
    let remote_bundle = || -> OmniResult<omni_crypto::PrekeyBundle> {
        match &target.device_id {
            Some(device_id) => storage.get_remote_prekey_bundle(remote_account_id, device_id)?,
            None => storage.get_latest_remote_prekey_bundle_for_account(remote_account_id)?,
        }
        .ok_or_else(|| {
            OmniError::Transport(
                "no remote prekey bundle is available for the selected contact".to_string(),
            )
        })
    };
    let mut session = match current_session {
        Some(existing_session)
            if existing_session.bootstrap_state
                == omni_crypto::SessionBootstrapState::Established =>
        {
            existing_session
        }
        Some(existing_session) => {
            if now_unix_ms - existing_session.updated_at_unix_ms
                < SESSION_BOOTSTRAP_REISSUE_AFTER_MS
            {
                return Ok(SendExistingMessageOutcome::DeferredUntilSessionEstablished);
            }
            let mut bundle = remote_bundle()?;
            bundle.one_time_prekey = None;
            let (session, bootstrap, observed_at) =
                bootstrap_session_for_remote_bundle(storage, &bundle, now_unix_ms)?;
            let envelope = omni_messaging::wrap_session_bootstrap(bootstrap.clone())?;
            transport.send_wire_envelope_for_dialog_key(&target.dialog_key, &envelope)?;
            storage.upsert_message_security_metadata(
                &format!(
                    "bootstrap::{}::{}::{}",
                    session.session_address.remote_account_id.0,
                    session.session_address.remote_device_id.0,
                    observed_at
                ),
                &existing.dialog_key,
                Some(&contact.contact_id),
                Some(&session.session_address),
                Some(&session.session_address.remote_account_id),
                Some(&session.session_address.remote_device_id),
                MessageEncryptionState::BootstrapSent.as_db_value(),
                Some(omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated),
                Some(SecurityPayloadKind::SessionBootstrap.as_db_value()),
                observed_at,
                now_unix_ms,
            )?;
            return Ok(SendExistingMessageOutcome::DeferredUntilSessionEstablished);
        }
        None => {
            let bundle = remote_bundle()?;
            let (session, bootstrap, observed_at) =
                bootstrap_session_for_remote_bundle(storage, &bundle, now_unix_ms)?;
            let envelope = omni_messaging::wrap_session_bootstrap(bootstrap.clone())?;
            transport.send_wire_envelope_for_dialog_key(&target.dialog_key, &envelope)?;
            storage.upsert_message_security_metadata(
                &format!(
                    "bootstrap::{}::{}::{}",
                    session.session_address.remote_account_id.0,
                    session.session_address.remote_device_id.0,
                    observed_at
                ),
                &existing.dialog_key,
                Some(&contact.contact_id),
                Some(&session.session_address),
                Some(&session.session_address.remote_account_id),
                Some(&session.session_address.remote_device_id),
                MessageEncryptionState::BootstrapSent.as_db_value(),
                Some(omni_crypto::SessionBootstrapState::InitiatorBootstrapCreated),
                Some(SecurityPayloadKind::SessionBootstrap.as_db_value()),
                observed_at,
                now_unix_ms,
            )?;
            return Ok(SendExistingMessageOutcome::DeferredUntilSessionEstablished);
        }
    };

    let db_path = storage.db_path().to_string_lossy().to_string();
    if transport.connection_snapshot_for_dialog(&existing.dialog_key).is_some()
        && runtime_chat_session_open_lease_waits_for_keepalive(
            &db_path,
            &existing.dialog_key,
            now_unix_ms,
        )?
    {
        return Ok(SendExistingMessageOutcome::DeferredUntilLiveSessionConfirmed);
    }

    // A delivered message going to one more device (catch-up of a device that was away, or new) is
    // not back in the queue: its status and queue row stay as they are. Marking the attempt turned
    // it into "sending" with a queue row, and a device that never confirms (switched off for good)
    // left the spinner on an old message forever (live 2026-10-07).
    if queue_driven && existing.status != MessageStatus::Delivered {
        storage.mark_queue_attempt_started(&existing.id, current_unix_ms())?;
    }

    // Повтор шлёт ТОТ ЖЕ конверт. Пересборка на каждую попытку тратила счётчик сессии, и при такте
    // ретраев ~4/с окно приёма пира (64) вылетало за ~16 секунд — после чего отвергалось всё, включая
    // presence-пинги, и петля кормила сама себя (955 отказов на живом замере).
    // См. docs/bugs/session-wedged-by-counter-gap.md.
    // Ключ ЭКЗЕМПЛЯРА сессии: по адресу пира кэш пережил бы пересборку и отдавал конверт под
    // мёртвыми ключами. Тем же ключом считается и число конвертов «в полёте» — записи умершей
    // сессии не должны занимать окно приёма новой.
    let session_key = session_instance_key(&session);
    if let Some(envelope) = cached_outgoing_envelope(&db_path, &existing.id, &session_key) {
        omni_app_trace_log(
            "queue/resend",
            &format!("message={} reused the pending envelope (counter untouched)", existing.id),
        );
        return finish_encrypted_send(
            storage,
            transport,
            existing,
            &target.dialog_key,
            &contact.contact_id,
            &envelope,
        );
    }

    // ОКНО ПРИЁМНИКА — ОГРАНИЧЕНИЕ ОТПРАВИТЕЛЯ. Дальше собирается НОВЫЙ конверт, то есть тратится
    // счётчик. Приёмник принимает конверт, только если его счётчик опережает принятый не больше чем
    // на `MAX_RECEIVE_GAP`; выложив за раз больше разных конвертов, мы гарантированно выносим окно —
    // и сессия заклинивает намертво, потому что отвергается уже ВСЁ, включая presence-пинги.
    //
    // Живой замер 2026-08-14 показал, что это не теория: очередь из 200 сообщений давала разрыв 377
    // при окне 64. Хуже того, так ломалась и сама починка — пересобранная сессия немедленно
    // перешифровывала всю очередь и выносила окно заново, то есть пересборка кормила петлю вместо
    // того чтобы её разорвать.
    //
    // Держим половину окна: остаётся запас на presence-пинги, расписки и профильные синхронизации,
    // которые тратят счётчик мимо очереди. Сообщение при этом никуда не девается — остаётся в
    // очереди и уйдёт, как только расписки освободят место.
    let in_flight = outgoing_envelopes_in_flight_for_session(&db_path, &session_key);
    let in_flight_limit = (omni_crypto::MAX_RECEIVE_GAP / 2) as usize;
    if in_flight >= in_flight_limit {
        omni_app_trace_log(
            "queue/paced",
            &format!(
                "message={} held: {in_flight} unconfirmed envelopes on session {session_key} (limit {in_flight_limit}, receive window {})",
                existing.id,
                omni_crypto::MAX_RECEIVE_GAP
            ),
        );
        return Ok(SendExistingMessageOutcome::HeldByReceiveWindow);
    }

    let outgoing = if let Some(attachment) = storage.get_attachment_runtime_info(&existing.id)? {
        build_encrypted_attachment_offer_envelope(
            &session,
            attachment.offer_for(&existing.id, &existing.body, existing.created_at_unix_ms),
            current_unix_ms(),
        )?
    } else {
        build_encrypted_text_envelope(
            &session,
            &existing.id,
            &existing.body,
            existing.created_at_unix_ms,
            current_unix_ms(),
        )?
    };
    session = outgoing.updated_session.clone();
    storage.store_session_record(&mut session)?;
    storage.upsert_message_security_metadata(
        &existing.id,
        &existing.dialog_key,
        Some(&contact.contact_id),
        Some(&session.session_address),
        Some(&session.session_address.remote_account_id),
        Some(&session.session_address.remote_device_id),
        MessageEncryptionState::EncryptedPending.as_db_value(),
        Some(session.bootstrap_state),
        Some(outgoing.payload_kind.as_db_value()),
        existing.created_at_unix_ms,
        current_unix_ms(),
    )?;
    remember_outgoing_envelope(&db_path, &existing.id, &session_key, &outgoing.envelope);
    // Счётчик отправки — В ТРАССЕ, потому что весь разбор заклинивания до сих пор выводил разрыв
    // КОСВЕННО (по числу отказов на той стороне), а прямого числа не было ни у кого. Пишем только
    // при сборке НОВОГО конверта: повтор счётчик не тратит, и строка тем самым считает ровно то,
    // от чего растёт разрыв. Пара к `recv/accepted` на приёмнике.
    omni_app_trace_log(
        "queue/encrypted",
        &format!(
            "message={} send_counter={} session={session_key}",
            existing.id, session.send_counter
        ),
    );
    finish_encrypted_send(
        storage,
        transport,
        existing,
        &target.dialog_key,
        &contact.contact_id,
        &outgoing.envelope,
    )
}

/// Общий хвост отправки — одинаковый и для свежесобранного конверта, и для повторно отправленного.
/// Вынесен, чтобы эскалации (durable-подстраховка, сброс фантомного линка) считались по попыткам, а
/// не по числу пересборок: раньше это было одно и то же, теперь нет.
fn finish_encrypted_send(
    storage: &Storage,
    transport: &TransportManager,
    existing: &ChatMessageRecord,
    device_dialog_key: &str,
    contact_id: &ContactId,
    envelope: &omni_types::WireEnvelopeV1,
) -> OmniResult<SendExistingMessageOutcome> {
    transport.send_wire_envelope_for_dialog_key(device_dialog_key, envelope)?;
    // Increment 3 (delivery-reliability-model.md): once a message has used up the fast-retry budget
    // still unconfirmed, ALSO back it with durable store-and-forward. A mesh `Sent` only means
    // "handed to QUIC" — over a relay whose route to the peer isn't re-established yet (e.g. right
    // after a relay restart) it black-holes, and the live ack-timeout retry can stall. Fire exactly
    // once (== threshold; mark_queue_attempt_started above bumped the count for this attempt) so the
    // bounded durable store isn't re-filled on every retry; the recipient dedups by message id if
    // both the live send and the durable reconcile land. Best-effort — never blocks the send result.
    let attempts_so_far = storage.queue_attempt_count_for_message(&existing.id)?;
    if attempts_so_far == DURABLE_BACKSTOP_AFTER_ATTEMPTS {
        let _ = transport.hold_durable_for_dialog_key(device_dialog_key, envelope);
    }
    // Phantom-link escalation: still unconfirmed well past the durable backstop ⇒ the mesh Link is
    // almost certainly dead-but-registered (mobile UDP flow killed), so BOTH the live send and the
    // durable reconcile above are pouring into the void. Drop the Link ONCE so the auto-connect /
    // rendezvous re-dial (which otherwise skips a "linked" peer) rebuilds a FRESH path, over which
    // the durable-held frame reconciles. A genuinely-live link just coalesces back on the next dial.
    if attempts_so_far == DROP_PHANTOM_LINK_AFTER_ATTEMPTS {
        let dropped = transport.drop_phantom_mesh_link_for_dialog_key(device_dialog_key);
        omni_app_trace_log(
            "queue/phantom_drop",
            &format!(
                "message={} dialog_key={device_dialog_key} attempts={attempts_so_far} dropped={dropped}",
                existing.id
            ),
        );
    }
    storage.record_contact_presence_activity(contact_id, current_unix_ms())?;
    Ok(SendExistingMessageOutcome::Sent)
}

fn bootstrap_session_for_remote_bundle(
    storage: &Storage,
    bundle: &omni_crypto::PrekeyBundle,
    now_unix_ms: i64,
) -> OmniResult<(omni_crypto::SessionRecord, SessionBootstrapMessageV1, i64)> {
    let (_profile, local_account, local_device) = active_profile_context(storage)?;
    let local_identity_key = storage
        .get_local_identity_key(&local_account.descriptor.account_id)?
        .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
    let local_device_key = storage
        .get_local_device_key(&local_device.device_id)?
        .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;

    let (mut session, bootstrap) =
        bootstrap_initiator_session(&local_identity_key, &local_device_key, bundle, now_unix_ms)?;
    storage.replace_session_record(&mut session)?;
    let replay_window = storage.ensure_replay_window(&session.session_address, now_unix_ms)?;
    storage.store_replay_window(&replay_window)?;

    let bootstrap_v1 = SessionBootstrapMessageV1 {
        version: SESSION_BOOTSTRAP_MESSAGE_VERSION_V1,
        initiator_account_id: bootstrap.initiator_account_id.0,
        initiator_device_id: bootstrap.initiator_device_id.0,
        initiator_identity_public_key: identity_public_key_to_wire(bootstrap.initiator_identity_key),
        initiator_identity_signing_public_key: identity_signing_public_key_to_wire(
            bootstrap.initiator_identity_signing_key,
        ),
        initiator_device_public_key: device_public_key_to_wire(bootstrap.initiator_device_key),
        initiator_ephemeral_public_key: ephemeral_public_key_to_wire(bootstrap.initiator_ephemeral_key),
        remote_signed_prekey_id: bootstrap.remote_signed_prekey_id,
        remote_one_time_prekey_id: bootstrap.remote_one_time_prekey_id,
        bootstrap_signature_r: signature_to_wire(bootstrap.bootstrap_signature.clone()).0,
        bootstrap_signature_s: signature_to_wire(bootstrap.bootstrap_signature.clone()).1,
        created_at_unix_ms: bootstrap.created_at_unix_ms,
        initiator_instance_id: storage.get_or_create_local_instance_id()?,
        initiator_device_certificate_chain_json: local_device_certificate_chain_json(storage)?,
    };

    Ok((session, bootstrap_v1, now_unix_ms))
}

#[cfg(test)]
mod own_device_backoff_tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    #[test]
    fn a_device_that_is_caught_up_is_checked_again_at_the_normal_interval() {
        let state = OwnDeviceBackoff::default().after_pass(0, NOW);
        assert_eq!(state, OwnDeviceBackoff::default());
    }

    #[test]
    fn a_device_that_keeps_not_confirming_is_retried_at_growing_intervals() {
        let mut state = OwnDeviceBackoff::default();
        let mut previous_wait = 0;
        for _ in 0..4 {
            state = state.after_pass(3, NOW);
            let wait = state.next_pass_at_unix_ms - NOW;
            assert!(wait > previous_wait, "wait must grow: {wait} after {previous_wait}");
            previous_wait = wait;
        }
    }

    #[test]
    fn the_wait_is_capped_and_never_overflows() {
        let mut state = OwnDeviceBackoff::default();
        for _ in 0..100 {
            state = state.after_pass(1, i64::MAX - 1);
        }
        assert_eq!(state.next_pass_at_unix_ms, i64::MAX);
        let capped = OwnDeviceBackoff::default().after_pass(1, NOW);
        let mut late = capped;
        for _ in 0..40 {
            late = late.after_pass(1, NOW);
        }
        assert_eq!(late.next_pass_at_unix_ms - NOW, OWN_DEVICE_RECONCILE_MAX_BACKOFF_MS);
    }

    #[test]
    fn confirming_resets_the_backoff() {
        let backed_off = OwnDeviceBackoff::default().after_pass(2, NOW).after_pass(2, NOW);
        assert_eq!(backed_off.after_pass(0, NOW), OwnDeviceBackoff::default());
    }
}
