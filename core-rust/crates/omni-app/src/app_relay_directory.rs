// Public relay directory (docs/decisions/public-relay-directory.md), stage 1: web couriers.
//
// A node whose owner turned on "be a relay for everyone" posts a signed entry — how to reach it, a
// name, how full it is — on the directory shelves; anyone looking for a relay reads every shelf on
// demand, keeps what verifies, and ranks it. Nothing happens in the background on the reader's side.
//
// The entry is signed by the node key, which IS this device's identity signing key (the mesh derives
// the node address from it): the relay is not a separate identity. A contact who knows our node
// address recognises us in the directory — the switch says so.
//
// Where and how often: the profile's plan for the directory (`app_publish_plans`; «Авто» — Nostr
// every 15 min, replacing — one entry per relay; ntfy, which keeps every message and is metered per
// IP, once a window and only if Nostr did not take it). An entry lives twice its block's period (at
// least 30 min, at most the window and an hour). A withdrawal (no endpoints) goes to every kind of
// the plan when the switch goes off.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use omni_mesh_runtime::MeshRuntime;
use omni_rendezvous::relay_directory::{
    mainline_info_hash,
    relay_entry_signing_bytes, relay_publish_topic, relay_read_topics, RelayDirectoryEntry, RelayLoad, RelayShelfKind,
};
use omni_storage::Storage;
use omni_types::{OmniError, OmniResult};

use crate::{current_unix_ms, omni_app_trace_log, CourierLegs, OmniApp, RendezvousCourier};

/// An entry lives twice its block's period: a missed post does not take the relay off the list. At
/// least half an hour, at most a window and an hour (a reader looks at this window and the last).
fn entry_ttl_secs(every_secs: u32) -> u32 {
    every_secs.saturating_mul(2).clamp(30 * 60, (omni_rendezvous::TOPIC_WINDOW_SECS + 60 * 60) as u32)
}

/// At most this many relays in one answer (§4).
pub const RELAY_SEARCH_MAX: usize = 100;

/// The operator's switch and what goes with it, per profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RelayRole {
    on: bool,
    #[serde(default)]
    name: String,
    /// When the switch went on — the entry's `since`.
    #[serde(default)]
    since_ms: u64,
    /// An entry may be out on the couriers: switching off owes them a withdrawal (also after a restart).
    #[serde(default)]
    announced: bool,
}

fn relay_role_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "public-relay.json")
}

fn load_relay_role(db_path: &str) -> RelayRole {
    std::fs::read_to_string(relay_role_path(db_path))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store_relay_role(db_path: &str, role: &RelayRole) -> OmniResult<()> {
    let json = serde_json::to_string(role).map_err(|error| OmniError::Internal(error.to_string()))?;
    std::fs::write(relay_role_path(db_path), json)
        .map_err(|error| OmniError::Storage(format!("failed to persist the relay role: {error}")))
}

/// When we first saw each relay (node pubkey hex → ms): its claimed seniority is believed only as far
/// as our own sightings go (§5).
fn relay_sightings_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "relay-sightings.json")
}

/// How many sightings are remembered; the oldest-seen go first beyond it.
const RELAY_SIGHTINGS_MAX: usize = 2_000;

fn load_relay_sightings(db_path: &str) -> HashMap<String, u64> {
    std::fs::read_to_string(relay_sightings_path(db_path))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn store_relay_sightings(db_path: &str, sightings: &HashMap<String, u64>) {
    let mut kept: Vec<(&String, &u64)> = sightings.iter().collect();
    kept.sort_by(|a, b| b.1.cmp(a.1));
    kept.truncate(RELAY_SIGHTINGS_MAX);
    let kept: HashMap<&String, &u64> = kept.into_iter().collect();
    if let Ok(json) = serde_json::to_string(&kept) {
        let _ = std::fs::write(relay_sightings_path(db_path), json);
    }
}

/// db_path → what went out last: when each kind last took the entry, and what it said. In memory:
/// after a restart the entry simply goes out once more.
#[derive(Clone, Default)]
struct Posted {
    taken: HashMap<String, u64>,
    fingerprint: u64,
}

static RELAY_DIRECTORY_POSTS: OnceLock<Mutex<HashMap<String, Posted>>> = OnceLock::new();

fn posts() -> std::sync::MutexGuard<'static, HashMap<String, Posted>> {
    RELAY_DIRECTORY_POSTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn clear_relay_directory_for_profile(db_path: &str) {
    posts().remove(db_path);
}

/// What a reader needs from the entry: where and who. Time and seq are left out.
fn entry_fingerprint(entry: &RelayDirectoryEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entry.udp.hash(&mut hasher);
    entry.tcp.hash(&mut hasher);
    entry.name.hash(&mut hasher);
    hasher.finish()
}

fn sign_entry(entry: &mut RelayDirectoryEntry, seed: &[u8; 32]) {
    entry.signature = Vec::new();
    entry.signature = SigningKey::from_bytes(seed).sign(&relay_entry_signing_bytes(entry)).to_bytes().to_vec();
}

/// The entry's signature is the node key's.
pub(crate) fn relay_entry_signed(entry: &RelayDirectoryEntry) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(&entry.node_pubkey) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&entry.signature) else {
        return false;
    };
    key.verify_strict(&relay_entry_signing_bytes(entry), &signature).is_ok()
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    seed: &[u8; 32],
    pubkey: [u8; 32],
    udp: Vec<String>,
    tcp: Vec<String>,
    name: &str,
    since_ms: u64,
    ttl_secs: u32,
    now_ms: u64,
) -> RelayDirectoryEntry {
    let mut entry = RelayDirectoryEntry {
        node_pubkey: pubkey,
        udp,
        tcp,
        name: name.to_string(),
        // TODO(2026-10-14): load from the stranger-link budget once it exists (stage 3); until then
        // the operator has no limits to be close to.
        load: RelayLoad::Free,
        since_ms,
        ts_ms: now_ms,
        ttl_secs,
        seq: now_ms,
        pow_nonce: 0,
        signature: Vec::new(),
    };
    sign_entry(&mut entry, seed);
    entry
}

fn shelf_kind(courier_kind: &str) -> Option<RelayShelfKind> {
    match courier_kind {
        "nostr" => Some(RelayShelfKind::Nostr),
        "ntfy" => Some(RelayShelfKind::Ntfy),
        "dht" => Some(RelayShelfKind::Mainline),
        _ => None,
    }
}

/// The relay directory's Mainline shelves (stage 2): `announce_peer` puts our address on a shelf, a
/// reader gets bare `ip:port`s and learns who sits there by dialing «whoever you are» and asking for
/// the relay card. Behind a trait so the sweep and the search stay testable without a swarm.
pub(crate) trait MainlineShelves: Sync {
    /// Put this node's IP (as the swarm sees it) with `port` on the shelf `info_hash`.
    fn announce(&self, info_hash: &[u8; 20], port: u16) -> bool;
    /// The addresses on the shelf; `None` — could not ask at all.
    fn peers(&self, info_hash: &[u8; 20]) -> Option<Vec<std::net::SocketAddr>>;
    /// Who answers at `addr` and its relay card (unverified — the caller checks the signature).
    fn probe(&self, addr: std::net::SocketAddr) -> Option<([u8; 20], Vec<u8>)>;
}

/// The live shelves: our own Mainline through the DHT helper; when it sees no swarm, reading goes
/// through a friend's (announcing cannot — BEP 5 records the sender's IP, the friend's).
pub(crate) struct LiveMainline<'a> {
    pub(crate) db_path: &'a str,
    pub(crate) runtime: &'a MeshRuntime,
}

/// How long one probe (dial «whoever you are», then the card) may take.
const MAINLINE_PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(4);

impl MainlineShelves for LiveMainline<'_> {
    fn announce(&self, info_hash: &[u8; 20], port: u16) -> bool {
        !crate::dht_locally_blocked(self.db_path)
            && crate::dht_helper_endpoint(self.db_path)
                .is_some_and(|endpoint| crate::dht_helper_announce(&endpoint, info_hash, port))
    }

    fn peers(&self, info_hash: &[u8; 20]) -> Option<Vec<std::net::SocketAddr>> {
        if !crate::dht_locally_blocked(self.db_path) {
            return crate::dht_helper_endpoint(self.db_path)
                .and_then(|endpoint| crate::dht_helper_peers(&endpoint, info_hash));
        }
        let own = crate::own_device_nodes_for(self.db_path);
        crate::dht_friends_in_order(self.runtime.dht_proxy_candidates(), &own).into_iter().find_map(|proxy| {
            self.runtime
                .dht_peers_over_peer(proxy, *info_hash, std::time::Duration::from_secs(5))
                .map(|peers| peers.into_iter().map(std::net::SocketAddr::V4).collect())
        })
    }

    fn probe(&self, addr: std::net::SocketAddr) -> Option<([u8; 20], Vec<u8>)> {
        self.runtime
            .probe_relay_at(addr, MAINLINE_PROBE_DEADLINE)
            .map(|(node, card)| (*node.as_bytes(), card))
    }
}

/// The port a reader dials: the first public UDP endpoint of the entry.
fn announce_port(entry: &RelayDirectoryEntry) -> Option<u16> {
    entry.udp.iter().find_map(|endpoint| endpoint.parse::<std::net::SocketAddr>().ok()).map(|addr| addr.port())
}

fn post_on(
    courier: &dyn RendezvousCourier,
    mainline: Option<&dyn MainlineShelves>,
    kind: RelayShelfKind,
    entry: &RelayDirectoryEntry,
    now_ms: u64,
) -> bool {
    let (topic, dht_key) = relay_publish_topic(kind, &entry.node_pubkey, now_ms);
    if kind == RelayShelfKind::Mainline {
        // A withdrawal has nothing to announce: the address ages out of the swarm by itself, and the
        // card is gone, so nobody answers «whoever you are» any more.
        return match (mainline, announce_port(entry)) {
            (Some(mainline), Some(port)) => mainline.announce(&mainline_info_hash(&dht_key), port),
            _ => false,
        };
    }
    let courier_kind = match kind {
        RelayShelfKind::Nostr => "nostr",
        RelayShelfKind::Ntfy => "ntfy",
        RelayShelfKind::Mainline => "dht",
    };
    courier
        .publish_on_kind(&topic.id(), &hex::encode(dht_key), &hex::encode(entry.to_wire()), entry.ttl_secs, courier_kind)
        .is_ok()
}

/// Post `entry` along `steps` of the plan: the kinds of a step at once, the next step only if none of
/// them took it. Each kind gets the entry with its own lifetime (`entry_ttl_secs` of its block),
/// signed again. Returns the kinds that took it.
fn post_along(
    courier: &dyn RendezvousCourier,
    mainline: Option<&dyn MainlineShelves>,
    plan: &crate::PublishPlanV1,
    steps: &[Vec<String>],
    entry: &RelayDirectoryEntry,
    seed: &[u8; 32],
    now_ms: u64,
) -> Vec<String> {
    let every = |kind: &str| {
        plan.steps.iter().flatten().find(|block| block.kind == kind).map(|block| block.every_secs).unwrap_or(15 * 60)
    };
    for step in steps {
        let mut took = Vec::new();
        for kind in step {
            let Some(shelf) = shelf_kind(kind) else {
                continue;
            };
            let mut copy = entry.clone();
            copy.ttl_secs = entry_ttl_secs(every(kind));
            sign_entry(&mut copy, seed);
            if post_on(courier, mainline, shelf, &copy, now_ms) {
                took.push(kind.clone());
            }
        }
        if !took.is_empty() {
            return took;
        }
    }
    Vec::new()
}

/// One sweep's worth for the operator: post, re-post when a block of the plan is due or the address
/// changed, withdraw once the switch went off. Called from the rendezvous sweep next to the name beacon.
pub(crate) fn sweep_relay_directory(
    storage: &Storage,
    courier: &dyn RendezvousCourier,
    runtime: &MeshRuntime,
    db_path: &str,
) {
    let mainline = LiveMainline { db_path, runtime };
    sweep_relay_directory_with(storage, courier, Some(&mainline), runtime, db_path);
}

fn sweep_relay_directory_with(
    storage: &Storage,
    courier: &dyn RendezvousCourier,
    mainline: Option<&dyn MainlineShelves>,
    runtime: &MeshRuntime,
    db_path: &str,
) {
    let mut role = load_relay_role(db_path);
    if !role.on {
        // No card: this node no longer answers «whoever you are» at its address.
        runtime.set_relay_card(None);
    }
    if !role.on && !role.announced {
        return;
    }
    let Ok((seed, pubkey)) = crate::app_screen_control::local_signing_identity(storage) else {
        return;
    };
    let plan = crate::app_publish_plans::load_publish_plans(db_path).relay;
    let now_ms = current_unix_ms() as u64;
    if !role.on {
        let withdrawal = build_entry(&seed, pubkey, Vec::new(), Vec::new(), "", 0, 30 * 60, now_ms);
        // Everywhere the plan posts: the withdrawal must replace the entry wherever it is.
        let every_kind: Vec<Vec<String>> =
            vec![plan.steps.iter().flatten().map(|block| block.kind.clone()).collect()];
        let took = post_along(courier, mainline, &plan, &every_kind, &withdrawal, &seed, now_ms);
        omni_app_trace_log("relay-directory/withdraw", &format!("took={took:?}"));
        if !took.is_empty() {
            role.announced = false;
            let _ = store_relay_role(db_path, &role);
            posts().remove(db_path);
        }
        return;
    }
    // Between posts the address is not asked again: STUN costs a round of queries, and the entry is
    // still fresh. A changed address shows on the next due post.
    let posted = posts().get(db_path).cloned();
    if posted.as_ref().is_some_and(|posted| plan.due_steps(&posted.taken, now_ms, false).is_empty()) {
        return;
    }
    let (udp, tcp) = crate::public_relay_endpoints(runtime);
    let entry = build_entry(&seed, pubkey, udp, tcp, &role.name, role.since_ms, 30 * 60, now_ms);
    if entry.is_withdrawal() || !entry.endpoints_are_public() {
        // Unreachable from outside: nobody could use the entry.
        omni_app_trace_log("relay-directory/publish", &format!("skip=not_reachable udp={:?} tcp={:?}", entry.udp, entry.tcp));
        return;
    }
    let fingerprint = entry_fingerprint(&entry);
    let mut next = posted.unwrap_or_default();
    let changed = next.fingerprint != fingerprint || next.taken.is_empty();
    let steps = plan.due_steps(&next.taken, now_ms, changed);
    // The card a searcher who found us on Mainline gets over the link: the entry as it goes out now.
    runtime.set_relay_card(Some(entry.to_wire()));
    let took = post_along(courier, mainline, &plan, &steps, &entry, &seed, now_ms);
    omni_app_trace_log(
        "relay-directory/publish",
        &format!("steps={steps:?} took={took:?} udp={:?} tcp={:?}", entry.udp, entry.tcp),
    );
    if !took.is_empty() {
        next.fingerprint = fingerprint;
        for kind in took {
            next.taken.insert(kind, now_ms);
        }
        posts().insert(db_path.to_string(), next);
        if !role.announced {
            role.announced = true;
            let _ = store_relay_role(db_path, &role);
        }
    }
}

/// One relay found in the directory.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PublicRelayV1 {
    /// The mesh node address (hex, 40) — what the list shows next to the name, beginning and end.
    pub node_id: String,
    pub name: String,
    pub udp: Vec<String>,
    pub tcp: Vec<String>,
    /// `free` · `busy` · `full`.
    pub load: String,
    /// Seniority we believe: its claim, capped by how long we have been seeing it.
    pub seniority_secs: u64,
    /// Where it was found: `nostr`, `ntfy`.
    pub found_on: Vec<String>,
    /// Higher first.
    pub score: f64,
}

/// The answer to a search for relays.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelaySearchResultV1 {
    /// `found` · `nobody` (couriers answered, no relay) · `unreachable` (no courier answered).
    pub status: String,
    /// Which shelf kinds answered at all: `nostr`, `ntfy`.
    pub answered: Vec<String>,
    pub relays: Vec<PublicRelayV1>,
}

/// Seniority we believe: the claim, but no more than our first sighting plus one topic window (a
/// relay seen for the first time gets the benefit of one window — it could not have been seen before).
fn believed_seniority_secs(since_ms: u64, first_seen_ms: u64, now_ms: u64) -> u64 {
    let claimed = now_ms.saturating_sub(since_ms) / 1000;
    let seen = now_ms.saturating_sub(first_seen_ms) / 1000 + omni_rendezvous::TOPIC_WINDOW_SECS;
    claimed.min(seen)
}

/// The composite score (§5): seniority, on a log scale (a week counts, a year is not 50× a week), minus
/// load — a full relay goes below every one that is not (a newcomer could not get in anyway). Latency joins when it is measured (stage 3: dialing); local reputation with pinning.
fn relay_score(seniority_secs: u64, load: RelayLoad) -> f64 {
    let hours = seniority_secs as f64 / 3600.0;
    let load_penalty = match load {
        RelayLoad::Free => 0.0,
        RelayLoad::Busy => 5.0,
        RelayLoad::Full => 1_000.0,
    };
    10.0 * (1.0 + hours).ln() - load_penalty
}

fn load_name(load: RelayLoad) -> &'static str {
    match load {
        RelayLoad::Free => "free",
        RelayLoad::Busy => "busy",
        RelayLoad::Full => "full",
    }
}

/// Read every shelf on every web courier, keep what verifies, freshest per relay, rank.
/// At most this many Mainline addresses are dialed in one search (they are bare — each costs a dial).
const MAINLINE_PROBES_MAX: usize = 40;
/// Probes at once.
const MAINLINE_PROBES_PARALLEL: usize = 8;

/// Read the Mainline shelves: the addresses on them, then who sits at each — a card counts only if it
/// is signed by the very key that answered «whoever you are». Returns the cards as hex wire (like the
/// web couriers' blobs) and whether the swarm answered at all.
fn read_mainline_shelves(mainline: &dyn MainlineShelves, now_ms: u64) -> (Vec<String>, bool) {
    let mut reached = false;
    let mut addresses: Vec<std::net::SocketAddr> = Vec::new();
    for (_, dht_key) in relay_read_topics(RelayShelfKind::Mainline, now_ms) {
        if let Some(found) = mainline.peers(&mainline_info_hash(&dht_key)) {
            reached = true;
            for addr in found {
                if !addresses.contains(&addr) {
                    addresses.push(addr);
                }
            }
        }
    }
    addresses.truncate(MAINLINE_PROBES_MAX);
    let mut cards = Vec::new();
    for batch in addresses.chunks(MAINLINE_PROBES_PARALLEL) {
        let found: Vec<Option<([u8; 20], Vec<u8>)>> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch.iter().map(|addr| scope.spawn(move || mainline.probe(*addr))).collect();
            handles.into_iter().map(|handle| handle.join().ok().flatten()).collect()
        });
        for (node, card) in found.into_iter().flatten() {
            let Some(entry) = RelayDirectoryEntry::from_wire(&card) else {
                continue;
            };
            let signed_by_the_answerer = omni_mesh_runtime::node_address_from_ed25519_pubkey(entry.node_pubkey)
                .is_ok_and(|from_card| *from_card.as_bytes() == node);
            if signed_by_the_answerer {
                cards.push(hex::encode(card));
            }
        }
    }
    (cards, reached)
}

pub(crate) fn search_public_relays_via(
    courier: &dyn RendezvousCourier,
    mainline: Option<&dyn MainlineShelves>,
    my_pubkey: Option<[u8; 32]>,
    sightings: &mut HashMap<String, u64>,
    now_ms: u64,
) -> RelaySearchResultV1 {
    // Each kind reads only its own shelves (the others do not keep them, and asking spends quota), all
    // of them in one call: a courier is asked once for every shelf it holds, over one connection —
    // 16 handshakes at once to one Nostr relay drew 429 (stand, 2026-10-14). The two kinds in parallel.
    let kinds = [(RelayShelfKind::Nostr, CourierLegs::Nostr, "nostr"), (RelayShelfKind::Ntfy, CourierLegs::Ntfy, "ntfy")];
    let answers: Vec<(&str, Vec<String>, bool)> = std::thread::scope(|scope| {
        let mut handles: Vec<_> = kinds
            .iter()
            .map(|(kind, legs, label)| {
                scope.spawn(move || {
                    let topics: Vec<String> =
                        relay_read_topics(*kind, now_ms).into_iter().map(|(topic, _)| topic.id()).collect();
                    let (blobs, reached) = courier.fetch_many_with_reach(&topics, *legs).unwrap_or_default();
                    (*label, blobs, reached)
                })
            })
            .collect();
        // Mainline in parallel with the web couriers: the shelves, then the probes.
        if let Some(mainline) = mainline {
            handles.push(scope.spawn(move || {
                let (cards, reached) = read_mainline_shelves(mainline, now_ms);
                ("dht", cards, reached)
            }));
        }
        handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
    });

    let mut answered: Vec<String> = Vec::new();
    // pubkey → (freshest entry, where found).
    let mut freshest: HashMap<[u8; 32], (RelayDirectoryEntry, Vec<String>)> = HashMap::new();
    for (label, blobs, reached) in answers {
        if reached && !answered.iter().any(|known| known == label) {
            answered.push(label.to_string());
        }
        for blob in blobs {
            let Some(entry) = hex::decode(&blob).ok().and_then(|bytes| RelayDirectoryEntry::from_wire(&bytes)) else {
                continue;
            };
            if !entry.is_fresh(now_ms) || !relay_entry_signed(&entry) || Some(entry.node_pubkey) == my_pubkey {
                continue;
            }
            match freshest.get_mut(&entry.node_pubkey) {
                Some((known, found_on)) => {
                    if !found_on.iter().any(|known| known == label) {
                        found_on.push(label.to_string());
                    }
                    if entry.seq > known.seq {
                        *known = entry;
                    }
                }
                None => {
                    freshest.insert(entry.node_pubkey, (entry, vec![label.to_string()]));
                }
            }
        }
    }

    let mut relays: Vec<PublicRelayV1> = Vec::new();
    for (pubkey, (entry, found_on)) in freshest {
        if entry.is_withdrawal() || !entry.endpoints_are_public() {
            continue;
        }
        let Ok(node) = omni_mesh_runtime::node_address_from_ed25519_pubkey(pubkey) else {
            continue;
        };
        let node_id = hex::encode(node.as_bytes());
        let first_seen = *sightings.entry(node_id.clone()).or_insert(now_ms);
        let seniority_secs = believed_seniority_secs(entry.since_ms, first_seen, now_ms);
        relays.push(PublicRelayV1 {
            node_id,
            name: entry.name,
            udp: entry.udp,
            tcp: entry.tcp,
            load: load_name(entry.load).to_string(),
            seniority_secs,
            found_on,
            score: relay_score(seniority_secs, entry.load),
        });
    }
    relays.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.node_id.cmp(&b.node_id)));
    relays.truncate(RELAY_SEARCH_MAX);
    let status = match (relays.is_empty(), answered.is_empty()) {
        (false, _) => "found",
        (true, false) => "nobody",
        (true, true) => "unreachable",
    };
    RelaySearchResultV1 { status: status.to_string(), answered, relays }
}

/// The operator's switch as the shell shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicRelayRoleV1 {
    pub on: bool,
    pub name: String,
}

impl OmniApp {
    pub fn public_relay_role(&self) -> PublicRelayRoleV1 {
        let role = load_relay_role(&self.storage.db_path().to_string_lossy());
        PublicRelayRoleV1 { on: role.on, name: role.name }
    }

    /// Turn "be a relay for everyone" on (with a name, ≤ 32 characters, may be empty) or off. On posts
    /// on the next sweep; off withdraws the entry there.
    pub fn set_public_relay_role(&self, on: bool, name: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_public_relay_role")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mut role = load_relay_role(&db_path);
        if on && !role.on {
            role.since_ms = current_unix_ms() as u64;
        }
        role.on = on;
        role.name = name.trim().chars().take(omni_rendezvous::relay_directory::RELAY_NAME_MAX_CHARS).collect();
        store_relay_role(&db_path, &role)?;
        clear_relay_directory_for_profile(&db_path);
        Ok(())
    }

    /// Look for public relays on the web couriers and the Mainline shelves (only when asked — nothing
    /// in the background).
    pub fn search_public_relays(&self) -> OmniResult<RelaySearchResultV1> {
        let _ = self.ensure_runtime_profile_active("search_public_relays")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let rendezvous = crate::profile_rendezvous_runtime(&db_path)?;
        let my_pubkey = crate::app_screen_control::local_signing_identity(&self.storage).ok().map(|(_, public)| public);
        let mut sightings = load_relay_sightings(&db_path);
        let runtime = self.mesh_runtime();
        let mainline = runtime.as_deref().map(|runtime| LiveMainline { db_path: &db_path, runtime });
        let result = search_public_relays_via(
            rendezvous.helper.as_ref(),
            mainline.as_ref().map(|mainline| mainline as &dyn MainlineShelves),
            my_pubkey,
            &mut sightings,
            current_unix_ms() as u64,
        );
        store_relay_sightings(&db_path, &sightings);
        omni_app_trace_log(
            "relay-directory/search",
            &format!(
                "status={} answered={:?} relays={} found={:?}",
                result.status,
                result.answered,
                result.relays.len(),
                result.relays.iter().map(|relay| format!("{}:{}", &relay.node_id[..8], relay.found_on.join("+"))).collect::<Vec<_>>()
            ),
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PublishPriority;

    /// Topic → posted blobs, and which kind each publish was aimed at.
    #[derive(Default)]
    struct Courier {
        topics: Mutex<HashMap<String, Vec<String>>>,
        kinds: Mutex<Vec<String>>,
    }

    impl RendezvousCourier for Courier {
        fn publish(&self, topic_hex: &str, _: &str, blob_hex: &str, _: u32, _: PublishPriority) -> OmniResult<()> {
            self.topics.lock().unwrap().entry(topic_hex.to_string()).or_default().push(blob_hex.to_string());
            Ok(())
        }
        fn publish_on_kind(&self, topic_hex: &str, key: &str, blob_hex: &str, ttl: u32, kind: &str) -> OmniResult<()> {
            self.kinds.lock().unwrap().push(kind.to_string());
            self.publish(topic_hex, key, blob_hex, ttl, PublishPriority::Background)
        }
        fn fetch(&self, topic_hex: &str, _: &str, _: CourierLegs) -> OmniResult<Vec<String>> {
            Ok(self.topics.lock().unwrap().get(topic_hex).cloned().unwrap_or_default())
        }
    }

    fn key(n: u8) -> ([u8; 32], [u8; 32]) {
        let seed = [n; 32];
        (seed, SigningKey::from_bytes(&seed).verifying_key().to_bytes())
    }

    fn relay(n: u8, since_ms: u64, now_ms: u64, load: RelayLoad) -> RelayDirectoryEntry {
        let (seed, pubkey) = key(n);
        let mut entry = build_entry(
            &seed,
            pubkey,
            vec![format!("8.8.{n}.1:4433")],
            vec![],
            &format!("relay-{n}"),
            since_ms,
            30 * 60,
            now_ms,
        );
        entry.load = load;
        sign_entry(&mut entry, &seed);
        entry
    }

    const NOW: u64 = 1_800_000_000_000;
    const DAY_MS: u64 = 24 * 3600 * 1000;

    /// Mainline shelves in memory: what was announced where, and who answers at which address.
    #[derive(Default)]
    struct Shelves {
        announced: Mutex<HashMap<[u8; 20], Vec<std::net::SocketAddr>>>,
        /// address → (the node that answers «whoever you are», the card it hands over).
        answers: Mutex<HashMap<std::net::SocketAddr, ([u8; 20], Vec<u8>)>>,
    }

    impl MainlineShelves for Shelves {
        fn announce(&self, info_hash: &[u8; 20], port: u16) -> bool {
            let addr = std::net::SocketAddr::from(([8, 8, 9, 9], port));
            self.announced.lock().unwrap().entry(*info_hash).or_default().push(addr);
            true
        }
        fn peers(&self, info_hash: &[u8; 20]) -> Option<Vec<std::net::SocketAddr>> {
            Some(self.announced.lock().unwrap().get(info_hash).cloned().unwrap_or_default())
        }
        fn probe(&self, addr: std::net::SocketAddr) -> Option<([u8; 20], Vec<u8>)> {
            self.answers.lock().unwrap().get(&addr).cloned()
        }
    }

    fn node_of(pubkey: [u8; 32]) -> [u8; 20] {
        *omni_mesh_runtime::node_address_from_ed25519_pubkey(pubkey).unwrap().as_bytes()
    }

    /// Stage 2: a relay found only on Mainline is listed (found on «dht»), its card checked against
    /// the key that answered at the address — a card signed by someone else is dropped.
    #[test]
    fn a_relay_found_on_mainline_is_listed_only_with_a_card_from_the_one_who_answered() {
        let shelves = Shelves::default();
        let honest = relay(3, NOW - DAY_MS, NOW, RelayLoad::Free);
        let (_, honest_key) = key(3);
        let honest_shelf = mainline_info_hash(&relay_publish_topic(RelayShelfKind::Mainline, &honest_key, NOW).1);
        let honest_at: std::net::SocketAddr = "8.8.3.1:4433".parse().unwrap();
        shelves.announced.lock().unwrap().entry(honest_shelf).or_default().push(honest_at);
        shelves.answers.lock().unwrap().insert(honest_at, (node_of(honest_key), honest.to_wire()));

        // Someone at another address hands over relay 4's card, but answered with key 5.
        let stolen = relay(4, NOW - DAY_MS, NOW, RelayLoad::Free);
        let stolen_at: std::net::SocketAddr = "8.8.5.1:4433".parse().unwrap();
        shelves.announced.lock().unwrap().entry(honest_shelf).or_default().push(stolen_at);
        shelves.answers.lock().unwrap().insert(stolen_at, (node_of(key(5).1), stolen.to_wire()));

        let mut sightings = HashMap::new();
        let found = search_public_relays_via(&Courier::default(), Some(&shelves), None, &mut sightings, NOW);
        assert_eq!(found.status, "found");
        assert!(found.answered.iter().any(|kind| kind == "dht"), "{:?}", found.answered);
        assert_eq!(found.relays.len(), 1, "only the honest one: {:?}", found.relays);
        assert_eq!(found.relays[0].node_id, hex::encode(node_of(honest_key)));
        assert_eq!(found.relays[0].found_on, ["dht"]);
    }

    /// The plan's «dht» block announces the relay's UDP port on its Mainline shelf.
    #[test]
    fn the_plans_dht_block_announces_the_port_on_the_relays_shelf() {
        let shelves = Shelves::default();
        let courier = Courier::default();
        let (seed, pubkey) = key(6);
        let entry = relay(6, NOW, NOW, RelayLoad::Free);
        let plan = crate::app_publish_plans::Publication::Relay.auto_plan();
        let steps = vec![vec!["dht".to_string()]];
        assert_eq!(post_along(&courier, Some(&shelves), &plan, &steps, &entry, &seed, NOW), ["dht"]);
        let shelf = mainline_info_hash(&relay_publish_topic(RelayShelfKind::Mainline, &pubkey, NOW).1);
        assert_eq!(shelves.peers(&shelf).unwrap(), ["8.8.9.9:4433".parse::<std::net::SocketAddr>().unwrap()]);
        // No Mainline at hand: the block is not taken, the plan goes on.
        assert!(post_along(&courier, None, &plan, &steps, &entry, &seed, NOW).is_empty());
    }

    #[test]
    fn a_signed_entry_verifies_and_a_moved_one_does_not() {
        let entry = relay(1, NOW, NOW, RelayLoad::Free);
        assert!(relay_entry_signed(&entry));
        let mut moved = entry.clone();
        moved.udp = vec!["8.8.4.4:1".into()];
        assert!(!relay_entry_signed(&moved));
        let mut stolen = entry;
        stolen.node_pubkey = key(2).1;
        assert!(!relay_entry_signed(&stolen));
    }

    #[test]
    fn relays_are_found_ranked_and_forged_withdrawn_stale_and_own_entries_are_left_out() {
        let courier = Courier::default();
        let old = relay(1, NOW - 30 * DAY_MS, NOW, RelayLoad::Free);
        let young = relay(2, NOW - DAY_MS, NOW, RelayLoad::Free);
        let old_but_full = relay(3, NOW - 30 * DAY_MS, NOW, RelayLoad::Full);
        for entry in [&old, &young, &old_but_full] {
            post_on(&courier, None, RelayShelfKind::Nostr, entry, NOW);
        }
        post_on(&courier, None, RelayShelfKind::Ntfy, &old, NOW);
        // Forged: someone else's key on our entry.
        let mut forged = relay(4, NOW, NOW, RelayLoad::Free);
        forged.node_pubkey = key(5).1;
        post_on(&courier, None, RelayShelfKind::Nostr, &forged, NOW);
        // Stale.
        post_on(&courier, None, RelayShelfKind::Nostr, &relay(6, NOW - DAY_MS, NOW - DAY_MS, RelayLoad::Free), NOW);
        // Withdrawn later.
        let mut gone = relay(7, NOW - DAY_MS, NOW - 1000, RelayLoad::Free);
        post_on(&courier, None, RelayShelfKind::Nostr, &gone, NOW);
        gone.udp.clear();
        gone.seq = NOW;
        gone.ts_ms = NOW;
        sign_entry(&mut gone, &key(7).0);
        post_on(&courier, None, RelayShelfKind::Nostr, &gone, NOW);
        // A LAN address.
        let (seed8, pub8) = key(8);
        let lan = build_entry(&seed8, pub8, vec!["192.168.1.2:4433".into()], vec![], "", NOW, 30 * 60, NOW);
        post_on(&courier, None, RelayShelfKind::Nostr, &lan, NOW);
        // Ours.
        post_on(&courier, None, RelayShelfKind::Nostr, &relay(9, NOW, NOW, RelayLoad::Free), NOW);

        // Seen for a month already: the old ones' claims are believed.
        let mut sightings: HashMap<String, u64> = [1u8, 3]
            .iter()
            .map(|n| {
                let node = omni_mesh_runtime::node_address_from_ed25519_pubkey(key(*n).1).unwrap();
                (hex::encode(node.as_bytes()), NOW - 40 * DAY_MS)
            })
            .collect();
        let found = search_public_relays_via(&courier, None, Some(key(9).1), &mut sightings, NOW);
        assert_eq!(found.status, "found");
        let names: Vec<&str> = found.relays.iter().map(|relay| relay.name.as_str()).collect();
        assert_eq!(names, ["relay-1", "relay-2", "relay-3"], "{:?}", found.relays);
        assert_eq!(found.relays[0].found_on, ["nostr", "ntfy"]);
        assert!(sightings.len() >= 3, "the new one is remembered");
    }

    #[test]
    fn a_claimed_seniority_is_believed_only_as_far_as_our_sightings_go() {
        assert_eq!(believed_seniority_secs(NOW - 365 * DAY_MS, NOW, NOW), omni_rendezvous::TOPIC_WINDOW_SECS);
        assert_eq!(believed_seniority_secs(NOW - DAY_MS, NOW - 40 * DAY_MS, NOW), DAY_MS / 1000);
        assert!(relay_score(30 * 86_400, RelayLoad::Free) > relay_score(86_400, RelayLoad::Free));
        assert!(relay_score(30 * 86_400, RelayLoad::Full) < relay_score(86_400, RelayLoad::Free));
    }

    #[test]
    fn an_entry_lives_twice_its_period_within_bounds() {
        assert_eq!(entry_ttl_secs(15 * 60), 30 * 60);
        assert_eq!(entry_ttl_secs(5 * 60), 30 * 60, "at least half an hour");
        assert_eq!(entry_ttl_secs(12 * 3600), 13 * 3600, "at most a window and an hour");
    }

    #[test]
    fn a_step_goes_on_only_if_the_one_before_took_nothing() {
        let courier = Courier::default();
        let (seed, pubkey) = key(3);
        let entry = build_entry(&seed, pubkey, vec!["8.8.3.1:4433".into()], vec![], "r", NOW, 30 * 60, NOW);
        let plan = crate::Publication::Relay.auto_plan();
        let steps = plan.due_steps(&HashMap::new(), NOW, true);
        assert_eq!(post_along(&courier, None, &plan, &steps, &entry, &seed, NOW), ["nostr"], "ntfy is the fallback");
        assert_eq!(courier.kinds.lock().unwrap().as_slice(), ["nostr"]);
    }

    #[test]
    fn nothing_found_is_told_apart_from_nobody_answering() {
        struct Silent;
        impl RendezvousCourier for Silent {
            fn publish(&self, _: &str, _: &str, _: &str, _: u32, _: PublishPriority) -> OmniResult<()> {
                Ok(())
            }
            fn fetch(&self, _: &str, _: &str, _: CourierLegs) -> OmniResult<Vec<String>> {
                Ok(Vec::new())
            }
            fn fetch_with_reach(&self, _: &str, _: &str, _: CourierLegs) -> OmniResult<(Vec<String>, bool)> {
                Ok((Vec::new(), false))
            }
        }
        let mut sightings = HashMap::new();
        assert_eq!(search_public_relays_via(&Silent, None, None, &mut sightings, NOW).status, "unreachable");
        assert_eq!(search_public_relays_via(&Courier::default(), None, None, &mut sightings, NOW).status, "nobody");
    }
}
