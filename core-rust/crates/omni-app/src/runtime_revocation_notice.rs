// A revocation through the couriers (docs/decisions/multi-device-equal-nodes.md, «Отзыв через
// маячок»). A contact hears that we took a device back over the next live link with any device of
// ours. One that is never on at the same time as our devices — or talks only to the device we took
// back, which is the one that should not be trusted — would never hear it. So while a revocation is
// fresh, the pair beacon of every device we rendezvous with carries a mark (`revoked_through_ms`), and
// the signed revocation itself waits under the pair secret in the pair's notice: a beacon cannot
// carry it (a BT-DHT value holds ~1000 bytes, one revocation is ~960). A reader fetches the notice
// only when it sees a mark it has not read yet, so the notice costs no reads otherwise.
//
// The notice is sealed and posted the way the account mailbox is (`omni_rendezvous::seal_mailbox`
// under the pair secret instead of the account secret), to the web couriers only.

/// How long a revocation rides the beacons: as long as a mailbox note lives in the account mailbox.
const REVOCATION_NOTICE_KEEP_MS: i64 = ACCOUNT_MAILBOX_KEEP_MS;

/// An unchanged notice is posted again this often: ntfy keeps a message ~12 h.
const REVOCATION_NOTICE_REPUBLISH_MS: i64 = ACCOUNT_MAILBOX_REPUBLISH_MS;

/// A mark whose notice was not found (not posted yet, a courier lagging) is looked for again after this.
const REVOCATION_NOTICE_RETRY_MS: i64 = 10 * 60 * 1000;

/// After the start, sending to a contact's devices waits at most this long for the beacons of every
/// pair with that contact's account to be read (maintainers' decision 2026-10-10, variant 1 in
/// docs/bugs/catch-up-reaches-a-revoked-device-before-its-revocation.md): otherwise what was queued
/// for a device its account took back while we were off goes to it before we hear of that.
const REVOCATION_CHECK_GRACE_MS: i64 = 15_000;

/// What the beacon of one pair says and its notice holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevocationNotice {
    /// The newest revocation in the notice.
    pub mark_ms: u64,
    /// The notice: the revocations, newest first, as many as fit a mailbox note.
    pub content: Vec<u8>,
}

#[derive(Default)]
struct RevocationNoticeState {
    /// Pair secret → when we posted its notice, and its mark.
    published: HashMap<[u8; 32], (i64, u64)>,
    /// Pair secret → the newest mark we have read the notice for, and when we last looked.
    read: HashMap<[u8; 32], (u64, i64)>,
    /// Read, not yet applied: the pair secret it came under, and the revocation.
    inbox: Vec<([u8; 32], omni_crypto::DeviceRevocationV1)>,
    /// Pair secrets whose beacons were read at the couriers since the start.
    beacons_read: std::collections::HashSet<[u8; 32]>,
    /// When sending first asked whether a contact's revocations are settled (0 = not yet).
    first_asked_at_unix_ms: i64,
    /// Catch-ups of a contact's device held until its account's revocations are settled:
    /// (device dialog, device).
    held_catch_ups: Vec<(String, DeviceId)>,
}

static REVOCATION_NOTICES: std::sync::OnceLock<Mutex<HashMap<String, RevocationNoticeState>>> =
    std::sync::OnceLock::new();

fn with_revocation_notices<T>(db_path: &str, f: impl FnOnce(&mut RevocationNoticeState) -> T) -> T {
    let registry = REVOCATION_NOTICES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.entry(db_path.to_string()).or_default())
}

/// The beacons of the pair with `secret` were read at the couriers (whatever they said).
pub(crate) fn note_pair_beacons_read(db_path: &str, secret: &[u8; 32]) {
    with_revocation_notices(db_path, |state| {
        state.beacons_read.insert(*secret);
    });
}

/// Whether the beacons of the pair with `secret` were read since the start.
pub(crate) fn pair_beacons_read(db_path: &str, secret: &[u8; 32]) -> bool {
    with_revocation_notices(db_path, |state| state.beacons_read.contains(secret))
}

/// Whether we may send to the devices of `account_id` without first hearing of a revocation it made
/// while we were off: the beacons of every pair with it were read and what they pointed to applied —
/// or [`REVOCATION_CHECK_GRACE_MS`] passed, or there are no couriers to wait for.
pub(crate) fn contact_revocations_settled(storage: &Storage, account_id: &AccountId, now_unix_ms: i64) -> bool {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let couriers_running = rendezvous_runtime_registry()
        .lock()
        .map(|registry| registry.contains_key(&db_path))
        .unwrap_or(false);
    if !couriers_running {
        return true;
    }
    let secrets: Vec<[u8; 32]> = rendezvous_peers_for_account(storage, account_id)
        .into_iter()
        .map(|peer| peer.secret)
        .collect();
    with_revocation_notices(&db_path, |state| revocations_settled_in(state, &secrets, now_unix_ms))
}

/// The rule of [`contact_revocations_settled`] over one profile's state and the account's pairs.
fn revocations_settled_in(state: &mut RevocationNoticeState, secrets: &[[u8; 32]], now_unix_ms: i64) -> bool {
    if state.first_asked_at_unix_ms == 0 {
        state.first_asked_at_unix_ms = now_unix_ms;
    }
    now_unix_ms - state.first_asked_at_unix_ms >= REVOCATION_CHECK_GRACE_MS
        || (secrets.iter().all(|secret| state.beacons_read.contains(secret))
            && !state.inbox.iter().any(|(secret, _)| secrets.contains(secret)))
}

/// Hold the catch-up of a contact's device until its account's revocations are settled.
fn hold_contact_catch_up(db_path: &str, device_dialog_key: &str, device_id: &DeviceId) {
    with_revocation_notices(db_path, |state| {
        if !state.held_catch_ups.iter().any(|(dialog, device)| dialog == device_dialog_key && device == device_id) {
            state.held_catch_ups.push((device_dialog_key.to_string(), device_id.clone()));
        }
    });
}

/// Run the held catch-ups whose account is settled now. Caller holds the session lock.
fn run_held_contact_catch_ups(storage: &Storage, transport: &TransportManager) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let held = with_revocation_notices(&db_path, |state| std::mem::take(&mut state.held_catch_ups));
    let now = current_unix_ms();
    for (dialog_key, device_id) in held {
        let account = storage
            .resolve_contact_for_dialog(&dialog_key)
            .ok()
            .flatten()
            .and_then(|contact| contact.remote_account_id);
        match account {
            Some(account) if !contact_revocations_settled(storage, &account, now) => {
                hold_contact_catch_up(&db_path, &dialog_key, &device_id);
            }
            Some(_) => {
                if let Err(error) = catch_up_contact_device(storage, transport, &dialog_key, &device_id) {
                    omni_app_trace_log("queue/catch_up", &format!("held device={} failed: {error}", device_id.0));
                }
            }
            None => {}
        }
    }
}

pub(crate) fn clear_revocation_notices_for_profile(db_path: &str) {
    if let Some(registry) = REVOCATION_NOTICES.get() {
        if let Ok(mut guard) = registry.lock() {
            guard.remove(db_path);
        }
    }
}

/// The notice for the pair with a device of `account_id` (a contact's, or our own): our account's
/// revocations of the last week that this account may not have heard of — a contact we have seen live
/// since a revocation was told it then (`tell_contacts_about_revocations`). `None` when there is none.
pub(crate) fn revocation_notice_for(
    storage: &Storage,
    account_id: &AccountId,
    now_unix_ms: i64,
) -> Option<RevocationNotice> {
    let own_account_id = storage.get_local_account().ok().flatten()?.descriptor.account_id;
    let seen_live_at = if *account_id == own_account_id {
        None
    } else {
        storage
            .find_contact_by_remote_account_id(account_id)
            .ok()
            .flatten()
            .and_then(|contact| contact.presence_last_seen_at_unix_ms)
    };
    let mut revocations: Vec<omni_crypto::DeviceRevocationV1> = storage
        .list_device_revocations_under_current_key(&own_account_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|revocation| now_unix_ms - revocation.revoked_at_unix_ms <= REVOCATION_NOTICE_KEEP_MS)
        .filter(|revocation| seen_live_at.is_none_or(|seen| seen < revocation.revoked_at_unix_ms))
        .collect();
    revocations.sort_by_key(|revocation| std::cmp::Reverse(revocation.revoked_at_unix_ms));
    // As many as fit, newest first: the newest is the one a contact is least likely to have heard.
    let mut kept: Vec<omni_crypto::DeviceRevocationV1> = Vec::new();
    for revocation in revocations {
        kept.push(revocation);
        if serde_json::to_vec(&kept).map_or(true, |json| json.len() > ACCOUNT_MAILBOX_MAX_CONTENT) {
            kept.pop();
            break;
        }
    }
    let mark_ms = kept.first()?.revoked_at_unix_ms.max(0) as u64;
    Some(RevocationNotice { mark_ms, content: serde_json::to_vec(&kept).ok()? })
}

/// Post the pair's notice if it changed or the republish floor passed. Runs on the dial worker.
fn publish_revocation_notice_if_due(
    courier: &dyn RendezvousCourier,
    db_path: &str,
    secret: &[u8; 32],
    notice: &RevocationNotice,
    now_unix_ms: i64,
) {
    let due = with_revocation_notices(db_path, |state| match state.published.get(secret) {
        Some((at, mark)) => *mark != notice.mark_ms || now_unix_ms - at >= REVOCATION_NOTICE_REPUBLISH_MS,
        None => true,
    });
    if !due {
        return;
    }
    let topics = omni_rendezvous::mailbox_topics_with_keys(secret, now_unix_ms.max(0) as u64);
    let Some((topic, dht_key)) = topics.first() else {
        return;
    };
    let wire = omni_rendezvous::seal_mailbox(secret, topic, &notice.content);
    let posted = courier.publish(
        &topic.id(),
        &hex::encode(dht_key),
        &hex::encode(wire),
        omni_rendezvous::MAILBOX_NOTE_TTL_SECS,
        PublishPriority::Background,
    );
    omni_app_trace_log(
        "rendezvous/revocation_notice",
        &format!("posted mark={} ok={}", notice.mark_ms, posted.is_ok()),
    );
    if posted.is_ok() {
        with_revocation_notices(db_path, |state| {
            state.published.insert(*secret, (now_unix_ms, notice.mark_ms));
        });
    }
}

/// A peer's beacon carried `mark`: read the pair's notice, unless we already have for this mark or
/// looked a moment ago. What is read waits for the queue tick ([`apply_revocation_notice_inbox`]).
/// Runs on the dial worker.
fn read_revocation_notice_if_marked(
    courier: &dyn RendezvousCourier,
    db_path: &str,
    secret: &[u8; 32],
    mark: u64,
    now_unix_ms: i64,
) {
    let due = with_revocation_notices(db_path, |state| match state.read.get(secret) {
        Some((read_mark, _)) if *read_mark >= mark => false,
        Some((_, looked_at)) if now_unix_ms - looked_at < REVOCATION_NOTICE_RETRY_MS => false,
        _ => true,
    });
    if !due {
        return;
    }
    let mut found: Vec<omni_crypto::DeviceRevocationV1> = Vec::new();
    for (topic, dht_key) in omni_rendezvous::mailbox_topics_with_keys(secret, now_unix_ms.max(0) as u64) {
        let Ok(blobs) = courier.fetch(&topic.id(), &hex::encode(dht_key), CourierLegs::Web) else {
            continue;
        };
        for blob in blobs {
            let Some(content) =
                hex::decode(&blob).ok().and_then(|wire| omni_rendezvous::open_mailbox(secret, &topic, &wire))
            else {
                continue;
            };
            let Ok(revocations) = serde_json::from_slice::<Vec<omni_crypto::DeviceRevocationV1>>(&content) else {
                continue;
            };
            for revocation in revocations {
                if !found.contains(&revocation) {
                    found.push(revocation);
                }
            }
        }
    }
    let reached = found.iter().any(|revocation| revocation.revoked_at_unix_ms.max(0) as u64 >= mark);
    omni_app_trace_log(
        "rendezvous/revocation_notice",
        &format!("read for mark={mark}: {} revocation(s), reached={reached}", found.len()),
    );
    with_revocation_notices(db_path, |state| {
        let read_mark = if reached { mark } else { state.read.get(secret).map_or(0, |(read, _)| *read) };
        state.read.insert(*secret, (read_mark, now_unix_ms));
        state.inbox.extend(found.into_iter().map(|revocation| (*secret, revocation)));
    });
}

/// Apply the revocations read from notices since the last tick. One counts only for the account of
/// the pair it came under — a device of an approved contact, or one of ours — and only when it proves
/// itself (`Storage::apply_device_revocation` checks the signature). Caller holds the session lock.
fn apply_revocation_notice_inbox(storage: &Storage, transport: &TransportManager) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let inbox = with_revocation_notices(&db_path, |state| std::mem::take(&mut state.inbox));
    if inbox.is_empty() {
        return;
    }
    let peers = rendezvous_peers(storage);
    for (secret, revocation) in inbox {
        let Some(peer) = peers.iter().find(|peer| peer.secret == secret) else {
            continue; // the pair is gone since
        };
        if revocation.account_id != peer.account_id {
            omni_app_trace_log(
                "rendezvous/revocation_notice",
                "dropped: it speaks for another account than the pair's",
            );
            continue;
        }
        apply_received_device_revocation(storage, transport, &revocation, None, "rendezvous/revocation_notice");
    }
}
