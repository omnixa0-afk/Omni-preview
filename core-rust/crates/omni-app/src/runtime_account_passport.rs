// The account passport (docs/decisions/account-recovery-from-friends.md, Б1).
//
// Lose every device, type the phrase on a new one — and the profile, the avatar, the contacts and the
// groups come back without anyone doing anything. Three roles, every device plays all of them:
//
// - **Owner.** Builds a small package of the account (`AccountPassportV1`), signs it with this
//   device's key (its certificate chain inside), compresses and seals it under the account secret and
//   hands it to up to five approved contacts it talks to — the holders. Rebuilt when what it says
//   changed; a holder that stops confirming for a month is replaced.
// - **Holder.** Keeps the newest sealed passport of each account that handed it one (within a quota,
//   storage_account_passport.rs), cannot read it, gives it back only to a device of that account, and
//   posts a mark at the couriers — its identity beacon, sealed under the owner's mark secret — so a
//   device that knows nothing but the phrase can find it.
// - **Restored device.** For a month after a restore from the phrase it reads the marks, makes the
//   holders its contacts, reaches them, asks for the passport and merges every one it gets.
//
// Robustness (the decision's rules): merging only adds and is idempotent — contacts go through the
// same versioned merge as the sync between our devices, so an old passport cannot undo a newer
// state; nothing local is erased because a passport lacks it; whatever arrives is checked by size
// before it is decoded, and a broken one is dropped whole; every step is a retry on a later tick.

use omni_storage::{HeldPassportOutcome, HeldPassportRecord, OwnPassportRecord, PassportHolderRecord, RecoveredGroupRecord};

/// Holders wanted at once («сколько есть», owner 2026-10-11: as many suitable contacts as there are,
/// up to this).
const PASSPORT_HOLDERS_WANTED: usize = 5;

/// What the passport says is checked against the last one built at most this often.
const PASSPORT_CHECK_EVERY_MS: i64 = 60_000;

/// A changed passport is rebuilt at most this often: a burst of contact changes is one rebuild.
const PASSPORT_REBUILD_MIN_INTERVAL_MS: i64 = 10 * 60 * 1000;

/// The chat fingerprints in the passport are refreshed this often even when nothing else changed.
const PASSPORT_FINGERPRINTS_EVERY_MS: i64 = 24 * 60 * 60 * 1000;

/// A passport sent and not confirmed goes out again after this.
const PASSPORT_RESEND_AFTER_MS: i64 = 60 * 60 * 1000;

/// A confirmed passport is sent again this often: the send doubles as the holder's proof it still
/// keeps it.
const PASSPORT_RECONFIRM_EVERY_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A holder that has not confirmed for this long gives its place to another candidate.
const PASSPORT_HOLDER_STALE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The sealed passport must fit one logical event.
const PASSPORT_SEALED_MAX_BYTES: usize = omni_protocol::ACCOUNT_PASSPORT_SEALED_MAX_BYTES;

/// An unpacked passport larger than this is not one of ours (a compression bomb).
const PASSPORT_PLAIN_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The avatar rides along only if it is small (the profile keeps it at a few dozen KB).
const PASSPORT_AVATAR_MAX_BYTES: usize = 64 * 1024;

const PASSPORT_SIGNING_CONTEXT: &[u8] = b"omni-account-passport-v1\0";

/// A holder's mark is posted again this often (ntfy keeps a message about 12 hours).
const PASSPORT_MARK_REPUBLISH_MS: i64 = 6 * 60 * 60 * 1000;

/// A passport kept for an account that never refreshes it is let go after this.
const HELD_PASSPORT_KEEP_MS: i64 = 180 * 24 * 60 * 60 * 1000;

/// How long a restored device keeps looking for its holders.
const RECOVERY_JOB_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// A restored device reads the marks this often in its first hour, then every
/// [`RECOVERY_MARKS_READ_LATER_MS`].
const RECOVERY_MARKS_READ_EARLY_MS: i64 = 2 * 60 * 1000;
const RECOVERY_MARKS_READ_LATER_MS: i64 = 30 * 60 * 1000;

/// A holder that has not answered is asked again after this.
const RECOVERY_FETCH_AGAIN_MS: i64 = 60_000;

const ACCOUNT_RECOVERY_META_KEY: &str = "account_recovery_v1";

/// The account as its devices know it. Unknown fields are ignored, missing ones default: a passport
/// written by a newer or older build still gives what it has.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
struct AccountPassportV1 {
    written_at_unix_ms: i64,
    /// The writing device's certificate chain (master → working key → device): who signed this.
    device_chain_json: String,
    #[serde(default)]
    profile: Option<PassportProfileV1>,
    #[serde(default)]
    contacts: Vec<omni_types::ContactSyncEntryV1>,
    #[serde(default)]
    groups: Vec<PassportGroupV1>,
    /// Each approved contact's device signing keys (hex) — 32 bytes a device where a full bundle is
    /// 2.6 KB. The bundles of only the most recent contacts fit; for the others the restored device
    /// dials these (the node address is derived from the key) and the link brings a fresh bundle.
    #[serde(default)]
    contact_device_keys: Vec<(String, Vec<String>)>,
    /// Each chat's fingerprint (contact account, line count, root, counted up to): what a restored
    /// device checks a contact's copy of the chat against (Б2). Refreshed daily, not on every line.
    #[serde(default)]
    chat_fingerprints: Vec<(String, u64, String, i64)>,
    /// Ed25519 by the device of the chain over [`passport_signing_bytes`].
    #[serde(default)]
    signature_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PassportProfileV1 {
    display_name: String,
    #[serde(default)]
    avatar_mime_type: Option<String>,
    #[serde(default)]
    avatar_b64: Option<String>,
    updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PassportGroupV1 {
    group_id: String,
    name: String,
    /// `admin` or `member`.
    role: String,
}

fn passport_signing_bytes(passport: &AccountPassportV1) -> Vec<u8> {
    let unsigned = AccountPassportV1 { signature_hex: String::new(), ..passport.clone() };
    let mut bytes = PASSPORT_SIGNING_CONTEXT.to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(&unsigned).unwrap_or_default());
    bytes
}

/// What the passport says, without when and by whom it was written.
fn passport_fingerprint(passport: &AccountPassportV1) -> String {
    let content = AccountPassportV1 {
        written_at_unix_ms: 0,
        device_chain_json: String::new(),
        signature_hex: String::new(),
        chat_fingerprints: Vec::new(),
        ..passport.clone()
    };
    hex::encode(Sha256::digest(serde_json::to_vec(&content).unwrap_or_default()))
}

fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(bytes);
    encoder.finish().unwrap_or_default()
}

/// Unpack at most [`PASSPORT_PLAIN_MAX_BYTES`]; `None` past it or for anything not deflate.
fn inflate_bounded(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    let mut decoder = flate2::read::DeflateDecoder::new(bytes).take(PASSPORT_PLAIN_MAX_BYTES as u64 + 1);
    decoder.read_to_end(&mut out).ok()?;
    (out.len() <= PASSPORT_PLAIN_MAX_BYTES).then_some(out)
}

/// Per profile, in memory: when the passport was last checked, when the marks were posted and read,
/// when each holder was last asked. Lost on restart, which only means one check sooner.
#[derive(Default)]
struct AccountPassportState {
    checked_at_unix_ms: i64,
    marks_posted_at_unix_ms: i64,
    marks_posted_fingerprint: u64,
    marks_read_at_unix_ms: i64,
    asked_at_unix_ms: HashMap<String, i64>,
}

static ACCOUNT_PASSPORTS: std::sync::OnceLock<Mutex<HashMap<String, AccountPassportState>>> =
    std::sync::OnceLock::new();

fn with_account_passport_state<T>(db_path: &str, f: impl FnOnce(&mut AccountPassportState) -> T) -> T {
    let registry = ACCOUNT_PASSPORTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    f(map.entry(db_path.to_string()).or_default())
}

pub(crate) fn clear_account_passport_state_for_profile(db_path: &str) {
    if let Some(registry) = ACCOUNT_PASSPORTS.get() {
        registry.lock().unwrap_or_else(|poison| poison.into_inner()).remove(db_path);
    }
}

/// What a restored device remembers between restarts while it looks for its passport.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
struct AccountRecoveryStateV1 {
    started_at_unix_ms: i64,
    /// Accounts whose mark we found: the holders to ask.
    #[serde(default)]
    holders: Vec<String>,
    /// (holder account, version) of every passport merged.
    #[serde(default)]
    applied: Vec<(String, i64)>,
    /// (contact account, node hex) of contacts the passport named without a bundle: dialed until a
    /// link brings one.
    #[serde(default)]
    dial_nodes: Vec<(String, String)>,
    /// The newest chat fingerprint any passport gave for each chat.
    #[serde(default)]
    chat_fingerprints: Vec<(String, u64, String, i64)>,
}

fn load_account_recovery(storage: &Storage) -> Option<AccountRecoveryStateV1> {
    let raw = storage.get_app_meta(ACCOUNT_RECOVERY_META_KEY).ok()??;
    serde_json::from_str(&raw).ok()
}

fn save_account_recovery(storage: &Storage, state: &AccountRecoveryStateV1) {
    if let Ok(raw) = serde_json::to_string(state) {
        let _ = storage.upsert_app_meta(ACCOUNT_RECOVERY_META_KEY, &raw);
    }
}

/// A device restored from the phrase starts looking for its passport. Called right after the restore.
pub(crate) fn start_account_recovery(storage: &Storage, now_unix_ms: i64) {
    if load_account_recovery(storage).is_none() {
        save_account_recovery(storage, &AccountRecoveryStateV1 { started_at_unix_ms: now_unix_ms, ..Default::default() });
    }
}

fn account_recovery_active(state: &AccountRecoveryStateV1, now_unix_ms: i64) -> bool {
    now_unix_ms - state.started_at_unix_ms < RECOVERY_JOB_MS
}

// --- owner ---------------------------------------------------------------------------------------

/// The passport as this device would write it now, unsigned and without its chain.
fn current_passport_content(storage: &Storage, own_account_id: &AccountId) -> OmniResult<AccountPassportV1> {
    let profile = storage.get_local_public_profile_state().ok().map(|state| {
        let avatar = state
            .avatar_bytes
            .filter(|bytes| bytes.len() <= PASSPORT_AVATAR_MAX_BYTES)
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
        PassportProfileV1 {
            display_name: state.display_name,
            avatar_mime_type: avatar.as_ref().and(state.avatar_mime_type),
            avatar_b64: avatar,
            updated_at_unix_ms: state.updated_at_unix_ms,
        }
    });
    let contacts = storage
        .list_contact_sync_records_after(0, i64::MAX as u64)?
        .iter()
        .filter(|record| &record.account_id != own_account_id)
        .map(|record| contact_sync_entry(storage, record))
        .collect();
    let mut groups = Vec::new();
    for group in storage.list_groups()? {
        if group.my_status != "member" {
            continue;
        }
        let role = storage
            .list_group_members(&group.group_id)
            .unwrap_or_default()
            .into_iter()
            .find(|member| member.member_pub == group.my_member_pub)
            .map_or_else(|| "member".to_string(), |member| member.role);
        groups.push(PassportGroupV1 { group_id: group.group_id, name: group.name, role });
    }
    let mut contact_device_keys = Vec::new();
    for contact in storage.list_contacts()? {
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        if contact.relationship_state != ContactRelationshipState::Approved || account_id == own_account_id {
            continue;
        }
        let mut keys: Vec<String> = storage
            .list_remote_prekey_bundles_for_account(account_id)
            .unwrap_or_default()
            .iter()
            .map(|bundle| hex::encode(bundle.identity_signing_key.0))
            .collect();
        keys.sort();
        keys.dedup();
        keys.truncate(8);
        if !keys.is_empty() {
            contact_device_keys.push((account_id.0.clone(), keys));
        }
    }
    contact_device_keys.sort();
    let chat_fingerprints = passport_chat_fingerprints(storage, own_account_id, current_unix_ms());
    Ok(AccountPassportV1 { profile, contacts, groups, contact_device_keys, chat_fingerprints, ..Default::default() })
}

/// Sign, compress and seal `content` as written by this device at `now_unix_ms`. Contact device
/// bundles are what makes it big: they are left out, from the contacts changed longest ago, until
/// the passport fits one message. `None` when it cannot be had (no certificate yet, no secret).
fn seal_passport_content(
    storage: &Storage,
    secret: &[u8; 32],
    mut content: AccountPassportV1,
    now_unix_ms: i64,
) -> OmniResult<Option<Vec<u8>>> {
    let (Some(account), Some(device)) = (storage.get_local_account()?, storage.get_local_device()?) else {
        return Ok(None);
    };
    let Some(identity) = storage.get_local_identity_key(&account.descriptor.account_id)? else {
        return Ok(None);
    };
    let Some(chain) = storage
        .list_account_devices(&account.descriptor.account_id)?
        .into_iter()
        .find(|record| record.chain.device.device_id == device.device_id)
        .map(|record| record.chain)
    else {
        return Ok(None);
    };
    content.written_at_unix_ms = now_unix_ms;
    content.device_chain_json = serde_json::to_string(&chain).map_err(|error| OmniError::Internal(error.to_string()))?;
    let seal = |content: &AccountPassportV1| -> OmniResult<Option<Vec<u8>>> {
        let mut signed = content.clone();
        signed.signature_hex = String::new();
        let signature = omni_crypto::sign_message(&identity.protected_signing_private_key, &passport_signing_bytes(&signed))?;
        signed.signature_hex = hex::encode(signature.bytes);
        let plain = serde_json::to_vec(&signed).map_err(|error| OmniError::Internal(error.to_string()))?;
        let sealed = omni_rendezvous::seal_passport(secret, &deflate(&plain));
        Ok((sealed.len() <= PASSPORT_SEALED_MAX_BYTES).then_some(sealed))
    };
    if let Some(sealed) = seal(&content)? {
        return Ok(Some(sealed));
    }
    // Too big. What makes it big are the contacts' device bundles: keep them for as many of the
    // most recently changed contacts as fit (records come oldest change first), found by halving —
    // each try signs and packs the whole passport, so one try per contact would be a thousand.
    let with_bundles: Vec<usize> = (0..content.contacts.len())
        .filter(|&index| !content.contacts[index].bundles_json.is_empty())
        .collect();
    let keeping = |keep: usize| {
        let mut trial = content.clone();
        for &index in &with_bundles[..with_bundles.len() - keep] {
            trial.contacts[index].bundles_json.clear();
        }
        trial
    };
    let (mut fits, mut too_many) = (0usize, with_bundles.len());
    let mut best = None;
    while fits < too_many {
        let middle = (fits + too_many).div_ceil(2);
        match seal(&keeping(middle))? {
            Some(sealed) => {
                fits = middle;
                best = Some(sealed);
            }
            None => too_many = middle - 1,
        }
    }
    let stripped = with_bundles.len() - fits;
    omni_app_trace_log("passport/build", &format!("device bundles of {stripped} contact(s) left out to fit"));
    if let Some(sealed) = best {
        return Ok(Some(sealed));
    }
    let mut trial = keeping(0);
    if let Some(sealed) = seal(&trial)? {
        return Ok(Some(sealed));
    }
    if let Some(profile) = trial.profile.as_mut() {
        profile.avatar_b64 = None;
        profile.avatar_mime_type = None;
    }
    if let Some(sealed) = seal(&trial)? {
        return Ok(Some(sealed));
    }
    // Still too big: the oldest contacts do not fit. The passport says what it can — as many of
    // the newest as fit, again by halving.
    let all = trial.contacts.clone();
    let (mut fits, mut too_many) = (0usize, all.len());
    let mut best = None;
    while fits < too_many {
        let middle = (fits + too_many).div_ceil(2);
        trial.contacts = all[all.len() - middle..].to_vec();
        match seal(&trial)? {
            Some(sealed) => {
                fits = middle;
                best = Some(sealed);
            }
            None => too_many = middle - 1,
        }
    }
    omni_app_trace_log("passport/build", &format!("{} oldest contact(s) do not fit", all.len() - fits));
    Ok(best)
}

/// Approved contacts we talk both ways with, oldest relationship first — the holder candidates.
fn passport_holder_candidates(storage: &Storage, own_account_id: &AccountId) -> Vec<AccountId> {
    let mut contacts: Vec<ContactSnapshot> = storage
        .list_contacts()
        .unwrap_or_default()
        .into_iter()
        .filter(|contact| contact.relationship_state == ContactRelationshipState::Approved && !contact.is_blocked)
        .filter(|contact| contact.remote_account_id.as_ref().is_some_and(|account| account != own_account_id))
        .collect();
    contacts.sort_by_key(|contact| contact.created_at_unix_ms);
    contacts
        .into_iter()
        .filter(|contact| {
            let Some(dialog_key) = contact.primary_dialog_key.as_deref() else {
                return false;
            };
            let chat = storage.chat_dialog_key_for(dialog_key).unwrap_or_else(|_| dialog_key.to_string());
            storage.dialog_has_messages_both_ways(&chat).unwrap_or(false)
        })
        .filter_map(|contact| contact.remote_account_id)
        .collect()
}

/// Keep up to [`PASSPORT_HOLDERS_WANTED`] holders: drop one that is no longer an approved contact,
/// replace one silent for a month when a candidate is free, fill free places.
fn maintain_passport_holders(storage: &Storage, own_account_id: &AccountId, now_unix_ms: i64) -> OmniResult<Vec<PassportHolderRecord>> {
    let candidates = passport_holder_candidates(storage, own_account_id);
    let mut holders = storage.list_passport_holders()?;
    for holder in &holders {
        let still_approved = storage
            .find_contact_by_remote_account_id(&holder.account_id)?
            .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved && !contact.is_blocked);
        if !still_approved {
            storage.drop_passport_holder(&holder.account_id)?;
        }
    }
    holders = storage.list_passport_holders()?;
    let free: Vec<&AccountId> = candidates
        .iter()
        .filter(|candidate| !holders.iter().any(|holder| &holder.account_id == *candidate))
        .collect();
    let mut free = free.into_iter();
    for holder in &holders {
        let silent_since = holder.confirmed_at_unix_ms.max(holder.chosen_at_unix_ms);
        if now_unix_ms - silent_since > PASSPORT_HOLDER_STALE_MS {
            if let Some(replacement) = free.next() {
                storage.drop_passport_holder(&holder.account_id)?;
                storage.choose_passport_holder(replacement, now_unix_ms)?;
                omni_app_trace_log(
                    "passport/holders",
                    &format!("{} silent for a month, replaced by {}", holder.account_id.0, replacement.0),
                );
            }
        }
    }
    let mut count = storage.list_passport_holders()?.len();
    for candidate in free {
        if count >= PASSPORT_HOLDERS_WANTED {
            break;
        }
        storage.choose_passport_holder(candidate, now_unix_ms)?;
        count += 1;
    }
    storage.list_passport_holders()
}

/// Rebuild the passport if what it says changed (not more often than the rebuild floor).
fn refresh_own_passport(storage: &Storage, secret: &[u8; 32], own_account_id: &AccountId, now_unix_ms: i64) -> OmniResult<Option<OwnPassportRecord>> {
    let existing = storage.get_own_passport()?;
    let content = current_passport_content(storage, own_account_id)?;
    let fingerprint = passport_fingerprint(&content);
    if let Some(existing) = &existing {
        let age = now_unix_ms - existing.built_at_unix_ms;
        // A change waits for the rebuild floor; the chat fingerprints alone, for a day.
        if age < PASSPORT_REBUILD_MIN_INTERVAL_MS || (existing.fingerprint == fingerprint && age < PASSPORT_FINGERPRINTS_EVERY_MS) {
            return Ok(Some(existing.clone()));
        }
    }
    let Some(sealed) = seal_passport_content(storage, secret, content, now_unix_ms)? else {
        return Ok(existing);
    };
    // The version only grows, even if the clock went back.
    let version = existing.as_ref().map_or(now_unix_ms, |existing| now_unix_ms.max(existing.version + 1));
    let record = OwnPassportRecord { version, fingerprint, sealed, built_at_unix_ms: now_unix_ms };
    storage.store_own_passport(&record)?;
    omni_app_trace_log("passport/build", &format!("version={version} sealed={} bytes", record.sealed.len()));
    Ok(Some(record))
}

/// A device of `account_id` we have a live link and a session with, if any — starting a session on
/// a linked device that has none (the passport goes on a later tick).
fn reachable_session_dialog(storage: &Storage, transport: &TransportManager, account_id: &AccountId) -> Option<String> {
    let contact = storage.find_contact_by_remote_account_id(account_id).ok()??;
    for link in storage.list_contact_device_dialogs(&contact.contact_id).ok()? {
        if transport.connection_snapshot_for_dialog(&link.dialog_key).is_none() {
            continue;
        }
        // A dialog whose device is not written down yet (the holder started the session before we
        // knew it): the session on it says which device it is.
        let Some(device_id) = link.remote_device_id.clone().or_else(|| {
            storage
                .get_session_for_dialog(account_id, &link.dialog_key)
                .ok()
                .flatten()
                .map(|session| session.session_address.remote_device_id)
        }) else {
            continue;
        };
        let device_id = &device_id;
        if let Ok(Some(_)) = established_session_with_own_device(
            storage,
            transport,
            account_id,
            device_id,
            &link.dialog_key,
            "passport waits",
        ) {
            return Some(link.dialog_key);
        }
    }
    None
}

/// One owner round: holders kept, passport rebuilt if due, sent to each holder that needs it.
/// Caller holds the session lock.
fn account_passport_round(storage: &Storage, transport: &TransportManager, now_unix_ms: i64) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let due = with_account_passport_state(&db_path, |state| {
        let due = now_unix_ms - state.checked_at_unix_ms >= PASSPORT_CHECK_EVERY_MS;
        if due {
            state.checked_at_unix_ms = now_unix_ms;
        }
        due
    });
    apply_recovery_round(storage, transport, now_unix_ms);
    if !due {
        return;
    }
    // A restored device that has not got its passport back yet must not hand out its own: the
    // holders keep only the newest version, and this one — nearly empty — would replace the real
    // one before it came back.
    if load_account_recovery(storage)
        .is_some_and(|state| account_recovery_active(&state, now_unix_ms) && state.applied.is_empty())
    {
        return;
    }
    let (Ok(Some(account)), Ok(Some(secret))) = (storage.get_local_account(), storage.get_account_rendezvous_secret()) else {
        return;
    };
    let own_account_id = account.descriptor.account_id;
    let device_id = storage.get_local_device().ok().flatten().map(|device| device.device_id);
    if let Some(device_id) = &device_id {
        let _ = storage.refresh_contact_sync_records(device_id, now_unix_ms);
    }
    drop_held_passports_no_longer_wanted(storage, now_unix_ms);
    let holders = match maintain_passport_holders(storage, &own_account_id, now_unix_ms) {
        Ok(holders) => holders,
        Err(error) => {
            omni_app_trace_log("passport/holders", &format!("not kept: {error}"));
            return;
        }
    };
    if holders.is_empty() {
        return;
    }
    let passport = match refresh_own_passport(storage, &secret, &own_account_id, now_unix_ms) {
        Ok(Some(passport)) => passport,
        Ok(None) => return,
        Err(error) => {
            omni_app_trace_log("passport/build", &format!("failed: {error}"));
            return;
        }
    };
    let locator_hex = hex::encode(omni_rendezvous::passport_locator(&secret));
    let mark_secret_hex = hex::encode(omni_rendezvous::passport_mark_secret(&secret));
    for holder in holders {
        let needs = if holder.confirmed_version >= passport.version {
            now_unix_ms - holder.confirmed_at_unix_ms >= PASSPORT_RECONFIRM_EVERY_MS
                && now_unix_ms - holder.sent_at_unix_ms >= PASSPORT_RESEND_AFTER_MS
        } else {
            holder.sent_version < passport.version || now_unix_ms - holder.sent_at_unix_ms >= PASSPORT_RESEND_AFTER_MS
        };
        if !needs {
            continue;
        }
        let Some(dialog_key) = reachable_session_dialog(storage, transport, &holder.account_id) else {
            continue;
        };
        let message = omni_types::AccountPassportMessageV1::Store {
            locator_hex: locator_hex.clone(),
            version: passport.version,
            sealed_b64: base64::engine::general_purpose::STANDARD.encode(&passport.sealed),
            mark_secret_hex: mark_secret_hex.clone(),
        };
        match send_account_event_to_dialog(
            storage,
            transport,
            &holder.account_id,
            &dialog_key,
            omni_types::LogicalEventEnvelopeV1::account_passport(message),
            SecurityPayloadKind::AccountPassport,
            "passport waits",
        ) {
            Ok(()) => {
                let _ = storage.record_passport_sent(&holder.account_id, passport.version, now_unix_ms);
                omni_app_trace_log(
                    "passport/send",
                    &format!("holder={} version={} bytes={}", holder.account_id.0, passport.version, passport.sealed.len()),
                );
            }
            Err(error) => omni_app_trace_log("passport/send", &format!("holder={} not sent: {error}", holder.account_id.0)),
        }
    }
}

/// A passport kept for a contact we no longer approve, or one never refreshed for half a year, goes.
fn drop_held_passports_no_longer_wanted(storage: &Storage, now_unix_ms: i64) {
    for held in storage.list_held_passports().unwrap_or_default() {
        let approved = storage
            .find_contact_by_remote_account_id(&held.owner_account_id)
            .ok()
            .flatten()
            .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved && !contact.is_blocked);
        if !approved || now_unix_ms - held.stored_at_unix_ms > HELD_PASSPORT_KEEP_MS {
            let _ = storage.drop_held_passport(&held.owner_account_id);
            omni_app_trace_log("passport/held", &format!("let go of {}", held.owner_account_id.0));
        }
    }
}

// --- incoming ------------------------------------------------------------------------------------

/// A passport message on a session. Holders answer devices of the account the passport is for;
/// owners take confirmations from their holders; restored devices take answers. Anything else is
/// dropped. Caller holds the session lock.
fn handle_incoming_account_passport(
    storage: &Storage,
    transport: &TransportManager,
    message: &omni_types::AccountPassportMessageV1,
    receive_context: &ReceivedChatContext,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let from = &session.session_address;
    let Some(account) = storage.get_local_account()? else {
        return Ok(());
    };
    if from.remote_account_id == account.descriptor.account_id {
        return Ok(()); // our own devices share the account directly (contact and profile sync)
    }
    let now_unix_ms = current_unix_ms();
    let from_approved_contact = storage
        .find_contact_by_remote_account_id(&from.remote_account_id)?
        .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved && !contact.is_blocked);
    if !from_approved_contact {
        omni_app_trace_log("passport/in", &format!("dropped: {} is not an approved contact", from.remote_account_id.0));
        return Ok(());
    }
    let reply = |message: omni_types::AccountPassportMessageV1| {
        send_account_event_to_dialog(
            storage,
            transport,
            &from.remote_account_id,
            &receive_context.dialog_key,
            omni_types::LogicalEventEnvelopeV1::account_passport(message),
            SecurityPayloadKind::AccountPassport,
            "a passport answer waits",
        )
    };
    match message {
        omni_types::AccountPassportMessageV1::Store { locator_hex, version, sealed_b64, mark_secret_hex } => {
            let Ok(sealed) = base64::engine::general_purpose::STANDARD.decode(sealed_b64) else {
                return Ok(());
            };
            if sealed.len() > PASSPORT_SEALED_MAX_BYTES {
                return Ok(());
            }
            let outcome = storage.store_held_passport(&HeldPassportRecord {
                owner_account_id: from.remote_account_id.clone(),
                locator_hex: locator_hex.clone(),
                version: *version,
                sealed,
                mark_secret_hex: mark_secret_hex.clone(),
                stored_at_unix_ms: now_unix_ms,
            })?;
            omni_app_trace_log(
                "passport/held",
                &format!("from={} version={version} {outcome:?}", from.remote_account_id.0),
            );
            let kept = match outcome {
                HeldPassportOutcome::OverQuota => return Ok(()),
                HeldPassportOutcome::Stored => {
                    // The new owner's mark goes out on the next courier round.
                    with_account_passport_state(&storage.db_path().to_string_lossy(), |state| state.marks_posted_fingerprint = 0);
                    *version
                }
                HeldPassportOutcome::AlreadyNewer => storage
                    .get_held_passport(&from.remote_account_id)?
                    .map_or(*version, |held| held.version),
            };
            reply(omni_types::AccountPassportMessageV1::Stored { locator_hex: locator_hex.clone(), version: kept })
        }
        omni_types::AccountPassportMessageV1::Stored { version, .. } => {
            if storage.record_passport_confirmed(&from.remote_account_id, *version, now_unix_ms)? {
                omni_app_trace_log("passport/send", &format!("holder={} confirmed version={version}", from.remote_account_id.0));
            }
            Ok(())
        }
        omni_types::AccountPassportMessageV1::Fetch { locator_hex } => {
            let held = storage
                .get_held_passport(&from.remote_account_id)?
                .filter(|held| &held.locator_hex == locator_hex);
            omni_app_trace_log(
                "passport/held",
                &format!("asked by {} — {}", from.remote_account_id.0, if held.is_some() { "handed back" } else { "nothing kept" }),
            );
            reply(match held {
                Some(held) => omni_types::AccountPassportMessageV1::Blob {
                    locator_hex: locator_hex.clone(),
                    version: held.version,
                    sealed_b64: base64::engine::general_purpose::STANDARD.encode(&held.sealed),
                },
                None => omni_types::AccountPassportMessageV1::Blob {
                    locator_hex: locator_hex.clone(),
                    version: 0,
                    sealed_b64: String::new(),
                },
            })
        }
        omni_types::AccountPassportMessageV1::Blob { version, sealed_b64, .. } => {
            let Some(mut state) = load_account_recovery(storage) else {
                return Ok(()); // not restoring: nobody asked
            };
            let holder = from.remote_account_id.0.clone();
            if *version == 0 {
                omni_app_trace_log("passport/recover", &format!("holder={holder} keeps nothing for us"));
                return Ok(());
            }
            if state.applied.iter().any(|(applied_from, applied)| applied_from == &holder && applied >= version) {
                return Ok(());
            }
            let Ok(sealed) = base64::engine::general_purpose::STANDARD.decode(sealed_b64) else {
                return Ok(());
            };
            let Some(secret) = storage.get_account_rendezvous_secret()? else {
                return Ok(());
            };
            match open_and_verify_passport(storage, &secret, &sealed) {
                Ok(passport) => {
                    let applied = apply_recovered_passport(storage, &passport, &state, now_unix_ms);
                    for fingerprint in passport.chat_fingerprints.iter().take(5000) {
                        if fingerprint.2.len() != 32 {
                            continue;
                        }
                        match state.chat_fingerprints.iter_mut().find(|known| known.0 == fingerprint.0) {
                            Some(known) if known.3 < fingerprint.3 => *known = fingerprint.clone(),
                            Some(_) => {}
                            None => state.chat_fingerprints.push(fingerprint.clone()),
                        }
                    }
                    for (account, node) in contacts_to_dial(storage, &passport) {
                        if !state.dial_nodes.iter().any(|(_, known)| known == &node) {
                            state.dial_nodes.push((account, node));
                        }
                    }
                    omni_app_trace_log(
                        "passport/recover",
                        &format!("holder={holder} version={version} merged: {applied}"),
                    );
                    state.applied.push((holder, *version));
                    save_account_recovery(storage, &state);
                }
                Err(error) => {
                    // Kept as it was: a broken or foreign passport changes nothing.
                    omni_app_trace_log("passport/recover", &format!("holder={holder} passport refused: {error}"));
                }
            }
            Ok(())
        }
    }
}

/// Open, unpack and check a passport: sealed under our account secret, written and signed by a
/// certified device of OUR account.
fn open_and_verify_passport(storage: &Storage, secret: &[u8; 32], sealed: &[u8]) -> OmniResult<AccountPassportV1> {
    let packed = omni_rendezvous::open_passport(secret, sealed)
        .ok_or_else(|| OmniError::InvalidInput("not sealed under this account".to_string()))?;
    let plain = inflate_bounded(&packed).ok_or_else(|| OmniError::InvalidInput("does not unpack".to_string()))?;
    let passport: AccountPassportV1 =
        serde_json::from_slice(&plain).map_err(|error| OmniError::InvalidInput(format!("unreadable: {error}")))?;
    let chain: omni_crypto::DeviceCertificateChainV1 = serde_json::from_str(&passport.device_chain_json)
        .map_err(|error| OmniError::InvalidInput(format!("writer chain unreadable: {error}")))?;
    omni_crypto::verify_device_certificate_chain(&chain)?;
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::InvalidInput("no local account".to_string()))?;
    let keys = storage
        .get_account_keys(&account.descriptor.account_id)?
        .ok_or_else(|| OmniError::InvalidInput("no account keys".to_string()))?;
    if chain.working.master_public_key != keys.master_public_key {
        return Err(OmniError::InvalidInput("written by another account".to_string()));
    }
    let signature = omni_crypto::Signature { bytes: hex::decode(&passport.signature_hex).unwrap_or_default() };
    omni_crypto::verify_signature(&chain.device.identity_signing_public_key, &passport_signing_bytes(&passport), &signature)?;
    Ok(passport)
}

/// Merge a passport into this device. Only adds: a contact goes through the versioned merge (an
/// older passport cannot undo what a newer one said), the profile is taken only while the user has
/// not changed it since the restore, groups are remembered for rejoining. One bad entry costs only
/// itself. Returns a short summary for the trace.
fn apply_recovered_passport(
    storage: &Storage,
    passport: &AccountPassportV1,
    recovery: &AccountRecoveryStateV1,
    now_unix_ms: i64,
) -> String {
    let own_account_id = storage.get_local_account().ok().flatten().map(|account| account.descriptor.account_id);
    let mut contacts = 0;
    let mut failed = 0;
    let mut removed = Vec::new();
    for entry in &passport.contacts {
        if own_account_id.as_ref().is_some_and(|own| own.0 == entry.account_id) {
            continue;
        }
        if omni_protocol::validate_logical_event_envelope_v1(&omni_types::LogicalEventEnvelopeV1::contact_sync(
            omni_types::ContactSyncV1 { entries: vec![entry.clone()], through_seq: 0 },
        ))
        .is_err()
        {
            failed += 1;
            continue;
        }
        match apply_contact_sync_entry(storage, entry, now_unix_ms) {
            Ok(Some(contact)) => removed.push(contact),
            Ok(None) => contacts += 1,
            Err(error) => {
                failed += 1;
                omni_app_trace_log("passport/recover", &format!("contact {} not merged: {error}", entry.account_id));
            }
        }
    }
    forget_runtime_state_of_removed_contacts(storage, &removed);

    let mut profile = "kept";
    if let (Some(incoming), Ok(local)) = (&passport.profile, storage.get_local_public_profile_state()) {
        // The restore wrote the profile at its start; anything later is the user's own change.
        if local.updated_at_unix_ms <= recovery.started_at_unix_ms + 60_000 {
            let avatar = incoming
                .avatar_b64
                .as_deref()
                .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                .filter(|bytes| bytes.len() <= PASSPORT_AVATAR_MAX_BYTES);
            let avatar_hash = avatar.as_ref().map(|bytes| hex::encode(Sha256::digest(bytes)));
            let taken = storage.adopt_public_profile_from_own_device(
                Some(incoming.display_name.as_str()).filter(|name| !name.trim().is_empty()),
                avatar_hash.as_deref(),
                incoming.avatar_mime_type.as_deref().filter(|_| avatar.is_some()),
                avatar.as_deref(),
                incoming.updated_at_unix_ms.max(local.updated_at_unix_ms),
            );
            profile = match taken {
                Ok(true) => "taken",
                Ok(false) => "same",
                Err(_) => "refused",
            };
        }
    }

    let mut groups = 0;
    for group in &passport.groups {
        if group.group_id.is_empty() || group.group_id.len() > 256 || group.name.len() > 1024 {
            continue;
        }
        if storage
            .remember_recovered_group(&RecoveredGroupRecord {
                group_id: group.group_id.clone(),
                name: group.name.clone(),
                role: if group.role == "admin" { "admin".to_string() } else { "member".to_string() },
                recovered_at_unix_ms: now_unix_ms,
            })
            .is_ok()
        {
            groups += 1;
        }
    }
    format!("contacts={contacts} removed={} failed={failed} profile={profile} groups={groups}", removed.len())
}

// --- restored device -----------------------------------------------------------------------------

/// The devices of approved contacts the passport named and of which we hold no bundle — the ones the
/// restored device has to dial by node to meet again.
fn contacts_to_dial(storage: &Storage, passport: &AccountPassportV1) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (account, keys) in passport.contact_device_keys.iter().take(5000) {
        let account_id = AccountId(account.clone());
        let approved = storage
            .find_contact_by_remote_account_id(&account_id)
            .ok()
            .flatten()
            .is_some_and(|contact| contact.relationship_state == ContactRelationshipState::Approved);
        if !approved || storage.get_latest_remote_prekey_bundle_for_account(&account_id).ok().flatten().is_some() {
            continue;
        }
        for key in keys.iter().take(8) {
            let Some(bytes) = hex::decode(key).ok().and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()) else {
                continue;
            };
            if let Ok(node) = node_address_from_ed25519_pubkey(bytes) {
                out.push((account.clone(), node.to_string()));
            }
        }
    }
    out
}

/// At most this many contact devices are dialed by node per courier round.
const RECOVERY_DIALS_PER_ROUND: usize = 16;

/// Dial the contact devices the passport named without a bundle; a contact whose bundle arrived
/// (the link brought it) is done.
fn dial_recovered_contact_nodes(storage: &Storage, runtime: &MeshRuntime, now_unix_ms: i64) {
    let Some(mut state) = load_account_recovery(storage) else {
        return;
    };
    if !account_recovery_active(&state, now_unix_ms) || state.dial_nodes.is_empty() {
        return;
    }
    let before = state.dial_nodes.len();
    state.dial_nodes.retain(|(account, _)| {
        storage.get_latest_remote_prekey_bundle_for_account(&AccountId(account.clone())).ok().flatten().is_none()
    });
    let mut dialed = 0;
    for (_, node_hex) in &state.dial_nodes {
        if dialed >= RECOVERY_DIALS_PER_ROUND {
            break;
        }
        let Some(node) = hex::decode(node_hex)
            .ok()
            .and_then(|bytes| <[u8; 20]>::try_from(bytes).ok())
            .map(MeshNodeAddress::from_bytes)
        else {
            continue;
        };
        if runtime.has_link(&node) {
            continue;
        }
        let _ = runtime.dial(node);
        dialed += 1;
    }
    if state.dial_nodes.len() != before {
        save_account_recovery(storage, &state);
    }
    if dialed > 0 {
        omni_app_trace_log("passport/recover", &format!("dialed {dialed} contact device(s) by node, {} left", state.dial_nodes.len()));
    }
}

/// A restored device asks every holder it reaches for the passport, again each minute until one
/// answers with something. Caller holds the session lock.
fn apply_recovery_round(storage: &Storage, transport: &TransportManager, now_unix_ms: i64) {
    let Some(state) = load_account_recovery(storage) else {
        return;
    };
    if !account_recovery_active(&state, now_unix_ms) || state.holders.is_empty() {
        return;
    }
    let Ok(Some(secret)) = storage.get_account_rendezvous_secret() else {
        return;
    };
    let db_path = storage.db_path().to_string_lossy().to_string();
    let locator_hex = hex::encode(omni_rendezvous::passport_locator(&secret));
    for holder in &state.holders {
        if state.applied.iter().any(|(applied_from, _)| applied_from == holder) {
            continue;
        }
        let due = with_account_passport_state(&db_path, |passport_state| {
            let last = passport_state.asked_at_unix_ms.get(holder).copied().unwrap_or(0);
            now_unix_ms - last >= RECOVERY_FETCH_AGAIN_MS
        });
        if !due {
            continue;
        }
        let account_id = AccountId(holder.clone());
        let Some(dialog_key) = reachable_session_dialog(storage, transport, &account_id) else {
            continue;
        };
        with_account_passport_state(&db_path, |passport_state| {
            passport_state.asked_at_unix_ms.insert(holder.clone(), now_unix_ms);
        });
        match send_account_event_to_dialog(
            storage,
            transport,
            &account_id,
            &dialog_key,
            omni_types::LogicalEventEnvelopeV1::account_passport(omni_types::AccountPassportMessageV1::Fetch {
                locator_hex: locator_hex.clone(),
            }),
            SecurityPayloadKind::AccountPassport,
            "a passport request waits",
        ) {
            Ok(()) => omni_app_trace_log("passport/recover", &format!("asked holder={holder}")),
            Err(error) => omni_app_trace_log("passport/recover", &format!("holder={holder} not asked: {error}")),
        }
    }
}

/// A holder's mark read at the couriers: make it our approved contact (it keeps our passport, so we
/// trusted it — the passport then says the rest) and remember to ask it. Returns whether it is new.
fn adopt_passport_holder_mark(
    storage: &Storage,
    beacon: &omni_rendezvous::IdentityBeacon,
    now_unix_ms: i64,
) -> OmniResult<bool> {
    let chain = identity_beacon_device_chain(beacon)?;
    let omni_id = omni_crypto::omni_id_bytes(&chain.working.master_public_key);
    verify_identity_beacon(&omni_id, beacon)?;
    let account_id = chain.account_id();
    let Some(account) = storage.get_local_account()? else {
        return Ok(false);
    };
    if account_id == account.descriptor.account_id {
        return Ok(false);
    }
    let bundle_v1 = crate::identity_beacon_prekey_bundle(beacon)
        .map_err(|error| OmniError::InvalidInput(format!("mark bundle unreadable: {error}")))?;
    omni_protocol::validate_prekey_bundle_v1(&bundle_v1)?;
    let bundle = prekey_bundle_from_protocol(&bundle_v1)?;
    if bundle.identity_signing_key != chain.device.identity_signing_public_key || bundle.account_id != account_id {
        return Err(OmniError::InvalidInput("mark carries another device's bundle".to_string()));
    }
    storage.record_remote_prekey_bundle(&bundle, None, Some(&bundle_v1.device.device_label), now_unix_ms)?;
    let existing = storage.find_contact_by_remote_account_id(&account_id)?;
    if let Some(contact) = existing.as_ref().filter(|contact| contact.relationship_state == ContactRelationshipState::Pending) {
        // The holder reached us first and we took it for a stranger's request: it keeps our
        // passport, so it was our contact.
        storage.approve_contact(&contact.contact_id, now_unix_ms)?;
    }
    if existing.is_none() {
        // Its name comes with the passport (or its profile, once a session is up). The record at
        // version 0 makes sure the passport's entry for it — any version — wins the merge.
        // The account id itself: the placeholder the shell knows to show the public name over.
        let placeholder = account_id.0.clone();
        storage.merge_contact_sync_record(
            &omni_storage::ContactSyncRecord {
                account_id: account_id.clone(),
                state: omni_storage::ContactSyncState::Approved,
                state_version: 0,
                state_writer: String::new(),
                petname: placeholder.clone(),
                petname_version: 0,
                petname_writer: String::new(),
                display_name: None,
                seq: 0,
            },
            now_unix_ms,
        )?;
        storage.add_contact(
            &AddContactInput {
                petname: placeholder,
                remote_account_id: Some(account_id.clone()),
                public_display_name: None,
                remote_instance_id: None,
                primary_dialog_key: None,
                initial_observation: None,
            },
            now_unix_ms,
        )?;
    }
    let mut state = load_account_recovery(storage).unwrap_or_default();
    if state.holders.contains(&account_id.0) {
        return Ok(false);
    }
    state.holders.push(account_id.0.clone());
    save_account_recovery(storage, &state);
    omni_app_trace_log("passport/recover", &format!("holder found: {}", account_id.0));
    Ok(true)
}

/// One courier round: a holder posts its marks for the accounts it keeps a passport of; a restored
/// device reads its own marks and dials the holders. Network calls run on a worker thread.
fn account_passport_courier_round(storage: &Storage, runtime: &Arc<MeshRuntime>, db_path: &str) {
    let now = current_unix_ms();
    dial_recovered_contact_nodes(storage, runtime, now);
    let held = storage.list_held_passports().unwrap_or_default();
    let marks: Vec<[u8; 32]> = held
        .iter()
        .filter_map(|held| hex::decode(&held.mark_secret_hex).ok()?.try_into().ok())
        .collect();
    let marks_fingerprint = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        marks.hash(&mut hasher);
        hasher.finish()
    };
    let recovery = load_account_recovery(storage).filter(|state| account_recovery_active(state, now));
    let own_mark_secret = recovery
        .as_ref()
        .and_then(|_| storage.get_account_rendezvous_secret().ok().flatten())
        .map(|secret| omni_rendezvous::passport_mark_secret(&secret));
    let (post, read) = with_account_passport_state(db_path, |state| {
        let post = !marks.is_empty()
            && (state.marks_posted_fingerprint != marks_fingerprint
                || now - state.marks_posted_at_unix_ms >= PASSPORT_MARK_REPUBLISH_MS);
        let read_every = match &recovery {
            Some(recovery) if now - recovery.started_at_unix_ms < 60 * 60 * 1000 => RECOVERY_MARKS_READ_EARLY_MS,
            _ => RECOVERY_MARKS_READ_LATER_MS,
        };
        let read = own_mark_secret.is_some() && now - state.marks_read_at_unix_ms >= read_every;
        if post {
            state.marks_posted_at_unix_ms = now;
            state.marks_posted_fingerprint = marks_fingerprint;
        }
        if read {
            state.marks_read_at_unix_ms = now;
        }
        (post, read)
    });
    if !post && !read {
        return;
    }
    let beacon = if post { build_signed_identity_beacon(storage, runtime, None) } else { None };
    let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
        return;
    };
    let runtime = Arc::clone(runtime);
    let db_path = db_path.to_string();
    let _ = std::thread::Builder::new().name("omni-account-passport".to_string()).spawn(move || {
        let courier = rendezvous.helper.as_ref();
        if let Some(beacon) = beacon {
            let wire = beacon.to_wire();
            let mut posted = 0;
            for mark_secret in &marks {
                posted += publish_account_mailbox_with(courier, mark_secret, &wire, now);
            }
            omni_app_trace_log("passport/marks", &format!("{} mark(s) posted to {posted} courier(s)", marks.len()));
        }
        let Some(mark_secret) = own_mark_secret.filter(|_| read) else {
            return;
        };
        let mut found = Vec::new();
        for (topic, dht_key) in omni_rendezvous::mailbox_topics_with_keys(&mark_secret, now.max(0) as u64) {
            let Ok(blobs) = courier.fetch(&topic.id(), &hex::encode(dht_key), CourierLegs::Web) else {
                continue;
            };
            for blob in blobs {
                let Some(beacon) = hex::decode(&blob)
                    .ok()
                    .and_then(|wire| omni_rendezvous::open_mailbox(&mark_secret, &topic, &wire))
                    .and_then(|content| omni_rendezvous::IdentityBeacon::from_wire(&content))
                else {
                    continue;
                };
                if !found.iter().any(|known: &omni_rendezvous::IdentityBeacon| known.node == beacon.node) {
                    found.push(beacon);
                }
            }
        }
        omni_app_trace_log("passport/marks", &format!("read {} holder mark(s)", found.len()));
        let Ok(storage) = Storage::open(&db_path) else {
            return;
        };
        let allow_direct = crate::app_pinned_relay::allows_direct(&db_path);
        for beacon in found {
            match adopt_passport_holder_mark(&storage, &beacon, current_unix_ms()) {
                Ok(_) => {
                    let targets = std::sync::Mutex::new(std::collections::HashSet::new());
                    dial_identity_beacon_target(
                        &runtime,
                        &storage,
                        &targets,
                        &beacon,
                        MeshNodeAddress::from_bytes(beacon.node),
                        &[],
                        allow_direct,
                    );
                }
                Err(error) => omni_app_trace_log("passport/marks", &format!("mark refused: {error}")),
            }
        }
    });
}
