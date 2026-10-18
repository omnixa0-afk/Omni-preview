// The account's mailbox (docs/decisions/multi-device-equal-nodes.md, «Почтовый ящик аккаунта»).
//
// Contact sync goes device to device over a live link. When the device where a contact was blocked or
// removed is switched off before another of ours comes online, nobody is left to tell it — there is
// no server of ours. So each device also leaves a note at the rendezvous couriers, under a topic only
// the account's devices can compute: its recent blocks and removals. A device that comes up reads the
// notes and merges them as it would a batch from that device.
//
// The note is sealed with the account's rendezvous secret (the couriers see an opaque blob) and signed
// by the writing device: the secret comes from the working key, which a revoked device still holds,
// so only a note signed by a device of ours that is not revoked is taken. Approvals are not in the
// mailbox: they need the contact's device bundles and are not urgent.
//
// The note also carries the account's recent device revocations. That is how a revoked device that
// had no session with any of ours learns it was taken back and erases itself: nobody will set one up
// with it any more, but it still reads the mailbox (docs/bugs/a-revoked-device-without-a-session-never-learns.md).
//
// The couriers are network calls, so they run on a worker thread; what the worker read waits in
// `ACCOUNT_MAILBOX` until the queue tick, which holds the session lock, applies it.

/// Blocks and removals older than this are not in the note: a device away longer catches up from
/// whichever device of ours it meets.
const ACCOUNT_MAILBOX_KEEP_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// At most this many contacts per note, newest first.
const ACCOUNT_MAILBOX_MAX_ENTRIES: u64 = 16;

/// Largest note content. Padded to 7 buckets of 256 and hex-encoded it stays under ntfy's 4096-byte
/// message limit (beyond it ntfy turns the message into an attachment). BT-DHT takes values of
/// about 1000 bytes, so a note of more than two or three contacts reaches only ntfy and Nostr.
const ACCOUNT_MAILBOX_MAX_CONTENT: usize = 7 * 256 - 4;

/// An unchanged note is posted again this often — ntfy keeps a message about 12 hours, and the
/// public brokers meter publishing.
const ACCOUNT_MAILBOX_REPUBLISH_MS: i64 = 4 * 60 * 60 * 1000;

/// How often the couriers are asked for the other devices' notes.
const ACCOUNT_MAILBOX_READ_EVERY_MS: i64 = 10 * 60 * 1000;

const ACCOUNT_MAILBOX_SIGNING_CONTEXT: &[u8] = b"omni-account-mailbox-note-v1\0";

/// One device's note.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct AccountMailboxNoteV1 {
    device_id: String,
    written_at_unix_ms: i64,
    entries: Vec<omni_types::ContactSyncEntryV1>,
    /// The account's revocations of the last [`ACCOUNT_MAILBOX_KEEP_MS`], newest first. Each is signed
    /// by the working key and checked on its own when applied.
    #[serde(default)]
    revocations: Vec<omni_crypto::DeviceRevocationV1>,
    /// Ed25519 by the writing device over [`account_mailbox_signing_bytes`].
    #[serde(default)]
    signature_hex: String,
}

fn account_mailbox_signing_bytes(note: &AccountMailboxNoteV1) -> Vec<u8> {
    let unsigned = AccountMailboxNoteV1 { signature_hex: String::new(), ..note.clone() };
    let mut bytes = ACCOUNT_MAILBOX_SIGNING_CONTEXT.to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&unsigned).unwrap_or_default());
    bytes
}

/// Per profile: what we last posted, when we last read, and notes read but not yet applied.
#[derive(Default)]
struct AccountMailboxState {
    published_at_unix_ms: i64,
    published_fingerprint: u64,
    read_at_unix_ms: i64,
    inbox: Vec<AccountMailboxNoteV1>,
}

static ACCOUNT_MAILBOX: std::sync::OnceLock<Mutex<HashMap<String, AccountMailboxState>>> =
    std::sync::OnceLock::new();

fn with_account_mailbox<T>(db_path: &str, f: impl FnOnce(&mut AccountMailboxState) -> T) -> T {
    let registry = ACCOUNT_MAILBOX.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    f(map.entry(db_path.to_string()).or_default())
}

pub(crate) fn clear_account_mailbox_for_profile(db_path: &str) {
    if let Some(registry) = ACCOUNT_MAILBOX.get() {
        registry.lock().unwrap_or_else(|poison| poison.into_inner()).remove(db_path);
    }
}

/// This device's note, signed — `None` when there is nothing recent to tell. What fits in
/// [`ACCOUNT_MAILBOX_MAX_CONTENT`]: contacts are dropped (oldest first) before revocations — a
/// revocation (~1 KB) is what a device taken back can learn only here.
fn build_account_mailbox_note(storage: &Storage, now_unix_ms: i64) -> Option<Vec<u8>> {
    let account = storage.get_local_account().ok()??;
    let device = storage.get_local_device().ok()??;
    let identity = storage.get_local_identity_key(&account.descriptor.account_id).ok()??;
    let records = storage
        .list_recent_contact_sync_blocks_and_removals(
            now_unix_ms - ACCOUNT_MAILBOX_KEEP_MS,
            ACCOUNT_MAILBOX_MAX_ENTRIES,
        )
        .ok()?;
    let mut revocations: Vec<omni_crypto::DeviceRevocationV1> = storage
        .list_device_revocations_under_current_key(&account.descriptor.account_id)
        .ok()?
        .into_iter()
        .filter(|revocation| revocation.revoked_at_unix_ms >= now_unix_ms - ACCOUNT_MAILBOX_KEEP_MS)
        .collect();
    revocations.sort_by_key(|revocation| std::cmp::Reverse(revocation.revoked_at_unix_ms));
    let mut note = AccountMailboxNoteV1 {
        device_id: device.device_id.0.clone(),
        written_at_unix_ms: now_unix_ms,
        entries: records.iter().map(|record| contact_sync_entry(storage, record)).collect(),
        revocations,
        signature_hex: String::new(),
    };
    while !note.entries.is_empty() || !note.revocations.is_empty() {
        let signature = omni_crypto::sign_message(
            &identity.protected_signing_private_key,
            &account_mailbox_signing_bytes(&note),
        )
        .ok()?;
        note.signature_hex = hex::encode(signature.bytes);
        let content = serde_json::to_vec(&note).ok()?;
        if content.len() <= ACCOUNT_MAILBOX_MAX_CONTENT {
            return Some(content);
        }
        if note.entries.pop().is_none() {
            note.revocations.pop();
        }
    }
    None
}

/// What a note says about the contacts, without the time it was written and the signature, which
/// change on every build: an unchanged note is not posted again before the republish floor.
fn account_mailbox_fingerprint(content: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(note) = serde_json::from_slice::<AccountMailboxNoteV1>(content) {
        note.device_id.hash(&mut hasher);
        serde_json::to_string(&note.entries).unwrap_or_default().hash(&mut hasher);
        serde_json::to_string(&note.revocations).unwrap_or_default().hash(&mut hasher);
    }
    hasher.finish()
}

/// Post the note under today's mailbox topic. Returns how many couriers took it.
fn publish_account_mailbox_with(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    content: &[u8],
    now_unix_ms: i64,
) -> usize {
    let topics = omni_rendezvous::mailbox_topics_with_keys(secret, now_unix_ms.max(0) as u64);
    let Some((topic, dht_key)) = topics.first() else {
        return 0;
    };
    let wire = omni_rendezvous::seal_mailbox(secret, topic, content);
    usize::from(
        courier
            .publish(
                &topic.id(),
                &hex::encode(dht_key),
                &hex::encode(wire),
                omni_rendezvous::MAILBOX_NOTE_TTL_SECS,
                PublishPriority::Background,
            )
            .is_ok(),
    )
}

/// Every note the couriers hold for the account over the days a reader looks back, opened but not
/// yet checked against our devices (that needs storage — see [`apply_account_mailbox_inbox`]).
fn read_account_mailbox_with(
    courier: &dyn RendezvousCourier,
    secret: &[u8; 32],
    now_unix_ms: i64,
) -> Vec<AccountMailboxNoteV1> {
    let mut notes: Vec<AccountMailboxNoteV1> = Vec::new();
    for (topic, dht_key) in omni_rendezvous::mailbox_topics_with_keys(secret, now_unix_ms.max(0) as u64) {
        // The web couriers only: a note is not kept in the DHT (see `MAILBOX_NOTE_TTL_SECS`).
        let Ok(blobs) = courier.fetch(&topic.id(), &hex::encode(dht_key), CourierLegs::Web) else {
            continue;
        };
        for blob in blobs {
            let Some(content) = hex::decode(&blob)
                .ok()
                .and_then(|wire| omni_rendezvous::open_mailbox(secret, &topic, &wire))
            else {
                continue;
            };
            let Ok(note) = serde_json::from_slice::<AccountMailboxNoteV1>(&content) else {
                continue;
            };
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
    }
    notes
}

/// One mailbox round for the profile, as due: post this device's note if it changed or the
/// republish floor passed, read the others' if the read interval passed. `spawn_helper` — whether
/// the rendezvous helper may be started for this (the sweep), or only an already running one used
/// (a contact changed by hand, where the sweep has not brought rendezvous up yet).
fn account_mailbox_round(storage: &Storage, spawn_helper: bool) {
    let Ok(Some(secret)) = storage.get_account_rendezvous_secret() else {
        return;
    };
    let db_path = storage.db_path().to_string_lossy().to_string();
    let now = current_unix_ms();
    let content = build_account_mailbox_note(storage, now);
    let (publish, read) = with_account_mailbox(&db_path, |state| {
        let publish = content.as_ref().filter(|content| {
            account_mailbox_fingerprint(content) != state.published_fingerprint
                || now - state.published_at_unix_ms >= ACCOUNT_MAILBOX_REPUBLISH_MS
        });
        let read = now - state.read_at_unix_ms >= ACCOUNT_MAILBOX_READ_EVERY_MS;
        if let Some(content) = publish {
            state.published_at_unix_ms = now;
            state.published_fingerprint = account_mailbox_fingerprint(content);
        }
        if read {
            state.read_at_unix_ms = now;
        }
        (publish.cloned(), read)
    });
    if publish.is_none() && !read {
        return;
    }
    let rendezvous = if spawn_helper {
        profile_rendezvous_runtime(&db_path)
        .ok()
    } else {
        rendezvous_runtime_registry()
            .lock()
            .ok()
            .and_then(|registry| registry.get(&db_path).cloned())
    };
    let Some(rendezvous) = rendezvous else {
        // Not posted after all: the next round tries again.
        with_account_mailbox(&db_path, |state| {
            if publish.is_some() {
                state.published_fingerprint = 0;
            }
            if read {
                state.read_at_unix_ms = 0;
            }
        });
        return;
    };
    let _ = std::thread::Builder::new()
        .name("omni-account-mailbox".to_string())
        .spawn(move || {
            let courier = rendezvous.helper.as_ref();
            if let Some(content) = publish {
                let posted = publish_account_mailbox_with(courier, &secret, &content, now);
                omni_app_trace_log("own_devices/mailbox", &format!("note posted to {posted} courier(s)"));
            }
            if read {
                let notes = read_account_mailbox_with(courier, &secret, now);
                omni_app_trace_log("own_devices/mailbox", &format!("read {} note(s)", notes.len()));
                if !notes.is_empty() {
                    with_account_mailbox(&db_path, |state| state.inbox.extend(notes));
                }
            }
        });
}

/// Apply the notes read since the last tick. A note counts only when signed by a device of ours that
/// is not revoked; its blocks and removals are merged, its revocations of our devices applied (one of
/// THIS device erases the profile). Caller holds the session lock.
fn apply_account_mailbox_inbox(storage: &Storage, transport: &TransportManager) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let notes = with_account_mailbox(&db_path, |state| std::mem::take(&mut state.inbox));
    if notes.is_empty() {
        return;
    }
    let Ok(Some(account)) = storage.get_local_account() else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    let devices = storage.list_account_devices(&own_account_id).unwrap_or_default();
    let now_unix_ms = current_unix_ms();
    for note in notes {
        let device_id = DeviceId(note.device_id.clone());
        if !storage.is_own_other_device(&own_account_id, &device_id).unwrap_or(false) {
            continue; // ours (we wrote it), revoked, or not a device of ours we know
        }
        let Some(signing_key) = devices
            .iter()
            .find(|record| record.chain.device.device_id == device_id)
            .map(|record| record.chain.device.identity_signing_public_key)
        else {
            continue;
        };
        let signature = omni_crypto::Signature {
            bytes: hex::decode(&note.signature_hex).unwrap_or_default(),
        };
        if omni_crypto::verify_signature(&signing_key, &account_mailbox_signing_bytes(&note), &signature).is_err() {
            omni_app_trace_log(
                "own_devices/mailbox",
                &format!("note from {} refused: bad signature", device_id.0),
            );
            continue;
        }
        let mut removed = Vec::new();
        for entry in &note.entries {
            if entry.account_id == own_account_id.0 || !matches!(entry.state.as_str(), "blocked" | "removed") {
                continue;
            }
            match apply_contact_sync_entry(storage, entry, now_unix_ms) {
                Ok(Some(contact)) => removed.push(contact),
                Ok(None) => {}
                Err(error) => omni_app_trace_log(
                    "own_devices/mailbox",
                    &format!("account={} not applied: {error}", entry.account_id),
                ),
            }
        }
        forget_runtime_state_of_removed_contacts(storage, &removed);
        omni_app_trace_log(
            "own_devices/mailbox",
            &format!(
                "note from {}: entries={} removed={} revocations={}",
                device_id.0,
                note.entries.len(),
                removed.len(),
                note.revocations.len()
            ),
        );
        for revocation in &note.revocations {
            if revocation.account_id != own_account_id {
                continue;
            }
            apply_received_device_revocation(storage, transport, revocation, None, "own_devices/mailbox");
            if storage.get_local_account().ok().flatten().is_none() {
                return; // this device was the one taken back: the profile is gone
            }
        }
    }
}
