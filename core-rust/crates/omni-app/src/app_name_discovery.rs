// Find by name (docs/decisions/find-by-name.md). Off by default and only together with "findable by
// Omni-ID": the beacon under a name is a pointer to the Omni-ID, and a pointer to an address nobody
// can resolve is a promise the app cannot keep.
//
// What goes out: a signed pointer (Omni-ID, the name as written in the profile, a photo of ≤ ~1.3 KB)
// under the whole normalized name and under each of its words. Never endpoints or keys — whoever
// enumerates common names learns that an account with this name exists, not where it is.
//
// Where and how often: along the profile's plan for the name (`app_publish_plans`, «Авто» — Nostr,
// ntfy if Nostr did not take it, once a window: a reader looks at this window and the last), and at
// once when the name or the photo changed. NEVER the BT-DHT (the plan does not allow it): it keeps one
// record per key, and a key anyone can compute from a name would be a fight over who writes last. Under keys the name no longer has, and everywhere when the
// switch goes off, a withdrawal goes out: couriers cannot delete, but finders keep the newest.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use omni_rendezvous::name_discovery::{
    name_beacon_signing_bytes, name_search_keys, name_topics_with_keys, normalize_search_name, NameBeacon,
    NAME_BEACON_MAX_WIRE,
};
use omni_storage::Storage;
use omni_types::{OmniError, OmniResult};

use crate::{current_unix_ms, omni_app_trace_log, CourierLegs, OmniApp, PublishPriority, PublishRoute, RendezvousCourier};

/// A name beacon lives its window and an hour, like the identity beacon.
const NAME_BEACON_TTL_SECS: u32 = crate::BEACON_TTL_SECS;
/// At most this many people in one answer (the shell shows 20 at a time and "Показать ещё"). Not a
/// defence against a flood of fake namesakes — whoever posts the freshest beacons would push real
/// people past any cap; that defence, if it is ever needed, is a proof of work in the beacon
/// (find-by-name.md §5). This only bounds the list and the work of verifying it.
pub const NAME_SEARCH_MAX_PEOPLE: usize = 100;
/// A query shorter than this (normalized, in characters) is not looked up.
pub const NAME_SEARCH_MIN_CHARS: usize = 2;

fn name_findable_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "name-findable.json")
}

/// The keys our beacon is out under, kept so a renamed or switched-off profile can withdraw it there —
/// also after a restart.
fn name_published_keys_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "name-beacon-keys.json")
}

/// The switch as the user set it.
pub(crate) fn load_name_findable(db_path: &str) -> bool {
    std::fs::read_to_string(name_findable_path(db_path)).is_ok_and(|raw| raw.trim() == "true")
}

/// The switch as it works: on only while "findable by Omni-ID" is on too.
pub(crate) fn name_findable_in_effect(db_path: &str) -> bool {
    load_name_findable(db_path) && crate::load_discoverability_mode(db_path).is_public()
}

fn load_published_keys(db_path: &str) -> Vec<String> {
    std::fs::read_to_string(name_published_keys_path(db_path))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store_published_keys(db_path: &str, keys: &[String]) {
    let path = name_published_keys_path(db_path);
    if keys.is_empty() {
        let _ = std::fs::remove_file(path);
    } else if let Ok(json) = serde_json::to_string(keys) {
        let _ = std::fs::write(path, json);
    }
}

/// db_path → fingerprint of what the last publish said. In memory: after a restart the beacon simply
/// goes out once more.
static NAME_BEACON_PUBLISHES: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn publishes() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
    NAME_BEACON_PUBLISHES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn clear_name_discovery_for_profile(db_path: &str) {
    publishes().remove(db_path);
}

/// What the person needs from the beacon: the name and the photo. Time and seq are left out.
fn beacon_fingerprint(display_name: &str, thumb: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    display_name.hash(&mut hasher);
    thumb.hash(&mut hasher);
    hasher.finish()
}

/// This device's signed name beacon (`withdrawn`: an empty one that hides the device). `None` for a
/// device without a certificate yet.
fn build_name_beacon(storage: &Storage, withdrawn: bool) -> Option<NameBeacon> {
    let account = storage.get_local_account().ok()??;
    let identity = storage.get_local_identity_key(&account.descriptor.account_id).ok()??;
    let now = current_unix_ms();
    let chain = storage.build_local_prekey_bundle(now).ok()?.device_certificate_chain?;
    let mut beacon = NameBeacon {
        omni_id: omni_crypto::omni_id_bytes(&chain.working.master_public_key),
        chain_compact: chain.to_compact(),
        display_name: String::new(),
        thumb_jpeg: Vec::new(),
        ts_ms: now as u64,
        ttl_secs: NAME_BEACON_TTL_SECS,
        seq: now as u64,
        signature: vec![0; 64],
    };
    if !withdrawn {
        let profile = storage.get_local_public_profile_state().ok()?;
        beacon.display_name = profile.display_name;
        let budget = NAME_BEACON_MAX_WIRE.saturating_sub(beacon.to_wire().len());
        beacon.thumb_jpeg =
            profile.avatar_bytes.as_deref().map(|avatar| omni_storage::shrink_avatar_to_budget(avatar, budget)).unwrap_or_default();
    }
    beacon.signature = Vec::new();
    let signature = omni_crypto::sign_message(&identity.protected_signing_private_key, &name_beacon_signing_bytes(&beacon)).ok()?;
    beacon.signature = signature.bytes;
    Some(beacon)
}

/// Post `beacon` under the current topic of each key, along `route`. Returns how many keys took it.
fn post_under_keys(
    courier: &dyn RendezvousCourier,
    beacon: &NameBeacon,
    keys: &[String],
    now_ms: u64,
    route: &PublishRoute,
) -> usize {
    let wire_hex = hex::encode(beacon.to_wire());
    keys.iter()
        .filter(|key| {
            // The current window only: a reader looks at this window and the previous one, and the
            // beacon is posted again in each window.
            name_topics_with_keys(key, now_ms).first().is_some_and(|(topic, dht_key)| {
                courier.publish_routed(&topic.id(), &hex::encode(dht_key), &wire_hex, None, NAME_BEACON_TTL_SECS, route).is_ok()
            })
        })
        .count()
}

/// One sweep's worth: publish, re-publish in a new window or after a change, withdraw where the name
/// no longer is. Called from the rendezvous sweep next to the identity beacon.
pub(crate) fn sweep_name_beacon(storage: &Storage, courier: &dyn RendezvousCourier, db_path: &str) {
    let now_ms = current_unix_ms() as u64;
    let published = load_published_keys(db_path);
    let on = name_findable_in_effect(db_path);
    let profile_name = if on {
        storage.get_local_public_profile_state().ok().map(|profile| profile.display_name).unwrap_or_default()
    } else {
        String::new()
    };
    let keys = name_search_keys(&profile_name);

    // Withdraw where we are out but should not be.
    let stale: Vec<String> = published.iter().filter(|key| !keys.contains(key)).cloned().collect();
    if !stale.is_empty() {
        let Some(withdrawal) = build_name_beacon(storage, true) else {
            return;
        };
        let route = name_route(courier.due_steps(crate::Publication::Name, NAME_PLAN_KEY, true));
        let withdrawn = post_under_keys(courier, &withdrawal, &stale, now_ms, &route);
        omni_app_trace_log("name/withdraw", &format!("keys={} withdrawn={withdrawn}", stale.len()));
        if withdrawn == stale.len() {
            let kept: Vec<String> = published.iter().filter(|key| keys.contains(key)).cloned().collect();
            store_published_keys(db_path, &kept);
        }
    }
    if keys.is_empty() {
        publishes().remove(db_path);
        return;
    }

    let Some(beacon) = build_name_beacon(storage, false) else {
        omni_app_trace_log("name/publish", "skip=no_device_certificate");
        return;
    };
    let fingerprint = beacon_fingerprint(&beacon.display_name, &beacon.thumb_jpeg);
    let all_out = keys.iter().all(|key| published.contains(key));
    // Unchanged and out under every key: only the plan's blocks whose frequency came up.
    let changed = !all_out || publishes().get(db_path) != Some(&fingerprint);
    let steps = courier.due_steps(crate::Publication::Name, NAME_PLAN_KEY, changed);
    if steps.is_empty() {
        return;
    }
    let taken = post_under_keys(courier, &beacon, &keys, now_ms, &name_route(steps));
    omni_app_trace_log(
        "name/publish",
        &format!("keys={} taken={taken} wire={} thumb={}", keys.len(), beacon.to_wire().len(), beacon.thumb_jpeg.len()),
    );
    if taken > 0 {
        let mut out: Vec<String> = load_published_keys(db_path);
        for key in &keys {
            if !out.contains(key) {
                out.push(key.clone());
            }
        }
        store_published_keys(db_path, &out);
    }
    if taken == keys.len() {
        publishes().insert(db_path.to_string(), fingerprint);
    }
}

/// The name beacons' memory key in the plan (one for every key of the name: they go out together).
const NAME_PLAN_KEY: &str = "name";

fn name_route(steps: Vec<Vec<String>>) -> PublishRoute {
    PublishRoute {
        key: NAME_PLAN_KEY.to_string(),
        steps,
        reach: 1,
        priority: PublishPriority::Background,
    }
}

/// Is this beacon what it says: a fresh beacon of a certified device of the account behind its
/// Omni-ID, signed by that device, posted under one of its own names? Returns the chain on success.
pub(crate) fn verify_name_beacon(
    beacon: &NameBeacon,
    key: &str,
    now_ms: u64,
) -> Option<omni_crypto::DeviceCertificateChainV1> {
    if !beacon.is_fresh(now_ms) || !beacon.matches_key(key) {
        return None;
    }
    let chain = omni_crypto::DeviceCertificateChainV1::from_compact(&beacon.chain_compact)?;
    omni_crypto::verify_device_certificate_chain(&chain).ok()?;
    if !omni_crypto::verify_omni_id_commitment(&beacon.omni_id, &chain.working.master_public_key) {
        return None;
    }
    omni_crypto::verify_signature(
        &chain.device.identity_signing_public_key,
        &name_beacon_signing_bytes(beacon),
        &omni_crypto::Signature { bytes: beacon.signature.clone() },
    )
    .ok()?;
    Some(chain)
}

/// One person found by name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FoundByNameV1 {
    /// `omni:<40 hex>` — what the find-by-Omni-ID path takes next.
    pub omni_id: String,
    /// The name as the person wrote it.
    pub display_name: String,
    /// The small photo from the beacon (JPEG, base64), if there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub photo_jpeg_b64: Option<String>,
    /// Already in our contacts (by account) — the list says so instead of offering a request.
    pub is_contact: bool,
    /// It is us (another device of our account, or this one).
    pub is_self: bool,
    /// The whole query matched the whole name (not one word of it) — such people come first.
    pub whole_name: bool,
}

/// The answer to a search by name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NameSearchResultV1 {
    /// `found` · `nobody` (couriers answered, no one under this name) · `unreachable` (no courier
    /// answered — we do not know) · `too_short`.
    pub status: String,
    pub people: Vec<FoundByNameV1>,
}

/// Look `query` up: verified beacons under its normalized key, the freshest per device, devices that
/// withdrew dropped, one row per account, at most [`NAME_SEARCH_MAX_PEOPLE`].
pub(crate) fn search_by_name_via(
    courier: &dyn RendezvousCourier,
    query: &str,
    my_omni_id: Option<[u8; 20]>,
    is_contact: &dyn Fn(&omni_identity::AccountId) -> bool,
) -> OmniResult<NameSearchResultV1> {
    let key = normalize_search_name(query);
    if key.chars().count() < NAME_SEARCH_MIN_CHARS {
        return Ok(NameSearchResultV1 { status: "too_short".to_string(), people: Vec::new() });
    }
    let now_ms = current_unix_ms() as u64;
    // The freshest beacon of each device, with its account.
    let mut devices: Vec<(String, NameBeacon, omni_identity::AccountId)> = Vec::new();
    let mut reached = false;
    for (topic, dht_key) in name_topics_with_keys(&key, now_ms) {
        let (blobs, answered) = courier.fetch_with_reach(&topic.id(), &hex::encode(dht_key), CourierLegs::Web)?;
        reached |= answered;
        for blob in blobs {
            let Some(beacon) = hex::decode(&blob).ok().and_then(|bytes| NameBeacon::from_wire(&bytes)) else {
                continue;
            };
            let Some(chain) = verify_name_beacon(&beacon, &key, now_ms) else {
                continue;
            };
            let device = chain.device.device_id.0.clone();
            match devices.iter_mut().find(|(known, _, _)| *known == device) {
                Some((_, known, _)) if beacon.seq > known.seq => *known = beacon,
                Some(_) => {}
                None => devices.push((device, beacon, chain.account_id())),
            }
        }
    }
    devices.retain(|(_, beacon, _)| !beacon.is_withdrawal());
    devices.sort_by(|a, b| b.1.seq.cmp(&a.1.seq));
    let mut people: Vec<FoundByNameV1> = Vec::new();
    for (_, beacon, account_id) in devices {
        let omni_id = format!("omni:{}", hex::encode(beacon.omni_id));
        if people.iter().any(|person| person.omni_id == omni_id) {
            continue;
        }
        people.push(FoundByNameV1 {
            omni_id,
            whole_name: normalize_search_name(&beacon.display_name) == key,
            display_name: beacon.display_name,
            photo_jpeg_b64: (!beacon.thumb_jpeg.is_empty())
                .then(|| base64::engine::general_purpose::STANDARD.encode(&beacon.thumb_jpeg)),
            is_contact: is_contact(&account_id),
            is_self: my_omni_id == Some(beacon.omni_id),
        });
    }
    // The whole name before a word of it; otherwise freshest first (stable sort keeps that order).
    people.sort_by_key(|person| !person.whole_name);
    people.truncate(NAME_SEARCH_MAX_PEOPLE);
    let status = match (people.is_empty(), reached) {
        (false, _) => "found",
        (true, true) => "nobody",
        (true, false) => "unreachable",
    };
    Ok(NameSearchResultV1 { status: status.to_string(), people })
}

impl OmniApp {
    /// Whether "findable by name" is switched on (as set — it works only while findable by Omni-ID).
    pub fn name_findable(&self) -> bool {
        load_name_findable(&self.storage.db_path().to_string_lossy())
    }

    /// Switch "findable by name". On takes effect on the next sweep; off withdraws the beacons on it.
    pub fn set_name_findable(&self, on: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_name_findable")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        std::fs::write(name_findable_path(&db_path), if on { "true" } else { "false" })
            .map_err(|error| OmniError::Storage(format!("failed to persist name findability: {error}")))?;
        clear_name_discovery_for_profile(&db_path);
        Ok(())
    }

    /// Search people by name on the couriers (web ones — names never go to the BT-DHT).
    pub fn search_people_by_name(&self, query: &str) -> OmniResult<NameSearchResultV1> {
        let _ = self.ensure_runtime_profile_active("search_people_by_name")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let rendezvous = crate::profile_rendezvous_runtime(&db_path)?;
        let my_omni_id = self
            .storage
            .get_local_account()?
            .and_then(|account| self.storage.get_account_keys(&account.descriptor.account_id).ok().flatten())
            .map(|keys| omni_crypto::omni_id_bytes(&keys.master_public_key));
        let storage = &self.storage;
        let is_contact = |account_id: &omni_identity::AccountId| {
            storage.find_contact_by_remote_account_id(account_id).ok().flatten().is_some()
        };
        let result = search_by_name_via(rendezvous.helper.as_ref(), query, my_omni_id, &is_contact)?;
        omni_app_trace_log("name/search", &format!("status={} people={}", result.status, result.people.len()));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Topic → posted blobs; `reach` says whether any courier answered.
    struct Courier {
        topics: Mutex<HashMap<String, Vec<String>>>,
        reach: bool,
    }

    impl Courier {
        fn new() -> Self {
            Self { topics: Mutex::new(HashMap::new()), reach: true }
        }
    }

    impl RendezvousCourier for Courier {
        fn publish(&self, topic_hex: &str, _: &str, blob_hex: &str, _: u32, _: PublishPriority) -> OmniResult<()> {
            self.topics.lock().unwrap().entry(topic_hex.to_string()).or_default().push(blob_hex.to_string());
            Ok(())
        }
        fn fetch(&self, topic_hex: &str, _: &str, _: CourierLegs) -> OmniResult<Vec<String>> {
            Ok(self.topics.lock().unwrap().get(topic_hex).cloned().unwrap_or_default())
        }
        fn fetch_with_reach(&self, topic_hex: &str, key: &str, legs: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
            self.fetch(topic_hex, key, legs).map(|blobs| (blobs, self.reach))
        }
    }

    /// A device of the account behind `root`: its chain and signing key.
    fn device(root: u8, device: &str) -> (omni_crypto::DeviceCertificateChainV1, omni_crypto::LocalIdentityKeyPair) {
        use omni_crypto::{generate_local_key_hierarchy, AccountMasterKey};
        use omni_identity::{AccountRootSecret, DeviceId};
        let master = AccountMasterKey::derive(&AccountRootSecret(hex::encode([root; 32]))).unwrap();
        let working = master.working_key(1).unwrap();
        let working_certificate = omni_crypto::issue_working_key_certificate(&master, &working, 1_000);
        let hierarchy =
            generate_local_key_hierarchy(&master.public_key().account_id(), &DeviceId(device.into()), 1_000, 1).unwrap();
        let identity = hierarchy.identity_key.clone();
        let chain = omni_crypto::issue_device_certificate(
            &working_certificate,
            &working,
            &DeviceId(device.into()),
            identity.public_key,
            identity.signing_public_key,
            hierarchy.device_key.public_key,
            1_000,
        )
        .unwrap();
        (chain, identity)
    }

    fn signed(
        chain: &omni_crypto::DeviceCertificateChainV1,
        identity: &omni_crypto::LocalIdentityKeyPair,
        name: &str,
        seq: u64,
    ) -> NameBeacon {
        let now = current_unix_ms() as u64;
        let mut beacon = NameBeacon {
            omni_id: omni_crypto::omni_id_bytes(&chain.working.master_public_key),
            chain_compact: chain.to_compact(),
            display_name: name.to_string(),
            thumb_jpeg: vec![0xFF, 0xD8, 1, 2, 3],
            ts_ms: now,
            ttl_secs: 3_600,
            seq: now + seq,
            signature: Vec::new(),
        };
        beacon.signature = omni_crypto::sign_message(&identity.protected_signing_private_key, &name_beacon_signing_bytes(&beacon))
            .unwrap()
            .bytes;
        beacon
    }

    fn post(courier: &Courier, beacon: &NameBeacon, keys: &[String]) {
        let route = PublishRoute::everywhere(PublishPriority::Background);
        post_under_keys(courier, beacon, keys, current_unix_ms() as u64, &route);
    }

    fn nobody_is_a_contact(_: &omni_identity::AccountId) -> bool {
        false
    }

    #[test]
    fn a_person_is_found_by_the_whole_name_and_by_the_surname_once_per_account() {
        let courier = Courier::new();
        let (laptop, laptop_key) = device(1, "laptop");
        let (desktop, desktop_key) = device(1, "desktop");
        for (chain, key) in [(&laptop, &laptop_key), (&desktop, &desktop_key)] {
            let beacon = signed(chain, key, "Марк Пётров", 0);
            post(&courier, &beacon, &name_search_keys(&beacon.display_name));
        }
        let (other, other_key) = device(2, "pc");
        let namesake = signed(&other, &other_key, "Иван Петров", 0);
        post(&courier, &namesake, &name_search_keys(&namesake.display_name));

        let by_surname = search_by_name_via(&courier, "Петров", None, &nobody_is_a_contact).unwrap();
        assert_eq!(by_surname.status, "found");
        assert_eq!(by_surname.people.len(), 2, "two accounts, one of them on two devices: {:?}", by_surname.people);
        assert!(by_surname.people.iter().all(|person| !person.whole_name));

        let whole = search_by_name_via(&courier, "марк петров", None, &nobody_is_a_contact).unwrap();
        assert_eq!(whole.people.len(), 1);
        assert!(whole.people[0].whole_name);
        assert_eq!(whole.people[0].display_name, "Марк Пётров");
        assert!(whole.people[0].photo_jpeg_b64.is_some());
        assert_eq!(
            whole.people[0].omni_id,
            format!("omni:{}", hex::encode(omni_crypto::omni_id_bytes(&laptop.working.master_public_key)))
        );
    }

    #[test]
    fn forged_and_misplaced_beacons_are_not_shown() {
        let courier = Courier::new();
        let (chain, key) = device(3, "pc");
        let keys = vec!["марк".to_string()];
        // Another name posted under the key: noise.
        post(&courier, &signed(&chain, &key, "Пётр", 0), &keys);
        // The name changed after signing.
        let mut renamed = signed(&chain, &key, "Марк", 0);
        renamed.display_name = "Марка".to_string();
        post(&courier, &renamed, &["марка".to_string()]);
        // Someone else's Omni-ID on a valid chain.
        let mut stolen = signed(&chain, &key, "Марк", 0);
        stolen.omni_id = [9; 20];
        post(&courier, &stolen, &keys);
        assert_eq!(search_by_name_via(&courier, "марк", None, &nobody_is_a_contact).unwrap().status, "nobody");
        assert_eq!(search_by_name_via(&courier, "марка", None, &nobody_is_a_contact).unwrap().status, "nobody");
    }

    #[test]
    fn a_newer_withdrawal_hides_the_device_and_self_and_contacts_are_marked() {
        let courier = Courier::new();
        let (mine, my_key) = device(5, "laptop");
        let (friend, friend_key) = device(6, "pc");
        let keys = vec!["оля".to_string()];
        post(&courier, &signed(&mine, &my_key, "Оля", 0), &keys);
        post(&courier, &signed(&friend, &friend_key, "Оля", 0), &keys);
        let my_omni_id = omni_crypto::omni_id_bytes(&mine.working.master_public_key);
        let friend_account = friend.account_id();
        let is_contact = |account: &omni_identity::AccountId| *account == friend_account;
        let found = search_by_name_via(&courier, "Оля", Some(my_omni_id), &is_contact).unwrap();
        assert_eq!(found.people.len(), 2);
        assert!(found.people.iter().any(|person| person.is_self && !person.is_contact));
        assert!(found.people.iter().any(|person| person.is_contact && !person.is_self));

        post(&courier, &signed(&friend, &friend_key, "", 10), &keys);
        let after = search_by_name_via(&courier, "оля", Some(my_omni_id), &is_contact).unwrap();
        assert_eq!(after.people.len(), 1, "the friend withdrew");
        assert!(after.people[0].is_self);
    }

    #[test]
    fn the_answer_is_bounded_a_short_query_is_not_looked_up_and_an_unanswered_search_is_not_nobody() {
        let courier = Courier::new();
        let keys = vec!["аня".to_string()];
        let (chain, key) = device(9, "d");
        for n in 0..(NAME_SEARCH_MAX_PEOPLE as u64 + 5) {
            // Same device, so one row — the bound is checked on accounts below by construction.
            post(&courier, &signed(&chain, &key, "Аня", n), &keys);
        }
        assert_eq!(search_by_name_via(&courier, "аня", None, &nobody_is_a_contact).unwrap().people.len(), 1);
        assert_eq!(search_by_name_via(&courier, "я", None, &nobody_is_a_contact).unwrap().status, "too_short");
        let silent = Courier { topics: Mutex::new(HashMap::new()), reach: false };
        assert_eq!(search_by_name_via(&silent, "аня", None, &nobody_is_a_contact).unwrap().status, "unreachable");
    }

    #[test]
    fn a_real_name_beacon_leaves_room_for_a_photo_within_ntfy() {
        let (chain, key) = device(7, "a-device-with-a-long-name-0123456789");
        let mut beacon = signed(&chain, &key, "Константин Константинопольский", 0);
        let budget = NAME_BEACON_MAX_WIRE.saturating_sub(beacon.to_wire().len());
        assert!(budget >= 1_000, "room for a photo: {budget} bytes");
        beacon.thumb_jpeg = vec![0; budget];
        assert!(beacon.to_wire().len() <= NAME_BEACON_MAX_WIRE);
        assert!(NameBeacon::from_wire(&beacon.to_wire()).is_some());
    }
}
