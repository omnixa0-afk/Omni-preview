// DHT rendezvous switch. The BT-DHT (BEP44) courier is a THIRD rendezvous class next to the
// web couriers (ntfy/Nostr): serverless (no IP/domain to block), high-collateral, no quota. Unlike
// them it exposes this node's IP to the global DHT swarm. It was off by default until 2026-10-10;
// the maintainers turned it ON by default (docs/decisions/courier-metadata-and-quota.md, step 5): meeting
// with no operator and no quota, and still working when the web couriers are blocked, outweighs the
// swarm seeing the IP. The user can still turn it off — see
// docs/decisions/serverless-connectivity-masterplan.md ("the user must be able to turn it off").
//
// The switch is the Mainline row of the courier set (`dht://mainline`): whether it is there and not
// turned off decides whether `spawn_rendezvous_runtime` starts the per-profile `omni-dht-helper`
// process at all: off → no helper, no swarm exposure, the web couriers still cover rendezvous; on →
// the helper spawns (if its binary ships next to the executable) and the BT-DHT leg joins the
// N-of-many fan-out. One switch for the courier card and this leg: it used to be a sidecar file
// beside a courier row that also said "Mainline", and the two could disagree.

/// The Mainline courier as it stands in the courier set.
pub(crate) const MAINLINE_COURIER_URL: &str = "dht://mainline";

/// Where the switch lived before 2026-10-10.
fn legacy_dht_rendezvous_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "dht-rendezvous.json")
}

/// A profile that had turned Mainline off with the old sidecar switch keeps it off: applied to the
/// Mainline row once, then the file is gone.
pub(crate) fn adopt_legacy_dht_switch(storage: &Storage) {
    let path = legacy_dht_rendezvous_sidecar_path(&storage.db_path().to_string_lossy());
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return;
    };
    if !matches!(raw.trim().trim_matches('"'), "on" | "true") {
        if let Some(row) = storage
            .list_rendezvous_providers()
            .unwrap_or_default()
            .into_iter()
            .find(|row| row.url == MAINLINE_COURIER_URL)
        {
            let _ = storage.set_rendezvous_provider_mode(row.id, omni_storage::RENDEZVOUS_PROVIDER_MODE_OFF);
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Whether the profile's courier set has Mainline in it (on by default since 2026-10-10, see the file
/// header). Read live by `spawn_rendezvous_runtime`; a change restarts the runtime.
pub(crate) fn load_dht_rendezvous_enabled(db_path: &str) -> bool {
    couriers_include_dht(&resolve_rendezvous_providers_for_db(db_path))
}

impl OmniApp {
    /// Whether this profile uses the BT-DHT courier (Mainline).
    pub fn dht_rendezvous_enabled(&self) -> bool {
        couriers_include_dht(&self.configured_rendezvous_providers())
    }

    /// Turn the Mainline courier on (post and read) or off — its row in the courier set. Applies at once.
    pub fn set_dht_rendezvous_enabled(&self, enabled: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_dht_rendezvous_enabled")?;
        let row = self
            .storage
            .list_rendezvous_providers()?
            .into_iter()
            .find(|row| row.url == MAINLINE_COURIER_URL);
        match (row, enabled) {
            (Some(row), true) => {
                self.set_rendezvous_provider_mode(row.id, omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ)
            }
            (Some(row), false) => self.set_rendezvous_provider_mode(row.id, omni_storage::RENDEZVOUS_PROVIDER_MODE_OFF),
            (None, true) => self.add_rendezvous_provider(MAINLINE_COURIER_URL),
            (None, false) => Ok(()),
        }
    }
}

// --- Entry-node (server) role: be a reachable in-segment DHT bootstrap node for OTHERS -------------
//
// A plain participant JOINS the swarm (client). An ENTRY node additionally SERVES: it runs the engine
// in server_mode on a FIXED UDP port and self-maps that port via UPnP so other nodes can bootstrap
// FROM it — the server side of an isolated-segment DHT (someone has to run the custom entry points).
// Off by default (it makes this node inbound-reachable + advertises presence). Persisted per-profile;
// read live at helper spawn. Distinct from `dht-rendezvous` (join) and from CLI-only genesis (start a
// brand-new segment). See docs/decisions/dht-bootstrap-mesh-propagation.md.

/// The fixed UDP port an entry node binds + asks UPnP to map. A known port is required (an ephemeral one
/// isn't a stable bootstrap target). BitTorrent's conventional DHT port.
pub(crate) const DHT_ENTRY_NODE_PORT: u16 = 6881;

fn dht_entry_node_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "dht-entry-node.json")
}

/// Read the per-profile DHT entry-node opt-in. Default `false`. Read live by `spawn_rendezvous_runtime`.
pub(crate) fn load_dht_entry_node_enabled(db_path: &str) -> bool {
    match std::fs::read_to_string(dht_entry_node_sidecar_path(db_path)) {
        Ok(raw) => matches!(raw.trim().trim_matches('"'), "on" | "true"),
        Err(_) => false,
    }
}

impl OmniApp {
    /// Whether this profile runs its DHT node as a reachable in-segment ENTRY node (default off).
    pub fn dht_entry_node_enabled(&self) -> bool {
        load_dht_entry_node_enabled(&self.storage.db_path().to_string_lossy())
    }

    /// Opt this profile in/out of the DHT entry-node role. Persisted per-profile; applies when the
    /// rendezvous runtime is next (re)created. Entry-node needs DHT-rendezvous on to have a running
    /// engine at all — the shell should keep them consistent, but this only writes the flag.
    pub fn set_dht_entry_node_enabled(&self, enabled: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_dht_entry_node_enabled")?;
        let path = dht_entry_node_sidecar_path(&self.storage.db_path().to_string_lossy());
        std::fs::write(&path, if enabled { "\"on\"" } else { "\"off\"" }).map_err(|error| {
            OmniError::Storage(format!("failed to persist dht entry-node flag: {error}"))
        })?;
        Ok(())
    }
}

/// Live status of this profile's BT-DHT rendezvous leg, for the settings UI. Serialized straight to the
/// FFI JSON payload.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DhtRendezvousStatus {
    /// The BT-DHT rendezvous toggle (persisted opt-in).
    pub enabled: bool,
    /// The entry-node toggle (persisted opt-in).
    pub entry_node_enabled: bool,
    /// The per-profile `omni-dht-helper` process is up (rendezvous runtime running with the DHT leg).
    pub running: bool,
    /// Size of the helper's live Mainline routing table — 0 while joining / not yet bootstrapped. The
    /// honest "connected to the swarm" signal.
    pub node_count: usize,
    /// The UPnP-mapped public `ip:port` when running as a reachable entry node; `None` otherwise / CGNAT.
    pub entry_endpoint: Option<String>,
    /// How long the helper has been up with an EMPTY routing table — a network that drops DHT traffic
    /// (live 2026-10-12: neither the routers nor any cached node answered, QUIC to the PC fine) looks
    /// like "joining" forever unless the UI is told how long it has been. 0 while it sees nodes.
    #[serde(default)]
    pub empty_for_ms: i64,
    /// While our own Mainline is dead: which way the last DHT op went — `"friend"` (a friend's engine,
    /// docs/decisions/dht-over-peer.md), `"pkarr relays"` (HTTPS, dht-over-pkarr-relays.md) or `"none"`
    /// (neither answered). `None` while our own engine works or no op ran lately.
    pub way_in: Option<&'static str>,
    /// A linked peer advertises the DHT-proxy role (a friend whose engine we may go through when our
    /// own Mainline is dead). Says whether «через друга» is open at all, before any op tried it.
    pub friend_proxy: bool,
}

/// db path → the way the last DHT op went while our own Mainline was dead, and when.
static DHT_WAY_IN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (&'static str, i64)>>> =
    std::sync::OnceLock::new();

/// A way told longer ago than this is not shown: the sweep asks every few minutes, so an older one
/// is stale (the leg was not used since — our own engine came back, or nothing to look up).
const DHT_WAY_IN_FRESH_MS: i64 = 15 * 60_000;

/// The DHT leg went this way (`"none"` — no way answered).
pub(crate) fn note_dht_way_in(db_path: &str, way: &'static str, now_unix_ms: i64) {
    if let Ok(mut ways) = DHT_WAY_IN.get_or_init(Default::default).lock() {
        if let Some((before, _)) = ways.insert(db_path.to_string(), (way, now_unix_ms)) {
            if before != way {
                omni_app_trace_log("rendezvous/dht-leg", &format!("way in: {before} -> {way}"));
            }
        } else {
            omni_app_trace_log("rendezvous/dht-leg", &format!("way in: {way}"));
        }
    }
}

/// The way to show: only while our own Mainline is dead, and only a fresh one.
pub(crate) fn dht_way_in(db_path: &str, now_unix_ms: i64) -> Option<&'static str> {
    if !dht_locally_blocked(db_path) {
        return None;
    }
    let ways = DHT_WAY_IN.get()?.lock().ok()?;
    let (way, at) = ways.get(db_path).copied()?;
    (now_unix_ms - at <= DHT_WAY_IN_FRESH_MS).then_some(way)
}

/// db path → since when its helper has seen no Mainline node (absent = it sees some, or not up).
static DHT_EMPTY_SINCE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, i64>>> =
    std::sync::OnceLock::new();

/// Note what the helper's routing table holds now; how long it has been empty (0 = not empty).
pub(crate) fn note_dht_node_count(db_path: &str, node_count: usize, now_unix_ms: i64) -> i64 {
    let Ok(mut since) = DHT_EMPTY_SINCE.get_or_init(Default::default).lock() else {
        return 0;
    };
    if node_count > 0 {
        since.remove(db_path);
        return 0;
    }
    now_unix_ms - *since.entry(db_path.to_string()).or_insert(now_unix_ms)
}

/// The profile uses Mainline and its helper has seen no node for half a minute: the network drops DHT
/// traffic, so DHT ops go over a peer whose Mainline works (docs/decisions/dht-over-peer.md). Where
/// DHT passes, the routers answer in well under a second and the table fills within seconds; two
/// minutes (until 2026-10-12) kept a blocked laptop waiting 3.5 min for the way in (owner: «2 минуты
/// слишком много»). A slow-but-open network wrongly called blocked only adds the friend's way for a
/// while — our own engine is still asked, and the verdict lifts on its first node.
pub(crate) const DHT_BLOCKED_AFTER_MS: i64 = 30_000;

pub(crate) fn dht_locally_blocked(db_path: &str) -> bool {
    load_dht_rendezvous_enabled(db_path)
        && DHT_EMPTY_SINCE
            .get()
            .and_then(|since| since.lock().ok().and_then(|since| since.get(db_path).copied()))
            .is_some_and(|since| crate::current_unix_ms() - since >= DHT_BLOCKED_AFTER_MS)
}

/// Forget the profile's empty-table clock and the way its DHT ops went (its rendezvous runtime goes
/// or is respawned).
pub(crate) fn clear_dht_empty_since(db_path: &str) {
    if let Some(since) = DHT_EMPTY_SINCE.get() {
        if let Ok(mut since) = since.lock() {
            since.remove(db_path);
        }
    }
    if let Some(ways) = DHT_WAY_IN.get() {
        if let Ok(mut ways) = ways.lock() {
            ways.remove(db_path);
        }
    }
}

impl OmniApp {
    /// Live status of the BT-DHT rendezvous leg for the settings UI: whether the helper is up and how
    /// many Mainline nodes it currently sees, plus the entry-node public address. Cheap — one
    /// short-timeout `nodes` line to the helper over loopback; the registry lock is held only long enough
    /// to clone the endpoint out, never across the network read.
    pub fn dht_rendezvous_status(&self) -> DhtRendezvousStatus {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let enabled = load_dht_rendezvous_enabled(&db_path);
        let entry_node_enabled = load_dht_entry_node_enabled(&db_path);

        let (endpoint, entry_endpoint) = match rendezvous_runtime_registry().lock() {
            Ok(registry) => match registry.get(&db_path).and_then(|rt| rt.dht_helper.as_ref()) {
                Some(helper) => (
                    Some(helper.endpoint().to_string()),
                    helper.public_endpoint().map(str::to_string),
                ),
                None => (None, None),
            },
            Err(_) => (None, None),
        };

        match endpoint {
            Some(endpoint) => {
                let node_count = dht_helper_nodes_count(&endpoint).unwrap_or(0);
                DhtRendezvousStatus {
                    enabled,
                    entry_node_enabled,
                    running: true,
                    node_count,
                    entry_endpoint,
                    empty_for_ms: note_dht_node_count(&db_path, node_count, crate::current_unix_ms()),
                    way_in: dht_way_in(&db_path, crate::current_unix_ms()),
                    friend_proxy: self.mesh_runtime().is_some_and(|runtime| !runtime.dht_proxy_candidates().is_empty()),
                }
            }
            None => DhtRendezvousStatus {
                enabled,
                entry_node_enabled,
                running: false,
                node_count: 0,
                entry_endpoint: None,
                empty_for_ms: 0,
                way_in: None,
                friend_proxy: false,
            },
        }
    }

    /// Apply the DHT-rendezvous / entry-node toggles IMMEDIATELY: drop this profile's rendezvous runtime
    /// and re-create it so the `omni-dht-helper` (re)spawns NOW with the freshly-persisted flags, instead
    /// of waiting for the next dial / profile restart. The flags are already persisted by the setters, so
    /// this only changes WHEN they take effect. Spawns a child process (reads its startup preamble), so
    /// callers must run it OFF the UI thread.
    pub fn respawn_rendezvous_runtime(&self) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        clear_rendezvous_runtime_for_profile(&db_path);
        self.ensure_rendezvous_runtime().map(|_| ())
    }
}

// --- Custom DHT bootstrap nodes (isolated-segment support) ---------------------------------------
//
// The Mainline engine (omni-dht-helper) defaults to the GLOBAL BitTorrent routers, which are
// unreachable inside an isolated network segment — so a fresh node can't join the swarm
// and DHT rendezvous is dead there. These per-profile custom bootstrap nodes are CUSTOM DHT
// entry points: Kademlia doesn't care whether the swarm is global or local, it only needs one
// reachable node to join. Persisted in a sidecar list (no schema migration); read live at helper
// spawn and handed over as `OMNI_DHT_BOOTSTRAP`. Empty = the global routers.

fn dht_bootstrap_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "dht-bootstrap.json")
}

pub(crate) fn load_dht_bootstrap_nodes(db_path: &str) -> Vec<String> {
    match std::fs::read_to_string(dht_bootstrap_sidecar_path(db_path)) {
        Ok(raw) => serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_dht_bootstrap_nodes(db_path: &str, nodes: &[String]) -> OmniResult<()> {
    let json = serde_json::to_string(nodes)
        .map_err(|e| OmniError::Internal(format!("dht bootstrap encode: {e}")))?;
    std::fs::write(dht_bootstrap_sidecar_path(db_path), json)
        .map_err(|e| OmniError::Storage(format!("failed to persist dht bootstrap nodes: {e}")))
}

/// Upper bound on the stored bootstrap list when merging nodes LEARNED automatically over the mesh
/// (step B), so a churny/malicious peer can't grow the sidecar without bound. Manual/invite adds are
/// user-driven and not subject to this. See docs/decisions/dht-bootstrap-mesh-propagation.md.
const DHT_BOOT_LEARNED_STORE_CAP: usize = 64;

/// Merge DHT bootstrap candidates LEARNED from a linked peer over the mesh (step B) into this profile's
/// configured list: dedup against what we already have, bounded by `DHT_BOOT_LEARNED_STORE_CAP`, persist
/// once. Candidates are already-validated `SocketAddr`s; a lying peer can only add junk that never dials
/// (fail-closed). Returns how many new nodes were added. See docs/decisions/dht-bootstrap-mesh-propagation.md.
pub(crate) fn merge_learned_dht_bootstrap_nodes(
    db_path: &str,
    learned: &[std::net::SocketAddr],
) -> usize {
    if learned.is_empty() {
        return 0;
    }
    let mut nodes = load_dht_bootstrap_nodes(db_path);
    let mut added = 0usize;
    for addr in learned {
        if nodes.len() >= DHT_BOOT_LEARNED_STORE_CAP {
            break;
        }
        let candidate = addr.to_string();
        if !nodes.iter().any(|existing| existing == &candidate) {
            nodes.push(candidate);
            added += 1;
        }
    }
    if added > 0 {
        let _ = save_dht_bootstrap_nodes(db_path, &nodes);
    }
    added
}

// ── Live-node cache (segment-reboot survival) ───────────────────────────────────────────────────
// An auto-harvested cache of DHT nodes this profile actually saw online (the engine's routing table,
// dumped via the helper's `nodes` command). On restart the engine re-bootstraps from it, re-joining the
// segment swarm without the well-known global routers — the key to a isolated-network DHT surviving a
// reboot. LOCAL only, never shared/gossiped (R1b untouched) — kept DISTINCT from the curated, shareable
// `dht-bootstrap.json`. See docs/decisions/dht-bootstrap-mesh-propagation.md.

/// Cap on the persisted live-node cache so a churny routing table can't grow the file without bound.
const DHT_NODES_CACHE_CAP: usize = 200;

/// How often to re-harvest the engine's routing table into the cache (throttle; the table changes
/// slowly). The rendezvous sweep calls the harvest, this bounds the write rate.
pub(crate) const DHT_NODES_HARVEST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) fn dht_nodes_cache_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "dht-nodes-cache.json")
}

pub(crate) fn load_dht_nodes_cache(db_path: &str) -> Vec<String> {
    match std::fs::read_to_string(dht_nodes_cache_sidecar_path(db_path)) {
        Ok(raw) => serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Replace the persisted live-node cache with the current `harvested` routing table (validated `ip:port`,
/// deduped, capped). Returns how many nodes were stored. An EMPTY harvest never writes — so a transient
/// empty table (bootstrap not yet done / failed this session) can't wipe a still-good cache. The current
/// routing table is the authoritative live view, so replace (not merge) keeps the cache fresh.
pub(crate) fn save_dht_nodes_cache(db_path: &str, harvested: &[String]) -> usize {
    let mut nodes: Vec<String> = Vec::new();
    for raw in harvested {
        if nodes.len() >= DHT_NODES_CACHE_CAP {
            break;
        }
        let addr = raw.trim();
        if addr.parse::<std::net::SocketAddr>().is_err() {
            continue;
        }
        if !nodes.iter().any(|existing| existing == addr) {
            nodes.push(addr.to_string());
        }
    }
    if nodes.is_empty() {
        return 0;
    }
    if let Ok(json) = serde_json::to_string(&nodes) {
        let _ = std::fs::write(dht_nodes_cache_sidecar_path(db_path), json);
    }
    nodes.len()
}

/// КУРИРУЕМЫЕ точки входа (manual/invite/mesh/bundle, `dht-bootstrap.json`), объединённые с кэшем
/// ТОЛЬКО когда курируемые заданы. Этот набор ЗАМЕНЯЕТ глобальные роутеры — то есть выражает
/// осознанный выбор «работаем в изолированном сегменте», где глобальные всё равно недостижимы.
///
/// Кэш попадает сюда лишь в сегментном случае, потому что там он собран из узлов сегмента и им же и
/// является. В обычном случае (курируемых нет) кэш идёт ОТДЕЛЬНО — см. `dht_startup_extra_nodes`.
pub(crate) fn dht_startup_bootstrap_set(db_path: &str) -> Vec<String> {
    let mut set = load_dht_bootstrap_nodes(db_path);
    if set.is_empty() {
        return set;
    }
    for node in load_dht_nodes_cache(db_path) {
        if !set.iter().any(|existing| existing == &node) {
            set.push(node);
        }
    }
    set
}

/// ДОПОЛНИТЕЛЬНЫЕ точки входа поверх набора по умолчанию — авто-собранный кэш живых узлов, когда
/// курируемых сидов нет.
///
/// Почему не в общий набор: любой непустой набор ЗАМЕНЯЕТ глобальные роутеры. Кэш собран нами и
/// протухает — узлы роя ротируются быстро. Узел, вернувшийся через неделю, получил бы список
/// мёртвых адресов ВМЕСТО работающих роутеров и не вошёл бы в рой вообще, не попробовав запасной
/// путь. Механизм выживания сегмента не должен превращаться в механизм отрезания — тем более что до
/// 2026-08-13 кэш вообще не писался, и эта ветка ни у кого не исполнялась
/// (docs/bugs/dht-segment-housekeeping-never-ran.md).
pub(crate) fn dht_startup_extra_nodes(db_path: &str) -> Vec<String> {
    if !load_dht_bootstrap_nodes(db_path).is_empty() {
        return Vec::new();
    }
    load_dht_nodes_cache(db_path)
}

/// Prefix for a shareable DHT bootstrap bundle (`om3dhtboot:` + base64url(JSON)). An UNSIGNED, versioned
/// list of in-segment DHT bootstrap `ip:port` nodes that anyone can publish out-of-band (website, forum,
/// QR, word of mouth) and anyone can import — the **contact-independent** seed-distribution channel for a
/// country-scale isolated segment (step A). Unsigned is safe: a bad entry can only fail to dial, and a
/// forged DHT node cannot forge an AEAD-sealed+signed rendezvous beacon (fail-closed). DHT node addresses
/// are public-in-the-swarm ("public infra, share freely"), not a pair-scoped secret — R1b applies to
/// scarce relays, not abundant DHT nodes. See docs/decisions/dht-bootstrap-mesh-propagation.md.
pub(crate) const DHT_BOOTSTRAP_BUNDLE_PREFIX: &str = "om3dhtboot:";

#[derive(serde::Serialize, serde::Deserialize)]
struct DhtBootstrapBundleV1 {
    version: u16,
    nodes: Vec<String>,
    exported_at_unix_ms: i64,
}

fn encode_dht_bootstrap_bundle(bundle: &DhtBootstrapBundleV1) -> OmniResult<String> {
    use base64::Engine as _;
    let json = serde_json::to_vec(bundle)
        .map_err(|e| OmniError::Internal(format!("dht bootstrap bundle encode failed: {e}")))?;
    Ok(format!(
        "{DHT_BOOTSTRAP_BUNDLE_PREFIX}{}",
        base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(json)
    ))
}

fn parse_dht_bootstrap_bundle(token: &str) -> OmniResult<DhtBootstrapBundleV1> {
    use base64::Engine as _;
    let payload = token
        .trim()
        .strip_prefix(DHT_BOOTSTRAP_BUNDLE_PREFIX)
        .ok_or_else(|| {
            OmniError::InvalidInput(
                "not a DHT bootstrap bundle (missing om3dhtboot: prefix)".to_string(),
            )
        })?
        .trim();
    let decoded = base64::prelude::BASE64_URL_SAFE_NO_PAD.decode(payload).map_err(|e| {
        OmniError::InvalidInput(format!("DHT bootstrap bundle is not valid base64: {e}"))
    })?;
    let parsed: DhtBootstrapBundleV1 = serde_json::from_slice(&decoded)
        .map_err(|e| OmniError::InvalidInput(format!("DHT bootstrap bundle is malformed: {e}")))?;
    if parsed.version != 1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported DHT bootstrap bundle version {}",
            parsed.version
        )));
    }
    Ok(parsed)
}

impl OmniApp {
    /// This profile's custom DHT bootstrap nodes (custom entry points for an isolated segment).
    pub fn dht_bootstrap_nodes(&self) -> Vec<String> {
        load_dht_bootstrap_nodes(&self.storage.db_path().to_string_lossy())
    }

    /// Export this profile's configured DHT bootstrap nodes as a shareable `om3dhtboot:` bundle (step A)
    /// — publish it out-of-band so anyone (not only contacts) can import the in-segment entry points.
    /// Errors when the list is empty (nothing to share). See docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub fn export_dht_bootstrap_bundle(&self) -> OmniResult<String> {
        let nodes = load_dht_bootstrap_nodes(&self.storage.db_path().to_string_lossy());
        if nodes.is_empty() {
            return Err(OmniError::InvalidInput(
                "no DHT bootstrap nodes to export".to_string(),
            ));
        }
        encode_dht_bootstrap_bundle(&DhtBootstrapBundleV1 {
            version: 1,
            nodes,
            exported_at_unix_ms: current_unix_ms(),
        })
    }

    /// Import a shareable `om3dhtboot:` bundle (step A): validate each `ip:port`, dedup against the
    /// current list, persist once. Returns how many NEW nodes were added. Contact-independent — the
    /// out-of-band seed channel for an isolated segment. Errors on a malformed token or when the bundle
    /// carries no valid node. See docs/decisions/dht-bootstrap-mesh-propagation.md.
    pub fn import_dht_bootstrap_bundle(&self, token: &str) -> OmniResult<usize> {
        let _ = self.ensure_runtime_profile_active("import_dht_bootstrap_bundle")?;
        let bundle = parse_dht_bootstrap_bundle(token)?;
        let db = self.storage.db_path().to_string_lossy().to_string();
        let mut nodes = load_dht_bootstrap_nodes(&db);
        let mut added = 0usize;
        let mut any_valid = false;
        for raw in &bundle.nodes {
            let addr = raw.trim();
            if addr.parse::<std::net::SocketAddr>().is_err() {
                continue;
            }
            any_valid = true;
            if !nodes.iter().any(|existing| existing == addr) {
                nodes.push(addr.to_string());
                added += 1;
            }
        }
        if !any_valid {
            return Err(OmniError::InvalidInput(
                "bundle contains no valid ip:port DHT bootstrap nodes".to_string(),
            ));
        }
        if added > 0 {
            save_dht_bootstrap_nodes(&db, &nodes)?;
        }
        Ok(added)
    }

    /// Add an custom DHT bootstrap node (`ip:port`). Validated + deduped; applies on the next DHT
    /// helper spawn (profile start / rendezvous sweep).
    pub fn add_dht_bootstrap_node(&self, addr: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("add_dht_bootstrap_node")?;
        let addr = addr.trim();
        addr.parse::<std::net::SocketAddr>().map_err(|_| {
            OmniError::InvalidInput(format!("'{addr}' is not a valid ip:port DHT bootstrap node"))
        })?;
        let db = self.storage.db_path().to_string_lossy().to_string();
        let mut nodes = load_dht_bootstrap_nodes(&db);
        if nodes.iter().any(|n| n == addr) {
            return Err(OmniError::InvalidInput(format!(
                "DHT bootstrap node '{addr}' is already in the list"
            )));
        }
        nodes.push(addr.to_string());
        save_dht_bootstrap_nodes(&db, &nodes)
    }

    /// Remove a custom DHT bootstrap node. No-op if it isn't present.
    pub fn remove_dht_bootstrap_node(&self, addr: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("remove_dht_bootstrap_node")?;
        let addr = addr.trim();
        let db = self.storage.db_path().to_string_lossy().to_string();
        let mut nodes = load_dht_bootstrap_nodes(&db);
        nodes.retain(|n| n != addr);
        save_dht_bootstrap_nodes(&db, &nodes)
    }
}

#[cfg(test)]
mod dht_rendezvous_tests {
    use super::*;

    /// Mainline is on by default (its row is in the seeded set); an old "off" switch is applied to
    /// the row once and the file goes; turning the row off is what turns the leg off.
    #[test]
    fn mainline_is_on_by_default_and_an_old_off_switch_is_kept() {
        let dir = std::env::temp_dir().join(format!("omni-dht-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("omni.db");
        let db_str = db.to_string_lossy().to_string();
        let storage = Storage::open(&db).unwrap();
        storage.migrate().unwrap();
        assert!(load_dht_rendezvous_enabled(&db_str), "seeded: on");

        std::fs::write(legacy_dht_rendezvous_sidecar_path(&db_str), "\"on\"").unwrap();
        assert!(load_dht_rendezvous_enabled(&db_str));
        assert!(!legacy_dht_rendezvous_sidecar_path(&db_str).exists(), "the old switch is gone");

        std::fs::write(legacy_dht_rendezvous_sidecar_path(&db_str), "\"off\"").unwrap();
        assert!(!load_dht_rendezvous_enabled(&db_str), "an old off stays off");
        let mainline = storage
            .list_rendezvous_providers()
            .unwrap()
            .into_iter()
            .find(|row| row.url == MAINLINE_COURIER_URL)
            .unwrap();
        assert_eq!(mainline.mode, omni_storage::RENDEZVOUS_PROVIDER_MODE_OFF);

        storage
            .set_rendezvous_provider_mode(mainline.id, omni_storage::RENDEZVOUS_PROVIDER_MODE_READ)
            .unwrap();
        assert!(load_dht_rendezvous_enabled(&db_str), "read-only still runs the DHT helper");
        drop(storage);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_learned_dedups_and_caps() {
        let dir = std::env::temp_dir().join(format!("omni-dhtboot-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("omni.db");
        let db_str = db.to_string_lossy().to_string();
        let _ = std::fs::remove_file(dht_bootstrap_sidecar_path(&db_str));

        // Empty learned set → no-op.
        assert_eq!(merge_learned_dht_bootstrap_nodes(&db_str, &[]), 0);

        // Two learned nodes are added.
        let a: std::net::SocketAddr = "203.0.113.5:6881".parse().unwrap();
        let b: std::net::SocketAddr = "203.0.113.6:6881".parse().unwrap();
        assert_eq!(merge_learned_dht_bootstrap_nodes(&db_str, &[a, b]), 2);
        assert_eq!(load_dht_bootstrap_nodes(&db_str).len(), 2);

        // Re-merging the same two + one new → only the new one is added (dedup).
        let c: std::net::SocketAddr = "203.0.113.7:6881".parse().unwrap();
        assert_eq!(merge_learned_dht_bootstrap_nodes(&db_str, &[a, b, c]), 1);
        assert_eq!(load_dht_bootstrap_nodes(&db_str).len(), 3);

        // Automatic growth is bounded by DHT_BOOT_LEARNED_STORE_CAP.
        let many: Vec<std::net::SocketAddr> = (0..(DHT_BOOT_LEARNED_STORE_CAP as u16 + 20))
            .map(|i| std::net::SocketAddr::from(([10, 0, 0, 1], 7000 + i)))
            .collect();
        merge_learned_dht_bootstrap_nodes(&db_str, &many);
        assert!(load_dht_bootstrap_nodes(&db_str).len() <= DHT_BOOT_LEARNED_STORE_CAP);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundle_token_round_trips_and_rejects_junk() {
        // `om3dhtboot:` bundle (step A): contact-independent out-of-band seed distribution.
        let bundle = DhtBootstrapBundleV1 {
            version: 1,
            nodes: vec!["203.0.113.9:6881".to_string(), "198.51.100.2:6881".to_string()],
            exported_at_unix_ms: 1_700_000_000_000,
        };
        let token = encode_dht_bootstrap_bundle(&bundle).unwrap();
        assert!(token.starts_with(DHT_BOOTSTRAP_BUNDLE_PREFIX));
        assert_eq!(parse_dht_bootstrap_bundle(&token).unwrap().nodes, bundle.nodes);

        // Missing prefix and bad base64 are rejected, not mis-parsed.
        assert!(parse_dht_bootstrap_bundle("203.0.113.9:6881").is_err());
        assert!(parse_dht_bootstrap_bundle("om3dhtboot:!!!not base64!!!").is_err());
    }

    #[test]
    fn node_cache_saves_dedups_caps_and_unions() {
        let dir = std::env::temp_dir().join(format!("omni-dhtcache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("omni.db");
        let db_str = db.to_string_lossy().to_string();
        let _ = std::fs::remove_file(dht_nodes_cache_sidecar_path(&db_str));
        let _ = std::fs::remove_file(dht_bootstrap_sidecar_path(&db_str));

        // An empty harvest writes nothing — a transient empty table can't wipe a good cache.
        assert_eq!(save_dht_nodes_cache(&db_str, &[]), 0);
        assert!(load_dht_nodes_cache(&db_str).is_empty());

        // Valid nodes persist; junk dropped; dedup within the set.
        let saved = save_dht_nodes_cache(
            &db_str,
            &[
                "203.0.113.1:6881".to_string(),
                "not-an-addr".to_string(),
                "203.0.113.1:6881".to_string(),
                "203.0.113.2:6881".to_string(),
            ],
        );
        assert_eq!(saved, 2);
        assert_eq!(load_dht_nodes_cache(&db_str).len(), 2);

        // Replace semantics: a fresh harvest overwrites the cache with the current table.
        save_dht_nodes_cache(&db_str, &["203.0.113.9:6881".to_string()]);
        assert_eq!(load_dht_nodes_cache(&db_str), vec!["203.0.113.9:6881".to_string()]);

        // The cap bounds the stored set.
        let many: Vec<String> = (0..(DHT_NODES_CACHE_CAP as u16 + 50))
            .map(|i| format!("10.0.0.{}:6881", i % 250))
            .collect();
        save_dht_nodes_cache(&db_str, &many);
        assert!(load_dht_nodes_cache(&db_str).len() <= DHT_NODES_CACHE_CAP);

        // Startup set = curated seeds ∪ cache, deduped.
        save_dht_bootstrap_nodes(
            &db_str,
            &["198.51.100.5:6881".to_string(), "203.0.113.9:6881".to_string()],
        )
        .unwrap();
        save_dht_nodes_cache(
            &db_str,
            &["203.0.113.9:6881".to_string(), "192.0.2.7:6881".to_string()],
        );
        let startup = dht_startup_bootstrap_set(&db_str);
        assert!(startup.contains(&"198.51.100.5:6881".to_string()), "curated seed present");
        assert!(startup.contains(&"192.0.2.7:6881".to_string()), "cached node present");
        assert_eq!(
            startup.iter().filter(|n| *n == "203.0.113.9:6881").count(),
            1,
            "node in both seeds and cache appears once"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_without_curated_seeds_supplements_the_routers_instead_of_replacing_them() {
        // Любой непустой bootstrap-набор ЗАМЕНЯЕТ глобальные роутеры. Кэш собран нами и протухает
        // (узлы роя ротируются), поэтому подменять им роутеры нельзя: узел, вернувшийся через
        // неделю, получил бы список мёртвых адресов вместо рабочих точек входа и не вошёл бы в рой
        // вообще. Механизм выживания сегмента не должен становиться механизмом отрезания.
        //
        // Дискриминирующий: до разделения кэш попадал в замещающий набор, и этот тест бы падал.
        let dir = std::env::temp_dir().join(format!("omni-dhtsplit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("omni.db");
        let db_str = db.to_string_lossy().to_string();
        let _ = std::fs::remove_file(dht_nodes_cache_sidecar_path(&db_str));
        let _ = std::fs::remove_file(dht_bootstrap_sidecar_path(&db_str));

        save_dht_nodes_cache(&db_str, &["203.0.113.9:6881".to_string()]);

        assert!(
            dht_startup_bootstrap_set(&db_str).is_empty(),
            "без курируемых сидов замещающий набор обязан остаться пустым — иначе глобальные роутеры отключены"
        );
        assert_eq!(
            dht_startup_extra_nodes(&db_str),
            vec!["203.0.113.9:6881".to_string()],
            "кэш обязан прийти как ДОПОЛНЕНИЕ"
        );

        // А вот с курируемыми сидами (осознанный изолированный сегмент) кэш идёт вместе с ними в
        // замещающий набор: там он собран из узлов сегмента и им же и является.
        save_dht_bootstrap_nodes(&db_str, &["198.51.100.5:6881".to_string()]).unwrap();
        let startup = dht_startup_bootstrap_set(&db_str);
        assert!(startup.contains(&"198.51.100.5:6881".to_string()));
        assert!(startup.contains(&"203.0.113.9:6881".to_string()));
        assert!(
            dht_startup_extra_nodes(&db_str).is_empty(),
            "в сегментном случае кэш не дублируется в дополнение"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
