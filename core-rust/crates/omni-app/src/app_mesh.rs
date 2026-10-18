// Mesh overlay integration (Option III — mesh carries media). Orchestrates the
// omni-mesh-runtime sync facade from omni-app: starts the mesh with the
// account identity key, maps dialog_key ↔ peer NodeAddress, dials a contact,
// and routes realtime-media datagrams over the verified mesh Link. Inbound
// mesh media goes straight to the call's media engine (RealtimeMediaInboundSinks).
//
// See docs/decisions/mesh-transport-integration-seam.md. The legacy
// TransportManager is untouched; the send path falls through to it whenever no
// mesh Link exists for the dialog.

use omni_mesh_runtime::{
    detect_global_ipv6, detect_primary_lan_ipv4, node_address_from_ed25519_pubkey,
    MeshBulkStreamCallback, MeshCallbacks, MeshConfig, MeshDhtBootstrapCallback,
    MeshDhtProxyCallback, MeshDhtProxyRequest,
    MeshDhtProxyResult,
    MeshHelloMaker, MeshInterfaceKind, MeshLinkQuality, MeshMediaCallback,
    MeshNodeAddress, MeshPeerDownCallback, MeshReachabilityState, MeshRuntime, MeshWireCallback,
};
// `HashMap` is already in crate-root scope via another include!'d module.

/// How long `mesh_connect_active_call` keeps retrying the dial while waiting
/// for the peer to be discovered via multicast announce. The mesh starts at
/// sign-in so discovery has usually completed by call time; this only absorbs
/// the race when a call begins moments after sign-in.
const MESH_DIAL_DISCOVERY_TIMEOUT_SECS: u64 = 5;

/// Pause between dial retries inside the discovery-wait window.
const MESH_DIAL_RETRY_INTERVAL_MS: u64 = 200;

/// How often the mesh auto-connect loop re-tries dialing approved contacts that
/// aren't yet mesh-linked. Discovery is async, so it keeps trying until the
/// peer announces and the Link comes up — the self-organizing behaviour that
/// lets a call/chat ride the mesh without an om3 token.
const MESH_AUTOCONNECT_INTERVAL_SECS: u64 = 3;
/// How often the frames the mesh holds (our own undelivered group/1:1 durable messages and those we
/// carry for others) are snapshotted to disk. Before 2026-10-03 this happened only on a clean
/// `stop_mesh`, so a crash or a killed process lost every undelivered message.
const DURABLE_PERSIST_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Serverless-rendezvous presence-refresh / cross-network reach cadence in the autoconnect
/// worker. Far longer than `MESH_AUTOCONNECT_INTERVAL_SECS` because each sweep publishes
/// beacons + does STUN/provider round-trips against FREE third-party services — but well
/// within the 600 s beacon TTL, so a contact on another network is reached (and our
/// presence kept fresh) without hammering the providers. See
/// docs/decisions/serverless-bootstrap-rendezvous.md.
const RENDEZVOUS_SWEEP_INTERVAL_SECS: u64 = 90;

/// R7 metadata hygiene — publish jitter. The sweep cadence is randomized within
/// `[INTERVAL, INTERVAL + JITTER]` so a rendezvous-channel observer can't fingerprint a node by a
/// regular, clock-aligned 90 s publish tick. (The beacon TTL is 300 s, so even the longest jittered
/// gap keeps the presence beacon valid between publishes.) See the rendezvous design notes R7.
const RENDEZVOUS_SWEEP_JITTER_SECS: u64 = 30;

/// One jittered sweep interval (seconds): `INTERVAL + rand(0..=JITTER)`. Best-effort crypto randomness;
/// a failure just yields the unjittered floor (still correct, only less unfingerprintable that tick).
fn jittered_rendezvous_sweep_interval_secs() -> u64 {
    let jitter = omni_crypto::random_bytes(1)
        .ok()
        .and_then(|b| b.first().copied())
        .map(|b| u64::from(b) % (RENDEZVOUS_SWEEP_JITTER_SECS + 1))
        .unwrap_or(0);
    RENDEZVOUS_SWEEP_INTERVAL_SECS + jitter
}

/// How often the autoconnect worker refreshes the warm-rejoin cache (G3): it upserts the live DIRECT
/// endpoints into `mesh_peer_book` so the NEXT start re-dials known peers immediately. Independent of
/// privacy mode (these are direct LAN/public endpoints — no third party). ~30 s keeps the entries'
/// age fresh without churning the db on the 3 s autoconnect cadence. A NEW endpoint does not wait for
/// it — see `peer_book_save_due`.
const PEER_BOOK_SNAPSHOT_INTERVAL_SECS: u64 = 30;

/// Save the warm-rejoin cache now? At once when a live direct endpoint isn't on disk yet, else on the
/// refresh interval. It used to be the interval alone, the first save a full 30 s after start: live
/// 2026-10-03 a run that linked at 24 s and was restarted at 30 s never saved its peer's address, and
/// the restart redialed only ports that had died (24 s back through rendezvous instead of ~1 s).
/// The ADR asked for "snapshot on link-up" from the start (omni-network-model.md D7). Pure + tested.
fn peer_book_save_due(
    live: &std::collections::HashSet<(MeshNodeAddress, String)>,
    saved: &std::collections::HashSet<(MeshNodeAddress, String)>,
    since_last_save: std::time::Duration,
) -> bool {
    live.iter().any(|endpoint| !saved.contains(endpoint))
        || since_last_save.as_secs() >= PEER_BOOK_SNAPSHOT_INTERVAL_SECS
}

/// Grace before the FIRST durable rendezvous sweep after the worker starts — short, so a contact on
/// another network is reached within seconds of opening the app (not a full 90 s interval), while
/// still giving same-network LAN announce a moment to link first (no needless provider traffic).
/// Tests stay off the network via the loopback gate, not this timing. See connection-privacy-modes.md.
const RENDEZVOUS_FIRST_SWEEP_GRACE_SECS: u64 = 15;

/// Prefix for a mesh pairing invite token (`om3mesh:` + base64url(JSON)).
/// Distinct from the legacy `om3`/compact signaling token so the two never
/// collide and the mesh path stays decoupled from legacy signaling.
const MESH_PAIRING_INVITE_PREFIX: &str = "om3mesh:";

/// How long a generated mesh pairing invite stays valid (7 days). Identity
/// doesn't rotate often, but bounding it limits replay of a leaked invite.
const MESH_PAIRING_INVITE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How long the FIRST-contact bootstrap rendezvous keeps auto-publishing/dialing for an invite
/// (2 h). Shorter than the 7-day invite validity ON PURPOSE: the auto-bootstrap covers the common
/// "I sent it, open it now" coordinated first contact, while bounding free-provider traffic. Past
/// this window the invite still pairs over the same LAN or a manual reach; the open-it-days-later
/// cross-network case wants persisted, long-lived republishing (TODO 2026-09, see invite-bootstrap-rendezvous.md).
const BOOTSTRAP_WINDOW_MS: i64 = 2 * 60 * 60 * 1000;

/// Retry cadence for an outstanding first-contact bootstrap rendezvous in the autoconnect worker.
/// Tighter than the durable contact sweep (`RENDEZVOUS_SWEEP_INTERVAL_SECS`) — first contact is an
/// active, user-is-waiting flow — but still gentle, and it only runs while ≥1 invite is outstanding
/// (normally 0). An immediate attempt also fires on generate/import, so this is just the retry.
const BOOTSTRAP_SWEEP_INTERVAL_SECS: u64 = 30;

/// One outstanding first-contact bootstrap rendezvous (registry keyed by invite nonce hex). `secret`
/// is the blinded-topic/beacon key both sides derive from the nonce
/// (`omni_crypto::derive_rendezvous_bootstrap_secret`). `inviter_node` is set on the IMPORTER side
/// (it learns the inviter's node from the invite pubkey) so the entry is pruned once that link is up;
/// it's None on the INVITER side (it doesn't yet know who will connect, so it self-expires).
/// `expires_at_ms` is the bootstrap window cap. See docs/decisions/invite-bootstrap-rendezvous.md.
#[derive(Clone)]
pub(crate) struct MeshBootstrapInvite {
    pub(crate) secret: [u8; 32],
    pub(crate) inviter_node: Option<MeshNodeAddress>,
    pub(crate) expires_at_ms: i64,
}

/// The single privacy axis for HOW a node connects — orthogonal to E2E encryption (always on) and
/// to the speed/reliability `omni_types::CarrierPolicy`. Three points on a "how much do I expose /
/// lean on infra" line:
/// - `Auto` (default) — full ladder: serverless rendezvous (a blinded, AEAD-sealed beacon via a
///   public provider — opaque blobs under an opaque topic, never content or identities) + relay +
///   opportunistic direct P2P punch.
/// - `RelayOnly` — rendezvous + relay are fine, but NEVER punch direct: the data path stays
///   relay-mediated so a contact never learns your IP. Hides your IP from the peer while still
///   using infra. (This is the connection-quality-ladder "axis B", unified into this one privacy
///   knob rather than a separate `DirectPolicy` — avoids two contradictory direct-vs-relay settings.)
/// - `DirectOnly` — ZERO third parties: LAN announce + router port-mapping (UPnP/NAT-PMP/IPv6) +
///   manual invite/endpoint exchange only — privacy-max for a user who won't touch any external
///   service, at the cost of needing a reachable side or a manual re-exchange on a stale address.
///
/// Per-profile, persisted in a sidecar (no migration). See docs/decisions/connection-privacy-modes.md
/// + docs/decisions/connection-quality-ladder.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPrivacyMode {
    Auto,
    RelayOnly,
    DirectOnly,
}

impl ConnectionPrivacyMode {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::RelayOnly => "relay-only",
            Self::DirectOnly => "direct-only",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value.trim() {
            "auto" => Ok(Self::Auto),
            "relay-only" | "relay" => Ok(Self::RelayOnly),
            "direct-only" | "direct" => Ok(Self::DirectOnly),
            other => Err(OmniError::InvalidInput(format!(
                "unknown connection privacy mode: {other}"
            ))),
        }
    }

    /// Whether this mode permits third-party (serverless) rendezvous — both the durable contact
    /// sweep and the first-contact bootstrap. `DirectOnly` answers false: no provider is touched.
    /// `RelayOnly` answers true (it leans on rendezvous + relay; it only forbids the *direct* punch).
    pub fn uses_rendezvous(self) -> bool {
        matches!(self, Self::Auto | Self::RelayOnly)
    }

    /// Whether the data path may climb to a direct P2P punch (which reveals your IP to a verified
    /// contact). `RelayOnly` answers false — stay relay-mediated to hide your IP; `Auto` and
    /// `DirectOnly` allow direct. Enforced end-to-end (tcp-direct follow-up d): the app forwards this
    /// to the runtime via `MeshRuntime::set_allow_direct_punch` (on mesh-start and on change), which
    /// gates BOTH the UDP hole-punch (`Network::punch_upgrade_peer`, the chokepoint for the periodic +
    /// event-driven upgrade) AND the rendezvous direct-dial — `rendezvous_connect` reads the live gate
    /// to skip the dial AND strip my reflexive/TCP-direct endpoints from the published beacon. GUI-
    /// selectable in Settings → "Сеть и обнаружение". См. connection-quality-ladder.md.
    pub fn allows_direct_punch(self) -> bool {
        !matches!(self, Self::RelayOnly)
    }
}

// Honest connection-state surface (the DTOs below are what the shell renders so the UI can say
// WHAT is happening and WHY a connection failed, instead of a binary/prose status). Pure read-model
// over already-existing signals — no wire/relay change. The Rust core derives it (Rust = truth);
// the shell only renders. See docs/decisions/omni-network-model.md (honest connection-state).
//
// Flat over the FFI: the shell deserializes plain records with string `kind`s + optional fields
// (no JSON polymorphism — the rest of the shell uses plain source-gen records too) and maps the
// strings to display. The core keeps exhaustiveness via the `*Kind` enums.

/// This node's own connectivity (about US, not a peer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeConnectivityKind {
    /// Mesh not started for this profile.
    NotStarted,
    /// Mesh up but no LAN/UPnP/IPv6/reachability yet — effectively unreachable & alone.
    Isolated,
    /// Mesh up, reachability verdict still Unknown (probing) — coming online.
    Connecting,
    /// Mesh up, AutoNAT says Unreachable and no public endpoint — online but only via mesh/relay.
    OnlineBehindNat,
    /// Directly dialable: UPnP-mapped and/or global-IPv6 public endpoint(s) present.
    OnlineReachable,
}

/// Transport reachability of a specific contact (about THEM). Strictly the link — trust
/// (`verified`) is a separate, orthogonal field (discovery ≠ identity ≠ relationship).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactLinkKind {
    /// A verified single-hop link is up.
    OnlineDirect,
    /// Reached via a relay (see `relay_hops`); R2 will let this carry media too.
    OnlineViaRelay,
    /// No link yet, but we are (or will be) actively trying to reach them.
    Searching,
    /// Доставлю позже: прямо сейчас пир нас не слышит, но durable store-and-forward держит конверт.
    /// Среднее значение троичного вердикта из docs/decisions/reachability-belongs-to-the-session.md.
    /// Выводится с 2026-08-13 из доказательства (наши пробы не отвечены дольше порога), а не из
    /// наличия pending-очереди — durable по проекту принимает всё, что не ушло сейчас.
    WillDeliverLater,
    /// No link and nothing in flight can help right now — with an honest `cannot_reach_reason`.
    CannotReach,
}

/// The transport carrier of a contact's current best link — the honest "how" for the UI. Present
/// only while linked (`OnlineDirect` / `OnlineViaRelay`); `None` when not linked. See
/// docs/decisions/connection-quality-ladder.md (rungs/carriers) + tcp-relay-fallback.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkCarrier {
    /// QUIC over UDP — the native, fastest path.
    Quic,
    /// QUIC tunnelled inside TLS-over-TCP (looks like HTTPS) — the fallback used when UDP is
    /// blocked/throttled (e.g. a mobile carrier). Honest to surface: "direct, but over the slower TCP fallback".
    TcpFallback,
    /// LAN multicast (same local network).
    Lan,
    /// Соединение QUIC МИГРИРОВАЛО на прямую TCP-дверь пира (Meta-B mux): интерфейс по-прежнему
    /// `QuicOverUdp`, а байты идут по TCP. Путь остаётся ПРЯМЫМ — дверь не посредник.
    TcpDoor,
}

impl LinkCarrier {
    /// Несущая ЛУЧШЕГО пути к пиру — с поправкой на дверь.
    ///
    /// Одного `peer_best_carrier` мало: он отдаёт интерфейс (`QuicOverUdp`) и тогда, когда mux под
    /// ним уехал на TCP-дверь пира — то есть один и тот же ответ на «идём по UDP» и «идём по TCP».
    /// Это уже было замечено и исправлено в `mesh_link_quality` (`direct:door` вместо лгущего
    /// `direct:udp`), но исправлено ТОЛЬКО там: состояние контакта — то, что читает пользователь в
    /// шапке чата, — про дверь не знало и подписывалось «· QUIC».
    ///
    /// Теперь правило одно и живёт здесь; оба вывода зовут его.
    fn observe(runtime: &MeshRuntime, node: &MeshNodeAddress) -> Option<Self> {
        let kind = runtime.peer_best_carrier(node)?;
        // Про дверь спрашиваем ТОЛЬКО у UDP-несущей: у остальных этот вопрос не имеет смысла, а
        // лишний вызов брал бы замок карты путей ещё раз.
        let indirect = match kind {
            MeshInterfaceKind::QuicOverUdp => runtime.peer_on_door(node),
            _ => false,
        };
        Some(Self::classify(kind, indirect))
    }

    /// Само правило — отдельно от опроса рантайма, чтобы его можно было проверить.
    /// `indirect` — путь несущей идёт через её посредника (TCP-дверь у родного QUIC).
    pub(crate) fn classify(kind: MeshInterfaceKind, indirect: bool) -> Self {
        match kind {
            MeshInterfaceKind::QuicOverUdp if indirect => LinkCarrier::TcpDoor,
            MeshInterfaceKind::QuicOverUdp => LinkCarrier::Quic,
            MeshInterfaceKind::QuicOverTcp => LinkCarrier::TcpFallback,
            MeshInterfaceKind::Mdns => LinkCarrier::Lan,
        }
    }

    /// Короткое имя для строки `mesh_link_quality` (`direct:udp` / `direct:door` / …). Это
    /// сложившийся контракт daemon/CLI и живых прогонов, поэтому имена НЕ совпадают с именами
    /// сериализации DTO и меняться не должны.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            LinkCarrier::Quic => "udp",
            LinkCarrier::TcpFallback => "tcp",
            LinkCarrier::Lan => "mdns",
            LinkCarrier::TcpDoor => "door",
        }
    }
}

/// Why a contact currently cannot be reached. Some variants are defined for the shell + forward-
/// compat but not yet derivable from the core (marked) — they land with R2 / warm-rejoin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CannotReachReason {
    /// Approved, resolvable, rendezvous-eligible peer that simply isn't online/announcing.
    PeerOffline,
    /// Обе стороны недостижимы (симметричный NAT/CGNAT) и релея нет. Выводится с Phase C
    /// (`direct_is_impossible_for_pair` + `any_relay_reachable`) — прежняя пометка «NOT derived yet, lands with
    /// R2» была устаревшей и заставляла считать баннер невозможным, пока пользователь его видел.
    NoRouteBothUnreachable,
    /// Direct-only privacy and no LAN/UPnP/IPv6 path — we won't touch a provider.
    RendezvousDisabled,
    /// A cached endpoint failed and re-discovery is pending. NOT derived yet — lands with warm-rejoin.
    AddressStale,
    /// Not an approved contact — a relationship gate, not a network failure.
    NotApproved,
    /// The identity challenge mismatched (possible impersonation). NOT derived here yet.
    VerificationFailed,
}

/// This node's connectivity + any directly-dialable public endpoints (empty unless reachable).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeConnectivityState {
    pub kind: NodeConnectivityKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
}

/// Per-contact connection state (flat): the link kind + its detail (hops / reason) + the
/// orthogonal `verified` (has_met) flag.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContactConnectionState {
    pub account_id: String,
    pub link: ContactLinkKind,
    /// Present only when `link == OnlineViaRelay`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_hops: Option<u8>,
    /// The transport carrier of the current best link (QUIC vs TCP-fallback vs LAN) — present only
    /// while linked. Lets the UI say "direct · QUIC" vs "direct · TCP fallback" vs "via N hops · …"
    /// instead of a bare "online". `None` when not linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<LinkCarrier>,
    /// Present only when `link == CannotReach`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cannot_reach_reason: Option<CannotReachReason>,
    /// `has_met`: a verified secure session has happened (TM-012). Orthogonal to the link —
    /// a link can be up while still unverified, and vice-versa.
    pub verified: bool,
    /// Tier-2 live trace: the recent connection-attempt steps (LAN → rendezvous → linked) for this
    /// contact — the «как происходит подключение / где запнулось» journey. Empty when nothing has
    /// been attempted yet (e.g. not approved, or mesh just started).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trace: Vec<ConnectionStep>,
    /// A device of this contact told us its couriers, and we share none with it one way or the
    /// other: through couriers we cannot meet it, only over a link that is already up.
    pub no_common_courier: bool,
    /// Couriers of ours, turned off (or only read), that would give us one — what the card suggests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub couriers_to_enable: Vec<String>,
}

/// The aggregate honest connection-state DTO: our node + per-contact links. One round-trip; the
/// shell renders, never derives.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MeshConnectionState {
    pub node: NodeConnectivityState,
    pub contacts: Vec<ContactConnectionState>,
}

/// The per-profile connection-privacy sidecar (a bare db-value string next to the db). Absent ⇒
/// `Auto` (the default — automatic connectivity, third-party rendezvous allowed). Keyed by the db
/// FILE name (not just its directory) so two profiles whose db files share a directory get distinct
/// sidecars — mirroring `transport_identity_sidecar_path`. The doc said "per-profile" but the old
/// fixed name was really per-directory (the stage-5 test peers share one dir and leaked into each
/// other). `load_connection_privacy_mode` still reads the old fixed name as a fallback so an
/// existing choice survives the rename.
fn connection_privacy_mode_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "connection-privacy-mode.json")
}

/// The pre-rename fixed-name sidecar; read-only fallback so an existing setting isn't lost.
fn legacy_connection_privacy_mode_sidecar_path(db_path: &str) -> std::path::PathBuf {
    std::path::Path::new(db_path).with_file_name("connection-privacy-mode.json")
}

fn load_connection_privacy_mode(db_path: &str) -> ConnectionPrivacyMode {
    let read = std::fs::read_to_string(connection_privacy_mode_sidecar_path(db_path))
        .or_else(|_| std::fs::read_to_string(legacy_connection_privacy_mode_sidecar_path(db_path)));
    match read {
        Ok(raw) => ConnectionPrivacyMode::from_db_value(raw.trim()).unwrap_or(ConnectionPrivacyMode::Auto),
        Err(_) => ConnectionPrivacyMode::Auto,
    }
}

/// The per-profile carrier-policy sidecar (R1a connection-quality ladder, axis A). Absent ⇒ `Auto`
/// (reliability-first TCP-relay base, then opportunistic climb to faster carriers). Keyed by the db
/// FILE name like `connection_privacy_mode_sidecar_path`, so co-located profiles get distinct
/// sidecars. See docs/decisions/connection-quality-ladder.md.
/// TODO(R1a): the per-file `*.connection-privacy-mode.json` and this `*.carrier-policy.json` sidecars
/// are NOT in `omni-storage::local_data_file_targets` (which lists only fixed-name siblings), so they
/// leak a tiny JSON on profile delete — pre-existing for the privacy sidecar; fix both together.
fn carrier_policy_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "carrier-policy.json")
}

fn load_carrier_policy(db_path: &str) -> omni_types::CarrierPolicy {
    match std::fs::read_to_string(carrier_policy_sidecar_path(db_path)) {
        Ok(raw) => {
            omni_types::CarrierPolicy::from_db_value(raw.trim()).unwrap_or(omni_types::CarrierPolicy::Auto)
        }
        Err(_) => omni_types::CarrierPolicy::Auto,
    }
}

/// Sidecar for the fixed direct-TCP front-door port (relay-less reliable inbound). Mirrors the
/// `carrier_policy` per-profile-file pattern — no storage migration, co-located profiles
/// stay distinct. Stored as the plain decimal port; absent/blank/0 → ephemeral (rely on UPnP).
fn direct_tcp_port_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "direct-tcp-port.json")
}

/// A port the mesh bound on its previous run, remembered per profile so the next run asks for the
/// same one: an address a peer remembered stays alive across a restart. One file per socket:
/// `mesh-quic-port.json` (`MeshConfig::preferred_quic_port`), `mesh-door-port.json`
/// (`preferred_direct_tcp_port`).
pub(crate) const QUIC_PORT_FILE: &str = "mesh-quic-port.json";
pub(crate) const DOOR_PORT_FILE: &str = "mesh-door-port.json";

pub(crate) fn load_remembered_port(db_path: &str, file: &str) -> Option<u16> {
    std::fs::read_to_string(crate::profile_sidecar_path(db_path, file))
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|port| *port != 0)
}

/// Remember the port this run actually bound. Best-effort: losing it only costs the next start a
/// fresh port, which is how every start worked before.
pub(crate) fn save_remembered_port(db_path: &str, file: &str, port: u16) {
    if let Err(error) = std::fs::write(crate::profile_sidecar_path(db_path, file), port.to_string()) {
        omni_app_trace_log("mesh/bind", &format!("could not remember port {port} ({file}): {error}"));
    }
}

fn load_direct_tcp_port(db_path: &str) -> Option<u16> {
    match std::fs::read_to_string(direct_tcp_port_sidecar_path(db_path)) {
        Ok(raw) => raw.trim().parse::<u16>().ok().filter(|p| *p != 0),
        Err(_) => None,
    }
}

/// Sidecar path for MY relay-bundle version (R1c inc.3b) — a monotone counter bumped whenever I change
/// my `shared` relay set, so a rotation publishes a STRICTLY-newer bundle (a friend ignores an
/// equal/older one via `max_bundle_version`). A per-profile scalar beside the db, mirroring the
/// carrier-policy sidecar. Kept off the DB so the `shared` flag stays a pure boolean orthogonal to the
/// `bundle_id`/`bundle_version` columns (which tag IMPORTED friend bundles). See
/// docs/decisions/r1c-rotatable-white-relay.md.
fn relay_bundle_version_sidecar_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "relay-bundle-version")
}

/// My current relay-bundle version (0 if never set / unreadable).
fn load_relay_bundle_version(db_path: &str) -> u32 {
    std::fs::read_to_string(relay_bundle_version_sidecar_path(db_path))
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// When my current bundle version was issued: when the version file was last written (0 — never).
/// My bundle rides every beacon, and a beacon whose bytes change is published again at once, past its
/// plan's period: an issue time of «now» made every beacon of every pair go out on all couriers each
/// sweep, ~every 2 min, and ate ntfy's quota within the hour (stand 2026-10-14).
fn load_relay_bundle_issued_at(db_path: &str) -> i64 {
    std::fs::metadata(relay_bundle_version_sidecar_path(db_path))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64)
}

/// Prefix for a "код связи" reach token (`om3reach:` + base64url(JSON)). A copy-pasteable bundle of an
/// ALREADY-paired contact's current reachable endpoints, for the manual / Direct-only path — when
/// rendezvous is off (or hasn't connected) two friends exchange these to dial each other directly.
/// Distinct from the first-contact `om3mesh:` invite (which carries identity, not endpoints).
const MESH_REACH_TOKEN_PREFIX: &str = "om3reach:";

/// A manual reach bundle ("код связи"): my mesh node + the endpoints a peer can dial me at right now.
/// EPHEMERAL — valid only until my address changes (router reboot / network move); re-share then. This
/// is the no-third-party reach: the user copies it and sends it over any channel they trust. See
/// docs/decisions/connection-privacy-modes.md.
#[derive(serde::Serialize, serde::Deserialize)]
struct MeshReachTokenV1 {
    version: u16,
    node_hex: String,
    endpoints: Vec<String>,
    exported_at_unix_ms: i64,
}

fn encode_reach_token(token: &MeshReachTokenV1) -> OmniResult<String> {
    use base64::Engine as _;
    let json = serde_json::to_vec(token)
        .map_err(|error| OmniError::Internal(format!("reach token encode failed: {error}")))?;
    Ok(format!(
        "{MESH_REACH_TOKEN_PREFIX}{}",
        base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(json)
    ))
}

fn parse_reach_token(token: &str) -> OmniResult<MeshReachTokenV1> {
    use base64::Engine as _;
    let payload = token
        .trim()
        .strip_prefix(MESH_REACH_TOKEN_PREFIX)
        .ok_or_else(|| {
            OmniError::InvalidInput("not a reach token (missing om3reach: prefix)".to_string())
        })?
        .trim();
    let decoded = base64::prelude::BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| OmniError::InvalidInput(format!("reach token is not valid base64: {error}")))?;
    let parsed: MeshReachTokenV1 = serde_json::from_slice(&decoded)
        .map_err(|error| OmniError::InvalidInput(format!("reach token is malformed: {error}")))?;
    if parsed.version != 1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported reach token version {}",
            parsed.version
        )));
    }
    Ok(parsed)
}

/// Prefix for an `om3relay:` relay-bundle token — a SIGNED, versioned SET of relays that an identity
/// publishes to its approved friends (R1c rotatable white-relay). base64url of the binary RelayBundleV1 (version 2).
/// Unlike `om3reach:` (ephemeral, UNSIGNED, per-pair endpoints), the bundle is signed by the issuer's
/// account identity so a friend can verify it really came from that contact — which is what lets it
/// ride the sealed per-pair beacon and auto-propagate on rotation (R1c phase 3). See
/// docs/decisions/r1c-rotatable-white-relay.md.
const MESH_RELAY_BUNDLE_PREFIX: &str = "om3relay:";

/// One relay inside a [`RelayBundleV1`] — the publishable subset of a `network_nodes` row. `endpoint`
/// is the UDP/QUIC hint (rung 1); `tcp_endpoint` the TLS-over-TCP bridge (rung 0); `cert_spki_pin_hex`
/// the optional outer-TLS SPKI pin (R2 fronting). At least one of `endpoint`/`tcp_endpoint` is present.
/// `node` is the relay's mesh node address: a receiver dials only relays whose node it knows (the
/// dial challenge checks it), so a relay without one is a hint nobody dials (version 3, 2026-10-14 —
/// before it the bundle carried none and a friend's relays were never dialed).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RelayEntryV1 {
    node: Option<[u8; 20]>,
    endpoint: Option<String>,
    tcp_endpoint: Option<String>,
    label: String,
    cert_spki_pin_hex: Option<String>,
}

/// A signed, versioned relay set (`om3relay:`). `bundle_version` is the rotation counter (monotone per
/// issuer); a strictly-greater version REPLACES the issuer's prior set. `issuer_signing_pubkey_hex` is
/// the issuer's ed25519 identity SIGNING public key — it both verifies `sig` AND derives the
/// `bundle_id` (= the issuer's mesh node), so a friend ties the bundle to the contact they already
/// trust. `sig` is base64url(64-byte ed25519) over [`relay_bundle_signing_bytes`] (the binary form
/// without the signature).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RelayBundleV1 {
    version: u16,
    bundle_version: u32,
    issuer_signing_pubkey_hex: String,
    relays: Vec<RelayEntryV1>,
    issued_at_unix_ms: i64,
    sig: String,
}

/// The publishable relay fields a caller hands [`OmniApp::generate_relay_bundle`]. Owned (not borrowed)
/// so FFI/GUI can build it from String fields without lifetime gymnastics. At least one of
/// `endpoint`/`tcp_endpoint` must be set; both, when present, must be `ip:port`.
#[derive(Debug, Clone, Default)]
pub struct RelayBundleEntryInput {
    /// The relay's mesh node address, when known — without it a receiver never dials the relay.
    pub node_address: Option<[u8; 20]>,
    pub endpoint: Option<String>,
    pub tcp_endpoint: Option<String>,
    pub label: String,
    pub cert_spki_pin_hex: Option<String>,
}

/// The relay bundle's wire version: binary (2026-10-10) — keys and signature as bytes, endpoints as
/// `ip:port` bytes — so a bundle with a couple of relays fits a beacon that fits a BT-DHT record
/// (docs/decisions/libp2p-kad-vs-mainline.md, «Что не влезает»). JSON before; not read any more.
/// Version 3 (2026-10-14) adds each relay's node address.
const RELAY_BUNDLE_VERSION: u16 = 3;

/// Domain of the bytes a relay bundle is signed over.
const RELAY_BUNDLE_SIGNING_DOMAIN: &[u8] = b"omni-relay-bundle-v3\0";

const RELAY_HAS_ENDPOINT: u8 = 1;
const RELAY_HAS_TCP_ENDPOINT: u8 = 2;
const RELAY_HAS_SPKI_PIN: u8 = 4;
const RELAY_HAS_NODE: u8 = 8;

/// The bundle as bytes: version, rotation counter, issuer key, issue time, the relays — and, with
/// `with_sig`, the signature. Without it, what the signature covers.
fn relay_bundle_bytes(bundle: &RelayBundleV1, with_sig: bool) -> OmniResult<Vec<u8>> {
    use base64::Engine as _;
    let invalid = |what: &str| OmniError::InvalidInput(format!("relay bundle {what}"));
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(&bundle.version.to_be_bytes());
    out.extend_from_slice(&bundle.bundle_version.to_be_bytes());
    let issuer: [u8; 32] = hex::decode(bundle.issuer_signing_pubkey_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid("issuer key is not 32 bytes of hex"))?;
    out.extend_from_slice(&issuer);
    out.extend_from_slice(&bundle.issued_at_unix_ms.to_be_bytes());
    out.push(u8::try_from(bundle.relays.len()).map_err(|_| invalid("has more than 255 relays"))?);
    for relay in &bundle.relays {
        let mut flags = 0;
        if relay.endpoint.is_some() {
            flags |= RELAY_HAS_ENDPOINT;
        }
        if relay.tcp_endpoint.is_some() {
            flags |= RELAY_HAS_TCP_ENDPOINT;
        }
        if relay.cert_spki_pin_hex.is_some() {
            flags |= RELAY_HAS_SPKI_PIN;
        }
        if relay.node.is_some() {
            flags |= RELAY_HAS_NODE;
        }
        out.push(flags);
        if let Some(node) = &relay.node {
            out.extend_from_slice(node);
        }
        for endpoint in [&relay.endpoint, &relay.tcp_endpoint].into_iter().flatten() {
            omni_rendezvous::wire::put_endpoint(&mut out, endpoint).ok_or_else(|| invalid("endpoint is too long"))?;
        }
        omni_rendezvous::wire::put_short_text(&mut out, &relay.label).ok_or_else(|| invalid("label is over 255 bytes"))?;
        if let Some(pin) = &relay.cert_spki_pin_hex {
            let pin: [u8; 32] = hex::decode(pin.trim())
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| invalid("SPKI pin is not 32 bytes of hex"))?;
            out.extend_from_slice(&pin);
        }
    }
    if with_sig {
        let sig: [u8; 64] = base64::prelude::BASE64_URL_SAFE_NO_PAD
            .decode(bundle.sig.trim())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| invalid("signature is not 64 bytes"))?;
        out.extend_from_slice(&sig);
    }
    Ok(out)
}

fn encode_relay_bundle(bundle: &RelayBundleV1) -> OmniResult<String> {
    use base64::Engine as _;
    Ok(format!(
        "{MESH_RELAY_BUNDLE_PREFIX}{}",
        base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(relay_bundle_bytes(bundle, true)?)
    ))
}

/// The bundle [`relay_bundle_bytes`] wrote, with its signature.
fn decode_relay_bundle(bytes: &[u8]) -> Option<RelayBundleV1> {
    use base64::Engine as _;
    let mut reader = omni_rendezvous::wire::Reader::new(bytes);
    let version = reader.u16()?;
    let bundle_version = reader.u32()?;
    let issuer = reader.array::<32>()?;
    let issued_at_unix_ms = reader.u64()? as i64;
    let count = reader.u8()?;
    let mut relays = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let flags = reader.u8()?;
        let node = if flags & RELAY_HAS_NODE != 0 { Some(reader.array::<20>()?) } else { None };
        let endpoint = if flags & RELAY_HAS_ENDPOINT != 0 { Some(reader.endpoint()?) } else { None };
        let tcp_endpoint = if flags & RELAY_HAS_TCP_ENDPOINT != 0 { Some(reader.endpoint()?) } else { None };
        let label = reader.short_text()?;
        let cert_spki_pin_hex = if flags & RELAY_HAS_SPKI_PIN != 0 { Some(hex::encode(reader.array::<32>()?)) } else { None };
        relays.push(RelayEntryV1 { node, endpoint, tcp_endpoint, label, cert_spki_pin_hex });
    }
    let sig = reader.array::<64>()?;
    if !reader.is_empty() {
        return None;
    }
    Some(RelayBundleV1 {
        version,
        bundle_version,
        issuer_signing_pubkey_hex: hex::encode(issuer),
        relays,
        issued_at_unix_ms,
        sig: base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(sig),
    })
}

fn parse_relay_bundle(token: &str) -> OmniResult<RelayBundleV1> {
    use base64::Engine as _;
    let payload = token
        .trim()
        .strip_prefix(MESH_RELAY_BUNDLE_PREFIX)
        .ok_or_else(|| {
            OmniError::InvalidInput("not a relay bundle (missing om3relay: prefix)".to_string())
        })?
        .trim();
    let decoded = base64::prelude::BASE64_URL_SAFE_NO_PAD.decode(payload).map_err(|error| {
        OmniError::InvalidInput(format!("relay bundle is not valid base64: {error}"))
    })?;
    let parsed = decode_relay_bundle(&decoded)
        .ok_or_else(|| OmniError::InvalidInput("relay bundle is malformed".to_string()))?;
    if parsed.version != RELAY_BUNDLE_VERSION {
        return Err(OmniError::InvalidInput(format!(
            "unsupported relay bundle version {}",
            parsed.version
        )));
    }
    Ok(parsed)
}

/// The bytes a relay bundle is signed over: its binary form without the signature, under a domain.
/// Sign and verify both recompute this, so the signature covers every field except itself.
fn relay_bundle_signing_bytes(bundle: &RelayBundleV1) -> OmniResult<Vec<u8>> {
    let mut bytes = RELAY_BUNDLE_SIGNING_DOMAIN.to_vec();
    bytes.extend_from_slice(&relay_bundle_bytes(bundle, false)?);
    Ok(bytes)
}

/// Verify a relay bundle's issuer signature and return the `bundle_id` (= issuer mesh node hex). Fails
/// (InvalidInput) on a bad issuer key, a non-32-byte key, a malformed signature, or a signature that
/// doesn't verify — so a poisoned/forged bundle never reaches storage.
fn verify_relay_bundle(bundle: &RelayBundleV1) -> OmniResult<String> {
    use base64::Engine as _;
    let pubkey_bytes = hex::decode(bundle.issuer_signing_pubkey_hex.trim()).map_err(|error| {
        OmniError::InvalidInput(format!("relay bundle issuer key is not hex: {error}"))
    })?;
    let pubkey: [u8; 32] = pubkey_bytes.as_slice().try_into().map_err(|_| {
        OmniError::InvalidInput("relay bundle issuer key is not 32 bytes".to_string())
    })?;
    let sig_bytes = base64::prelude::BASE64_URL_SAFE_NO_PAD
        .decode(bundle.sig.trim())
        .map_err(|error| {
            OmniError::InvalidInput(format!("relay bundle signature is not base64: {error}"))
        })?;
    let signing_bytes = relay_bundle_signing_bytes(bundle)?;
    omni_crypto::verify_signature(
        &omni_crypto::IdentitySigningPublicKey(pubkey),
        &signing_bytes,
        &omni_crypto::Signature { bytes: sig_bytes },
    )
    .map_err(|_| OmniError::InvalidInput("relay bundle signature does not verify".to_string()))?;
    let node = node_address_from_ed25519_pubkey(pubkey)
        .map_err(|error| OmniError::InvalidInput(format!("relay bundle issuer key invalid: {error}")))?;
    Ok(node.to_string())
}

/// True if `url` is a courier endpoint we can actually build a channel for: `http(s)://` (ntfy),
/// `ws(s)://` (Nostr relay), or `dht://` (the BT-DHT courier, e.g. `dht://mainline`). Mirrors the
/// helper's `channel_for_url` scheme dispatch, so the app never stores a provider the helper would
/// silently skip (and a user can re-add `dht://mainline` after removing it).
pub(crate) fn is_valid_provider_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("wss://")
        || url.starts_with("ws://")
        || url.starts_with("dht://")
}

/// The dev/stand provider override (`OMNI_RENDEZVOUS_PROVIDERS`, comma-separated), or None when unset/blank.
fn rendezvous_env_override() -> Option<Vec<String>> {
    std::env::var("OMNI_RENDEZVOUS_PROVIDERS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|env| omni_rendezvous::parse_provider_urls(Some(env)))
}

/// The compiled cross-protocol default courier set (fallback when nothing is configured).
fn rendezvous_default_providers() -> Vec<String> {
    omni_rendezvous::DEFAULT_RENDEZVOUS_PROVIDERS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Resolve a profile's courier set from its `db_path` WITHOUT an app handle — for the autoconnect worker,
/// which spawns the rendezvous helper but holds only `db_path`. Same as
/// `OmniApp::configured_rendezvous_providers` (see [`courier_entries`]); opens a short-lived read
/// connection (any error falls back to the default).
pub(crate) fn resolve_rendezvous_providers_for_db(db_path: &str) -> Vec<String> {
    if let Some(env) = rendezvous_env_override() {
        return env;
    }
    match omni_storage::Storage::open(std::path::Path::new(db_path)) {
        Ok(storage) => courier_entries(&storage),
        Err(_) => rendezvous_default_providers(),
    }
}

/// The courier set the helper gets: the env override (the dev/stand knob) when set; else every stored
/// courier the user has not turned off — read-only ones marked (`omni_rendezvous::courier_entry`) —
/// and the web couriers the devices we talk to use that are not in our list at all, marked pair-only
/// (used for those devices' pairs only, see [`pair_couriers`]); else, with nothing stored, the compiled
/// default. Applies a pre-2026-10-10 Mainline "off" switch to the Mainline row once
/// (see `adopt_legacy_dht_switch`).
pub(crate) fn courier_entries(storage: &omni_storage::Storage) -> Vec<String> {
    use omni_rendezvous::CourierUse;
    if let Some(env) = rendezvous_env_override() {
        return env;
    }
    adopt_legacy_dht_switch(storage);
    let rows = match storage.list_rendezvous_providers() {
        Ok(rows) if !rows.is_empty() => rows,
        _ => return rendezvous_default_providers(),
    };
    let mut entries: Vec<String> = rows
        .iter()
        .filter(|row| row.mode != omni_storage::RENDEZVOUS_PROVIDER_MODE_OFF)
        .map(|row| {
            let courier_use = if row.mode == omni_storage::RENDEZVOUS_PROVIDER_MODE_READ {
                CourierUse::ReadOnly
            } else {
                CourierUse::PublishRead
            };
            omni_rendezvous::courier_entry(&row.url, courier_use)
        })
        .collect();
    // A courier we turned off stays off, for every pair; the DHT has no operator to differ by.
    for url in storage.list_peer_courier_urls().unwrap_or_default() {
        if is_valid_provider_url(&url) && !url.starts_with("dht://") && !rows.iter().any(|row| row.url == url) {
            entries.push(omni_rendezvous::courier_entry(&url, CourierUse::PairOnly));
        }
    }
    entries
}

/// The couriers one pair goes through, given what the other device told us of its own
/// (docs/decisions/courier-metadata-and-quota.md, step 6): post where it looks, look where it posts —
/// among the couriers we use (and the pair-only ones it brought). `None` — our own set: the device has
/// not told us, or nothing is common that way (then `*_common` says so, for the contact's warning).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PairCouriers {
    pub(crate) publish_to: Option<Vec<String>>,
    pub(crate) read_from: Option<Vec<String>>,
    /// The device told us its couriers.
    pub(crate) known: bool,
    /// Something common for our posts (it looks where we may post) / for its posts (it posts where we look).
    pub(crate) publish_common: bool,
    pub(crate) read_common: bool,
}

/// Whether some device of `account_id` that told us its couriers shares none with us one way or the
/// other, and which of our own couriers — turned off, or only read — would give us one.
pub(crate) fn contact_courier_gap(storage: &omni_storage::Storage, account_id: &AccountId) -> (bool, Vec<String>) {
    let rows = storage.list_rendezvous_providers().unwrap_or_default();
    let mut gap = false;
    let mut to_enable: Vec<String> = Vec::new();
    for bundle in storage.list_remote_prekey_bundles_for_account(account_id).unwrap_or_default() {
        let pair = pair_couriers(storage, account_id, &bundle.device_id);
        if !pair.known || (pair.publish_common && pair.read_common) {
            continue;
        }
        gap = true;
        let Ok(Some(theirs)) = storage.get_peer_courier_set(account_id, &bundle.device_id) else {
            continue;
        };
        let mut suggest = |url: &String, enough: &[&str]| {
            if rows.iter().any(|row| row.url == *url && !enough.contains(&row.mode.as_str())) && !to_enable.contains(url) {
                to_enable.push(url.clone());
            }
        };
        if !pair.read_common {
            // It posts where we do not look: looking there is enough.
            for url in &theirs.publish {
                suggest(url, &[omni_storage::RENDEZVOUS_PROVIDER_MODE_READ, omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ]);
            }
        }
        if !pair.publish_common {
            // It looks where we do not post: we have to post there.
            for url in &theirs.read {
                suggest(url, &[omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ]);
            }
        }
    }
    (gap, to_enable)
}

pub(crate) fn pair_couriers(
    storage: &omni_storage::Storage,
    account_id: &AccountId,
    device_id: &DeviceId,
) -> PairCouriers {
    use omni_rendezvous::CourierUse;
    let Ok(Some(theirs)) = storage.get_peer_courier_set(account_id, device_id) else {
        return PairCouriers::default();
    };
    let ours: Vec<(String, CourierUse)> = courier_entries(storage)
        .iter()
        .map(|entry| {
            let (url, courier_use) = omni_rendezvous::parse_courier_entry(entry);
            (url.to_string(), courier_use)
        })
        .collect();
    let we_may_post = |url: &String| ours.iter().any(|(ours, courier_use)| ours == url && *courier_use != CourierUse::ReadOnly);
    let we_look = |url: &String| ours.iter().any(|(ours, _)| ours == url);
    let publish_to: Vec<String> = theirs.read.iter().filter(|url| we_may_post(url)).cloned().collect();
    let read_from: Vec<String> = theirs.publish.iter().filter(|url| we_look(url)).cloned().collect();
    PairCouriers {
        publish_common: !publish_to.is_empty(),
        read_common: !read_from.is_empty(),
        publish_to: (!publish_to.is_empty()).then_some(publish_to),
        read_from: (!read_from.is_empty()).then_some(read_from),
        known: true,
    }
}

/// Whether a courier set has the BT-DHT courier in it (in any use) — then the profile's
/// `omni-dht-helper` runs.
pub(crate) fn couriers_include_dht(entries: &[String]) -> bool {
    entries
        .iter()
        .any(|entry| omni_rendezvous::parse_courier_entry(entry).0.starts_with("dht://"))
}

/// Encode a [`MeshPairingInviteV1`] as a copy-pasteable `om3mesh:` token.
fn encode_mesh_pairing_invite(invite: &omni_types::MeshPairingInviteV1) -> OmniResult<String> {
    use base64::Engine as _;
    let json = serde_json::to_vec(invite)
        .map_err(|error| OmniError::Internal(format!("mesh invite encode failed: {error}")))?;
    Ok(format!(
        "{MESH_PAIRING_INVITE_PREFIX}{}",
        base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(json)
    ))
}

/// Parse + version-check an `om3mesh:` token back into a [`MeshPairingInviteV1`].
fn parse_mesh_pairing_invite(token: &str) -> OmniResult<omni_types::MeshPairingInviteV1> {
    use base64::Engine as _;
    let payload = token
        .trim()
        .strip_prefix(MESH_PAIRING_INVITE_PREFIX)
        .ok_or_else(|| {
            OmniError::InvalidInput(
                "not a mesh pairing invite (missing om3mesh: prefix)".to_string(),
            )
        })?
        .trim();
    let decoded = base64::prelude::BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| {
            OmniError::InvalidInput(format!("mesh invite is not valid base64: {error}"))
        })?;
    let invite: omni_types::MeshPairingInviteV1 = serde_json::from_slice(&decoded)
        .map_err(|error| OmniError::InvalidInput(format!("mesh invite is malformed: {error}")))?;
    if invite.version != omni_types::MESH_PAIRING_INVITE_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported mesh invite version {}",
            invite.version
        )));
    }
    Ok(invite)
}

/// Bidirectional dialog_key ↔ peer NodeAddress map for active mesh links.
/// Both directions are kept so the hot send path (dialog → node) and the
/// inbound media pump (node → dialog) are O(1).
#[derive(Default)]
pub(crate) struct MeshDialogMap {
    by_node: HashMap<MeshNodeAddress, String>,
    by_dialog: HashMap<String, MeshNodeAddress>,
}

impl MeshDialogMap {
    fn insert(&mut self, dialog_key: String, node: MeshNodeAddress) {
        self.by_node.insert(node, dialog_key.clone());
        self.by_dialog.insert(dialog_key, node);
    }

    /// Register an *additional* dialog-key alias that resolves to `node` for
    /// outbound routing only. Unlike [`insert`], this does not touch the reverse
    /// `node → dialog` map the inbound pump reads — so caching a send-side
    /// resolution can never repoint inbound media at a different dialog. Used
    /// when the send path resolves a peer's node from contact identity for a
    /// canonical dialog-key alias the SessionHello never mapped.
    fn insert_dialog_alias(&mut self, dialog_key: String, node: MeshNodeAddress) {
        self.by_dialog.insert(dialog_key, node);
    }

    fn node_for_dialog(&self, dialog_key: &str) -> Option<MeshNodeAddress> {
        self.by_dialog.get(dialog_key).copied()
    }

    fn dialog_for_node(&self, node: &MeshNodeAddress) -> Option<String> {
        self.by_node.get(node).cloned()
    }

    /// Drop every mapping (both directions) referencing `node`. Used by
    /// `drop_mesh_link_for_removed_contact` so a forgotten peer cannot
    /// leave stale dialog routes that would repoint a fresh chat at the deleted contact.
    fn forget_node(&mut self, node: &MeshNodeAddress) {
        if let Some(dialog) = self.by_node.remove(node) {
            self.by_dialog.remove(&dialog);
        }
        // Defensive: a rebound dialog alias could point at `node` without a matching by_node
        // entry (see `insert_dialog_alias`). Sweep those too.
        self.by_dialog.retain(|_, mapped| mapped != node);
    }
}

/// Mesh-backed [`omni_transport::PeerCarrier`]: routes a dialog's outbound
/// wire-envelopes over the verified mesh Link when one exists, wrapped as a
/// mesh app-frame (`TransportRequestEnvelopeV1::wire_envelope`). Returns
/// `NotConnected` (not an error) when there is no mesh Link, so the transport
/// transparently falls back to its legacy session path.
struct MeshPeerCarrier {
    mesh: Arc<Mutex<Option<Arc<MeshRuntime>>>>,
    dialogs: Arc<Mutex<MeshDialogMap>>,
    /// DB path to open a fresh `Storage` handle for the durable feed-seq counter
    /// (per-recipient, persisted) on the offline branch. Mirrors the fresh-open
    /// pattern used by other mesh background workers (`Storage` isn't shareable).
    db_path: String,
}

impl omni_transport::PeerCarrier for MeshPeerCarrier {
    fn try_send_wire_envelope(
        &self,
        dialog_key: &str,
        envelope: &omni_types::WireEnvelopeV1,
    ) -> omni_transport::PeerCarrierOutcome {
        use omni_transport::PeerCarrierOutcome;
        let node = match self
            .dialogs
            .lock()
            .ok()
            .and_then(|map| map.node_for_dialog(dialog_key))
        {
            Some(node) => node,
            None => {
                omni_app_trace_log(
                    "carrier/out",
                    &format!("dialog_key={dialog_key} outcome=no_node_for_dialog"),
                );
                return PeerCarrierOutcome::NotConnected;
            }
        };
        // Clone the runtime out under a brief lock, then send off the lock so a
        // concurrent dial can't stall this send path (and vice versa).
        let runtime = {
            let guard = match self.mesh.lock() {
                Ok(guard) => guard,
                Err(_) => return PeerCarrierOutcome::NotConnected,
            };
            match guard.as_ref() {
                Some(runtime) => Arc::clone(runtime),
                None => return PeerCarrierOutcome::NotConnected,
            }
        };
        let frame = omni_transport::TransportRequestEnvelopeV1::wire_envelope(envelope.clone());
        let bytes = match serde_json::to_vec(&frame) {
            Ok(bytes) => bytes,
            Err(error) => {
                return PeerCarrierOutcome::Failed(OmniError::Transport(format!(
                    "mesh wire-envelope encode failed: {error}"
                )))
            }
        };
        let has_link = runtime.has_link(&node);
        // Где положить пакет — по-прежнему решает несущая: нет пути, значит трубы нет, и вердикт
        // сессии тут ничего не добавит. См. docs/decisions/reachability-belongs-to-the-session.md
        // («несущие сообщают только СПОСОБНОСТЬ»). Durable доступен: и маршрут (`node`), и рантайм
        // уже разрешены выше — ровно то, что нужно `hold_durable`.
        let assessment = session_delivery_for_dialog(&self.db_path, dialog_key, has_link, true);
        if !has_link {
            omni_app_trace_log(
                "carrier/out",
                &format!("dialog_key={dialog_key} outcome=no_mesh_link"),
            );
            // No live mesh link. Do NOT hold durable here — durable is the
            // last-resort `hold_durable` step, run only after the legacy path is
            // also ruled out, so it never preempts a live legacy connection.
            return PeerCarrierOutcome::NotConnected;
        }
        match runtime.send_wire(node, &bytes) {
            Ok(()) => {
                // ШАГ 3: «доставлено» говорит СЕССИЯ, а не несущая. Байты в трубу мы отдали, но
                // если пир не отвечает на наши пробы дольше порога, исходящее плечо доказано
                // мёртвым — ровно тот случай, что дал 496 `outcome=sent` при нулевом входящем.
                //
                // Пакет при этом уходит НЕСМОТРЯ на приговор, и это не небрежность: presence-проба
                // ездит через этот же гейт, и если её глушить, ответа не будет никогда — сессия
                // не сможет ожить. Труба продолжает работать, меняется только заявление.
                //
                // Заявление — `SentUnconfirmed`, НЕ `NotConnected`. Разница не косметическая:
                // `NotConnected` значит «маршрута нет», и вызывающий по нему уходит в legacy-путь,
                // который на живом mesh-линке гарантированно падает («no active QUIC carrier
                // session»). Так переставали ходить подтверждение bootstrap'а и просьба пересобрать
                // сессию — то есть ровно те сообщения, которыми пара выбирается из заклинивания.
                // Замерено живьём 2026-08-14; docs/bugs/bootstrap-ack-never-held-durable.md.
                if !matches!(assessment.verdict, SessionDeliveryVerdict::DeliverNow) {
                    omni_app_trace_log(
                        "carrier/out",
                        &format!(
                            "dialog_key={dialog_key} outcome=sent_unconfirmed evidence={}",
                            assessment.evidence.as_trace_value()
                        ),
                    );
                    return PeerCarrierOutcome::SentUnconfirmed;
                }
                omni_app_trace_log("carrier/out", &format!("dialog_key={dialog_key} outcome=sent"));
                PeerCarrierOutcome::Sent
            }
            Err(error) => PeerCarrierOutcome::Failed(OmniError::Transport(format!(
                "mesh wire-envelope send failed: {error}"
            ))),
        }
    }

    fn try_open_attachment_stream(&self, dialog_key: &str) -> omni_transport::PeerCarrierStreamOutcome {
        use omni_transport::PeerCarrierStreamOutcome;
        let Some(node) = self
            .dialogs
            .lock()
            .ok()
            .and_then(|map| map.node_for_dialog(dialog_key))
        else {
            omni_app_trace_log(
                "attachment/lane",
                &format!("dialog_key={dialog_key} lane=none reason=no_node_for_dialog"),
            );
            return PeerCarrierStreamOutcome::NotConnected;
        };
        // Clone the runtime out under a brief lock, then open off the lock (same discipline as the
        // wire-envelope path: a concurrent dial must not stall this, nor this that).
        let runtime = {
            let guard = match self.mesh.lock() {
                Ok(guard) => guard,
                Err(_) => return PeerCarrierStreamOutcome::NotConnected,
            };
            match guard.as_ref() {
                Some(runtime) => Arc::clone(runtime),
                None => return PeerCarrierStreamOutcome::NotConnected,
            }
        };
        if !runtime.has_link(&node) {
            omni_app_trace_log(
                "attachment/lane",
                &format!("dialog_key={dialog_key} lane=none reason=no_mesh_link"),
            );
            return PeerCarrierStreamOutcome::NotConnected;
        }
        let opening = std::time::Instant::now();
        match runtime.open_bulk_stream(node) {
            Ok(bulk) => match omni_transport::AttachmentTransferStream::from_halves(
                bulk.send, bulk.recv,
            ) {
                Ok(stream) => {
                    omni_app_trace_log(
                        "attachment/lane",
                        &format!(
                            "dialog_key={dialog_key} lane=mesh open={}ms",
                            opening.elapsed().as_millis()
                        ),
                    );
                    PeerCarrierStreamOutcome::Opened(Box::new(stream))
                }
                Err(error) => PeerCarrierStreamOutcome::Failed(error),
            },
            // No bulk plane on the current path — not a failure of the transfer, the caller falls
            // back to the legacy carrier.
            Err(error) => {
                omni_app_trace_log(
                    "attachment/lane",
                    &format!("dialog_key={dialog_key} lane=none reason={error}"),
                );
                PeerCarrierStreamOutcome::NotConnected
            }
        }
    }

    fn hold_durable(&self, dialog_key: &str, envelope: &omni_types::WireEnvelopeV1) -> bool {
        // Resolve the peer's NodeAddress from the dialog map (restored from the
        // persisted routes at start). Without a route we cannot durable-send.
        let Some(node) = self
            .dialogs
            .lock()
            .ok()
            .and_then(|map| map.node_for_dialog(dialog_key))
        else {
            omni_app_trace_log("durable/hold", &format!("no_route dialog={dialog_key}"));
            return false;
        };
        let runtime = match self.mesh.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(runtime) => Arc::clone(runtime),
                None => {
                    omni_app_trace_log("durable/hold", "no_runtime");
                    return false;
                }
            },
            Err(_) => return false,
        };
        let frame = omni_transport::TransportRequestEnvelopeV1::wire_envelope(envelope.clone());
        let Ok(bytes) = serde_json::to_vec(&frame) else {
            return false;
        };
        // The feed seq is persisted (omni-storage) so a restart can't reuse one and
        // have the recipient false-dedup the new message. The send path marks the
        // message sent on a successful hold, so this runs once per message — no
        // retry loop re-holding frames into the bounded store.
        let storage = match omni_storage::Storage::open(&self.db_path) {
            Ok(storage) => storage,
            Err(error) => {
                omni_app_trace_log("durable/hold", &format!("storage_open_err={error}"));
                return false;
            }
        };
        let seq = match storage.next_durable_seq(node.as_bytes()) {
            Ok(seq) => seq,
            Err(error) => {
                omni_app_trace_log("durable/hold", &format!("seq_err={error}"));
                return false;
            }
        };
        let held = runtime.send_durable(node, &bytes, seq as u64).is_ok();
        omni_app_trace_log(
            "durable/hold",
            &format!("dialog={dialog_key} node={node} held={held}"),
        );
        held
    }

    fn drop_phantom_link(&self, dialog_key: &str) -> bool {
        // Resolve the peer node the same cheap way `hold_durable` does (in-memory dialog map,
        // restored from persisted routes at start).
        let Some(node) = self
            .dialogs
            .lock()
            .ok()
            .and_then(|map| map.node_for_dialog(dialog_key))
        else {
            return false;
        };
        let Some(runtime) = self
            .mesh
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(Arc::clone))
        else {
            return false;
        };
        // S3 honest failover (Meta-B): an undelivered message over the phantom UDP link means our
        // OUTBOUND UDP is dead (asymmetric mobile — inbound keeps the link "alive"-looking). If we know
        // the peer's direct-TCP door (learned from its beacon `tcp_direct`), MIGRATE this peer's ONE quinn
        // connection onto it — the reliable base on a UDP-hostile carrier — instead of the inert demote
        // that only reshuffles a ranking `best()` never consulted for the mux. Same connection, no re-dial.
        // See docs/bugs/relay-less-mobile-transport-findings.md §THE REMAINING BLOCKER + mux_socket.
        if let Some(door) = crate::app_peer_door::peer_door(&self.db_path, node.as_bytes()) {
            match runtime.route_peer_over_door(&node, door) {
                Ok(true) => {
                    omni_app_trace_log(
                        "durable/drop_phantom",
                        &format!("dialog={dialog_key} node={node} routed_over_door={door} (Meta-B failover)"),
                    );
                    return true;
                }
                // Ok(false) = peer not dialed over UDP yet (nothing to migrate) → fall through to demote.
                Ok(false) => {}
                Err(error) => omni_app_trace_log(
                    "durable/drop_phantom",
                    &format!("dialog={dialog_key} node={node} route_over_door={door} FAILED: {error}"),
                ),
            }
        }
        // No known door (CGNAT peer) or nothing to migrate → keep the fail-back demote so `best()` at
        // least deprioritises the dead UDP path (kept alongside; no drop/re-dial churn).
        let demoted = runtime.demote_udp_direct(&node);
        omni_app_trace_log(
            "durable/drop_phantom",
            &format!("dialog={dialog_key} node={node} udp_demoted={demoted} (no known door — fallback)"),
        );
        demoted
    }
}

impl OmniApp {
    /// Start the mesh overlay for the active profile (idempotent). The mesh
    /// announces + accepts immediately so peers can discover and reach us; a
    /// call later dials the specific contact via [`OmniApp::mesh_connect_dialog`].
    /// Start the mesh overlay and self-organizing auto-connect (the normal path).
    /// Same-host binding (loopback QUIC) by default — the same-machine tests and
    /// the multi-instance gates rely on it. For real cross-host operation use
    /// [`OmniApp::start_mesh_networked`].
    pub fn start_mesh(&self) -> OmniResult<()> {
        self.start_mesh_internal(true, false)
    }

    /// Start the mesh overlay WITHOUT the auto-connect worker, so the caller drives
    /// links explicitly via [`OmniApp::mesh_dial`]. Used by the headless `omni-node`
    /// to build a deterministic topology (e.g. a relay where A and C never link).
    pub fn start_mesh_no_autoconnect(&self) -> OmniResult<()> {
        self.start_mesh_internal(false, false)
    }

    /// Start the mesh for REAL cross-host operation: bind QUIC to the detected LAN
    /// address (so the announced address is dialable from another machine) + send
    /// multicast on that interface, with self-organizing auto-connect. This is the
    /// path the shell uses. Falls back to same-host binding if no routable LAN
    /// interface is found (isolated host / CI). Env (`OMNI_MESH_QUIC_BIND` /
    /// `OMNI_MESH_MCAST_IFACE`) still overrides. See docs/decisions/mesh-network-binding.md.
    pub fn start_mesh_networked(&self) -> OmniResult<()> {
        self.start_mesh_internal(true, true)
    }

    fn start_mesh_internal(&self, autoconnect: bool, prefer_lan: bool) -> OmniResult<()> {
        if self.mesh_lock()?.is_some() {
            return Ok(());
        }
        // КОГДА МЕШ ПОДНЯЛСЯ — ОТМЕТКА, А НЕ ДОГАДКА.
        //
        // Пока узел не объявил себя, найти его нельзя, и это прямое время до связи. Замер
        // 2026-08-30 на второй машине: от старта процесса до первой публикации маячка — 41 с, из
        // них 33 с приходятся на промежуток, в котором в трассе НЕТ НИ ОДНОГО события. Пятнадцать
        // секунд из них — заявленное льготное окно первого цикла, остальные ~18 не объяснены
        // ничем: наблюдаемости там просто нет. Эти три отметки её дают.
        let started_at = std::time::Instant::now();
        omni_app_trace_log("mesh/start", "requested");

        let active = self.current_active_profile_context_required()?;
        // The mesh app-hello (make_hello below) ships our local prekey bundle so a
        // freshly-dialed stranger can pair (finish_mesh_pairing_from_hello needs it).
        // Legacy signaling only refreshes this lazily on connect/initiate, but the
        // mesh builds hellos at link-up — well before any legacy op — so ensure it
        // is set now. Without this the live hello carries no bundle and pairing
        // skips with "hello carried no prekey bundle" (found in the #60 live run).
        self.refresh_transport_prekey_bundle()?;
        let identity_key = self
            .storage
            .get_local_identity_key(&active.account.descriptor.account_id)?
            .ok_or_else(|| {
                OmniError::Internal("active profile has no local identity key".to_string())
            })?;
        let signing_bytes = omni_crypto::mesh_identity_signing_key_bytes(&identity_key)?;

        // Inbound media pump: map peer NodeAddress → dialog_key and deliver
        // through the same approval-gated path the transport receive uses.
        let sinks = Arc::clone(&self.realtime_media_inbound);
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let dialogs = Arc::clone(&self.mesh_dialogs);
        let mesh_for_media = Arc::clone(&self.mesh);
        let screen_sink = Arc::clone(&self.screen_control_sink);
        let on_media: MeshMediaCallback = Arc::new(move |peer, payload| {
            // Group-call media (G9): a datagram carrying the group-media magic is
            // re-addressed by the forwarder (no decrypt) and/or opened with the
            // sender's key + enqueued to the group thread. Handled first; returns
            // true if it was group media so we don't also treat it as 1:1.
            if let Some(mesh) = mesh_for_media.lock().ok().and_then(|g| g.as_ref().map(Arc::clone)) {
                let enqueue = |dialog: &str, bytes: Vec<u8>| {
                    // Group media is already E2E-validated on open; deliver it to
                    // the group dialog without the 1:1 negotiation gate.
                    push_incoming_group_media_datagram(&sinks, dialog, &bytes);
                };
                let send = |node: &[u8; 20], bytes: &[u8]| {
                    mesh.send_media(omni_mesh_runtime::MeshNodeAddress::from_bytes(*node), bytes)
                };
                let screen = |delivery: &crate::ScreenControlDelivery| screen_sink.deliver(delivery);
                if crate::app_group_calls::handle_incoming_group_media(
                    &db_path,
                    &send,
                    &payload,
                    &enqueue,
                    &screen,
                ) {
                    return;
                }
            }
            let dialog = dialogs.lock().ok().and_then(|map| map.dialog_for_node(&peer));
            if let Some(dialog) = dialog {
                // Remote screen control shares the plane with RTP but never reaches the media
                // engine: the core verifies it and hands it to the shell's injector.
                if crate::app_screen_control::handle_incoming_screen_control_datagram(
                    &db_path,
                    &dialog,
                    &payload,
                    &screen_sink,
                ) {
                    return;
                }
                let _ = push_incoming_realtime_media_datagram_for_dialog(
                    &sinks, &db_path, &dialog, &payload,
                );
            }
        });

        // Inbound wire plane: each mesh app-frame is a TransportRequestEnvelopeV1.
        // A SessionHello registers the peer as a verified connection (reusing the
        // transport's session machinery) + records the ctx; subsequent
        // WireEnvelope frames are injected into the normal dispatch via that ctx.
        let transport_for_wire = self.transport.clone();
        let node_ctx = Arc::clone(&self.mesh_node_ctx);
        let wire_dialogs = Arc::clone(&self.mesh_dialogs);
        let locally_dialed = Arc::clone(&self.mesh_locally_dialed);
        let pairing_targets = Arc::clone(&self.mesh_pairing_targets);
        let wire_db_path = self.storage.db_path().to_string_lossy().to_string();
        // For group-feed ingest that must publish a control in response (a Join on adopt, or a
        // media-state re-announce so a late joiner learns pre-join state) — populated after start().
        let mesh_for_wire = Arc::clone(&self.mesh);
        // Hello-reply throttle for the symmetric-hello fix (bug A/B, 2026-07-20). When we receive
        // an inbound hello from a peer we did not dial, we push our own hello back so a peer that
        // dialed us as part of `find_contact_by_omni_id` (its pairing_target, not ours) sees our
        // hello and can run `finish_mesh_pairing_from_hello`. Without a throttle the two sides
        // would ping-pong hellos forever; 5 s per peer stops the loop while still letting a
        // legitimate re-dial reach us within the human wait window.
        let hello_reply_throttle: Arc<Mutex<std::collections::HashMap<MeshNodeAddress, std::time::Instant>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let hello_reply_throttle_for_wire = Arc::clone(&hello_reply_throttle);
        let transport_for_hello_reply = self.transport.clone();
        let mesh_for_hello_reply = Arc::clone(&self.mesh);
        let on_wire: MeshWireCallback = Arc::new(move |peer, bytes| {
            let frame: omni_transport::TransportRequestEnvelopeV1 =
                match serde_json::from_slice(&bytes) {
                    Ok(frame) => frame,
                    Err(error) => {
                        omni_app_trace_log(
                            "mesh_wire/in",
                            &format!("peer={peer} parse_error={error} bytes={}", bytes.len()),
                        );
                        return;
                    }
                };
            // on_wire runs INLINE on the mesh pump's tokio worker (register_link) or the durable
            // loop's worker. A step logged without its successor is where that worker is parked —
            // and if it's a blocking mesh send, it is starving the very runtime that must drive it.
            // See findings §RESOLVED / pair #8. Cheap (env-gated) and low-volume (one per frame).
            omni_app_trace_log(
                "on_wire/enter",
                &format!("peer={peer} tid={:?}", std::thread::current().id()),
            );
            match frame.payload {
                omni_transport::TransportRequestPayloadV1::SessionHello(hello) => {
                    let peer_id = peer.to_string();
                    let initiated = locally_dialed
                        .lock()
                        .map(|set| set.contains(&peer))
                        .unwrap_or(false);
                    // Symmetric-hello reply (bug A/B, 2026-07-20). SessionHello is sent from
                    // `register_link` on link-up and from `dial_mesh_pairing_targets` when we
                    // want to pair. If a peer dials us as part of its own add-by-Omni-ID
                    // (its pairing_target, not ours) OVER A SURVIVING LINK, `Network::dial`
                    // coalesces and no fresh hello lands here — so `finish_mesh_pairing_from_hello`
                    // never fires on either side and the whole handshake deadlocks (needed a
                    // full app restart to break). Answer an inbound hello with our own hello so
                    // the peer's pairing path can complete. Rate-limited per peer to break
                    // ping-pong; a symbolic re-arm every 5 s covers the human wait window
                    // without turning the link into a hello loop (a reply to a reply lands
                    // inside the 5 s while a round trip is under 2.5 s).
                    //
                    // Answered whether or not WE dialed too. Both sides dialing at once is the
                    // usual reconnect, and each then counted on the hello its own link-up sent —
                    // which is lost when that link is superseded a moment later: live 2026-10-16
                    // the laptop restarted, both dialed, the PC's hello went down the link that
                    // was pruned, and the laptop never had the PC's session context — every file
                    // from the PC dropped, its download asked for again every queue tick.
                    {
                        let should_reply = {
                            let mut throttle = hello_reply_throttle_for_wire.lock().unwrap_or_else(|p| p.into_inner());
                            let now = std::time::Instant::now();
                            let due = throttle
                                .get(&peer)
                                .map(|last| now.duration_since(*last).as_secs() >= 5)
                                .unwrap_or(true);
                            if due {
                                throttle.insert(peer, now);
                            }
                            due
                        };
                        if should_reply {
                            if let Some(mesh_rt) = mesh_for_hello_reply
                                .lock()
                                .ok()
                                .and_then(|guard| guard.as_ref().map(Arc::clone))
                            {
                                let hello_bytes = serde_json::to_vec(
                                    &transport_for_hello_reply.build_local_session_hello_envelope(),
                                )
                                .unwrap_or_default();
                                if !hello_bytes.is_empty() {
                                    match mesh_rt.send_wire(peer, &hello_bytes) {
                                        Ok(()) => omni_app_trace_log(
                                            "mesh_wire/hello_reply",
                                            &format!("peer={peer} replied to unsolicited hello"),
                                        ),
                                        Err(error) => omni_app_trace_log(
                                            "mesh_wire/hello_reply",
                                            &format!("peer={peer} reply FAILED: {error}"),
                                        ),
                                    }
                                }
                            }
                        }
                    }
                    // Pairing-over-mesh (layer 3): if this peer is a deliberately
                    // imported invite target, turn its hello into an
                    // outgoing-Pending contact + initiator session + dialog route
                    // BEFORE we register the connection, so the SessionActivated
                    // contact-request trigger ships the request over this Link.
                    finish_mesh_pairing_from_hello(
                        &wire_db_path,
                        &pairing_targets,
                        &wire_dialogs,
                        &transport_for_wire,
                        peer,
                        &hello,
                    );
                    match transport_for_wire.register_mesh_connection_from_hello(
                        &peer_id,
                        &hello,
                        initiated,
                    ) {
                        Ok(ctx) => {
                            omni_app_trace_log(
                                "mesh_wire/in",
                                &format!(
                                    "peer={peer} hello registered dialog_key={} initiated={initiated}",
                                    ctx.dialog_key
                                ),
                            );
                            if let Ok(mut map) = wire_dialogs.lock() {
                                map.insert(ctx.dialog_key.clone(), peer);
                            }
                            // Refresh the persisted node↔dialog bridge on every
                            // link-up so durable delivery stays correct across a
                            // re-pair / instance change (overwrites stale routes).
                            if let Ok(storage) = omni_storage::Storage::open(&wire_db_path) {
                                let _ = storage
                                    .set_mesh_dialog_route(peer.as_bytes(), &ctx.dialog_key);
                            }
                            if let Ok(mut ctxs) = node_ctx.lock() {
                                ctxs.insert(peer, ctx);
                            }
                            // Presence event for the daemon's event plane: a verified
                            // link to this peer is up (a hello = (re)established link).
                            record_link_state_event(&wire_db_path, &peer.to_string(), true);
                            // A new link starts with a clean probe window — see the fn.
                            if let Some(dialog_key) = wire_dialogs
                                .lock()
                                .ok()
                                .and_then(|map| map.dialog_for_node(&peer))
                            {
                                restart_session_probe_window_for_dialog(&wire_db_path, &dialog_key);
                            }
                        }
                        Err(error) => {
                            omni_app_trace_log(
                                "mesh_wire/in",
                                &format!("peer={peer} hello register FAILED: {error}"),
                            );
                        }
                    }
                }
                omni_transport::TransportRequestPayloadV1::GroupFeed { feed: _, item_json } => {
                    // Group durable-feed frame (G3): sender-key crypto replaces the
                    // pairwise ctx, so ingest straight from storage. Same path for
                    // live gossip and store-and-forward delivery.
                    if let Ok(storage) = omni_storage::Storage::open(&wire_db_path) {
                        if let Err(error) = crate::app_groups::ingest_group_feed_item(
                            &storage,
                            &transport_for_wire,
                            &item_json,
                        ) {
                            omni_app_trace_log(
                                "mesh_wire/in",
                                &format!("peer={peer} group_feed ingest error={error}"),
                            );
                        }
                        // Ingest may have queued replies (a Commit for a member leaving, call
                        // control): publish them now, we hold the mesh.
                        if let Some(mesh) = mesh_for_wire.lock().ok().and_then(|g| g.as_ref().map(Arc::clone)) {
                            crate::app_groups::flush_group_feed_outbox(&storage, &mesh);
                        }
                    }
                }
                omni_transport::TransportRequestPayloadV1::WireEnvelope { envelope } => {
                    let ctx = node_ctx.lock().ok().and_then(|map| map.get(&peer).cloned());
                    match ctx {
                        Some(ctx) => {
                            omni_app_trace_log(
                                "mesh_wire/in",
                                &format!(
                                    "peer={peer} wire-envelope injected dialog_key={}",
                                    ctx.dialog_key
                                ),
                            );
                            transport_for_wire.inject_inbound_wire_envelope(envelope, ctx);
                        }
                        None => {
                            // No active node-ctx: we never linked this peer this
                            // session. If it's a durable (store-and-forward) frame
                            // from a known contact, the node↔dialog bridge lets us
                            // rebuild a receive-ctx and decrypt via the contact's
                            // ratchet — the never-online-together path. The in-memory
                            // map is the authoritative runtime lookup (restored from
                            // the persisted routes at start, refreshed on link-up),
                            // so the receive side resolves it the same cheap way the
                            // send side does — no per-frame storage open.
                            let route = wire_dialogs
                                .lock()
                                .ok()
                                .and_then(|map| map.dialog_for_node(&peer));
                            match route {
                                Some(dialog_key) => {
                                    omni_app_trace_log(
                                        "mesh_wire/in",
                                        &format!(
                                            "peer={peer} wire-envelope injected via persisted route dialog_key={dialog_key}"
                                        ),
                                    );
                                    let ctx = transport_for_wire
                                        .received_context_for_dialog_key(&dialog_key);
                                    transport_for_wire.inject_inbound_wire_envelope(envelope, ctx);
                                }
                                None => {
                                    // First-contact self-contained bootstrap (2026-07-20): a
                                    // SessionBootstrap can arrive WITHOUT a preceding hello (durable
                                    // delivery over a lossy mobile link that dropped the live frames).
                                    // It carries everything needed — build the ctx from it and process,
                                    // instead of dropping. Persist the node↔dialog route so the ack and
                                    // any later frame resolve. Non-bootstrap frames (or an old bootstrap
                                    // with no instance id) still drop honestly. Build the ctx under a
                                    // borrow of `envelope.payload`, then move `envelope` into inject once
                                    // the borrow has ended.
                                    let bootstrap_ctx = match &envelope.payload {
                                        omni_types::WirePayloadV1::SessionBootstrap(message) => {
                                            Some(received_context_from_bootstrap(message))
                                        }
                                        _ => None,
                                    };
                                    match bootstrap_ctx {
                                        Some(Some(ctx)) => {
                                            if let Ok(mut map) = wire_dialogs.lock() {
                                                map.insert(ctx.dialog_key.clone(), peer);
                                            }
                                            if let Ok(storage) =
                                                omni_storage::Storage::open(&wire_db_path)
                                            {
                                                let _ = storage.set_mesh_dialog_route(
                                                    peer.as_bytes(),
                                                    &ctx.dialog_key,
                                                );
                                            }
                                            omni_app_trace_log(
                                                "mesh_wire/in",
                                                &format!(
                                                    "peer={peer} first-contact bootstrap injected (no hello) dialog_key={}",
                                                    ctx.dialog_key
                                                ),
                                            );
                                            transport_for_wire
                                                .inject_inbound_wire_envelope(envelope, ctx);
                                        }
                                        Some(None) => omni_app_trace_log(
                                            "mesh_wire/in",
                                            &format!(
                                                "peer={peer} bootstrap DROPPED (no initiator_instance_id — pre-fix sender)"
                                            ),
                                        ),
                                        None => omni_app_trace_log(
                                            "mesh_wire/in",
                                            &format!(
                                                "peer={peer} wire-envelope DROPPED (no node_ctx, no persisted route)"
                                            ),
                                        ),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        // Outbound app-hello: our SessionHello sent as the first reliable frame
        // on link-up (the facade ships these bytes from its async context).
        let transport_for_hello = self.transport.clone();
        let make_hello: MeshHelloMaker = Arc::new(move |_peer| {
            serde_json::to_vec(&transport_for_hello.build_local_session_hello_envelope())
                .unwrap_or_default()
        });

        // Proactive teardown: when a peer's last mesh path drops, clear its stale
        // transport verified-connection + receive-ctx so the next message to it falls
        // through to durable store-and-forward instead of retrying a dead link. The
        // persisted node↔dialog route is kept — durable still needs it to send.
        let transport_for_down = self.transport.clone();
        let node_ctx_for_down = Arc::clone(&self.mesh_node_ctx);
        let down_db_path = self.storage.db_path().to_string_lossy().to_string();
        let on_peer_down: MeshPeerDownCallback = Arc::new(move |peer| {
            transport_for_down.deregister_mesh_connection(&peer.to_string());
            if let Ok(mut ctxs) = node_ctx_for_down.lock() {
                ctxs.remove(&peer);
            }
            // Presence event for the daemon's event plane: the link to this peer is down.
            record_link_state_event(&down_db_path, &peer.to_string(), false);
            omni_app_trace_log("mesh/peer_down", &format!("peer={peer} cleared stale session state"));
        });

        // DHT-over-peer responder (R7 Phase 2 mode b): serve a peer's BEP44 op on THIS profile's local
        // omni-dht-helper and return the result; the facade routes the response back. On by default for
        // our own devices and approved contacts (owner decision 2026-10-12 — a network that drops DHT
        // traffic, live on the laptop, then reaches Mainline through the PC); anyone else is refused.
        // A node advertises the role (`ANNOUNCE_CAP_DHT_PROXY`) while its own Mainline sees the swarm:
        // off at start, the sweep turns it on (`refresh_dht_proxy_role`). `OMNI_DHT_PROXY` keeps it on
        // for a volunteer relay. See docs/decisions/dht-over-peer.md.
        let dht_proxy_for_anyone = std::env::var("OMNI_DHT_PROXY")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let dht_proxy_advertised = dht_proxy_for_anyone;
        let on_dht_proxy_request: Option<MeshDhtProxyCallback> = {
            let proxy_db_path = self.storage.db_path().to_string_lossy().to_string();
            Some(Arc::new(move |req: MeshDhtProxyRequest| {
                if !dht_proxy_for_anyone && !dht_proxy_origin_is_ours(&proxy_db_path, &req.from) {
                    return MeshDhtProxyResult { ok: false, value: Vec::new() };
                }
                // Anti-abuse (masterplan §4 — minimal harm): refuse if this origin is over its
                // per-window serve budget; a flood through us can't be amplified.
                if !dht_proxy_admit_origin(&req.from.to_string(), current_unix_ms() as u64) {
                    return MeshDhtProxyResult { ok: false, value: Vec::new() };
                }
                let (ok, value) =
                    dht_proxy_serve(&proxy_db_path, req.op, &req.key, &req.salt, &req.value);
                MeshDhtProxyResult { ok, value }
            }))
        };

        // DHT bootstrap seed responder (step B): a direct neighbour asks for the in-segment DHT
        // bootstrap addresses we know. Share our CONFIGURED list (community/invite/manual-vetted entry
        // points) iff we ourselves use the DHT (DHT-rendezvous enabled) — a node not using DHT does not
        // participate in seeding. These are public-in-the-swarm addresses, shared point-to-point on the
        // link (never in the announce/gossip plane → R1b intact); the wire encoder caps the list to
        // DHT_BOOT_MAX_NODES. Re-reads the sidecars each call so a runtime toggle takes effect. See
        // docs/decisions/dht-bootstrap-mesh-propagation.md.
        let boot_db_path = self.storage.db_path().to_string_lossy().to_string();
        let on_dht_bootstrap_request: Option<MeshDhtBootstrapCallback> = Some(Arc::new(move || {
            if !load_dht_rendezvous_enabled(&boot_db_path) {
                return Vec::new();
            }
            load_dht_bootstrap_nodes(&boot_db_path)
                .iter()
                .filter_map(|node| node.parse::<std::net::SocketAddr>().ok())
                .collect()
        }));

        // Inbound bulk plane: a peer opened an attachment-transfer stream on its verified link.
        // Resolve its `ReceivedChatContext` (recorded when its SessionHello registered the peer)
        // and hand the stream to the SAME receive handler the legacy carrier feeds — only the pipe
        // differs, the transfer protocol and persistence are shared.
        let bulk_storage = Arc::clone(&self.runtime_event_storage);
        let bulk_transport_handle = Arc::clone(&self.transport_handle);
        let bulk_session_lock = Arc::clone(&self.session_lock);
        let bulk_node_ctx = Arc::clone(&self.mesh_node_ctx);
        let bulk_dialogs = Arc::clone(&self.mesh_dialogs);
        let bulk_transport = self.transport.clone();
        let bulk_mesh = Arc::clone(&self.mesh);
        let on_bulk_stream: MeshBulkStreamCallback = Arc::new(move |peer, bulk| {
            // A member pulling a group file is served apart: it may be no contact of ours, so it
            // has no session context, and the stream carries the answer back to it.
            let bulk = match crate::app_group_attachments::classify_inbound_bulk(bulk) {
                Ok(crate::app_group_attachments::InboundBulk::Transfer(bulk)) => bulk,
                Ok(crate::app_group_attachments::InboundBulk::GroupPull(request, bulk)) => {
                    let db_path = bulk_storage
                        .lock()
                        .map(|storage| storage.db_path().to_string_lossy().into_owned());
                    // Taken out of its lock first: a link check must not hold the mesh's slot.
                    let mesh = bulk_mesh.lock().ok().and_then(|mesh| mesh.as_ref().map(Arc::clone));
                    let linked = |node: &omni_mesh_runtime::MeshNodeAddress| mesh.as_ref().is_some_and(|mesh| mesh.has_link(node));
                    if let Ok(db_path) = db_path {
                        crate::app_group_attachments::serve_group_pull(&db_path, peer, request, bulk, &linked);
                    }
                    return;
                }
                Err(error) => {
                    omni_app_trace_log(
                        "attachment/bulk-in",
                        &format!("peer={peer:?} dropped reason={error}"),
                    );
                    return;
                }
            };
            // No live context (no hello this mesh session): the persisted node↔dialog route, as a
            // message takes (`on_wire`). A mesh restart clears the live contexts, and a peer whose
            // session is up sends no new hello — every file from it was dropped, the sender retried
            // and the receiver asked again, round and round (live 2026-10-16: 873 streams dropped in
            // minutes, messages from the same peer arriving fine by the route).
            let ctx = bulk_node_ctx.lock().ok().and_then(|map| map.get(&peer).cloned()).or_else(|| {
                let dialog_key = bulk_dialogs.lock().ok().and_then(|map| map.dialog_for_node(&peer))?;
                omni_app_trace_log(
                    "attachment/bulk-in",
                    &format!("peer={peer:?} via persisted route dialog_key={dialog_key}"),
                );
                Some(bulk_transport.received_context_for_dialog_key(&dialog_key))
            });
            let Some(ctx) = ctx else {
                // Neither a session context nor a route for this peer: dropping the stream closes
                // it, and the sender retries — better than persisting into an unknown dialog.
                omni_app_trace_log(
                    "attachment/bulk-in",
                    &format!("peer={peer:?} dropped reason=no_session_context"),
                );
                return;
            };
            let stream = match omni_transport::AttachmentTransferStream::from_halves(
                bulk.send, bulk.recv,
            ) {
                Ok(stream) => stream,
                Err(error) => {
                    omni_app_trace_log(
                        "attachment/bulk-in",
                        &format!("peer={peer:?} dropped reason={error}"),
                    );
                    return;
                }
            };
            omni_app_trace_log(
                "attachment/bulk-in",
                &format!("peer={peer:?} dialog_key={} accepted", ctx.dialog_key),
            );
            if let Err(error) = handle_incoming_attachment_transfer_stream(
                &bulk_storage,
                &bulk_transport_handle,
                &bulk_session_lock,
                stream,
                ctx,
            ) {
                omni_app_trace_log(
                    "attachment/bulk-in",
                    &format!("peer={peer:?} handler failed: {error}"),
                );
            }
        });

        let callbacks = MeshCallbacks {
            on_wire: Some(on_wire),
            on_media,
            make_hello: Some(make_hello),
            on_peer_down: Some(on_peer_down),
            on_dht_proxy_request,
            on_dht_bootstrap_request,
            on_bulk_stream: Some(on_bulk_stream),
        };

        // Binding selection (see docs/decisions/mesh-network-binding.md):
        //  - default (same_host): loopback QUIC + multicast loop — what the same-machine
        //    tests and multi-instance gates rely on;
        //  - `prefer_lan` (the shell's start_mesh_networked): bind the detected LAN
        //    address so the announced QUIC address is dialable cross-host, and send
        //    multicast on that interface. Falls back to same_host if no routable LAN
        //    interface is found (isolated host / CI).
        // Env always overrides: `OMNI_MESH_QUIC_BIND` / `OMNI_MESH_MCAST_IFACE`.
        let mut mesh_config = MeshConfig::same_host(signing_bytes);
        // Advertise the DHT-proxy role iff we opted in (same flag that arms the responder above).
        mesh_config.dht_proxy = dht_proxy_advertised;
        // Friend-proxy (Axis 2): the advertised ANNOUNCE_CAP_FRIEND_RELAY role is DERIVED from the
        // persisted per-contact consents — we advertise iff we carry for at least one friend (no
        // separate global toggle to forget). The resolved nodes seed the runtime's consent set
        // right after start, below. See docs/decisions/friend-proxy-by-consent.md (Phase C).
        let friend_relay_consented_nodes = self.friend_relay_consented_nodes();
        mesh_config.friend_relay = !friend_relay_consented_nodes.is_empty();
        // R1a connection-quality ladder (axis A): carry the persisted carrier policy and the
        // rung-0 carrier target. The relay TCP endpoint is the first registry relay that
        // advertises a TLS-over-TCP bridge (base node sorts first); parsed best-effort.
        mesh_config.carrier_policy = self.carrier_policy();
        // Fixed direct-TCP front-door port for a reliable relay-less inbound path (the user
        // port-forwards it). `None` keeps the ephemeral/UPnP behaviour. See tcp-direct-peer-fallback.md.
        mesh_config.direct_tcp_bind_port = self.direct_tcp_bind_port();
        // `relay_tcp_endpoint` (the rung-0 TCP-carrier target) is resolved AFTER quic_bind is
        // finalized and gated on a non-loopback bind — see below the env overrides.
        if prefer_lan {
            if let Some(lan) = detect_primary_lan_ipv4() {
                mesh_config.multicast_iface = lan;
                mesh_config.advertise_ipv4 = Some(lan);
                if let Some(v6) = detect_global_ipv6() {
                    // Dual-stack: bind `[::]` so we LISTEN on both IPv4 (v4-mapped) AND native
                    // IPv6, and advertise the LAN v4 + global v6. The proven v4/UPnP path is
                    // preserved via `advertise_ipv4`; a dual-stack peer dials our v6 with no
                    // NAT — the cheapest cross-network reach. See mesh-network-binding.md.
                    mesh_config.quic_bind = std::net::SocketAddr::new(
                        std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                        0,
                    );
                    mesh_config.advertise_ipv6 = Some(v6);
                } else {
                    // IPv4-only host: bind the LAN v4 directly — byte-identical to the path
                    // that was proven live, so a host without native v6 is unaffected.
                    mesh_config.quic_bind =
                        std::net::SocketAddr::new(std::net::IpAddr::V4(lan), 0);
                }
            } else {
                omni_app_trace_log(
                    "mesh/bind",
                    "start_mesh_networked: no routable LAN interface — falling back to same-host",
                );
            }
        }
        if let Ok(bind) = std::env::var("OMNI_MESH_QUIC_BIND") {
            match bind.parse() {
                Ok(addr) => {
                    mesh_config.quic_bind = addr;
                    // An explicit bind override drives the advertised address off the bound
                    // socket again (announce falls back to quic_addr) — keep them consistent.
                    mesh_config.advertise_ipv4 = None;
                    mesh_config.advertise_ipv6 = None;
                }
                Err(error) => {
                    return Err(OmniError::InvalidInput(format!(
                        "OMNI_MESH_QUIC_BIND='{bind}' is not a valid socket address: {error}"
                    )))
                }
            }
        }
        if let Ok(iface) = std::env::var("OMNI_MESH_MCAST_IFACE") {
            match iface.parse() {
                Ok(ip) => mesh_config.multicast_iface = ip,
                Err(error) => {
                    return Err(OmniError::InvalidInput(format!(
                        "OMNI_MESH_MCAST_IFACE='{iface}' is not a valid IPv4 address: {error}"
                    )))
                }
            }
        }
        // R1a connection-quality ladder (axis A) — the rung-0 TCP-carrier target = the first registry
        // relay advertising a TLS-over-TCP bridge (the base node sorts first). Gated on a NON-loopback
        // bind: a same-host / loopback node (unit tests, same-machine pairing) has no relay to climb to,
        // and binding the TCP carrier would eagerly TCP-connect to the relay VPS — so loopback stays
        // single-carrier UDP (byte-identical to the proven same-host path). A networked node (the real
        // UDP-hostile / open-net case) gets the dual carrier. With `OMNI_RELAY_TCP` set on a loopback node the
        // env path inside `bind()` still applies (STEP 2a). See docs/decisions/connection-quality-ladder.md.
        // Come back on the previous run's QUIC port, so the address peers remember stays alive across
        // a restart (`MeshConfig::preferred_quic_port`). Networked binds only: a loopback mesh is a
        // same-host test or pairing and has nobody remembering it across runs.
        let db_path_string = self.storage.db_path().to_string_lossy().to_string();
        let networked = !mesh_config.quic_bind.ip().is_loopback();
        if networked {
            mesh_config.preferred_quic_port = load_remembered_port(&db_path_string, QUIC_PORT_FILE);
            mesh_config.preferred_direct_tcp_port =
                load_remembered_port(&db_path_string, DOOR_PORT_FILE);
        }
        if networked {
            // Never ourselves: a friend's relay bundle that lists US as a relay lands in our own
            // registry too, and our own door as the "relay" cost every start a 10 s connect timeout
            // (the door is not listening yet) and made each carrier switch restart the mesh for
            // nothing (stand, 2026-10-17).
            let own_node = *omni_mesh_runtime::node_address_from_ed25519_pubkey(
                ed25519_dalek::SigningKey::from_bytes(&mesh_config.signing_key_bytes)
                    .verifying_key()
                    .to_bytes(),
            )
            .map_err(|error| OmniError::Internal(format!("own mesh address: {error}")))?
            .as_bytes();
            mesh_config.relay_tcp_endpoint = relay_tcp_endpoint_of(
                &self.storage.list_network_nodes().unwrap_or_default(),
                &own_node,
            );
        }
        let mesh_config_fixed_door = mesh_config.direct_tcp_bind_port;
        let runtime = Arc::new(
            MeshRuntime::start(mesh_config, callbacks)
                .map_err(|error| OmniError::Transport(format!("mesh start failed: {error}")))?,
        );
        if networked {
            save_remembered_port(&db_path_string, QUIC_PORT_FILE, runtime.quic_addr().port());
            // Only a door on a port of its own choosing: a FIXED port is the user's setting already.
            if mesh_config_fixed_door.is_none() {
                if let Some(port) = runtime.direct_tcp_port() {
                    save_remembered_port(&db_path_string, DOOR_PORT_FILE, port);
                }
            }
        }
        // Apply the persisted connection-privacy choice to the live runtime (follow-up d): `RelayOnly`
        // forbids the relay→direct upgrade — the UDP hole-punch AND the rendezvous direct-dial — so a
        // contact can't learn our IP. The sidecar is the source of truth, re-read on every start;
        // `set_connection_privacy_mode` flips it live thereafter. See docs/decisions/connection-privacy-modes.md.
        crate::app_pinned_relay::apply_direct_gate(&self.storage, &runtime, &db_path_string);
        // Friend-proxy (Axis 2): re-seed the runtime's in-memory per-friend consent set from the
        // persisted per-contact flags (the runtime set is per-run; storage is the source of truth).
        for node in friend_relay_consented_nodes {
            runtime.consent_to_relay_for(node);
        }
        // Restore store-and-forward durable frames persisted before the last stop,
        // so frames held for offline peers survive a restart (best-effort: a
        // storage error just means we start with an empty store). P4 durable.
        let persisted = self.storage.load_durable_frames().unwrap_or_default();
        runtime.import_durable_rows(persisted);
        // Restore the persisted node↔dialog routes into the in-memory dialog map so
        // a durable send to a peer paired in an earlier run can resolve its
        // NodeAddress after a restart (the map is otherwise empty until a fresh
        // link, which the never-online-together case never gets). Mirror of the
        // receive-side bridge. See docs/decisions/mesh-durable-delivery.md.
        if let Ok(mut map) = self.mesh_dialogs.lock() {
            for (node_bytes, dialog_key) in self.storage.load_mesh_dialog_routes().unwrap_or_default()
            {
                if let Ok(raw) = <[u8; 20]>::try_from(node_bytes.as_slice()) {
                    map.insert(dialog_key, MeshNodeAddress::from_bytes(raw));
                }
            }
        }

        // Warm rejoin (G3): re-dial last-good DIRECT endpoints from the persisted cache right away,
        // so a restart reconnects to known peers without first waiting on LAN announce / UPnP /
        // rendezvous. Endpoints are hints — the ed25519 dial challenge is the authority, so a stale
        // one just wastes a dial; `dial_at` is a no-op if already linked. Load on this thread (cheap
        // SQLite read), dial off it (`dial_at` blocks on the mesh executor). See omni-network-model.md.
        let cached_peers = self
            .storage
            .load_mesh_peer_book(current_unix_ms())
            .unwrap_or_default();
        if !cached_peers.is_empty() {
            let warm_runtime = Arc::clone(&runtime);
            let _ = std::thread::Builder::new()
                .name("omni-mesh-warm-rejoin".to_string())
                .spawn(move || {
                    for (node_bytes, endpoint) in cached_peers {
                        let Ok(raw) = <[u8; 20]>::try_from(node_bytes.as_slice()) else {
                            continue;
                        };
                        let node = MeshNodeAddress::from_bytes(raw);
                        // A socket address; anything else (an `iroh:` string a spike build wrote,
                        // docs/decisions/iroh-connectivity-evaluation.md) is skipped.
                        if let Ok(addr) = endpoint.parse::<std::net::SocketAddr>() {
                            let _ = warm_runtime.dial_at(node, vec![addr]);
                        }
                    }
                });
        }

        // Always-connect step 5: dial the user-managed network-node registry (base + manual relays /
        // bootstrap entries that carry a PINNED identity) so they come up as entry points right after
        // start. Endpoint-only entries (no pinned node) are skipped until anonymous learn-on-connect
        // lands. Off-thread (dial_at blocks on the mesh executor) + no-op if already linked; endpoints
        // are hints, the dial challenge is the authority. See docs/notes/roadmap-CONTINUE.md.
        let registry_nodes = self.storage.list_network_nodes().unwrap_or_default();
        if !registry_nodes.is_empty() {
            let relay_runtime = Arc::clone(&runtime);
            let _ = std::thread::Builder::new()
                .name("omni-mesh-relay-dial".to_string())
                .spawn(move || {
                    for node in registry_nodes {
                        let (Some(endpoint), Some(node_bytes)) = (node.endpoint, node.node_address)
                        else {
                            continue; // hint-only entry (no pinned identity) — skip for now
                        };
                        let (Ok(raw), Ok(addr)) = (
                            <[u8; 20]>::try_from(node_bytes.as_slice()),
                            endpoint.parse::<std::net::SocketAddr>(),
                        ) else {
                            continue;
                        };
                        // R1a: dual-carrier (TCP base + additive UDP A↔R link) when this relay is the
                        // bound rung-0 TCP anchor, else single-carrier UDP. See connection-quality-ladder.md.
                        let tcp_addr = node
                            .tcp_endpoint
                            .and_then(|t| t.parse::<std::net::SocketAddr>().ok());
                        let _ = relay_runtime.autoconnect_network_node(
                            MeshNodeAddress::from_bytes(raw),
                            addr,
                            tcp_addr,
                        );
                    }
                });
        }

        *self.mesh_lock()? = Some(runtime);
        if let Ok(mut mode) = self.mesh_start_mode.lock() {
            *mode = Some((autoconnect, prefer_lan));
        }

        // Install the mesh carrier so outbound wire-envelopes for a mesh-linked
        // dialog ride the Link; the transport falls back to legacy otherwise.
        self.transport.set_peer_carrier(Arc::new(MeshPeerCarrier {
            mesh: Arc::clone(&self.mesh),
            dialogs: Arc::clone(&self.mesh_dialogs),
            db_path: self.storage.db_path().to_string_lossy().to_string(),
        }));

        // Self-organizing reachability: keep dialing approved contacts until each
        // one's Link is up, so a chat/call rides the mesh without an om3 token.
        if autoconnect {
            // Re-arm any outstanding bootstrap invites persisted before a restart, so the autoconnect
            // worker's bootstrap sweep resumes first-contact rendezvous without re-pairing. See
            // docs/bugs/relay-less-first-contact-validate-before-advertise.md.
            self.reload_persisted_bootstrap_invites();
            self.spawn_mesh_autoconnect();
        }

        // Group subscriptions (G3): re-register every known group's synthetic
        // destination so inbound group frames deliver and our sends gossip to
        // co-members. Done after the runtime is installed so the facade exists.
        // Groups that lost their rows come back first (docs/bugs/old-build-wiped-groups.md).
        crate::app_groups::restore_groups_without_rows(&self.storage);
        self.resubscribe_all_groups();
        omni_app_trace_log(
            "mesh/start",
            &format!("ready in {}ms (autoconnect={autoconnect})", started_at.elapsed().as_millis()),
        );
        Ok(())
    }

    /// Stop the mesh overlay and forget all dialog mappings.
    pub fn stop_mesh(&self) {
        // Drop the auto-connect sender first so its worker stops dialing before
        // we tear the runtime down (its next `recv` returns `Disconnected`).
        if let Ok(mut stop) = self.mesh_autoconnect_stop.lock() {
            *stop = None;
        }
        if let Ok(mut guard) = self.mesh.lock() {
            if let Some(runtime) = guard.take() {
                // Snapshot the store-and-forward frames we hold for others so they
                // survive the stop/restart (best-effort). P4 durable persistence.
                let rows = runtime.export_durable_rows();
                let _ = self.storage.replace_durable_frames(&rows);
                runtime.shutdown();
            }
        }
        if let Ok(mut map) = self.mesh_dialogs.lock() {
            *map = MeshDialogMap::default();
        }
        if let Ok(mut ctxs) = self.mesh_node_ctx.lock() {
            ctxs.clear();
        }
        if let Ok(mut dialed) = self.mesh_locally_dialed.lock() {
            dialed.clear();
        }
        // Kill the owned rendezvous helper too: it only serves the mesh's autoconnect
        // sweep, and it respawns on demand (spawn-once-reuse), so dropping it here keeps
        // it from outliving the mesh — e.g. orphaning on a headless `shutdown`, which
        // tears down via `stop_mesh` rather than `stop_runtime_context`.
        clear_rendezvous_runtime_for_profile(&self.storage.db_path().to_string_lossy());
    }

    /// Our mesh NodeAddress as a hex string (for diagnostics / FFI). None if
    /// the mesh isn't running.
    pub fn mesh_node_address_hex(&self) -> OmniResult<Option<String>> {
        Ok(self
            .mesh_lock()?
            .as_ref()
            .map(|runtime| runtime.node_address().to_string()))
    }

    /// The public external endpoint of our UPnP-IGD port mapping (`host:port`), once
    /// established — the address a peer behind a different NAT can dial. None if the mesh
    /// isn't running, the bind is loopback, or the router didn't yield a public mapping
    /// (no IGD / CGNAT). Diagnostics for the cross-NAT reachability path.
    pub fn mesh_upnp_endpoint(&self) -> Option<String> {
        self.mesh_runtime()
            .and_then(|runtime| runtime.mapped_external_endpoint())
            .map(|endpoint| endpoint.to_string())
    }

    /// Task 3 — our UPnP-mapped direct-TCP front door endpoint (the one we advertise in the beacon
    /// `tcp_direct` so a peer on a UDP-hostile carrier dials us over TCP-direct), or None if not
    /// mapped. Diagnostics for the TCP-direct fallback path. See tcp-direct-peer-fallback.md.
    pub fn mesh_direct_tcp_endpoint(&self) -> Option<String> {
        self.mesh_runtime()
            .and_then(|runtime| runtime.direct_tcp_endpoint())
            .map(|endpoint| endpoint.to_string())
    }

    /// This host's primary global (internet-routable) IPv6 address, or None if it is
    /// IPv4-only. A pure host probe — works WITHOUT the mesh running, so it can be checked
    /// on any network to answer "will two of these connect directly?": two peers each with
    /// a global IPv6 dial each other with NO NAT and NO relay (most mobile carriers are
    /// dual-stack / IPv6-only). This reports REACHABILITY-CANDIDACY; actually LISTENING on
    /// it (the dual-stack bind) is the next step. See docs/decisions/mesh-network-binding.md.
    pub fn mesh_global_ipv6(&self) -> Option<String> {
        detect_global_ipv6().map(|ip| ip.to_string())
    }

    /// Honest connection-state DTO: this node's connectivity + each contact's link-state (with an
    /// honest failure reason) + the orthogonal `verified` flag. Pure read-model over signals that
    /// already exist (mesh lifecycle, AutoNAT reachability, UPnP/IPv6 endpoints, `has_link` +
    /// `peer_link_quality`, privacy mode, contact relationship/has_met) — no wire/relay change. The
    /// shell renders this and never derives, replacing the old prose reachability summary
    /// (Rust = truth, C# = render). See docs/decisions/omni-network-model.md.
    pub fn mesh_connection_state(&self) -> OmniResult<MeshConnectionState> {
        let Some(runtime) = self.mesh_runtime() else {
            return Ok(MeshConnectionState {
                node: NodeConnectivityState {
                    kind: NodeConnectivityKind::NotStarted,
                    endpoints: Vec::new(),
                },
                contacts: Vec::new(),
            });
        };

        // Node-state: a public endpoint (UPnP / global IPv6) means directly dialable; otherwise
        // fold the AutoNAT verdict (Reachable / Unreachable / still-probing). Validate-before-claim
        // (consistent with `beacon_reflexive` / Phase B): a UPnP mapping counts as a "directly
        // reachable" endpoint only once AutoNAT confirmed it actually forwards — otherwise it is a
        // phantom the UI must not promise, and it would disagree with what we actually advertise to a
        // peer. So a node behind a router that merely *reports* a mapping shows "behind NAT", honestly.
        // (Global IPv6 has no NAT; validating it is a follow-up.) См. connectivity-upgrade-via-peer-relay.md.
        let mut endpoints = Vec::new();
        if let (Some(upnp), Some(confirmed)) =
            (runtime.mapped_external_endpoint(), runtime.confirmed_reachable_endpoint())
        {
            if upnp == confirmed {
                endpoints.push(upnp.to_string());
            }
        }
        if let Some(v6) = runtime.advertised_ipv6_endpoint() {
            endpoints.push(v6.to_string());
        }
        let node_kind = if !endpoints.is_empty() {
            NodeConnectivityKind::OnlineReachable
        } else {
            match runtime.reachability_state() {
                MeshReachabilityState::Reachable => NodeConnectivityKind::OnlineReachable,
                MeshReachabilityState::Unreachable => NodeConnectivityKind::OnlineBehindNat,
                MeshReachabilityState::Unknown => NodeConnectivityKind::Connecting,
            }
        };
        let node = NodeConnectivityState {
            kind: node_kind,
            endpoints,
        };

        // Per-contact link-state. `verified` (has_met) is orthogonal to the transport link.
        let uses_rendezvous = self.connection_privacy_mode().uses_rendezvous();
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mut contacts = Vec::new();
        for contact in self.storage.list_contacts()? {
            let Some(account_id) = contact.remote_account_id.clone() else {
                continue; // no remote account → nothing to address
            };
            let verified = contact.has_met;
            // (link kind, relay hops if relayed, reason if unreachable).
            // (link kind, relay hops if relayed, reason if unreachable, carrier if linked).
            let (link, relay_hops, cannot_reach_reason, carrier) = if contact.relationship_state
                != omni_identity::ContactRelationshipState::Approved
            {
                (
                    ContactLinkKind::CannotReach,
                    None,
                    Some(CannotReachReason::NotApproved),
                    None,
                )
            } else {
                match self.mesh_node_for_account(&account_id)? {
                    // No stored prekey bundle yet → can't derive a node to dial.
                    None => (
                        ContactLinkKind::CannotReach,
                        None,
                        Some(CannotReachReason::PeerOffline),
                        None,
                    ),
                    Some(node_addr) => {
                        // Здесь наконец применяются оба правила вердикта (шаг 3 ADR
                        // reachability-belongs-to-the-session): на гейте отправки они не нужны — там
                        // вопрос «в какую трубу класть», — а вот интерфейсу нужен ответ «дойдёт ли».
                        let now_unix_ms = current_unix_ms();
                        let proven_dead = session_outbound_leg_is_proven_dead_for_remote_account(
                            &db_path,
                            &account_id.0,
                            now_unix_ms,
                        );
                        let acknowledged_recently =
                            session_acknowledgement_age_for_remote_account(
                                &db_path,
                                &account_id.0,
                                now_unix_ms,
                            )
                            .is_some_and(|age_ms| age_ms < SESSION_ACK_FRESH_MS);
                        if proven_dead {
                            // Несущая может сколько угодно видеть путь — пир не отвечает на наши
                            // пробы дольше порога. Рисовать «Онлайн» поверх этого и значит показывать
                            // «В сети» рядом с сообщением, застрявшим на «ждём подтверждения».
                            // Durable держит конверт, поэтому честное состояние — «доставлю позже».
                            (ContactLinkKind::WillDeliverLater, None, None, None)
                        } else if runtime.has_link(&node_addr) {
                            // The honest carrier of the best path — QUIC vs the TCP fallback vs LAN.
                            let carrier = LinkCarrier::observe(&runtime, &node_addr);
                            match runtime.peer_link_quality(&node_addr) {
                                Some(MeshLinkQuality::Relay { hops }) => {
                                    (ContactLinkKind::OnlineViaRelay, Some(hops), None, carrier)
                                }
                                Some(MeshLinkQuality::Direct) => {
                                    (ContactLinkKind::OnlineDirect, None, None, carrier)
                                }
                                // `None` = no proven-reachable path (e.g. a relay peer whose end-to-end
                                // liveness went stale between prune ticks). `has_link` already gates on
                                // `peer_link_quality.is_some()`, so reaching here means not-reachable —
                                // report offline, NOT optimistically "direct". See
                                // docs/bugs/relay-presence-false-online-dead-peer.md.
                                None => (
                                    ContactLinkKind::CannotReach,
                                    None,
                                    Some(CannotReachReason::PeerOffline),
                                    None,
                                ),
                            }
                        } else if acknowledged_recently {
                            // Обратное правило: доказательство сильнее способности. Пир ответил нам
                            // только что, значит связь есть — несущая просто не успела перерегистрировать
                            // путь. Печатать в этот момент «нужен relay» и было тем противоречием на
                            // одном экране, с которого начался разбор. Несущую не знаем, поэтому не
                            // врём про «напрямую», а честно показываем, что путь пересобирается.
                            (ContactLinkKind::Searching, None, None, None)
                        } else if !uses_rendezvous {
                            // Direct-only: we won't touch a provider to reach them.
                            (
                                ContactLinkKind::CannotReach,
                                None,
                                Some(CannotReachReason::RendezvousDisabled),
                                None,
                            )
                        } else if rendezvous_peers_for_account(&self.storage, &account_id)
                            .iter()
                            .any(|peer| peer.node == node_addr)
                        {
                            // Phase C honest surface: if a direct link is genuinely impossible (both
                            // ends symmetric AND neither advertises a forward or a TCP door), an endless
                            // "searching" is a lie. If no relay is currently reachable to backstop the
                            // pair either, surface `NoRouteBothUnreachable` honestly instead. With a
                            // reachable relay we stay `Searching` (the relay route can still converge).
                            // The class alone is NOT enough — a forward on either side is reachable
                            // outbound from behind any NAT; see app_nat_class::direct_is_impossible_for_pair.
                            // См. docs/decisions/connectivity-upgrade-via-peer-relay.md (Phase C).
                            if crate::app_nat_class::direct_is_impossible_for_pair(&db_path, node_addr.as_bytes())
                                && !any_relay_reachable(&self.storage, &runtime)
                            {
                                (
                                    ContactLinkKind::CannotReach,
                                    None,
                                    Some(CannotReachReason::NoRouteBothUnreachable),
                                    None,
                                )
                            } else {
                                // Approved + resolvable + rendezvous-eligible → actively being reached.
                                (ContactLinkKind::Searching, None, None, None)
                            }
                        } else {
                            // No pair secret yet (pre-rendezvous session) → nothing to dial through.
                            (
                                ContactLinkKind::CannotReach,
                                None,
                                Some(CannotReachReason::PeerOffline),
                                None,
                            )
                        }
                    }
                }
            };
            let trace = connection_trace_for(&db_path, &account_id.0);
            let (no_common_courier, couriers_to_enable) = contact_courier_gap(&self.storage, &account_id);
            contacts.push(ContactConnectionState {
                account_id: account_id.0,
                link,
                relay_hops,
                carrier,
                cannot_reach_reason,
                verified,
                trace,
                no_common_courier,
                couriers_to_enable,
            });
        }

        Ok(MeshConnectionState { node, contacts })
    }

    /// Manually dial a peer by its NodeAddress hex — no contact required. The
    /// headless `omni-node` uses this to build a deterministic mesh topology (e.g. a
    /// relay where A and C never link directly).
    pub fn mesh_dial(&self, node_hex: &str) -> OmniResult<()> {
        let node = parse_mesh_node_hex(node_hex)?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        runtime
            .dial(node)
            .map_err(|error| OmniError::Transport(format!("mesh dial failed: {error}")))
    }

    /// Dial `node_hex` at an explicit `host:port` endpoint (cross-NAT direct connect). The
    /// endpoint comes from a rendezvous beacon or a UPnP-mapped public address, NOT from a
    /// same-LAN announce — so it covers the case where the two nodes never share a multicast
    /// domain. Injects the endpoint into the peer book then dials (see `dial_at`). Diagnostics
    /// / the cross-network reachability proof.
    pub fn mesh_dial_at(&self, node_hex: &str, endpoint: &str) -> OmniResult<()> {
        let node = parse_mesh_node_hex(node_hex)?;
        let addr: std::net::SocketAddr = endpoint
            .parse()
            .map_err(|error| OmniError::Transport(format!("bad endpoint {endpoint}: {error}")))?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        // Bounded: a live endpoint answers in well under a second even across the internet, and an
        // unbounded dial whose first packets were lost sat out the whole handshake timeout — a
        // caller polling "dial, then check the link" got one look in 20 s (two gate flakes,
        // 2026-10-13). The caller retries.
        runtime
            .dial_at_within(node, vec![addr], std::time::Duration::from_secs(5))
            .map_err(|error| OmniError::Transport(format!("mesh dial_at failed: {error}")))
    }

    /// Dial `node_hex` over the direct-TCP door at an explicit `host:port` (the TCP analogue of
    /// [`mesh_dial_at`]). Establishes a TLS-over-TCP carrier to the peer's bridge and challenges it
    /// over that carrier — the relay-less path that survives a UDP-hostile carrier. Diagnostics /
    /// headless transport-reachability harness (isolates the QUIC-over-TCP path from rendezvous).
    pub fn mesh_dial_tcp_direct(&self, node_hex: &str, endpoint: &str) -> OmniResult<()> {
        let node = parse_mesh_node_hex(node_hex)?;
        let addr: std::net::SocketAddr = endpoint.parse().map_err(|error| {
            OmniError::Transport(format!("bad tcp endpoint {endpoint}: {error}"))
        })?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        // Surface "nothing to migrate" as an error: this is the harness's `dial-tcp`, and it used to
        // answer `OK` while no connection existed — which sent a live debugging session chasing the
        // wrong layer. See docs/bugs/door-first-contact.md.
        match runtime.dial_peer_tcp_direct(node, vec![addr]) {
            Ok(true) => Ok(()),
            Ok(false) => Err(OmniError::Transport(format!(
                "no live connection to {node_hex} to route over its door — the door migrates an \
                 existing connection, it cannot make first contact"
            ))),
            Err(error) => Err(OmniError::Transport(format!(
                "mesh dial_tcp_direct failed: {error}"
            ))),
        }
    }

    /// Force the connection-quality-ladder FAIL-BACK for `node_hex`: demote its direct-UDP path so
    /// `best()` prefers the direct-TCP door for `UDP_DEMOTE_DURATION` (the same signal the app raises
    /// on a phantom-drop). Dev/validation ONLY — lets the headless harness DETERMINISTICALLY put the
    /// link on the TCP door (instead of waiting for the carrier to throttle a real message), so
    /// laptop→PC delivery over the door can be tested in isolation. Returns whether the peer had a
    /// path set to flag. See docs/decisions/connection-quality-ladder.md + tcp-direct-peer-fallback.md.
    pub fn mesh_demote_udp(&self, node_hex: &str) -> OmniResult<bool> {
        let node = parse_mesh_node_hex(node_hex)?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        Ok(runtime.demote_udp_direct(&node))
    }

    /// Meta-B fail-down: route `node_hex`'s single quinn connection over its direct-TCP door at
    /// `door_endpoint` — the connection MIGRATES onto the door (same connection id, no separate
    /// carrier, no re-dial). Returns `false` if the peer isn't dialed over UDP yet. See mux_socket.
    pub fn mesh_route_door(&self, node_hex: &str, door_endpoint: &str) -> OmniResult<bool> {
        let node = parse_mesh_node_hex(node_hex)?;
        let door: std::net::SocketAddr = door_endpoint.parse().map_err(|error| {
            OmniError::Transport(format!("bad door endpoint {door_endpoint}: {error}"))
        })?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        runtime
            .route_peer_over_door(&node, door)
            .map_err(|error| OmniError::Transport(format!("route_peer_over_door failed: {error}")))
    }

    /// Meta-B optimistic upgrade: route `node_hex` back over the UDP base (migrates back, drops the
    /// door tunnel). Returns whether a direct-UDP path was found to switch.
    pub fn mesh_route_udp(&self, node_hex: &str) -> OmniResult<bool> {
        let node = parse_mesh_node_hex(node_hex)?;
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        Ok(runtime.route_peer_over_udp(&node))
    }

    /// The RAW UPnP/NAT-PMP-mapped direct-TCP door endpoint a peer would dial (`None` if unmapped /
    /// CGNAT). This is the front door BEFORE any AutoNAT confirmation — exactly what a UDP-hostile
    /// peer needs relay-less. Exposed for the headless reachability harness so the dialer learns the
    /// door without going through rendezvous. See docs/decisions/tcp-direct-peer-fallback.md.
    pub fn mesh_mapped_tcp_door(&self) -> Option<String> {
        self.mesh_runtime()
            .and_then(|runtime| runtime.mapped_tcp_external_endpoint())
            .map(|endpoint| endpoint.to_string())
    }

    /// Export a "код связи" reach token (`om3reach:…`) — my mesh node + the endpoints a peer can dial
    /// me at right now (router-mapped public UPnP/NAT-PMP, native global IPv6, and the LAN bind). The
    /// no-third-party reach: a user copies it and sends it over any channel they trust; the friend
    /// imports it with [`OmniApp::dial_reach_token`]. EPHEMERAL — re-export when the address changes.
    pub fn generate_reach_token(&self) -> OmniResult<String> {
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        let mut endpoints: Vec<String> = Vec::new();
        let mut push = |endpoint: String| {
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        };
        if let Some(mapped) = runtime.mapped_external_endpoint() {
            push(mapped.to_string());
        }
        if let Some(v6) = runtime.advertised_ipv6_endpoint() {
            push(v6.to_string());
        }
        let quic = runtime.quic_addr();
        if !quic.ip().is_loopback() && !quic.ip().is_unspecified() {
            push(quic.to_string());
        }
        if endpoints.is_empty() {
            return Err(OmniError::Transport(
                "no reachable endpoint to share — start the mesh on a network first".to_string(),
            ));
        }
        encode_reach_token(&MeshReachTokenV1 {
            version: 1,
            node_hex: runtime.node_address().to_string(),
            endpoints,
            exported_at_unix_ms: current_unix_ms(),
        })
    }

    /// This node's LOCAL transport (QUIC) bind address as `ip:port`. The PORT is what a peer on the
    /// same segment dials to reach my listener — the same socket `generate_reach_token` shares. The IP
    /// is usually unspecified (`0.0.0.0`), so the GUI composes `<LAN IPv4>:<port>` from the interface
    /// inventory for a local-segment "add me as an entry node" address. Errors if the mesh isn't up.
    pub fn local_transport_endpoint(&self) -> OmniResult<String> {
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        Ok(runtime.quic_addr().to_string())
    }

    /// Import a friend's "код связи" reach token and dial them at the carried endpoints (manual /
    /// Direct-only reach to an already-paired contact). Returns the resolved NodeAddress. Pairs with
    /// [`OmniApp::generate_reach_token`].
    pub fn dial_reach_token(&self, token: &str) -> OmniResult<MeshNodeAddress> {
        let parsed = parse_reach_token(token)?;
        let node = parse_mesh_node_hex(&parsed.node_hex)?;
        let endpoints: Vec<std::net::SocketAddr> =
            parsed.endpoints.iter().filter_map(|s| s.parse().ok()).collect();
        if endpoints.is_empty() {
            return Err(OmniError::InvalidInput(
                "reach token carries no valid endpoints".to_string(),
            ));
        }
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh is not started".to_string()))?;
        runtime
            .dial_at(node, endpoints)
            .map_err(|error| OmniError::Transport(format!("reach token dial failed: {error}")))?;
        Ok(node)
    }

    /// Whether there is a live mesh Link to `node_hex`.
    pub fn mesh_has_link(&self, node_hex: &str) -> OmniResult<bool> {
        let node = parse_mesh_node_hex(node_hex)?;
        Ok(self
            .mesh_runtime()
            .map(|runtime| runtime.has_link(&node))
            .unwrap_or(false))
    }

    /// Diagnostic: the current best-path quality to `node_hex` — `direct`, `relay:<hops>`,
    /// or `none` (no link / mesh down). Surfaces `peer_link_quality` so the R1a
    /// connection-quality-ladder climb can be verified live (did we reach a direct rung,
    /// or are we still relayed?). Read-only.
    /// Применить смену контроллера перегрузки к живому мешу и сказать, сколько соединений
    /// пришлось закрыть. Ноль — меша нет или соединений не было; это НЕ «применено к линку».
    ///
    /// Оболочка зовёт это вместо перезапуска меша: перезапуск сносит дискавери, анонсы и проброс
    /// порта и стоит больше минуты без связи (замер 2026-08-31, `link=searching` > 60 с).
    pub fn apply_congestion_control_now(&self) -> OmniResult<usize> {
        let Some(runtime) = self.mesh_runtime() else {
            return Ok(0);
        };
        runtime
            .refresh_congestion_control()
            .map_err(|error| OmniError::Internal(format!("refresh congestion control: {error}")))
    }

    pub fn mesh_link_quality(&self, node_hex: &str) -> OmniResult<String> {
        let node = parse_mesh_node_hex(node_hex)?;
        let Some(runtime) = self.mesh_runtime() else {
            return Ok("none".to_string());
        };
        let Some(quality) = runtime.peer_link_quality(&node) else {
            return Ok("none".to_string());
        };
        // The carrier distinguishes rung 0 (TCP-relay) from rung 1 (UDP-relay) — both `Relay{hops:1}`.
        // A `QuicOverUdp` carrier whose Meta-B mux is routed over the peer's TCP door reads as `door`,
        // NOT `udp` — otherwise the readout lies (`direct:udp` while the bytes ride TCP). See
        // docs/bugs/relay-less-mobile-transport-findings.md §RESOLVED follow-ups.
        let carrier = LinkCarrier::observe(&runtime, &node).map_or("?", LinkCarrier::wire_name);
        Ok(match quality {
            MeshLinkQuality::Direct => format!("direct:{carrier}"),
            MeshLinkQuality::Relay { hops } => format!("relay:{hops}:{carrier}"),
        })
    }

    /// Number of durable store-and-forward frames currently held (for relay /
    /// diagnostics — a relay that picked up a frame for another node reports > 0).
    pub fn mesh_durable_count(&self) -> usize {
        self.mesh_runtime()
            .map(|runtime| runtime.export_durable_rows().len())
            .unwrap_or(0)
    }

    /// Drain buffered core events (inbound messages, link state, …) recorded since
    /// the last call — the core's event hook for the daemon's event plane (push, not
    /// poll). One drain for every kind. See docs/decisions/daemon-ipc-phase2.md.
    pub fn drain_events(&self) -> Vec<CoreEvent> {
        drain_core_events(&self.storage.db_path().to_string_lossy())
    }

    /// Dial a contact over the mesh and bind it to `dialog_key`, so subsequent
    /// realtime-media for that dialog routes over the verified Link. The peer's
    /// Ed25519 identity *signing* public key (32 bytes — the
    /// `identity_signing_public_key` from their prekey bundle) yields their
    /// NodeAddress. Requires `start_mesh` first and that the peer has been
    /// discovered (announced) on the mesh.
    pub fn mesh_connect_dialog(
        &self,
        dialog_key: &str,
        peer_signing_pubkey: [u8; 32],
    ) -> OmniResult<()> {
        let node = node_address_from_ed25519_pubkey(peer_signing_pubkey)
            .map_err(|error| OmniError::InvalidInput(format!("invalid peer mesh key: {error}")))?;
        // Record that we initiated this Link, so the peer's hello is registered
        // with initiated_locally = true.
        if let Ok(mut dialed) = self.mesh_locally_dialed.lock() {
            dialed.insert(node);
        }
        // Dial off the start/stop lock (it can block up to the challenge timeout)
        // so a slow dial never stalls the live media/wire send path.
        let runtime = self
            .mesh_runtime()
            .ok_or_else(|| OmniError::Transport("mesh not started".to_string()))?;
        runtime
            .dial(node)
            .map_err(|error| OmniError::Transport(format!("mesh dial failed: {error}")))?;
        self.mesh_dialogs
            .lock()
            .map_err(|_| OmniError::Internal("mesh dialog map lock is poisoned".to_string()))?
            .insert(dialog_key.to_string(), node);
        Ok(())
    }

    /// Dial the peer of the **currently active call** over the mesh and bind
    /// it to that call's dialog. Resolves the peer's Ed25519 identity signing
    /// key from the secure session record (no params needed from the shell).
    /// Both peers call this on call-connect so each side's dialog↔node map is
    /// populated — required for bidirectional media routing.
    pub fn mesh_connect_active_call(&self) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let runtime = current_call_runtime(&db_path)?
            .ok_or_else(|| OmniError::InvalidInput("there is no active call".to_string()))?;
        let dialog_key = runtime.summary.dialog_key.clone();
        let session_address = omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id,
            remote_device_id: runtime.remote_device_id,
        };
        let session = self.storage.get_session_record(&session_address)?.ok_or_else(|| {
            OmniError::InvalidInput("no secure session for the active call".to_string())
        })?;
        let peer_pubkey = session.remote_identity_signing_key.0;

        // Fast-fail if the mesh isn't running (don't burn the retry window on a
        // misconfiguration); otherwise the only retryable failure is "peer not
        // discovered yet", which the bounded loop below absorbs.
        if self.mesh_lock()?.is_none() {
            return Err(OmniError::Transport(
                "mesh is not started; call start_mesh after sign-in".to_string(),
            ));
        }

        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(MESH_DIAL_DISCOVERY_TIMEOUT_SECS);
        loop {
            match self.mesh_connect_dialog(&dialog_key, peer_pubkey) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(
                        MESH_DIAL_RETRY_INTERVAL_MS,
                    ));
                }
            }
        }
    }

    /// The peer's mesh node for `dialog_key` — an identity fact, independent of whether a Link is
    /// up right now. `Ok(None)` when the dialog has no resolvable approved contact with a known
    /// signing key. Resolved once per call (`realtime_media_route`), so the per-packet media send
    /// never reaches storage.
    pub(crate) fn mesh_node_for_dialog(&self, dialog_key: &str) -> OmniResult<Option<MeshNodeAddress>> {
        // Fast path: the dialog-key was mapped directly when we received the
        // peer's SessionHello (keyed by `dialog_key_from_instance_id`).
        let mapped = self
            .mesh_dialogs
            .lock()
            .map_err(|_| OmniError::Internal("mesh dialog map lock is poisoned".to_string()))?
            .node_for_dialog(dialog_key);
        if mapped.is_some() {
            return Ok(mapped);
        }
        // Fallback: a call addresses its peer by the *canonical* dialog-key (which can be
        // account-based or an explicit alias), not necessarily the instance-based key the
        // SessionHello populated. Resolve the peer's mesh node from the contact's stored identity
        // so media still rides the mesh instead of silently dropping to the legacy transport, and
        // cache the alias (outbound-only).
        let Some(resolved) = self.mesh_node_for_contact_dialog(dialog_key)? else {
            return Ok(None);
        };
        if let Ok(mut map) = self.mesh_dialogs.lock() {
            map.insert_dialog_alias(dialog_key.to_string(), resolved);
        }
        Ok(Some(resolved))
    }

    /// Resolve the mesh [`MeshNodeAddress`] for the contact behind `dialog_key`
    /// from its stored identity — the Ed25519 signing key in its latest prekey
    /// bundle, the same key both sides derive the address from (mirrors the
    /// auto-connect worker). Returns `Ok(None)` when the dialog has no resolvable
    /// approved contact with a known signing key. Dialog-key-format agnostic:
    /// `resolve_contact_card_for_dialog` canonicalises aliases first.
    pub(crate) fn mesh_node_for_contact_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Option<MeshNodeAddress>> {
        let Some(card) = self.storage.resolve_contact_card_for_dialog(dialog_key)? else {
            return Ok(None);
        };
        let Some(remote_account_id) = card.contact.remote_account_id.as_ref() else {
            return Ok(None);
        };
        let bundle = match self
            .storage
            .get_latest_remote_prekey_bundle_for_account(remote_account_id)
        {
            Ok(Some(bundle)) => bundle,
            Ok(None) | Err(_) => return Ok(None),
        };
        Ok(node_address_from_ed25519_pubkey(bundle.identity_signing_key.0).ok())
    }

    /// Generate a copy-pasteable mesh pairing invite carrying our instance id +
    /// Ed25519 signing pubkey, so a friend can dial + verify us over the mesh
    /// and pair without any legacy connection. See
    /// docs/decisions/pairing-over-mesh-adoption.md.
    pub fn generate_mesh_pairing_invite(&self) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("generate_mesh_pairing_invite")?;
        let source_instance_id = self.storage.get_or_create_local_instance_id()?;
        let bundle = self.get_local_prekey_bundle()?;
        let now = current_unix_ms();
        // Mint a random bootstrap nonce so a friend on ANOTHER network can find us at first
        // contact (no X3DH session yet → no durable pair secret to meet on). Both sides derive
        // the same blinded bootstrap topic from it; we start publishing our endpoints there now.
        let nonce: [u8; 32] = omni_crypto::random_bytes(32)?
            .try_into()
            .map_err(|_| OmniError::Internal("bootstrap nonce must be 32 bytes".to_string()))?;
        let bootstrap_nonce_hex = hex::encode(nonce);
        let secret = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce)?;
        self.register_bootstrap_invite(&bootstrap_nonce_hex, secret, None, now + BOOTSTRAP_WINDOW_MS);
        let token = encode_mesh_pairing_invite(&omni_types::MeshPairingInviteV1 {
            version: omni_types::MESH_PAIRING_INVITE_VERSION_V1,
            source_instance_id,
            signing_pubkey_hex: bundle.account.identity_signing_public_key,
            bootstrap_nonce_hex,
            // The friend gets where we can be met — URLs only, how we use them is our own business,
            // and a courier we only know from another contact is not ours to hand on.
            rendezvous_providers: self
                .configured_rendezvous_providers()
                .iter()
                .map(|entry| omni_rendezvous::parse_courier_entry(entry))
                .filter(|(_, courier_use)| *courier_use != omni_rendezvous::CourierUse::PairOnly)
                .map(|(url, _)| url.to_string())
                .collect(),
            dht_bootstrap_nodes: self.dht_bootstrap_nodes(),
            exported_at_unix_ms: now,
            expires_at_unix_ms: now + MESH_PAIRING_INVITE_TTL_MS,
        })?;
        // Make ourselves findable immediately (publish a bootstrap beacon now), not only on the
        // next worker sweep — best-effort, a no-op until the mesh is started.
        let _ = self.spawn_one_bootstrap_dial(secret);
        Ok(token)
    }

    /// Import a friend's mesh pairing invite: derive their mesh NodeAddress from
    /// the carried signing pubkey and register it as a pairing target. The
    /// auto-connect worker then dials it, and the on_wire SessionHello handler
    /// finishes the pairing (creates the outgoing-pending contact + sends the
    /// contact request over the mesh). Returns the resolved NodeAddress.
    pub fn register_mesh_pairing_invite(&self, token: &str) -> OmniResult<MeshNodeAddress> {
        let _ = self.ensure_runtime_profile_active("register_mesh_pairing_invite")?;
        let invite = parse_mesh_pairing_invite(token)?;
        if invite.expires_at_unix_ms <= current_unix_ms() {
            return Err(OmniError::InvalidInput(
                "mesh pairing invite has expired".to_string(),
            ));
        }
        // Adopt the inviter's courier set (best-effort) so first contact converges on a working SHARED set
        // even if our compiled default is blocked here — takes effect on the next helper spawn (step 2c).
        self.merge_invite_rendezvous_providers(&invite.rendezvous_providers);
        // Adopt the inviter's custom DHT bootstrap nodes too (isolated-segment convergence) — sealed
        // per-pair, first-party only (R1b): the friend now has working DHT entry points for the segment.
        self.merge_invite_dht_bootstrap_nodes(&invite.dht_bootstrap_nodes);
        let pubkey_bytes: [u8; 32] = hex::decode(invite.signing_pubkey_hex.trim())
            .map_err(|error| {
                OmniError::InvalidInput(format!("invite signing pubkey is not valid hex: {error}"))
            })?
            .try_into()
            .map_err(|_| {
                OmniError::InvalidInput("invite signing pubkey must be 32 bytes".to_string())
            })?;
        let node = node_address_from_ed25519_pubkey(pubkey_bytes).map_err(|error| {
            OmniError::InvalidInput(format!(
                "invite signing pubkey is not a valid mesh identity: {error}"
            ))
        })?;
        self.mesh_pairing_targets
            .lock()
            .map_err(|_| OmniError::Internal("mesh pairing targets lock is poisoned".to_string()))?
            .insert(node);
        // If the invite carries a bootstrap nonce, seed the first-contact rendezvous so we can
        // reach the inviter even on another network (no shared LAN, no session yet). Best-effort:
        // a malformed nonce just falls back to announce/manual pairing — it doesn't fail the import.
        if let Some(nonce) = hex::decode(invite.bootstrap_nonce_hex.trim())
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        {
            let secret = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce)?;
            let window = current_unix_ms() + BOOTSTRAP_WINDOW_MS;
            let expires = window.min(invite.expires_at_unix_ms);
            self.register_bootstrap_invite(&invite.bootstrap_nonce_hex, secret, Some(node), expires);
            // Try to reach the inviter right now, then the worker retries until the link is up.
            let _ = self.spawn_one_bootstrap_dial(secret);
        }
        Ok(node)
    }

    /// Record (or refresh) an outstanding first-contact bootstrap rendezvous entry. Best-effort: a
    /// poisoned registry lock is ignored (the worker sweep + the immediate kick both retry).
    fn register_bootstrap_invite(
        &self,
        nonce_hex: &str,
        secret: [u8; 32],
        inviter_node: Option<MeshNodeAddress>,
        expires_at_ms: i64,
    ) {
        if let Ok(mut map) = self.mesh_bootstrap_invites.lock() {
            map.insert(
                nonce_hex.to_string(),
                MeshBootstrapInvite { secret, inviter_node, expires_at_ms },
            );
        }
        // Persist (nonce, not the derived secret) so a restart mid-window re-arms this bootstrap
        // rendezvous — `reload_persisted_bootstrap_invites` on mesh-start re-derives the secret from
        // the nonce. Best-effort: a storage error just loses the restart-survival, not the live entry.
        let node_bytes = inviter_node.map(|node| *node.as_bytes());
        let _ = self.storage.upsert_bootstrap_invite(nonce_hex, node_bytes, expires_at_ms);
    }

    /// Repopulate the in-memory bootstrap-invite + pairing-target sets from storage on mesh-start, so a
    /// process restart mid-window re-arms first-contact bootstrap rendezvous (the entries were in-memory
    /// only — docs/bugs/relay-less-first-contact-validate-before-advertise.md). Prunes expired rows first,
    /// then re-derives each secret from its persisted nonce. Best-effort: a storage error leaves the
    /// in-memory sets as-is (a live session already has them; only restart-survival is affected).
    fn reload_persisted_bootstrap_invites(&self) {
        let now = current_unix_ms();
        let _ = self.storage.prune_expired_bootstrap_invites(now);
        let Ok(rows) = self.storage.list_active_bootstrap_invites(now) else {
            return;
        };
        for row in rows {
            let Some(nonce) = hex::decode(row.nonce_hex.trim())
                .ok()
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            else {
                continue; // malformed persisted nonce — skip, don't fail the reload
            };
            let Ok(secret) = omni_crypto::derive_rendezvous_bootstrap_secret(&nonce) else {
                continue;
            };
            let inviter_node = row.inviter_node.map(MeshNodeAddress::from_bytes);
            // Importer side: also re-arm the direct dial target (the inviter's node) so the LAN /
            // known-node path retries too, mirroring import_mesh_pairing_invite.
            if let Some(node) = inviter_node {
                if let Ok(mut targets) = self.mesh_pairing_targets.lock() {
                    targets.insert(node);
                }
            }
            // register_bootstrap_invite re-persists idempotently (upsert) — a harmless no-op on reload.
            self.register_bootstrap_invite(&row.nonce_hex, secret, inviter_node, row.expires_at_ms);
        }
    }

    /// Fire ONE bootstrap rendezvous dial under `secret` off the hot path (publish my endpoints to
    /// the blinded bootstrap topic + fetch/dial the other side). No-op until the mesh is started —
    /// the autoconnect worker's bootstrap sweep then keeps retrying. Best-effort.
    fn spawn_one_bootstrap_dial(&self, secret: [u8; 32]) -> OmniResult<()> {
        let Some(runtime) = self.mesh_runtime() else {
            return Ok(());
        };
        // Bootstrap rendezvous reaches a peer ACROSS networks via public providers; a loopback /
        // same-host mesh has nothing to reach that way (it pairs over loopback multicast), so skip
        // it there. This also keeps same-host unit tests (which bind loopback) off STUN/ntfy.
        if runtime.quic_addr().ip().is_loopback() {
            return Ok(());
        }
        // Honor the user's connection-privacy choice: Direct-only means no third-party rendezvous.
        if !self.connection_privacy_mode().uses_rendezvous() {
            return Ok(());
        }
        let Some(my_node) = self.mesh_node_bytes() else {
            return Ok(());
        };
        let rendezvous = self.ensure_rendezvous_runtime()?;
        // No relay bundle on a BOOTSTRAP beacon (R1c): first contact, pre-approval — see the sweep
        // note below. The bundle rides only the approved per-pair beacon.
        spawn_rendezvous_dial(rendezvous, runtime, secret, PairCouriers::default(), my_node, Vec::new(), None, None, None, None)
            .map_err(|error| OmniError::Internal(format!("spawn bootstrap dial thread: {error}")))
    }

    /// The active profile's connection-privacy mode (Auto by default). Auto allows third-party
    /// serverless rendezvous; Direct-only restricts connectivity to LAN + router port-mapping +
    /// manual exchange (zero third parties). See [`ConnectionPrivacyMode`].
    pub fn connection_privacy_mode(&self) -> ConnectionPrivacyMode {
        load_connection_privacy_mode(&self.storage.db_path().to_string_lossy())
    }

    /// Set the active profile's connection-privacy mode (persisted in the sidecar). Takes effect on
    /// the next rendezvous sweep / bootstrap attempt — the autoconnect worker re-reads it each sweep.
    pub fn set_connection_privacy_mode(&self, mode: ConnectionPrivacyMode) -> OmniResult<()> {
        let path = connection_privacy_mode_sidecar_path(&self.storage.db_path().to_string_lossy());
        std::fs::write(&path, mode.as_db_value()).map_err(|error| {
            OmniError::Storage(format!(
                "write connection privacy mode sidecar '{}': {error}",
                path.display()
            ))
        })?;
        // Take effect immediately on a running mesh: switching to `RelayOnly` stops new direct
        // upgrades AND closes the direct links already up (only relays keep one). A stopped mesh picks
        // it up from the sidecar on the next `start_mesh`.
        if let Some(runtime) = self.mesh_runtime() {
            crate::app_pinned_relay::apply_direct_gate(
                &self.storage,
                &runtime,
                &self.storage.db_path().to_string_lossy(),
            );
        }
        Ok(())
    }

    /// The active profile's carrier-upgrade policy (R1a, `Auto` by default): how aggressively the
    /// node climbs from the reliable TCP-relay base toward faster QUIC/UDP carriers. Orthogonal to
    /// [`ConnectionPrivacyMode`] (the privacy axis). NB: the climb itself is wired in a later R1a
    /// increment — this is the persisted setting only. See docs/decisions/connection-quality-ladder.md.
    pub fn carrier_policy(&self) -> omni_types::CarrierPolicy {
        load_carrier_policy(&self.storage.db_path().to_string_lossy())
    }

    /// Set the active profile's carrier policy (persisted in the sidecar) and apply it to the running
    /// mesh at once: a connection the new carrier refuses closes and is redialed on it — over the
    /// peer's door for TCP (its LAN door, else the one its beacon advertised), over UDP for QUIC
    /// (`MeshRuntime::switch_carrier_policy`). A mesh started without the relay's TCP carrier cannot
    /// take TCP or Auto live while the relay answers on TCP; it restarts, the way it was started.
    /// See docs/decisions/connection-quality-ladder.md.
    pub fn set_carrier_policy(&self, policy: omni_types::CarrierPolicy) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let path = carrier_policy_sidecar_path(&db_path);
        std::fs::write(&path, policy.as_db_value()).map_err(|error| {
            OmniError::Storage(format!("write carrier policy sidecar '{}': {error}", path.display()))
        })?;
        let Some(runtime) = self.mesh_runtime() else {
            return Ok(()); // the next start takes it from the sidecar
        };
        let doors = crate::app_peer_door::peer_doors(&db_path)
            .into_iter()
            .filter_map(|(node, door)| parse_mesh_node_hex(&node).ok().map(|node| (node, door)))
            .collect();
        if runtime.switch_carrier_policy(policy, doors) {
            return Ok(());
        }
        drop(runtime);
        let mode = self.mesh_start_mode.lock().ok().and_then(|mode| *mode);
        omni_app_trace_log(
            "mesh/carrier-switch",
            &format!("policy={} needs the relay's TCP carrier — restarting the mesh", policy.as_db_value()),
        );
        self.stop_mesh();
        let (autoconnect, prefer_lan) = mode.unwrap_or((true, true));
        self.start_mesh_internal(autoconnect, prefer_lan)
    }

    /// The active profile's fixed direct-TCP front-door port (`None` = ephemeral/UPnP). When set, the
    /// user is expected to port-forward this TCP port to their machine on their router, giving a peer
    /// on a UDP-hostile network (mobile) a reliable relay-less inbound path (it dials OUT to this port).
    /// Takes effect on the next mesh start. See docs/decisions/tcp-direct-peer-fallback.md.
    pub fn direct_tcp_bind_port(&self) -> Option<u16> {
        load_direct_tcp_port(&self.storage.db_path().to_string_lossy())
    }

    /// Persist the active profile's fixed direct-TCP front-door port. `None`/`Some(0)` clears it back
    /// to ephemeral. A restart / mesh reconnect applies it (the listener binds at mesh start).
    pub fn set_direct_tcp_bind_port(&self, port: Option<u16>) -> OmniResult<()> {
        let path = direct_tcp_port_sidecar_path(&self.storage.db_path().to_string_lossy());
        match port.filter(|p| *p != 0) {
            Some(port) => std::fs::write(&path, port.to_string()).map_err(|error| {
                OmniError::Storage(format!(
                    "write direct-tcp-port sidecar '{}': {error}",
                    path.display()
                ))
            }),
            None => match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(OmniError::Storage(format!(
                    "clear direct-tcp-port sidecar '{}': {error}",
                    path.display()
                ))),
            },
        }
    }

    /// Friend-proxy (Axis 2): set/revoke the persisted per-contact consent to carry `contact_id`'s
    /// mesh traffic through our node, and mirror the change into the RUNNING mesh immediately —
    /// the consent set gates admission (`admit_friend_relay`), and the advertised
    /// `ANNOUNCE_CAP_FRIEND_RELAY` role is re-derived (any consent → advertise). Off by default,
    /// revocable; storage is the durable source of truth, re-seeded on every mesh start.
    /// See docs/decisions/friend-proxy-by-consent.md (Phase C).
    pub fn set_contact_friend_relay_consent(
        &self,
        contact_id: &ContactId,
        consent: bool,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("set_contact_friend_relay_consent")?;
        let snapshot =
            self.storage
                .set_contact_friend_relay_consent(contact_id, consent, current_unix_ms())?;
        if let Some(runtime) = self.mesh_runtime() {
            // A contact without a stored prekey bundle (no secure session yet) has no resolvable
            // NodeAddress — the persisted flag still holds and seeds a later mesh start.
            if let Some(node) = snapshot
                .remote_account_id
                .as_ref()
                .and_then(|account_id| self.mesh_node_for_account(account_id).ok().flatten())
            {
                if consent {
                    runtime.consent_to_relay_for(node);
                } else {
                    runtime.revoke_relay_consent(&node);
                }
            }
            runtime.set_friend_relay_advertised(
                !self
                    .storage
                    .list_friend_relay_consented_account_ids()
                    .unwrap_or_default()
                    .is_empty(),
            );
        }
        Ok(snapshot)
    }

    /// Friend-proxy (Axis 2): the persisted carriage consents resolved to mesh NodeAddresses —
    /// what mesh start seeds the runtime consent set from. Consents whose contact has no stored
    /// prekey bundle yet are skipped (unresolvable until a secure session exists).
    fn friend_relay_consented_nodes(&self) -> Vec<MeshNodeAddress> {
        self.storage
            .list_friend_relay_consented_account_ids()
            .unwrap_or_default()
            .iter()
            .filter_map(|account_id| self.mesh_node_for_account(account_id).ok().flatten())
            .collect()
    }

    fn mesh_lock(&self) -> OmniResult<std::sync::MutexGuard<'_, Option<Arc<MeshRuntime>>>> {
        self.mesh
            .lock()
            .map_err(|_| OmniError::Internal("mesh runtime lock is poisoned".to_string()))
    }

    /// Cheap clone of the running mesh runtime, if any. Locks only long enough to
    /// clone the `Arc`; the (blocking) dial/send then runs off the lock so it
    /// can't stall the live media/wire send path that needs the same handle.
    fn mesh_runtime(&self) -> Option<Arc<MeshRuntime>> {
        self.mesh
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(Arc::clone))
    }

    /// Факты живого линка для диалога: адрес соединения и ИЗМЕРЕННЫЙ транспортом RTT.
    /// `None` — контакт не разрешается в узел, mesh не поднят или линка нет.
    ///
    /// Через неё панель связи получает измерения вместо пересказа: до 2026-08-31 «Пинг» там
    /// считался по presence-обмену (медиана 322 мс при 49 мс на транспорте), а «Endpoint»
    /// показывал адрес УЗЛА, из-за чего соседнее поле «Сеть» печатало то же самое.
    /// См. docs/bugs/ping-was-our-own-pipeline.md.
    ///
    /// С фактами — какое из устройств собеседника на том конце и сколько их всего: у контакта с
    /// ноутбуком и ПК один чат, и «4 мс» ничего не говорит, пока не сказано, кто ответил (стенд
    /// 2026-10-14: Алиса видела 4 мс до «Марка» и думала, что это ПК, а это был его ноутбук рядом).
    pub(crate) fn peer_link_facts_for_dialog(&self, dialog_key: &str) -> Option<crate::PeerLinkView> {
        let card = self.storage.resolve_contact_card_for_dialog(dialog_key).ok()??;
        let account_id = card.contact.remote_account_id?;
        let node = self.mesh_node_for_account(&account_id).ok()??;
        let facts = self.mesh_runtime()?.peer_link_facts(&node)?;
        let bundles = self.storage.list_remote_prekey_bundles_for_account(&account_id).unwrap_or_default();
        let device_id = bundles
            .iter()
            .find(|bundle| {
                omni_mesh_runtime::node_address_from_ed25519_pubkey(bundle.identity_signing_key.0).ok() == Some(node)
            })
            .map(|bundle| bundle.device_id.0.clone());
        Some(crate::PeerLinkView { facts, device_id, device_count: bundles.len() as u32 })
    }

    /// Spawn the background worker that keeps approved contacts mesh-linked.
    /// Idempotent in practice: `start_mesh` only calls this on a None→Some
    /// transition, and installing a fresh sender drops any previous one (so a
    /// stale worker exits). The worker self-terminates when the sender is
    /// dropped (`stop_mesh`) or the runtime goes away.
    fn spawn_mesh_autoconnect(&self) {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        if let Ok(mut stop) = self.mesh_autoconnect_stop.lock() {
            *stop = Some(stop_tx);
        }
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mesh = Arc::clone(&self.mesh);
        let locally_dialed = Arc::clone(&self.mesh_locally_dialed);
        let pairing_targets = Arc::clone(&self.mesh_pairing_targets);
        let bootstrap_invites = Arc::clone(&self.mesh_bootstrap_invites);
        // SessionHello bytes for an already-linked pairing target (push pairing over a live link;
        // see dial_mesh_pairing_targets). Same builder as the on-link-up `make_hello` above.
        let hello_maker: MeshHelloMaker = {
            let transport = self.transport.clone();
            Arc::new(move |_peer| {
                serde_json::to_vec(&transport.build_local_session_hello_envelope())
                    .unwrap_or_default()
            })
        };
        std::thread::spawn(move || {
            run_mesh_autoconnect_worker(
                db_path,
                mesh,
                locally_dialed,
                pairing_targets,
                bootstrap_invites,
                hello_maker,
                stop_rx,
            );
        });
    }
}

/// Background loop: every `MESH_AUTOCONNECT_INTERVAL_SECS`, dial any approved
/// contact that isn't yet mesh-linked. Opens its own `Storage` handle (rusqlite
/// connections aren't `Sync`), matching the queue worker. Exits when the stop
/// sender is dropped or the runtime is gone.
fn run_mesh_autoconnect_worker(
    db_path: String,
    mesh: Arc<Mutex<Option<Arc<MeshRuntime>>>>,
    locally_dialed: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    pairing_targets: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    bootstrap_invites: Arc<Mutex<std::collections::HashMap<String, MeshBootstrapInvite>>>,
    hello_maker: MeshHelloMaker,
    stop_rx: mpsc::Receiver<()>,
) {
    let storage = match Storage::open(&db_path) {
        Ok(storage) => storage,
        Err(_) => return,
    };
    // Fire the FIRST rendezvous sweep after a short grace (not a full interval), so a contact on
    // another network is reached within seconds of the app opening — the LAN-announce grace is brief.
    // Fast tests stay off the network via the loopback gate in the sweep itself, not this timing.
    let mut last_rendezvous_sweep = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(
            RENDEZVOUS_SWEEP_INTERVAL_SECS.saturating_sub(RENDEZVOUS_FIRST_SWEEP_GRACE_SECS),
        ))
        .unwrap_or_else(std::time::Instant::now);
    // R7 publish jitter: the threshold for the NEXT sweep is re-randomized after each one (the first
    // still fires at ~grace via the back-dated `last_rendezvous_sweep` + this floor).
    let mut next_rendezvous_sweep_secs = RENDEZVOUS_SWEEP_INTERVAL_SECS;
    // First-contact bootstrap is time-sensitive (a user is waiting), so let its first sweep
    // run on the next tick rather than after a full interval.
    let mut last_bootstrap_sweep = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(BOOTSTRAP_SWEEP_INTERVAL_SECS))
        .unwrap_or_else(std::time::Instant::now);
    // Воркер ЗАПУЩЕН (не первый такт): от этой точки отсчитывается льготное окно до первого цикла
    // рандеву, и именно она отделяет «долго стартовало приложение» от «долго ждали окна».
    omni_app_trace_log("mesh/worker", "started");
    // Узлы, дозвон до которых сейчас идёт в фоне. Живёт у воркера: других пользователей нет.
    let dials_in_flight: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    // ОДИН поток дозвона на воркер, с очередью.
    //
    // Первый вариант правки запускал поток НА КАЖДЫЙ контакт. Цикл он действительно разблокировал,
    // но параллелизм начинал расти с числом контактов — и это оплатили тесты: гейт ядра до правки
    // давал 3 зелёных из 3, с ней 2 зелёных и 4 отказа, а с откатом ОДНОЙ этой правки (всё
    // остальное на месте) снова зелёный. Падали двухузловые тесты с окнами ожидания в 5-10 с, то
    // есть ровно те, кому дороже всего чужая конкуренция.
    //
    // Очередь даёт то же, ради чего всё делалось (цикл не ждёт дозвона), и не даёт того, что
    // сломало: число потоков постоянно. Заодно честнее по сети — веер одновременных дозвонов ко
    // всем недостижимым пирам не нужен никому.
    let (dial_tx, dial_rx) = mpsc::channel::<MeshNodeAddress>();
    {
        let dial_mesh = Arc::clone(&mesh);
        let dial_in_flight = Arc::clone(&dials_in_flight);
        let dial_db_path = db_path.clone();
        let _ = std::thread::Builder::new()
            .name("omni-mesh-dial".to_string())
            .spawn(move || {
                let mut last_good = LastGoodRedial::default();
                // Перезвоны по последнему адресу идут СВОИМИ потоками, по одному на контакт: в
                // общем потоке мёртвые контакты (до 8 с на попытку) задерживали живые, а сам
                // перезвон ждал, пока отвисит `dial` (живьём 2026-10-06: за минуту обрыва — две
                // попытки к мёртвым тестовым контактам и ни одной к ПК).
                let redials_in_flight: Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>> =
                    Arc::new(Mutex::new(std::collections::HashSet::new()));
                // Отправитель живёт у цикла обслуживания: когда тот завершится, `recv` вернёт
                // ошибку и поток выйдет сам.
                while let Ok(node) = dial_rx.recv() {
                    if let Some(runtime) = dial_mesh
                        .lock()
                        .ok()
                        .and_then(|guard| guard.as_ref().map(Arc::clone))
                    {
                        if runtime.has_link(&node) {
                            last_good.linked(&node);
                        } else if last_good.due(&node, std::time::Instant::now()) {
                            spawn_last_good_redial(&runtime, &dial_db_path, node, &redials_in_flight);
                        }
                        let _ = runtime.dial(node);
                        if runtime.has_link(&node) {
                            last_good.linked(&node);
                        }
                    }
                    release_dial_slot(&dial_in_flight, &node);
                }
            });
    }
    // Warm-rejoin cache: what was last written, and when (`peer_book_save_due`).
    let mut last_peer_book_snapshot = std::time::Instant::now();
    let mut saved_peer_endpoints: std::collections::HashSet<(MeshNodeAddress, String)> =
        std::collections::HashSet::new();
    // Event-triggered relay→direct upgrade state: last-seen link grade + last event-rendezvous time
    // per peer, so a degrade fires an early per-peer rendezvous (flap-floored). See
    // `early_rendezvous_contacts`.
    let mut contact_link_grades: std::collections::HashMap<[u8; 20], ContactLinkGrade> =
        std::collections::HashMap::new();
    let mut last_event_rendezvous: std::collections::HashMap<[u8; 20], std::time::Instant> =
        std::collections::HashMap::new();
    // Early re-rendezvous for contacts the last attempt left without a direct link — see
    // `RENDEZVOUS_RETRY_AFTER`.
    let mut rendezvous_episodes: std::collections::HashMap<[u8; 20], RendezvousEpisode> =
        std::collections::HashMap::new();
    // Periodic rendezvous thins out for contacts that stay silent — see `silent_contact_sweep_due`.
    let mut contact_silence = ContactSilence::new(std::time::Instant::now());
    let mut active_circle = ActiveCircle::default();
    // Push wake-ups: courier subscriptions on the topics of the contacts without a direct link and on
    // the account topic — see `BeaconWatches`.
    let mut beacon_watches = BeaconWatches::new();
    let mut last_durable_persist = std::time::Instant::now();
    let mut persisted_durable_marks: u64 = 0;
    loop {
        match stop_rx.recv_timeout(std::time::Duration::from_secs(MESH_AUTOCONNECT_INTERVAL_SECS)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Snapshot the running runtime (brief lock) so the dials below run off
        // the start/stop lock. None → mesh stopped → exit.
        let Some(runtime) = mesh.lock().ok().and_then(|guard| guard.as_ref().map(Arc::clone)) else {
            return;
        };
        // ЦИКЛ ОБСЛУЖИВАНИЯ ОБЯЗАН ГОВОРИТЬ, КОГДА ОН ЗАСТРЯЛ.
        //
        // Такт объявлен трёхсекундным, и ВСЁ, что живёт на нём, молча наследует его длину: и
        // sweep рандеву (условие проверяется раз в такт), и поддержание линков. Замер 2026-08-30
        // показал sweep раз в 183-243 с при объявленных 90-120 — то есть такт где-то растягивался,
        // и по трассе этого не было видно вообще. Дозвоны здесь синхронные, а мёртвый адрес стоит
        // до таймаута, поэтому кандидат на растяжение очевиден — но пока он не измерен пофазно,
        // это догадка. Строка ниже делает его фактом или снимает.
        let tick_at = std::time::Instant::now();
        // First, before any dial can stall the tick: our video passport to the calls we are in.
        crate::app_group_calls::send_due_group_passports(&db_path, &runtime);
        // Dial imported pairing targets (strangers we want to pair with) first,
        // then maintain links to already-approved contacts.
        dial_mesh_pairing_targets(&runtime, &pairing_targets, &locally_dialed, &hello_maker);
        let after_pairing_ms = tick_at.elapsed().as_millis();
        // Keep the pinned relay/bootstrap registry links alive — re-dial any whose Link
        // dropped (mobile outage / relay restart) so a both-NAT pair's only path self-heals
        // (the dial discards a dead cached connection and reconnects). Before approved-contact
        // dials, since those peers may only be reachable THROUGH a relay.
        autoconnect_redial_network_nodes(&storage, &runtime);
        crate::app_pinned_relay::tend_pinned_relay(&storage, &runtime, &db_path);
        let after_nodes_ms = tick_at.elapsed().as_millis();
        // Group feed messages queued while no mesh handle was at hand (the 1:1 receive path, an
        // offline moment): publish them.
        crate::app_groups::resubscribe_all_groups_via(&storage, &runtime);
        // A group that went on in two branches: ask to be put back, or put a member back.
        crate::app_group_branches::group_branch_tick(&storage, current_unix_ms());
        crate::app_groups::flush_group_feed_outbox(&storage, &runtime);
        crate::app_group_attachments::pull_requested_group_attachments(&storage, &runtime);
        if last_durable_persist.elapsed() >= DURABLE_PERSIST_EVERY {
            last_durable_persist = std::time::Instant::now();
            let rows = runtime.export_durable_rows();
            let marks = durable_rows_marks(&rows);
            if marks != persisted_durable_marks && storage.replace_durable_frames(&rows).is_ok() {
                persisted_durable_marks = marks;
            }
        }
        active_circle.refresh_if_due(&storage);
        let _ = autoconnect_dial_approved_contacts(
            &storage,
            &runtime,
            &locally_dialed,
            &dials_in_flight,
            &dial_tx,
            &active_circle.accounts,
        );
        dial_group_call_members(&db_path, &runtime, &dials_in_flight, &dial_tx);
        let after_contacts_ms = tick_at.elapsed().as_millis();
        // Full-sweep duplicate-contact merge: used to run inside every UI `list_contacts` call,
        // which put a write on the read path and blocked the UI thread on the WAL busy timeout.
        // Runs here in the background instead, so the UI stays a pure read. Best-effort — a
        // transient sqlite error just retries next tick; write-time `find_*` merges cover
        // mutations in the meantime so no duplicate lingers past its next relevant lookup.
        let _ = storage.merge_duplicate_contacts_for_active_profile(current_unix_ms());
        // Serverless-rendezvous fallback for contacts LAN/announce dialing can't reach
        // (they're on another network). Throttled to RENDEZVOUS_SWEEP_INTERVAL_SECS —
        // independent of the 3 s autoconnect cadence — because it hits free providers.
        // Third-party (serverless) rendezvous — both the durable contact sweep and the first-contact
        // bootstrap — is gated by the profile's connection-privacy choice. Direct-only means we never
        // touch a public provider; connectivity is then LAN + router port-mapping + manual exchange.
        let allow_rendezvous = load_connection_privacy_mode(&db_path).uses_rendezvous();
        // A carrier switch just closed links that are being redialed over the new carrier; looking
        // for those contacts through the providers meanwhile only spends their quota. The sweep and
        // the early rendezvous wait it out (`MeshRuntime::carrier_switch_settling`).
        let switch_settling = runtime.carrier_switch_settling();
        if allow_rendezvous
            && !switch_settling
            && last_rendezvous_sweep.elapsed().as_secs() >= next_rendezvous_sweep_secs
        {
            last_rendezvous_sweep = std::time::Instant::now();
            next_rendezvous_sweep_secs = jittered_rendezvous_sweep_interval_secs();
            let swept_at = std::time::Instant::now();
            for node in
                rendezvous_sweep_unlinked_contacts(&storage, &runtime, &db_path, &mut contact_silence)
            {
                record_rendezvous_attempt(&mut rendezvous_episodes, node, swept_at);
            }
            // Relay-less retry for add-by-Omni-ID (2026-07-20): an outgoing Pending created by
            // `find_contact_by_omni_id` has NO session yet, so the per-pair sweep above (which needs
            // an Approved contact + a pair secret) never covers it. Its FIRST dial can miss — the
            // peer's UPnP/TCP front door may not be mapped when its beacon was first published, or a
            // mobile carrier dropped the UDP burst. Re-fetch each such peer's FRESH identity beacon
            // (which now carries the optimistic UPnP-TCP front door) and re-dial, on the same
            // throttled cadence. Without this, one flaky first attempt is a permanent dead end.
            retry_unlinked_outgoing_id_pendings(
                &storage,
                &runtime,
                &db_path,
                &pairing_targets,
                &hello_maker,
            );
        }
        // Early rendezvous between sweeps, every 3 s tick: a contact whose link just DEGRADED is
        // rendezvoused at once (per-peer flap-floored) — so a relay→direct upgrade on a UDP-hostile
        // network happens in seconds, not up to a full ~90 s sweep later — and one the last attempt
        // left without a direct link is looked at again on `RENDEZVOUS_RETRY_AFTER`. Same privacy
        // gate as the sweep; the flap floor and the bounded retry schedule cap provider load.
        // Push before poll: a contact whose fresh beacon just arrived on a subscription is heard and
        // rendezvoused now, not on its next periodic look.
        if allow_rendezvous {
            let now = std::time::Instant::now();
            for wake in beacon_watches.woken(now) {
                match wake {
                    BeaconWake::Contact(node) => {
                        omni_app_trace_log(
                            "rendezvous/watch",
                            &format!("wake node={}", MeshNodeAddress::from_bytes(node)),
                        );
                        contact_silence.heard_now(node, now);
                        rendezvous_contact_by_node(&storage, &runtime, &db_path, node);
                    }
                    BeaconWake::OwnAccount => {
                        omni_app_trace_log("rendezvous/watch", "wake own account: a device of ours not met yet");
                        rendezvous_own_account_now(&storage, &runtime, &db_path);
                    }
                }
            }
            if beacon_watches.reconcile_due(now) {
                reconcile_beacon_watches(&storage, &runtime, &db_path, &mut beacon_watches, now);
            }
        } else {
            beacon_watches.stop_all();
        }
        if allow_rendezvous && !switch_settling {
            early_rendezvous_contacts(
                &storage,
                &runtime,
                &db_path,
                &mut contact_link_grades,
                &mut last_event_rendezvous,
                &mut rendezvous_episodes,
            );
        }
        // First-contact bootstrap: while an invite is outstanding, keep publishing our endpoints
        // to its blinded bootstrap topic + dialing the other side until the link forms / it expires.
        if allow_rendezvous
            && last_bootstrap_sweep.elapsed().as_secs() >= BOOTSTRAP_SWEEP_INTERVAL_SECS
        {
            last_bootstrap_sweep = std::time::Instant::now();
            bootstrap_rendezvous_sweep(&bootstrap_invites, &runtime, &db_path);
        }
        // Warm-rejoin cache (G3): refresh the live DIRECT endpoints so the NEXT start re-dials known
        // peers immediately (start_mesh_internal seeds dial_at from here). Upsert (not wholesale) — a
        // peer offline right now keeps its cached endpoint and ages out by TTL. Not privacy-gated:
        // these are direct LAN/public endpoints, no third party. See docs/decisions/omni-network-model.md.
        let live_peer_endpoints: std::collections::HashSet<_> =
            runtime.snapshot_peer_book().into_iter().collect();
        if peer_book_save_due(
            &live_peer_endpoints,
            &saved_peer_endpoints,
            last_peer_book_snapshot.elapsed(),
        ) {
            last_peer_book_snapshot = std::time::Instant::now();
            let now = current_unix_ms();
            for (node, endpoint) in &live_peer_endpoints {
                let _ = storage.upsert_mesh_peer_endpoint(node.as_bytes(), endpoint, now);
            }
            saved_peer_endpoints = live_peer_endpoints;
        }
        // Печатаем только ПЕРЕБОР — такт, который не уложился в свой же интервал. Нормальный такт
        // молчит, иначе строка раз в три секунды утопит трассу и её перестанут читать. Разбивка
        // кумулятивная: видно не только «застряли», но и на чём.
        let tick_ms = tick_at.elapsed().as_millis();
        if tick_ms > u128::from(MESH_AUTOCONNECT_INTERVAL_SECS) * 1000 {
            omni_app_trace_log(
                "mesh/tick-overrun",
                &format!(
                    "tick={tick_ms}ms pairing={after_pairing_ms}ms nodes={after_nodes_ms}ms contacts={after_contacts_ms}ms budget={}ms",
                    MESH_AUTOCONNECT_INTERVAL_SECS * 1000
                ),
            );
        }
    }
}

/// Dial every imported pairing target that isn't yet linked. Unlike approved
/// contacts (dialed by their prekey-bundle key), a pairing target is just a
/// NodeAddress derived from the invite's pubkey — which is all `dial` needs.
/// The on_wire SessionHello handler turns a linked target into a contact.
fn dial_mesh_pairing_targets(
    runtime: &MeshRuntime,
    pairing_targets: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    locally_dialed: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    hello_maker: &MeshHelloMaker,
) {
    let targets: Vec<MeshNodeAddress> = match pairing_targets.lock() {
        Ok(set) => set.iter().copied().collect(),
        Err(_) => return,
    };
    for node in targets {
        if runtime.has_link(&node) {
            // Already linked (discovered on the LAN / reached via a relay BEFORE the invite was
            // imported): a fresh `dial` would COALESCE onto the live connection and emit NO new
            // SessionHello, so the peer's `finish_mesh_pairing_from_hello` never fires and pairing
            // stalls indefinitely (live-found 2026-06-25 — same-LAN peers couldn't pair). Push a
            // SessionHello over the EXISTING link so a mutually-importing peer completes the
            // handshake. Idempotent (the peer just re-registers the connection) and self-limiting:
            // the one-shot pairing claim removes the target once it pairs, so this stops.
            let _ = runtime.send_wire(node, &hello_maker(node));
            continue;
        }
        // Record initiator intent before dialing so the peer's hello registers
        // as initiated_locally = true (mirrors approved-contact auto-connect).
        if let Ok(mut dialed) = locally_dialed.lock() {
            dialed.insert(node);
        }
        let _ = runtime.dial(node);
    }
}

/// One network entry node (bootstrap / relay) for the «Узлы сети / Релеи» GUI: where/whom to dial when
/// LAN + invite don't reach. `endpoint` = ip:port; `node_address_hex` = mesh identity if known; `source`
/// is "base" (the removable base node, if one is seeded — none is right now, the VPS expired) or
/// "manual". `removable` is always true — even a base node may be deleted (the empty list still
/// connects via LAN + invite).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkNodeView {
    pub id: i64,
    pub endpoint: Option<String>,
    pub node_address_hex: Option<String>,
    pub label: String,
    pub source: String,
    pub removable: bool,
    /// Honest live reachability of this entry node, for the GUI: `online` (pinned + linked now),
    /// `offline` (pinned, mesh up, not linked), `mesh_off` (pinned, mesh not started), or `hint`
    /// (no pinned identity — reachability unknown until dialed; step 5.3 learn-on-connect).
    pub status: String,
    /// R1c inc.3b: this relay is flagged as part of MY OWN bundle — auto-propagated to approved
    /// friends over the per-pair beacon. The GUI renders a "share with friends" checkbox bound to it
    /// (toggle via `omni_network_relay_set_shared`). See docs/decisions/r1c-rotatable-white-relay.md.
    pub shared: bool,
    /// R1c inc.4: the issuing friend's bundle id this relay was IMPORTED under (None for a manual /
    /// base / my-own row), so the GUI can group + label rows by the friend's set. Short-hex in the UI.
    pub bundle_id: Option<String>,
    /// R1c inc.4: the version of that imported bundle (None when `bundle_id` is None) — shown so the
    /// operator can see a friend's set rotate. See docs/decisions/r1c-rotatable-white-relay.md.
    pub bundle_version: Option<i64>,
}

/// One courier row for the courier card in Settings. `url` is the courier endpoint, `source` is
/// `base`/`manual`/`invite`, `id` the handle. A default courier (`base`) is turned off rather than
/// removed, so it can come back; one the user or an invite added can be removed.
/// See docs/decisions/courier-metadata-and-quota.md (step 6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RendezvousProviderView {
    pub id: i64,
    pub url: String,
    pub source: String,
    pub removable: bool,
    /// `publish_read` / `read` / `off`.
    pub mode: String,
    /// `ntfy` / `nostr` / `dht` — what the card says about who sees what.
    pub kind: String,
    /// One of the relays topics are shared out among (each topic uses a few of them).
    pub pooled: bool,
    /// What the courier did last: `ok` / `quota` / `cooling` / `fail`, or `unknown` (not used since the
    /// courier set last changed).
    pub status: String,
    pub status_detail: String,
    pub status_age_secs: Option<u64>,
}

impl OmniApp {
    /// The active profile's network entry-node registry (base + user-added relays), base first — the
    /// list the «Узлы сети / Релеи» GUI shows and the mesh dials when LAN + invite don't reach. An empty
    /// list is valid (LAN + invite still connect; no node is privileged).
    pub fn list_network_nodes(&self) -> OmniResult<Vec<NetworkNodeView>> {
        let runtime = self.mesh_runtime();
        Ok(self
            .storage
            .list_network_nodes()?
            .into_iter()
            .map(|node| {
                let node_address_hex = node
                    .node_address
                    .as_ref()
                    .map(|bytes| bytes.iter().map(|b| format!("{b:02x}")).collect());
                // Honest reachability: a PINNED entry is checked against the live mesh; a hint-only
                // entry (no pinned node) reads "hint" until it's dialed (5.3 learn-on-connect).
                let status = match node
                    .node_address
                    .as_ref()
                    .and_then(|bytes| <[u8; 20]>::try_from(bytes.as_slice()).ok())
                {
                    None => "hint",
                    Some(raw) => match &runtime {
                        None => "mesh_off",
                        Some(rt) if rt.has_link(&MeshNodeAddress::from_bytes(raw)) => "online",
                        Some(_) => "offline",
                    },
                }
                .to_string();
                NetworkNodeView {
                    id: node.id,
                    endpoint: node.endpoint,
                    node_address_hex,
                    label: node.label,
                    source: node.source,
                    removable: true,
                    status,
                    shared: node.shared,
                    bundle_id: node.bundle_id,
                    bundle_version: node.bundle_version,
                }
            })
            .collect())
    }

    /// Add a relay / bootstrap entry node with a user label. Accepts either a plain `ip:port`
    /// (a HINT — surfaced but not dialed until anonymous learn-on-connect lands) or a **pinned**
    /// `node_hex@ip:port` (the secure form the mesh dials right now: the ed25519 challenge proves
    /// that exact key, so you trust a specific relay, not "whoever answers the IP"). Validates the
    /// endpoint (and the node hex when present), rejects a duplicate endpoint. Returns the new id.
    /// Add a user relay. `endpoint` is `[node_hex@]ip:port` (the UDP/QUIC hint), optionally followed
    /// by `|tcp=<ip:port>` to also register the relay's TLS-over-TCP bridge (the rung-0 carrier — R1a
    /// connection-quality ladder). The GUI/CLI assemble the suffix from a separate field; an absent
    /// suffix → UDP-only relay. `cert_spki_pin` has no surface yet (it lands with R2 fronting).
    pub fn add_network_node(&self, endpoint: &str, label: &str) -> OmniResult<i64> {
        let raw = endpoint.trim();
        let label = label.trim();
        // Optional TLS-over-TCP bridge address (rung-0 carrier target), validated up front.
        let (udp_part, tcp_endpoint) = match raw.split_once("|tcp=") {
            Some((left, tcp)) => {
                let tcp = tcp.trim();
                tcp.parse::<std::net::SocketAddr>().map_err(|_| {
                    OmniError::InvalidInput(format!("'{tcp}' is not a valid TCP-bridge ip:port"))
                })?;
                (left.trim(), Some(tcp))
            }
            None => (raw, None),
        };
        // Optional pinned identity: "node_hex@ip:port"; plain "ip:port" → no pinned node (hint).
        let (node_hex, addr_str) = match udp_part.split_once('@') {
            Some((node, addr)) => (Some(node.trim()), addr.trim()),
            None => (None, udp_part),
        };
        addr_str.parse::<std::net::SocketAddr>().map_err(|_| {
            OmniError::InvalidInput(format!("'{addr_str}' is not a valid ip:port endpoint"))
        })?;
        let node_address: Option<[u8; 20]> = match node_hex {
            Some(hex) => Some(*parse_mesh_node_hex(hex)?.as_bytes()),
            None => None,
        };
        if self.storage.network_node_endpoint_exists(addr_str)? {
            return Err(OmniError::InvalidInput(format!(
                "network node '{addr_str}' is already in the list"
            )));
        }
        let label = if label.is_empty() { addr_str } else { label };
        self.storage.add_network_node(&omni_storage::NewNetworkNode {
            endpoint: Some(addr_str),
            node_address: node_address.as_ref().map(|bytes| bytes.as_slice()),
            label,
            source: omni_storage::NETWORK_NODE_SOURCE_MANUAL,
            added_at_unix_ms: current_unix_ms(),
            tcp_endpoint,
            cert_spki_pin: None,
            // A hand-added relay belongs to no signed bundle — only an `om3relay:` import tags these.
            ..Default::default()
        })
    }

    /// Remove ANY entry node by id — including a base node if one is seeded (the user may delete it; the
    /// empty list still connects). No-op if the id is already gone. If the removed relay was part of MY
    /// `shared` bundle, the published set changed → bump my bundle version so the rotation propagates
    /// (R1c inc.3b).
    pub fn remove_network_node(&self, id: i64) -> OmniResult<()> {
        let was_shared = self
            .storage
            .list_shared_network_nodes()
            .map(|nodes| nodes.iter().any(|node| node.id == id))
            .unwrap_or(false);
        self.storage.remove_network_node(id)?;
        if was_shared {
            let _ = self.bump_my_relay_bundle_version();
            self.push_relay_set_update_to_linked_contacts();
        }
        Ok(())
    }

    /// The active profile's rendezvous courier providers (seeded defaults + user/invite-added), base
    /// first — the «Провайдеры рандеву» GUI list. These are the public pub/sub brokers the helper fans
    /// publish/fetch across to MEET a peer; distinct from `list_network_nodes` (mesh transport relays).
    pub fn list_rendezvous_providers(&self) -> OmniResult<Vec<RendezvousProviderView>> {
        adopt_legacy_dht_switch(&self.storage);
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let helper = rendezvous_runtime_registry()
            .lock()
            .ok()
            .and_then(|registry| registry.get(&db_path).map(|runtime| std::sync::Arc::clone(&runtime.helper)));
        let now = current_unix_ms() as u64;
        Ok(self
            .storage
            .list_rendezvous_providers()?
            .into_iter()
            .map(|p| {
                let label = omni_rendezvous::courier_label(&p.url);
                let status = helper.as_ref().and_then(|helper| helper.courier_status(&label));
                let kind = label.split(':').next().unwrap_or("courier").to_string();
                RendezvousProviderView {
                    id: p.id,
                    pooled: omni_rendezvous::NOSTR_RELAY_POOL.contains(&p.url.as_str()),
                    removable: p.source != omni_storage::RENDEZVOUS_PROVIDER_SOURCE_BASE,
                    url: p.url,
                    source: p.source,
                    mode: p.mode,
                    kind,
                    status: status.as_ref().map_or("unknown".to_string(), |s| s.state.clone()),
                    status_detail: status.as_ref().map(|s| s.detail.clone()).unwrap_or_default(),
                    status_age_secs: status.map(|s| now.saturating_sub(s.at_unix_ms) / 1000),
                }
            })
            .collect())
    }

    /// Set how a courier is used: `publish_read` (we post our beacons there and look for contacts),
    /// `read` (we only look), `off`. At least one courier must stay `publish_read` — with none, nobody
    /// could find us. Applies at once: the rendezvous runtime restarts with the new set.
    pub fn set_rendezvous_provider_mode(&self, id: i64, mode: &str) -> OmniResult<()> {
        let rows = self.storage.list_rendezvous_providers()?;
        if !rows.iter().any(|row| row.id == id) {
            return Err(OmniError::InvalidInput(format!("no courier with id {id}")));
        }
        if mode != omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ
            && !rows.iter().any(|row| row.id != id && row.mode == omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ)
        {
            return Err(OmniError::InvalidInput(
                "at least one courier has to stay where we post — otherwise nobody can find us".to_string(),
            ));
        }
        self.storage.set_rendezvous_provider_mode(id, mode)?;
        self.restart_rendezvous_with_new_couriers();
        Ok(())
    }

    /// The courier set changed: restart the profile's rendezvous runtime, so the helper — which reads
    /// the set once, when it starts — takes up the new one now rather than at the next mesh restart.
    /// Every user takes the runtime anew ([`profile_rendezvous_runtime`]).
    fn restart_rendezvous_with_new_couriers(&self) {
        clear_rendezvous_runtime_for_profile(&self.storage.db_path().to_string_lossy());
        // ...and the devices we talk to hear the new set now, not at the next reconcile.
        crate::note_courier_set_changed();
    }

    /// Add a rendezvous provider URL by hand (GUI). Validates the scheme (`http(s)://` ntfy /
    /// `ws(s)://` Nostr relay / `dht://` BT-DHT), rejects a duplicate. Applies at once: the rendezvous
    /// runtime restarts with the new set.
    pub fn add_rendezvous_provider(&self, url: &str) -> OmniResult<()> {
        let url = url.trim();
        if !is_valid_provider_url(url) {
            return Err(OmniError::InvalidInput(format!(
                "'{url}' is not a valid provider URL (use http(s):// for ntfy, ws(s):// for a Nostr relay, or dht:// for BT-DHT)"
            )));
        }
        if self.storage.rendezvous_provider_exists(url)? {
            return Err(OmniError::InvalidInput(format!(
                "rendezvous provider '{url}' is already in the list"
            )));
        }
        self.storage.add_rendezvous_provider(
            url,
            omni_storage::RENDEZVOUS_PROVIDER_SOURCE_MANUAL,
            current_unix_ms(),
        )?;
        self.restart_rendezvous_with_new_couriers();
        Ok(())
    }

    /// Remove a courier the user or an invite added. A default one is turned off instead (it keeps
    /// its row, so it can come back). The last courier we post to is not removed. No-op if the id is
    /// already gone.
    pub fn remove_rendezvous_provider(&self, id: i64) -> OmniResult<()> {
        let rows = self.storage.list_rendezvous_providers()?;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return Ok(());
        };
        if row.source == omni_storage::RENDEZVOUS_PROVIDER_SOURCE_BASE {
            return Err(OmniError::InvalidInput(
                "a default courier is turned off, not removed".to_string(),
            ));
        }
        if !rows.iter().any(|other| other.id != id && other.mode == omni_storage::RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ) {
            return Err(OmniError::InvalidInput(
                "at least one courier has to stay where we post — otherwise nobody can find us".to_string(),
            ));
        }
        self.storage.remove_rendezvous_provider(id)?;
        self.restart_rendezvous_with_new_couriers();
        Ok(())
    }

    /// The rendezvous courier set this app actually uses. Precedence: an explicit `OMNI_RENDEZVOUS_PROVIDERS`
    /// env override (the dev/stand knob) wins; else the per-profile user-managed set from storage; else the
    /// compiled cross-protocol default. This is the SINGLE source the app embeds into outgoing invites and
    /// (step 2c) hands the helper at spawn, so app and helper never diverge.
    pub(crate) fn configured_rendezvous_providers(&self) -> Vec<String> {
        courier_entries(&self.storage)
    }

    /// Persist the providers a friend's pairing invite contributed (best-effort, idempotent, source
    /// `invite`) so a first contact on a restrictive network converges on a working SHARED set. Invalid-scheme entries are
    /// skipped; a known URL is a no-op (UNIQUE). Never fails the import — a courier list is advisory.
    fn merge_invite_rendezvous_providers(&self, providers: &[String]) {
        let now = current_unix_ms();
        for url in providers {
            let url = url.trim();
            if is_valid_provider_url(url) {
                let _ = self.storage.add_rendezvous_provider(
                    url,
                    omni_storage::RENDEZVOUS_PROVIDER_SOURCE_INVITE,
                    now,
                );
            }
        }
    }

    /// Best-effort merge of DHT bootstrap nodes carried by a friend's invite into this profile's set
    /// (isolated-segment convergence; validated `ip:port`, deduped, one write). Mirrors
    /// `merge_invite_rendezvous_providers`; the nodes ride the sealed per-pair invite (R1b), not gossip.
    fn merge_invite_dht_bootstrap_nodes(&self, nodes: &[String]) {
        if nodes.is_empty() {
            return;
        }
        let db = self.storage.db_path().to_string_lossy().to_string();
        let mut current = load_dht_bootstrap_nodes(&db);
        let mut changed = false;
        for addr in nodes {
            let addr = addr.trim();
            if addr.parse::<std::net::SocketAddr>().is_ok() && !current.iter().any(|n| n == addr) {
                current.push(addr.to_string());
                changed = true;
            }
        }
        if changed {
            let _ = save_dht_bootstrap_nodes(&db, &current);
        }
    }

    /// Export `relays` as a SIGNED `om3relay:` bundle at `bundle_version` (R1c). The bundle is signed
    /// by the active profile's account identity, so a friend who imports it can verify it really came
    /// from me — the prerequisite for auto-propagating a rotated set over the per-pair beacon (phase
    /// 3). `bundle_version` is the rotation counter: publish N+1 to replace the prior set. Each relay
    /// needs at least one valid `ip:port` (`endpoint` and/or `tcp_endpoint`).
    pub fn generate_relay_bundle(
        &self,
        bundle_version: u32,
        relays: Vec<RelayBundleEntryInput>,
    ) -> OmniResult<String> {
        let active = self.current_active_profile_context_required()?;
        let identity_key = self
            .storage
            .get_local_identity_key(&active.account.descriptor.account_id)?
            .ok_or_else(|| {
                OmniError::Internal("active profile has no local identity key".to_string())
            })?;
        sign_relay_bundle(&identity_key, bundle_version, current_unix_ms(), relays)
    }

    /// Import a SIGNED `om3relay:` bundle: verify the issuer signature, then REPLACE that issuer's prior
    /// relay set with the new one (tagged `bundle_id` = issuer node + `bundle_version`). Returns the
    /// number of relays imported, or `0` if the bundle is not strictly newer than what's stored for
    /// this issuer (replay / older version is ignored — an attacker can't roll a friend back to a burned
    /// set). Signature failure is an error (a forged/poisoned bundle never reaches storage). On the
    /// MANUAL path the user pastes the token from a trusted channel; the per-pair beacon path (phase 3)
    /// adds the approved-contact gate before calling this.
    pub fn import_relay_bundle(&self, token: &str) -> OmniResult<usize> {
        // Manual paste: no propagating contact to bind to — the issuer signature is the sole authority.
        apply_relay_bundle(&self.storage, token, None)
    }

    /// R1c propagation (beacon path): apply a relay bundle that arrived sealed inside `contact_node`'s
    /// per-pair presence beacon. GATED **first-party-only** — the bundle's issuer MUST equal
    /// `contact_node`, so a contact can propagate ONLY its OWN relay set (never a friend-of-friend's;
    /// honours the R1b "no transitive relay propagation" invariant). Same signature verify + strict-newer
    /// rotation as the manual import. Returns relays imported (0 = not newer / ignored). The rendezvous
    /// consume path (increment 3b) calls this once it has opened the contact's sealed beacon.
    pub fn ingest_propagated_relay_bundle(
        &self,
        contact_node: [u8; 20],
        token: &str,
    ) -> OmniResult<usize> {
        let issuer = MeshNodeAddress::from_bytes(contact_node).to_string();
        apply_relay_bundle(&self.storage, token, Some(&issuer))
    }

    /// R1c inc.3b — flag (or unflag) entry node `id` as part of MY OWN relay bundle: the set I publish
    /// to approved friends over the per-pair beacon. A real change bumps my bundle version so the
    /// rotated set propagates strictly-newer. GUI checkbox + FFI toggle call this; no-op for an unknown
    /// id. See docs/decisions/r1c-rotatable-white-relay.md.
    pub fn set_relay_node_shared(&self, id: i64, shared: bool) -> OmniResult<()> {
        // First-party-only (owner decision #1 + the R1b no-transitive invariant): I may share only
        // relays I added myself — NEVER re-propagate a friend's IMPORTED bundle. A row carrying a
        // `bundle_id` was imported from a friend, so refuse to flag it as mine (it would get re-signed
        // under my identity and shipped to my friends as mine = transitive re-share). Unflagging is
        // always allowed. See docs/decisions/r1c-rotatable-white-relay.md.
        if shared {
            let is_imported = self
                .storage
                .list_network_nodes()?
                .iter()
                .any(|node| node.id == id && node.bundle_id.is_some());
            if is_imported {
                return Err(OmniError::InvalidInput(
                    "cannot share a relay imported from a friend's bundle \
                     (first-party-only — no transitive re-share)"
                        .to_string(),
                ));
            }
        }
        let updated = self.storage.set_network_node_shared(id, shared)?;
        if updated > 0 {
            self.bump_my_relay_bundle_version()?;
            // Live-channel propagation: push the changed set to currently-linked approved friends now
            // (the per-pair ntfy beacon covers unlinked ones). See r1c-rotatable-white-relay.md §B.
            self.push_relay_set_update_to_linked_contacts();
        }
        Ok(())
    }

    /// R1c live-channel propagation (PRIMARY path): a change to MY shared relay set → push the new set
    /// to every approved contact I currently hold a LIVE mesh link to. The per-pair ntfy beacon stays
    /// the FALLBACK for UNLINKED contacts. Best-effort; the sender rate-limits by version (the bump
    /// makes it send, an unchanged set is a no-op). Runs on the serialized FFI path (this is an
    /// FFI-called method), so the per-session ratchet advances are safe. See r1c-rotatable-white-relay.md.
    fn push_relay_set_update_to_linked_contacts(&self) {
        let Some(runtime) = self.mesh_runtime() else {
            return;
        };
        let contacts = match self.storage.list_contacts() {
            Ok(contacts) => contacts,
            Err(_) => return,
        };
        for contact in contacts {
            if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
                continue;
            }
            let Some(account_id) = contact.remote_account_id.as_ref() else {
                continue;
            };
            // Each device of the contact is its own node with its own session
            // (docs/decisions/multi-device-equal-nodes.md): push to every linked one.
            let bundles = self
                .storage
                .list_remote_prekey_bundles_for_account(account_id)
                .unwrap_or_default();
            for bundle in bundles {
                let Ok(node) = node_address_from_ed25519_pubkey(bundle.identity_signing_key.0) else {
                    continue;
                };
                if !runtime.has_link(&node) {
                    continue; // not linked → the unlinked per-pair beacon fallback reaches them
                }
                let session = match self.storage.get_session_record(&omni_crypto::SessionAddress {
                    remote_account_id: account_id.clone(),
                    remote_device_id: bundle.device_id.clone(),
                }) {
                    Ok(Some(session)) => session,
                    Ok(None) | Err(_) => continue,
                };
                // Address the device by the dialog key the MESH actually routes by (the per-connection
                // key the carrier's node↔dialog map holds), NOT the account dialog key — the mesh carrier
                // can't resolve the latter (`no_node_for_dialog`). Skip if there's no current mesh route.
                let Some(dialog_key) = self
                    .mesh_dialogs
                    .lock()
                    .ok()
                    .and_then(|map| map.dialog_for_node(&node))
                else {
                    continue;
                };
                let _ = send_relay_set_update_for_session(
                    &self.storage,
                    &self.transport,
                    &session,
                    Some(&dialog_key),
                );
            }
        }
    }

    /// Bump + persist my relay-bundle version (any change to my `shared` set) so the next published
    /// bundle is strictly newer than what friends already hold. Returns the new version.
    fn bump_my_relay_bundle_version(&self) -> OmniResult<u32> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let next = load_relay_bundle_version(&db_path).saturating_add(1);
        let path = relay_bundle_version_sidecar_path(&db_path);
        std::fs::write(&path, next.to_string()).map_err(|error| {
            OmniError::Storage(format!(
                "write relay-bundle version sidecar '{}': {error}",
                path.display()
            ))
        })?;
        Ok(next)
    }

    /// R1c inc.3b — MY current signed `om3relay:` bundle, built from the relays I flagged `shared` at my
    /// current bundle version; `None` when I share no relay (the presence beacon then carries no bundle,
    /// as before). This is what `build_self_beacon` attaches so an approved friend's mesh auto-learns my
    /// (possibly rotated) relay set on the next sweep. See docs/decisions/r1c-rotatable-white-relay.md.
    pub fn my_relay_bundle_token(&self) -> OmniResult<Option<String>> {
        build_my_relay_bundle_token(&self.storage, &self.storage.db_path().to_string_lossy())
    }
}

/// Build + SIGN an `om3relay:` bundle from `relays` at `bundle_version` using `identity_key`. The
/// storage-level signing core shared by the GUI export ([`OmniApp::generate_relay_bundle`], active-
/// profile key) and the rendezvous sweep's auto-publish ([`build_my_relay_bundle_token`], a
/// storage-resolved key) — one signing path, no drift. At least one relay; each needs a valid `ip:port`
/// `endpoint` and/or `tcp_endpoint`. See docs/decisions/r1c-rotatable-white-relay.md.
fn sign_relay_bundle(
    identity_key: &omni_crypto::LocalIdentityKeyPair,
    bundle_version: u32,
    issued_at_unix_ms: i64,
    relays: Vec<RelayBundleEntryInput>,
) -> OmniResult<String> {
    if relays.is_empty() {
        return Err(OmniError::InvalidInput(
            "a relay bundle needs at least one relay".to_string(),
        ));
    }
    let mut entries: Vec<RelayEntryV1> = Vec::with_capacity(relays.len());
    for relay in relays {
        let endpoint = relay.endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let tcp_endpoint = relay.tcp_endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if endpoint.is_none() && tcp_endpoint.is_none() {
            return Err(OmniError::InvalidInput(
                "a relay needs an endpoint or a tcp_endpoint".to_string(),
            ));
        }
        if let Some(ep) = endpoint {
            ep.parse::<std::net::SocketAddr>().map_err(|_| {
                OmniError::InvalidInput(format!("'{ep}' is not a valid ip:port endpoint"))
            })?;
        }
        if let Some(tcp) = tcp_endpoint {
            tcp.parse::<std::net::SocketAddr>().map_err(|_| {
                OmniError::InvalidInput(format!("'{tcp}' is not a valid ip:port tcp_endpoint"))
            })?;
        }
        let label = {
            let trimmed = relay.label.trim();
            if trimmed.is_empty() {
                endpoint.or(tcp_endpoint).unwrap_or("relay").to_string()
            } else {
                trimmed.to_string()
            }
        };
        entries.push(RelayEntryV1 {
            node: relay.node_address,
            endpoint: endpoint.map(str::to_string),
            tcp_endpoint: tcp_endpoint.map(str::to_string),
            label,
            cert_spki_pin_hex: relay
                .cert_spki_pin_hex
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    let mut bundle = RelayBundleV1 {
        version: RELAY_BUNDLE_VERSION,
        bundle_version,
        issuer_signing_pubkey_hex: hex::encode(identity_key.signing_public_key.0),
        relays: entries,
        issued_at_unix_ms,
        sig: String::new(),
    };
    let signing_bytes = relay_bundle_signing_bytes(&bundle)?;
    let signature =
        omni_crypto::sign_message(&identity_key.protected_signing_private_key, &signing_bytes)?;
    use base64::Engine as _;
    bundle.sig = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(&signature.bytes);
    encode_relay_bundle(&bundle)
}

/// MY current signed `om3relay:` bundle: the relays flagged `shared` at my current bundle version, or
/// `None` when I share none / there's no signed-in identity to sign with. Storage-level (no `&OmniApp`)
/// so the rendezvous sweep — which holds only a `Storage` + `db_path` on a background worker — can
/// publish it too; [`OmniApp::my_relay_bundle_token`] delegates here. See r1c-rotatable-white-relay.md.
fn build_my_relay_bundle_token(storage: &Storage, db_path: &str) -> OmniResult<Option<String>> {
    let shared = storage.list_shared_network_nodes()?;
    let pinned = crate::app_pinned_relay::pinned_relay_bundle_entry(db_path);
    if shared.is_empty() && pinned.is_none() {
        return Ok(None);
    }
    let Some(account) = storage.get_local_account()? else {
        return Ok(None); // no signed-in account — nothing to sign with
    };
    let Some(identity_key) = storage.get_local_identity_key(&account.descriptor.account_id)? else {
        return Ok(None);
    };
    let version = load_relay_bundle_version(db_path);
    // The pinned relay first: it is the one I go through (stage 3, public-relay-directory.md).
    let pinned_node = pinned.as_ref().and_then(|entry| entry.node_address);
    let shared = shared.into_iter().map(|node| RelayBundleEntryInput {
        node_address: node.node_address.as_deref().and_then(|bytes| <[u8; 20]>::try_from(bytes).ok()),
        endpoint: node.endpoint,
        tcp_endpoint: node.tcp_endpoint,
        label: node.label,
        cert_spki_pin_hex: node.cert_spki_pin.as_deref().map(hex::encode),
    });
    let entries: Vec<RelayBundleEntryInput> = pinned
        .into_iter()
        .chain(shared.filter(|entry| pinned_node.is_none() || entry.node_address != pinned_node))
        .collect();
    // The same set at the same version signs to the same bytes (ed25519 is deterministic).
    sign_relay_bundle(&identity_key, version, load_relay_bundle_issued_at(db_path), entries).map(Some)
}

/// The relay TCP endpoint the mesh binds its rung-0 TCP carrier to: the first registry node with
/// one (base node sorts first) that is not `own_node` — see the call in `start_mesh_internal`.
fn relay_tcp_endpoint_of(
    nodes: &[omni_storage::NetworkNodeRecord],
    own_node: &[u8; 20],
) -> Option<std::net::SocketAddr> {
    nodes
        .iter()
        .filter(|node| node.node_address.as_deref() != Some(own_node.as_slice()))
        .find_map(|node| node.tcp_endpoint.clone())
        .and_then(|endpoint| endpoint.parse::<std::net::SocketAddr>().ok())
}

/// Verify + apply an `om3relay:` bundle to the `network_nodes` registry (storage-level, no `&OmniApp`
/// — so a context holding only a `Storage` handle can apply one too, e.g. the rendezvous worker in
/// increment 3b). Verifies the issuer signature, optionally binds the issuer to
/// `expected_issuer_node_hex` (the first-party-only beacon gate; `None` for the trusted manual paste),
/// applies the strict-newer rotation guard, then REPLACES the issuer's prior set. Returns relays
/// imported (`0` = not newer / ignored). A malformed relay entry is skipped, not fatal.
fn apply_relay_bundle(
    storage: &Storage,
    token: &str,
    expected_issuer_node_hex: Option<&str>,
) -> OmniResult<usize> {
    let bundle = parse_relay_bundle(token)?;
    let bundle_id = verify_relay_bundle(&bundle)?;
    if let Some(expected) = expected_issuer_node_hex {
        if !bundle_id.eq_ignore_ascii_case(expected) {
            return Err(OmniError::InvalidInput(
                "relay bundle issuer is not the propagating contact (first-party-only)".to_string(),
            ));
        }
    }
    if bundle.relays.is_empty() {
        return Err(OmniError::InvalidInput(
            "relay bundle carries no relays".to_string(),
        ));
    }
    // Rotation guard: ignore a bundle that isn't strictly newer than what we already hold.
    if let Some(current) = storage.max_bundle_version(&bundle_id)? {
        if i64::from(bundle.bundle_version) <= current {
            return Ok(0);
        }
    }
    // Replace the issuer's prior set, then insert the new relays as one bundle generation.
    storage.remove_network_nodes_by_bundle(&bundle_id)?;
    let added_at = current_unix_ms();
    let bundle_version = i64::from(bundle.bundle_version);
    let mut imported = 0usize;
    for relay in &bundle.relays {
        let endpoint = relay.endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let tcp_endpoint = relay.tcp_endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty());
        // Skip a malformed entry rather than abort the whole bundle — a friend still gets the
        // relays that ARE dialable.
        if endpoint.is_none() && tcp_endpoint.is_none() {
            continue;
        }
        if endpoint.is_some_and(|ep| ep.parse::<std::net::SocketAddr>().is_err()) {
            continue;
        }
        if tcp_endpoint.is_some_and(|tcp| tcp.parse::<std::net::SocketAddr>().is_err()) {
            continue;
        }
        let cert_spki_pin = match relay.cert_spki_pin_hex.as_deref().map(str::trim) {
            Some(h) if !h.is_empty() => Some(hex::decode(h).map_err(|error| {
                OmniError::InvalidInput(format!("relay bundle spki pin is not hex: {error}"))
            })?),
            _ => None,
        };
        let label = {
            let trimmed = relay.label.trim();
            if trimmed.is_empty() {
                endpoint.or(tcp_endpoint).unwrap_or("relay")
            } else {
                trimmed
            }
        };
        storage.add_network_node(&omni_storage::NewNetworkNode {
            endpoint,
            node_address: relay.node.as_ref().map(|node| node.as_slice()),
            label,
            source: omni_storage::NETWORK_NODE_SOURCE_MANUAL,
            added_at_unix_ms: added_at,
            tcp_endpoint,
            cert_spki_pin: cert_spki_pin.as_deref(),
            bundle_id: Some(&bundle_id),
            bundle_version: Some(bundle_version),
        })?;
        imported += 1;
    }
    Ok(imported)
}

/// Pairing-over-mesh, finishing half (importer/initiator side). When `peer` is a
/// deliberately imported invite target ([`OmniApp::register_mesh_pairing_invite`]),
/// turn the `SessionHello` it sent on the new Link into an **outgoing-Pending**
/// contact plus an **initiator** X3DH session, install the dialog→node route, and
/// send the session-bootstrap envelope over the Link. Receiving that bootstrap
/// makes the peer establish a responder session, create its own incoming-Pending
/// contact (`persist_incoming_contact_request_signal`), and ack back — which
/// advances us to `Established`. The peer then approves and the approval rides
/// back, promoting both contacts to `Approved`. This is the same first-message
/// handshake the send queue uses; no separate encrypted contact-request ping is
/// needed — the unencrypted bootstrap is the X3DH first frame and already carries
/// the contact request.
///
/// The bootstrap is sent on a **dedicated std thread**, not inline and not via the
/// `SessionActivated` trigger:
/// - `on_wire` runs on the mesh tokio runtime, and mesh sends use `block_on`,
///   which panics if called from within that runtime — so the send must hop to a
///   plain thread.
/// - the trigger only re-sends an encrypted request once the session is already
///   `Established`; the fresh initiator session is `InitiatorBootstrapCreated`,
///   so the trigger can't drive the initial handshake. It stays a backstop.
///
/// Best-effort and **one-shot**: the target is atomically claimed (removed) once
/// a usable hello arrives, so a duplicate hello can't double-create the
/// contact/session (which would reset and desync the X3DH handshake). Any failure
/// is traced and the caller still registers the raw connection; the user
/// re-imports the invite to retry.
///
/// Deliberately does NOT persist the remote prekey bundle — `handle_session_activated`
/// records it from the same receive context. And it does NOT enforce the TM-008
/// Parse a 20-byte mesh NodeAddress from its hex string. The IPv4/IPv6 route-detection helpers moved to
/// `omni_mesh_runtime` (`detect_primary_lan_ipv4`/`detect_global_ipv6`/`is_global_ipv6`) so the runtime
/// can re-detect the live network each announce cycle (roaming); the app imports them from there.
fn parse_mesh_node_hex(node_hex: &str) -> OmniResult<MeshNodeAddress> {
    let bytes: [u8; 20] = hex::decode(node_hex.trim())
        .map_err(|error| OmniError::InvalidInput(format!("node address is not valid hex: {error}")))?
        .try_into()
        .map_err(|_| OmniError::InvalidInput("node address must be 20 bytes".to_string()))?;
    Ok(MeshNodeAddress::from_bytes(bytes))
}

/// Build a `ReceivedChatContext` from a SELF-CONTAINED `SessionBootstrapMessageV1`, so the receiver
/// can process a FIRST-CONTACT bootstrap that arrived WITHOUT a preceding `SessionHello` (durable
/// store-and-forward over a lossy link — the mobile-UDP-drop path). The `dialog_key` is derived from
/// the initiator's instance id (now carried in the bootstrap), so it MATCHES the one a later hello
/// would produce → no duplicate dialog. The reconstructed prekey bundle carries the initiator's REAL
/// account + device descriptors (from the bootstrap's own fields); only `signed_prekey` is a
/// placeholder, and the first-contact receive path (`persist_incoming_contact_request_signal` reads
/// only account_id + device) never touches it — the responder session comes from the bootstrap's own
/// X3DH material. Returns `None` if the bootstrap carries no instance id (an older sender) — then the
/// drop stands and the hello path is the only route. See docs/decisions/mesh-durable-delivery.md.
fn received_context_from_bootstrap(
    message: &omni_types::SessionBootstrapMessageV1,
) -> Option<omni_transport::ReceivedChatContext> {
    if message.initiator_instance_id.trim().is_empty() {
        return None;
    }
    let dialog_key = omni_types::dialog_key_from_instance_id(&message.initiator_instance_id);
    Some(omni_transport::ReceivedChatContext {
        peer_id: String::new(),
        dialog_key,
        remote_instance_id: message.initiator_instance_id.clone(),
        remote_prekey_bundle: Some(PrekeyBundleV1 {
            version: PREKEY_BUNDLE_VERSION_V1,
            account: AccountCryptoDescriptorV1 {
                version: ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
                account_id: message.initiator_account_id.clone(),
                identity_public_key: message.initiator_identity_public_key.clone(),
                identity_signing_public_key: message.initiator_identity_signing_public_key.clone(),
            },
            device: DeviceCryptoDescriptorV1 {
                version: DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1,
                device_id: message.initiator_device_id.clone(),
                account_id: message.initiator_account_id.clone(),
                // The bootstrap carries no human-readable device label; the observation the pending
                // is created from requires a NON-empty one. Use the device id as a stable placeholder
                // — the real label arrives with the first public-profile sync (same as the petname,
                // which is the account id until then).
                device_label: message.initiator_device_id.clone(),
                device_public_key: message.initiator_device_public_key.clone(),
            },
            // Placeholder: never read on the first-contact receive path (the responder X3DH session
            // is built from the bootstrap itself). A real bundle arrives later via a hello / profile sync.
            signed_prekey: omni_protocol::SignedPrekeyDescriptorV1 {
                prekey_id: 0,
                public_key: String::new(),
                signature_r: String::new(),
                signature_s: String::new(),
                expires_at_unix_ms: None,
            },
            one_time_prekey: None,
            generated_at_unix_ms: message.created_at_unix_ms,
            device_certificate_chain_json: message.initiator_device_certificate_chain_json.clone(),
        }),
        initiated_locally: false,
    })
}

fn finish_mesh_pairing_from_hello(
    db_path: &str,
    pairing_targets: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    dialogs: &Arc<Mutex<MeshDialogMap>>,
    transport: &omni_transport::TransportManager,
    peer: MeshNodeAddress,
    hello: &omni_types::SessionHelloV1,
) {
    // Only deliberate invite targets take this path — discovery never pairs.
    let is_target = pairing_targets
        .lock()
        .map(|set| set.contains(&peer))
        .unwrap_or(false);
    if !is_target {
        return;
    }
    // A hello must carry a valid prekey bundle to pair; otherwise leave the
    // target in place and wait for a usable hello.
    let bundle =
        match omni_transport::validate_remote_prekey_bundle(hello.prekey_bundle_json.as_deref()) {
            Ok(Some(bundle)) => bundle,
            Ok(None) => {
                omni_app_trace_log("mesh_pairing/finish", "skip: hello carried no prekey bundle");
                return;
            }
            Err(error) => {
                omni_app_trace_log(
                    "mesh_pairing/finish",
                    &format!("skip: invalid remote prekey bundle: {error}"),
                );
                return;
            }
        };
    // Claim the target one-shot now that we have a usable hello: a concurrent or
    // duplicate hello that loses this race sees `false` and bails.
    let claimed = pairing_targets
        .lock()
        .map(|mut set| set.remove(&peer))
        .unwrap_or(false);
    if !claimed {
        return;
    }

    let account_id = bundle.account.account_id.clone();
    let dialog_key = omni_types::dialog_key_from_instance_id(&hello.instance_id);
    let storage = match Storage::open(db_path) {
        Ok(storage) => storage,
        Err(error) => {
            omni_app_trace_log(
                "mesh_pairing/finish",
                &format!("abort: storage open failed: {error}"),
            );
            return;
        }
    };
    let now_unix_ms = current_unix_ms();
    // The invite is spent once we hold its peer — never re-arm it on the next start.
    let _ = storage.remove_bootstrap_invites_for_inviter(peer.as_bytes());

    // Already paired (an approved contact for this account): a stale invite must NOT pair again.
    // Re-pairing creates a NEW initiator session and sends its bootstrap, while the peer — whose
    // session is established — keeps its own: the two sides diverge and every 1:1 frame (presence,
    // chat, call signalling) dies until someone re-pairs by hand. Found on the live 3-node pass.
    let already_paired = storage
        .find_contact_by_remote_account_id(&omni_identity::AccountId(account_id.clone()))
        .ok()
        .flatten()
        .is_some_and(|contact| {
            contact.removed_at_unix_ms.is_none()
                && contact.relationship_state == omni_identity::ContactRelationshipState::Approved
        });
    if already_paired {
        omni_app_trace_log(
            "mesh_pairing/finish",
            &format!("skip: account={account_id} is already an approved contact (stale invite)"),
        );
        return;
    }

    // Outgoing-Pending contact for the invited peer. The petname is an account-id
    // placeholder (mirrors the incoming side); the real display name arrives later
    // via public-profile sync.
    let remote_instance_id = hello.instance_id.trim();
    let contact_input = omni_identity::AddContactInput {
        petname: account_id.clone(),
        remote_account_id: Some(omni_identity::AccountId(account_id.clone())),
        public_display_name: None,
        remote_instance_id: (!remote_instance_id.is_empty()).then(|| remote_instance_id.to_string()),
        primary_dialog_key: Some(dialog_key.clone()),
        initial_observation: None,
    };
    if let Err(error) = storage.add_outgoing_pending_contact(&contact_input, now_unix_ms) {
        omni_app_trace_log(
            "mesh_pairing/finish",
            &format!("abort: add_outgoing_pending_contact failed: {error}"),
        );
        return;
    }

    // Establish the initiator X3DH session from the hello's bundle. The first
    // message encrypted with it carries the X3DH bootstrap, so sending the
    // contact-request ping below both establishes the peer's responder session
    // and delivers the request. Reuses the queue's self-less bootstrap.
    let domain_bundle = match prekey_bundle_from_protocol(&bundle) {
        Ok(domain_bundle) => domain_bundle,
        Err(error) => {
            omni_app_trace_log(
                "mesh_pairing/finish",
                &format!("abort: prekey bundle conversion failed: {error}"),
            );
            return;
        }
    };
    let bootstrap = match bootstrap_session_for_remote_bundle(&storage, &domain_bundle, now_unix_ms) {
        Ok((_, bootstrap, _)) => bootstrap,
        Err(error) => {
            omni_app_trace_log(
                "mesh_pairing/finish",
                &format!("abort: initiator session bootstrap failed: {error}"),
            );
            return;
        }
    };

    // Install the dialog→node route now, so the bootstrap send below (and any
    // later wire send) resolves this mesh Link via the dialog map — the mesh
    // `PeerCarrier` has no contact-identity fallback.
    if let Ok(mut map) = dialogs.lock() {
        map.insert(dialog_key.clone(), peer);
    }
    // Persist the node↔dialog binding (the durable twin of the in-memory map) so a
    // later store-and-forward frame from this peer can be decrypted even if we
    // never re-link this session. See docs/decisions/mesh-durable-delivery.md.
    if let Err(error) = storage.set_mesh_dialog_route(peer.as_bytes(), &dialog_key) {
        omni_app_trace_log(
            "mesh_pairing/finish",
            &format!("warn: persist mesh_dialog_route failed: {error}"),
        );
    }
    omni_app_trace_log(
        "mesh_pairing/finish",
        &format!("paired: account={account_id} dialog_key={dialog_key}"),
    );

    // Kick off the X3DH handshake off-tokio (see the fn doc): `on_wire` runs on
    // the mesh tokio runtime and the wire send does `block_on`. Sending the
    // session-bootstrap envelope makes the peer establish a responder session,
    // create its incoming-Pending contact, and ack back (advancing us to
    // Established) — the same first-message path the send queue uses. One-shot,
    // fire-and-forget.
    let transport = transport.clone();
    let send_dialog_key = dialog_key.clone();
    std::thread::spawn(move || {
        let envelope = match omni_messaging::wrap_session_bootstrap(bootstrap) {
            Ok(envelope) => envelope,
            Err(error) => {
                omni_app_trace_log(
                    "mesh_pairing/finish",
                    &format!("send abort: wrap session bootstrap failed: {error}"),
                );
                return;
            }
        };
        match transport.send_wire_envelope_for_dialog_key(send_dialog_key.as_str(), &envelope) {
            Ok(()) => omni_app_trace_log(
                "mesh_pairing/finish",
                &format!("session bootstrap sent over mesh dialog_key={send_dialog_key}"),
            ),
            Err(error) => omni_app_trace_log(
                "mesh_pairing/finish",
                &format!("session bootstrap send failed: {error}"),
            ),
        }
        // Durable backstop (relay-less flaky-link fix, 2026-07-20). The live send above rides the
        // mesh Link when one exists, but a first-contact link over a UDP-hostile mobile carrier can
        // die a beat later (the provider drops the UDP flow after a few packets) — and the request
        // frame goes with it, one-shot, lost forever (the live symptom: sender "sent", receiver got
        // nothing). ALSO hold the bootstrap in durable store-and-forward so the mesh reconcile loop
        // re-delivers it over WHATEVER link survives (a later UDP burst or the TCP-direct front
        // door). Delivering the SAME bootstrap envelope twice (live + durable) is safe: it is one
        // identical X3DH first frame, i.e. a retransmit — the responder derives the same session
        // idempotently and `persist_incoming_contact_request_signal` merges the pending rather than
        // duplicating it. (The desync guard elsewhere is against RE-CREATING a bootstrap with a new
        // ephemeral, which this is not.) See docs/decisions/mesh-durable-delivery.md.
        let held = transport.hold_durable_for_dialog_key(send_dialog_key.as_str(), &envelope);
        omni_app_trace_log(
            "mesh_pairing/finish",
            &format!("session bootstrap durable backstop dialog_key={send_dialog_key} held={held}"),
        );
    });
}

/// One auto-connect tick: dial each approved contact that has a known identity
/// (prekey bundle) and isn't already linked. Best-effort — an undiscovered peer
/// returns immediately (no path) and is retried next tick.
/// Always-connect: keep the user-managed network-node registry (base bootstrap + manual
/// relays that carry a PINNED identity) LINKED — re-dial any whose Link is currently down.
/// The registry is dialed once at `start_mesh_internal`, but a relay link can drop later: a
/// mobile-NAT outage longer than the liveness window prunes it (cascading to every peer
/// routed through it), or the relay process restarts. Without a periodic re-dial nothing
/// re-establishes it, so a both-NAT pair silently loses its only path and never recovers
/// (observed live: chat stops after a relay restart / mobile blip and stays dead). `dial_at`
/// is a no-op when already linked and now discards a dead cached connection before
/// re-dialing (warm-rejoin), so this self-heals the relay link within one autoconnect tick.
/// Endpoint-only (hint) entries with no pinned identity are skipped. See
/// docs/bugs/relay-pairing-approval-gap.md + docs/notes/roadmap-CONTINUE.md.
fn autoconnect_redial_network_nodes(storage: &Storage, runtime: &MeshRuntime) {
    for node in storage.list_network_nodes().unwrap_or_default() {
        let (Some(endpoint), Some(node_bytes)) = (node.endpoint, node.node_address) else {
            continue; // hint-only entry (no pinned identity) — skip until learn-on-connect
        };
        let (Ok(raw), Ok(addr)) = (
            <[u8; 20]>::try_from(node_bytes.as_slice()),
            endpoint.parse::<std::net::SocketAddr>(),
        ) else {
            continue;
        };
        let node_addr = MeshNodeAddress::from_bytes(raw);
        // R1a connection-quality ladder: hold the carrier link(s) per policy. Dual-carrier (this
        // relay is the bound rung-0 TCP anchor) keeps a TCP base + an additive UDP A↔R link, so
        // `upgrade_relay_carriers` can climb. Idempotency lives INSIDE `autoconnect_network_node`
        // (the TCP base is skipped when linked; the UDP carrier is added when missing) — so we must
        // NOT short-circuit on `has_link` here, or the rung-1 UDP link would never be added once the
        // TCP base is up. See docs/decisions/connection-quality-ladder.md.
        let tcp_addr = node
            .tcp_endpoint
            .and_then(|t| t.parse::<std::net::SocketAddr>().ok());
        let _ = runtime.autoconnect_network_node(node_addr, addr, tcp_addr);
    }
}

/// Занять место под дозвон до `node`, если его ещё никто не занял. `false` — дозвон уже идёт.
///
/// Без этого неблокирующий дозвон превращается в лавину: такт цикла три секунды, неудачный дозвон
/// до тридцати, и на ОДИН недостижимый контакт пришлось бы десять параллельных попыток — каждая со
/// своим TCP/QUIC-соединением. Место освобождается в `release_dial_slot`, когда попытка завершилась
/// любым исходом.
fn claim_dial_slot(
    in_flight: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    node: MeshNodeAddress,
) -> bool {
    match in_flight.lock() {
        // `insert` возвращает false, если узел уже был в множестве, — это и есть «занято».
        Ok(mut set) => set.insert(node),
        // Отравленный мьютекс: не дозваниваемся вовсе, чем плодим потоки вслепую.
        Err(_) => false,
    }
}

fn release_dial_slot(
    in_flight: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    node: &MeshNodeAddress,
) {
    if let Ok(mut set) = in_flight.lock() {
        set.remove(node);
    }
}

/// Re-dial a lost contact at its LAST-GOOD direct endpoints (the warm-rejoin cache).
///
/// `dial` only knows the peer book (announces, seeds); an address learned through rendezvous never
/// lands there. So after an outage a contact reached across the internet was not re-dialed at its
/// unchanged address until the next rendezvous sweep found a FRESH beacon — live 2026-10-06: the
/// PC came back at the same `ip:port`, and the laptop waited ~2 min (two sweeps, the first finding
/// no beacon yet). The warm-rejoin cache already holds that endpoint; start-up re-dials it, and now
/// a drop does too. Endpoints are hints — the identity challenge is the authority.
fn redial_last_good_endpoints(runtime: &MeshRuntime, db_path: &str, node: MeshNodeAddress) {
    let Ok(storage) = Storage::open(db_path) else {
        return;
    };
    let endpoints: Vec<std::net::SocketAddr> = storage
        .load_mesh_peer_book(current_unix_ms())
        .unwrap_or_default()
        .into_iter()
        .filter(|(node_bytes, _)| node_bytes.as_slice() == node.as_bytes())
        .filter_map(|(_, endpoint)| endpoint.parse().ok())
        .collect();
    if endpoints.is_empty() {
        return;
    }
    // Bounded: this runs on the one dial thread every contact shares.
    let linked = runtime
        .dial_at_within(node, endpoints.clone(), std::time::Duration::from_secs(8))
        .is_ok()
        && runtime.has_link(&node);
    omni_app_trace_log(
        "mesh/redial-last-good",
        &format!("node={node} endpoints={endpoints:?} linked={linked}"),
    );
}

/// [`redial_last_good_endpoints`] on its own thread; at most one per contact at a time.
fn spawn_last_good_redial(
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    node: MeshNodeAddress,
    in_flight: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
) {
    if !in_flight.lock().map(|mut set| set.insert(node)).unwrap_or(false) {
        return; // the previous attempt for this contact is still running
    }
    let runtime = Arc::clone(runtime);
    let db_path = db_path.to_string();
    let in_flight_done = Arc::clone(in_flight);
    let spawned = std::thread::Builder::new()
        .name("omni-mesh-redial".to_string())
        .spawn(move || {
            redial_last_good_endpoints(&runtime, &db_path, node);
            if let Ok(mut set) = in_flight_done.lock() {
                set.remove(&node);
            }
        });
    if spawned.is_err() {
        if let Ok(mut set) = in_flight.lock() {
            set.remove(&node);
        }
    }
}

/// Backoff for [`redial_last_good_endpoints`] per contact: 3 s doubling to 10 min, reset when
/// linked — prompt after a drop, but an address that stays dead is not hammered every tick.
#[derive(Default)]
struct LastGoodRedial {
    next: std::collections::HashMap<MeshNodeAddress, (std::time::Instant, std::time::Duration)>,
}

impl LastGoodRedial {
    const FIRST: std::time::Duration = std::time::Duration::from_secs(3);
    // The cache keeps an endpoint 48 h; a contact that stays gone must not take the shared dial
    // thread for 8 s every minute of that. The rendezvous sweep (~90 s) covers the long tail.
    const MAX: std::time::Duration = std::time::Duration::from_secs(600);

    fn linked(&mut self, node: &MeshNodeAddress) {
        self.next.remove(node);
    }

    /// Whether to try now; if so, schedules the next try.
    fn due(&mut self, node: &MeshNodeAddress, now: std::time::Instant) -> bool {
        let (at, pause) = self.next.get(node).copied().unwrap_or((now, Self::FIRST));
        if now < at {
            return false;
        }
        self.next.insert(*node, (now + pause, (pause * 2).min(Self::MAX)));
        true
    }
}

/// Every node the autoconnect worker keeps a link to: each known device of each approved contact
/// (a contact is an account, and every device of it is its own node), and our own other devices
/// (docs/decisions/multi-device-equal-nodes.md). A node's address is the Ed25519 identity signing
/// key of that device — from its prekey bundle, or from its certificate for our own devices.
/// A DHT-proxy request from one of our own devices or an approved contact's device — the nodes we
/// keep links with (`autoconnect_targets`). Reads the profile fresh: requests are rare (a sweep).
fn dht_proxy_origin_is_ours(db_path: &str, origin: &MeshNodeAddress) -> bool {
    Storage::open(db_path)
        .ok()
        .and_then(|storage| autoconnect_targets(&storage).ok())
        .is_some_and(|targets| targets.iter().any(|(_, node)| node == origin))
}

fn autoconnect_targets(storage: &Storage) -> OmniResult<Vec<(AccountId, MeshNodeAddress)>> {
    // Runs every tick over every contact: one query, and a set for the duplicates (a linear scan
    // here made the tick quadratic in the number of contacts).
    let mut targets: Vec<(AccountId, MeshNodeAddress)> = Vec::new();
    let mut seen: std::collections::HashSet<MeshNodeAddress> = std::collections::HashSet::new();
    let mut add = |account: &AccountId, signing_key: &[u8; 32]| {
        if let Ok(node) = node_address_from_ed25519_pubkey(*signing_key) {
            if seen.insert(node) {
                targets.push((account.clone(), node));
            }
        }
    };
    // A contact with no bundle yet has no row; it is picked up on a later tick.
    for (remote_account_id, signing_key) in storage.list_approved_contact_device_signing_keys()? {
        add(&remote_account_id, &signing_key.0);
    }
    if let (Some(account), Some(device)) = (storage.get_local_account()?, storage.get_local_device()?) {
        for record in storage.list_account_devices(&account.descriptor.account_id)? {
            if record.chain.device.device_id == device.device_id || record.revoked_at_unix_ms.is_some() {
                continue;
            }
            add(
                &account.descriptor.account_id,
                &record.chain.device.identity_signing_public_key.0,
            );
        }
    }
    Ok(targets)
}

/// Who the mesh keeps a live link with (docs/bugs/autoconnect-holds-a-link-to-every-contact.md, owner's
/// choice 2026-10-14): our own devices always; of the contacts, the active circle — added, heard from
/// or written with in the last `ACTIVE_CIRCLE_WINDOW_MS`, something queued for them, their chat open,
/// or a holder of our account passport.
/// A link to every approved contact, all the time, does not scale to hundreds: keepalives, NAT
/// entries, a dial queue forever busy with the absent. The rest are reached by durable delivery and
/// the couriers' beacons, and join the circle as soon as there is something to send.
#[derive(Default)]
struct ActiveCircle {
    accounts: std::collections::HashSet<AccountId>,
    refreshed_at: Option<std::time::Instant>,
}

/// How long a contact stays in the active circle after the last sign of life between us.
const ACTIVE_CIRCLE_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// How often the circle is recomputed: a queued message or an opened chat brings a contact in within it.
const ACTIVE_CIRCLE_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

impl ActiveCircle {
    fn refresh_if_due(&mut self, storage: &Storage) {
        if self.refreshed_at.is_some_and(|at| at.elapsed() < ACTIVE_CIRCLE_REFRESH) {
            return;
        }
        self.refreshed_at = Some(std::time::Instant::now());
        let Ok(mut accounts) = storage
            .list_recently_active_contact_accounts(current_unix_ms() - ACTIVE_CIRCLE_WINDOW_MS)
            .map(|list| list.into_iter().collect::<std::collections::HashSet<_>>())
        else {
            return; // keep the last circle on a transient read error
        };
        // The holders of our account passport: a few, chosen, and the passport round reaches them
        // only over a live link (account-recovery-from-friends.md).
        for holder in storage.list_passport_holders().unwrap_or_default() {
            accounts.insert(holder.account_id);
        }
        for lease in list_open_chat_session_leases(storage).unwrap_or_default() {
            if let Ok(Some(account)) = storage
                .resolve_contact_for_dialog(&lease.dialog_key)
                .map(|contact| contact.and_then(|contact| contact.remote_account_id))
            {
                accounts.insert(account);
            }
        }
        self.accounts = accounts;
    }
}

/// Links with everyone in our calls, contacts or not (group membership is what admits them —
/// docs/decisions/group-trust-model.md). Not marked as dialed for a chat session: a link is all the
/// call needs, and a member who is not a contact gets no 1:1 hello from us.
fn dial_group_call_members(
    db_path: &str,
    runtime: &Arc<MeshRuntime>,
    dials_in_flight: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    dial_queue: &mpsc::Sender<MeshNodeAddress>,
) {
    for node in crate::app_group_calls::group_call_member_nodes(db_path) {
        let node = MeshNodeAddress::from_bytes(node);
        if runtime.has_link(&node) || !claim_dial_slot(dials_in_flight, node) {
            continue;
        }
        if dial_queue.send(node).is_err() {
            release_dial_slot(dials_in_flight, &node);
            return;
        }
    }
}

fn autoconnect_dial_approved_contacts(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    locally_dialed: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    dials_in_flight: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    dial_queue: &mpsc::Sender<MeshNodeAddress>,
    active_circle: &std::collections::HashSet<AccountId>,
) -> OmniResult<()> {
    if !storage.is_profile_signed_in()? {
        return Ok(());
    }
    let db_path = storage.db_path().to_string_lossy().to_string();
    let own_account = storage.get_local_account()?.map(|account| account.descriptor.account_id);
    let mut targets = autoconnect_targets(storage)?;
    targets.retain(|(account, _)| Some(account) == own_account.as_ref() || active_circle.contains(account));
    // The connection journey is kept per account (one contact, one journey), while each device is
    // its own target: with one device linked and another away, recording both made the journey
    // alternate "linked" / "looking on the LAN" every tick — a contact that looked like it kept
    // dropping. An account with a linked device is reachable; its other devices are dialed quietly.
    let linked_accounts: std::collections::HashSet<AccountId> = targets
        .iter()
        .filter(|(_, node)| runtime.has_link(node))
        .map(|(account, _)| account.clone())
        .collect();
    for (account_id, node) in targets {
        let remote_account_id = &account_id;
        if runtime.has_link(&node) {
            // Terminal success — record it (de-duped) so the trace shows the journey ending in a
            // verified link, with how it's carried (direct vs via the mesh).
            let detail = match runtime.peer_link_quality(&node) {
                Some(MeshLinkQuality::Relay { hops }) => format!("через сеть, {hops} хоп(ов)"),
                _ => "напрямую".to_string(),
            };
            record_connection_step(
                &db_path,
                &remote_account_id.0,
                ConnectionStage::Linked,
                ConnectionStepStatus::Ok,
                Some(detail),
            );
            continue;
        }
        if !linked_accounts.contains(remote_account_id) {
            record_connection_step(
                &db_path,
                &remote_account_id.0,
                ConnectionStage::Lan,
                ConnectionStepStatus::Started,
                Some("ищу в локальной сети".to_string()),
            );
        }
        // Record initiator intent before dialing so the peer's hello registers as
        // initiated_locally = true (mirrors `mesh_connect_dialog`).
        if let Ok(mut dialed) = locally_dialed.lock() {
            dialed.insert(node);
        }
        // ДОЗВОН УХОДИТ С ЦИКЛА.
        //
        // Здесь стоял `runtime.dial(node)` — синхронный, с таймаутом в 30 с на недостижимый адрес.
        // Такт цикла объявлен трёхсекундным, и замер 2026-08-30 показал, во что это обходилось:
        //
        //   tick=30268ms  pairing=0ms nodes=0ms contacts=30001ms budget=3000ms
        //   tick=97006ms  pairing=0ms nodes=0ms contacts=97001ms budget=3000ms
        //
        // Такт до 97 с, и на 100% это дозвоны — по 30 с каждый, ПОДРЯД. Пока цикл стоял, не делалось
        // ничего из того, что на нём живёт: ни дозвон целей пейринга, ни поддержание линков с
        // релеями, ни проверка условия sweep (отсюда рандеву раз в 183-243 с при объявленных
        // 90-120). Один недостижимый контакт останавливал обслуживание всей сети.
        // См. docs/bugs/the-maintenance-loop-stalls-on-a-dead-address.md.
        //
        // Место под дозвон занимается ДО потока: без этого попытка переиздавалась бы каждые три
        // секунды поверх ещё идущей.
        if !claim_dial_slot(dials_in_flight, node) {
            continue;
        }
        if dial_queue.send(node).is_err() {
            // Поток дозвона ушёл (меш останавливается) — место надо вернуть, иначе оно останется
            // занятым навсегда и контакт больше никогда не будет дозвонен.
            release_dial_slot(dials_in_flight, &node);
            break;
        }
    }
    Ok(())
}

/// Serverless-rendezvous fallback for the autoconnect worker: for every approved contact
/// the mesh has NO link to — announce/LAN dialing couldn't reach it, e.g. it's on another
/// network — publish my presence beacon and dial its reflexive endpoints over public
/// rendezvous providers (each via a windowless background dial, off this loop). Skips
/// contacts already linked, with no stored bundle/session, or whose session predates the
/// rendezvous pair secret. Re-opened-`Storage` safe: the pair secret unseals via DPAPI
/// (the OS user), needing no in-memory profile key — so it works here in the worker. The
/// caller throttles this to `RENDEZVOUS_SWEEP_INTERVAL_SECS` (free providers). See
/// docs/decisions/serverless-bootstrap-rendezvous.md.
/// Whether a live link is DIRECT — so the rendezvous sweep can stop for that contact. A `Relay` link is
/// NOT terminal (we keep rendezvousing to learn + dial the peer's advertised endpoints for a relay→direct
/// upgrade); `None` (linked, no path snapshot) counts as direct, matching `mesh_connection_state`.
fn rendezvous_link_is_direct(quality: Option<MeshLinkQuality>) -> bool {
    !matches!(quality, Some(MeshLinkQuality::Relay { .. }))
}

/// Whether the link to `node` is as good as this profile allows, so rendezvous can leave it alone:
/// direct — or, when direct is off («Только через релей», a pinned relay), any link. Looking for a
/// direct path the gate will refuse spent a courier read on that contact every minute (stand
/// 2026-10-14: 557 reads an hour with two contacts).
/// Whether a sweep has nothing to do for this contact: its beacons were read and the link is where
/// it should be — unless it rides the peer's TCP door, which is never where it should be.
///
/// A door link is retried on UDP even when direct links are off: with direct off, the links kept
/// direct are the ones to our relays, and only they can be on a door. Skipping them pinned the link
/// to the relay on TCP for good (stand 2026-10-15: laptop pinned to the PC relay, `carrier=tcp_door`
/// on a network that passes UDP, until a restart dialled UDP afresh).
fn rendezvous_already_there(beacons_read: bool, link_is_the_destination: bool, on_door: bool) -> bool {
    beacons_read && link_is_the_destination && !on_door
}

fn rendezvous_link_is_the_destination(runtime: &MeshRuntime, node: &MeshNodeAddress) -> bool {
    runtime.has_link(node)
        && (!runtime.allows_direct_punch() || rendezvous_link_is_direct(runtime.peer_link_quality(node)))
}

/// How reachable a contact currently is, for the event-triggered rendezvous upgrade. Ordered
/// worst→best so a "worsening" transition is just `curr != prev && curr != Direct`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContactLinkGrade {
    /// No live link at all.
    None,
    /// Reachable only via a relay hop (a fallback, not the destination).
    Relay,
    /// A direct link — the destination; nothing to upgrade.
    Direct,
}

fn contact_link_grade(has_link: bool, quality: Option<MeshLinkQuality>) -> ContactLinkGrade {
    if !has_link {
        ContactLinkGrade::None
    } else if matches!(quality, Some(MeshLinkQuality::Relay { .. })) {
        ContactLinkGrade::Relay
    } else {
        ContactLinkGrade::Direct
    }
}

/// Minimum gap between EVENT-triggered rendezvous attempts for the SAME peer — a flap guard. The
/// periodic sweep (~90 s) is the steady-state cadence; this fires an early upgrade attempt when a
/// peer's link degrades, without letting a flapping peer hammer the free rendezvous providers.
const EVENT_RENDEZVOUS_PER_PEER_FLOOR: std::time::Duration = std::time::Duration::from_secs(20);

/// Should we fire an EVENT-triggered rendezvous for a peer whose link just changed? Only on a real
/// transition to a non-direct state (`direct→relay`, `direct→none`, `relay→none`, `none→relay`) that
/// we actually witnessed (a first sighting is left to the periodic sweep, so startup doesn't burst),
/// and only if the per-peer flap floor has elapsed. Pure + unit-tested.
fn should_event_rendezvous(
    prev: Option<ContactLinkGrade>,
    curr: ContactLinkGrade,
    since_last_event: Option<std::time::Duration>,
) -> bool {
    let Some(prev) = prev else {
        return false; // first sighting — the periodic sweep handles the initial state
    };
    if curr == ContactLinkGrade::Direct || curr == prev {
        return false; // already direct, or no change → nothing to upgrade
    }
    !matches!(since_last_event, Some(elapsed) if elapsed < EVENT_RENDEZVOUS_PER_PEER_FLOOR)
}

/// A contact rendezvoused this recently is not rendezvoused again on a degrade (`early_rendezvous_contacts`).
const RENDEZVOUS_JUST_LOOKED: std::time::Duration = std::time::Duration::from_secs(15);

/// When to look again at a contact a rendezvous left WITHOUT a direct link: after the k-th attempt
/// of an episode wait `RENDEZVOUS_RETRY_AFTER[k-1]`, then the periodic sweep takes over again.
///
/// Live 2026-10-03, both machines starting together: the first attempt (~22 s) read the peer's
/// beacon from its PREVIOUS run, dialed ports that had died with it and failed — the peer's fresh
/// beacon was up seconds later, but nobody looked until the next sweep 90–120 s on, so the link took
/// 121–237 s (docs/bugs/dead-ports-queue-in-front-of-the-live-one.md §«Живой A/B»). Two short looks
/// bridge that gap. Bounded on purpose: a contact that stays offline costs exactly two extra
/// rendezvous per episode (each one hits the free providers), not a faster sweep forever.
const RENDEZVOUS_RETRY_AFTER: [std::time::Duration; 2] =
    [std::time::Duration::from_secs(30), std::time::Duration::from_secs(60)];

/// When each contact was last HEARD: a link of any kind, or its fresh beacon found on our pair topic.
/// Written by the autoconnect worker and by the rendezvous dial threads (`spawn_rendezvous_dial`).
pub(crate) type PeerHeardLog = Arc<Mutex<std::collections::HashMap<[u8; 20], std::time::Instant>>>;

/// How long a contact may stay silent before its periodic rendezvous starts thinning out.
const SILENCE_BEFORE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// The longest gap between two periodic rendezvous for a silent contact. Two hours (maintainers' choice,
/// 2026-10-14; was 30 min): a contact silent for days cost ~48 publication rounds a day, and a
/// thousand of them got the couriers' 429. A returning contact is not slowed by it — its own app
/// starts with no silence and finds us at once.
const SILENT_SWEEP_MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

/// Is the periodic rendezvous due for a contact silent for `silent_for`, `since_last_sweep` after its
/// previous one (`None` = none yet)? Pure + unit-tested.
///
/// Every rendezvous publishes our beacon (two topics) to free couriers, and the sweep used to do it
/// for every approved contact every 90–120 s FOREVER. A contact that never comes back — live
/// 2026-10-03, the PC still held the dead profile of a recreated laptop — cost hundreds of
/// publications a day from one IP, and ntfy.sh answered `429` to the LIVE contact's beacon for most
/// of the session (docs/bugs/reconnect-hangs-on-public-couriers.md). Now: normal cadence for the
/// first `SILENCE_BEFORE_BACKOFF` of silence, then every quarter of the silence so far, capped at
/// `SILENT_SWEEP_MAX_INTERVAL` — 12 a day instead of ~800 for a contact silent for days.
///
/// What a returning contact costs: its own app starts with no silence, so it publishes and dials at
/// full cadence (and from its peer book) — it finds US; our thinned fetch only bounds the other
/// direction, and finding its fresh beacon ends the silence at once.
fn silent_contact_sweep_due(
    silent_for: std::time::Duration,
    since_last_sweep: Option<std::time::Duration>,
) -> bool {
    if silent_for < SILENCE_BEFORE_BACKOFF {
        return true;
    }
    let interval = (silent_for / 4).min(SILENT_SWEEP_MAX_INTERVAL);
    since_last_sweep.is_none_or(|since| since >= interval)
}

/// The worker's view of contacts' silence for the periodic sweep (`silent_contact_sweep_due`).
pub(crate) struct ContactSilence {
    /// Contacts never heard in this run count as heard at worker start (a grace, not a verdict).
    started: std::time::Instant,
    heard: PeerHeardLog,
    /// How long before the start each contact not heard in this run was last heard (from storage).
    /// A duration, not an `Instant`: on Windows an `Instant` counts from boot, so «six days ago» on a
    /// machine booted this morning cannot be one — and the seed used to be silently dropped, leaving
    /// every long-gone contact at full cadence after a restart (stand 2026-10-15).
    silent_before_start: Mutex<std::collections::HashMap<[u8; 20], std::time::Duration>>,
    last_sweep: std::collections::HashMap<[u8; 20], std::time::Instant>,
}

impl ContactSilence {
    pub(crate) fn new(now: std::time::Instant) -> Self {
        Self {
            started: now,
            heard: Arc::default(),
            silent_before_start: Mutex::new(std::collections::HashMap::new()),
            last_sweep: std::collections::HashMap::new(),
        }
    }

    fn heard_now(&self, node: [u8; 20], now: std::time::Instant) {
        if let Ok(mut heard) = self.heard.lock() {
            heard.insert(node, now);
        }
    }

    /// A contact not heard yet in this run starts from when it was last heard at all (stored), not
    /// from the worker's start: otherwise every restart gave each long-silent contact
    /// `SILENCE_BEFORE_BACKOFF` of full-cadence rendezvous again — on a big contact list, thousands
    /// of courier publications per start for contacts that are simply gone. Kept only if older than
    /// the start (a newer one is no news) and representable as an `Instant`.
    fn seed_from_storage(
        &self,
        node: [u8; 20],
        last_heard_unix_ms: i64,
        now: std::time::Instant,
        now_unix_ms: i64,
    ) {
        let ago = std::time::Duration::from_millis(now_unix_ms.saturating_sub(last_heard_unix_ms).max(0) as u64);
        let since_start = now.saturating_duration_since(self.started);
        if ago <= since_start {
            return; // newer than the start: no news
        }
        if let Ok(mut before) = self.silent_before_start.lock() {
            before.entry(node).or_insert(ago - since_start);
        }
    }

    fn silent_for(&self, node: &[u8; 20], now: std::time::Instant) -> std::time::Duration {
        if let Some(last) = self.heard.lock().ok().and_then(|heard| heard.get(node).copied()) {
            return now.saturating_duration_since(last); // heard in this run
        }
        let before = self
            .silent_before_start
            .lock()
            .ok()
            .and_then(|before| before.get(node).copied())
            .unwrap_or_default();
        now.saturating_duration_since(self.started) + before
    }
}

/// The push-model wake-ups of the profile (`spawn_beacon_watch_group`): the contacts without a direct
/// link and the account topic, in groups, one courier subscription per group. Owned by the
/// autoconnect worker; dropping it stops every subscription.
pub(crate) struct BeaconWatches {
    groups: BeaconWatchGroups,
    /// Stop flags of the running subscriptions; subscription `i` serves group `i`.
    running: Vec<Arc<std::sync::atomic::AtomicBool>>,
    wake_tx: mpsc::Sender<BeaconWake>,
    wake_rx: mpsc::Receiver<BeaconWake>,
    last_reconcile: Option<std::time::Instant>,
    last_wake: std::collections::HashMap<BeaconWake, std::time::Instant>,
}

/// How often the watched targets are brought in line with who lacks a direct link.
const BEACON_WATCH_RECONCILE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
/// A contact that keeps publishing (a new address, a restart) wakes us at most this often — each
/// wake costs one rendezvous.
const BEACON_WAKE_FLOOR: std::time::Duration = std::time::Duration::from_secs(30);

impl BeaconWatches {
    pub(crate) fn new() -> Self {
        let (wake_tx, wake_rx) = mpsc::channel();
        Self {
            groups: Arc::new(Mutex::new(Vec::new())),
            running: Vec::new(),
            wake_tx,
            wake_rx,
            last_reconcile: None,
            last_wake: std::collections::HashMap::new(),
        }
    }

    /// Wake-ups that arrived since the last call, each at most once per `BEACON_WAKE_FLOOR`.
    fn woken(&mut self, now: std::time::Instant) -> Vec<BeaconWake> {
        let mut woken: Vec<BeaconWake> = Vec::new();
        for wake in self.wake_rx.try_iter() {
            let floored = self
                .last_wake
                .get(&wake)
                .is_some_and(|at| now.saturating_duration_since(*at) < BEACON_WAKE_FLOOR);
            if !floored && !woken.contains(&wake) {
                self.last_wake.insert(wake, now);
                woken.push(wake);
            }
        }
        woken
    }

    fn reconcile_due(&self, now: std::time::Instant) -> bool {
        self.last_reconcile
            .is_none_or(|at| now.saturating_duration_since(at) >= BEACON_WATCH_RECONCILE_EVERY)
    }

    /// Put `targets` in place as groups of [`BEACON_WATCH_TOPICS_PER_SUBSCRIPTION`] topics (two per
    /// target) and stop the subscriptions of groups that are gone. Returns how many groups there are —
    /// the subscriptions beyond `running.len()` are the caller's to start.
    fn set_targets(&mut self, targets: Vec<BeaconWatchTarget>) -> usize {
        let per_group = (BEACON_WATCH_TOPICS_PER_SUBSCRIPTION / 2).max(1);
        let groups: Vec<Vec<BeaconWatchTarget>> = targets.chunks(per_group).map(<[_]>::to_vec).collect();
        let count = groups.len();
        *self.groups.lock().unwrap_or_else(|poison| poison.into_inner()) = groups;
        while self.running.len() > count {
            if let Some(stop) = self.running.pop() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        count
    }

    fn stop_all(&mut self) {
        self.set_targets(Vec::new());
    }
}

impl Drop for BeaconWatches {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Bring the subscriptions in line: every approved contact device with a pair secret and no DIRECT
/// link, and the account topic — where a device of ours we have not met yet announces itself, so it
/// is met at once rather than on the next sweep.
fn reconcile_beacon_watches(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    watches: &mut BeaconWatches,
    now: std::time::Instant,
) {
    watches.last_reconcile = Some(now);
    // Cross-network only, like the sweep.
    if runtime.quic_addr().ip().is_loopback() {
        watches.stop_all();
        return;
    }
    let mut targets: Vec<BeaconWatchTarget> = rendezvous_peers(storage)
        .into_iter()
        .filter(|peer| !rendezvous_link_is_the_destination(runtime, &peer.node))
        .map(|peer| BeaconWatchTarget {
            secret: peer.secret,
            node: Some(*peer.node.as_bytes()),
            exclude: Vec::new(),
            read_from: peer.couriers.read_from.clone(),
        })
        .collect();
    if let Ok(Some(secret)) = storage.get_account_rendezvous_secret() {
        targets.push(BeaconWatchTarget {
            secret,
            node: None,
            exclude: linked_own_device_nodes(storage, runtime),
            read_from: None,
        });
    }
    let groups = watches.set_targets(targets);
    let my_node = *runtime.node_address().as_bytes();
    while watches.running.len() < groups {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if spawn_beacon_watch_group(
            db_path.to_string(),
            Arc::clone(&watches.groups),
            watches.running.len(),
            my_node,
            Arc::clone(&stop),
            watches.wake_tx.clone(),
        )
        .is_err()
        {
            break; // the rest on the next reconcile
        }
        watches.running.push(stop);
    }
}

/// The mesh nodes of every device of ours (linked or not) — the DHT leg asks them first.
pub(crate) fn own_device_nodes_for(db_path: &str) -> Vec<[u8; 20]> {
    let Ok(storage) = Storage::open(db_path) else {
        return Vec::new();
    };
    let Ok(Some(account)) = storage.get_local_account() else {
        return Vec::new();
    };
    storage
        .list_account_devices(&account.descriptor.account_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| node_address_from_ed25519_pubkey(record.chain.device.identity_signing_public_key.0).ok())
        .map(|node| *node.as_bytes())
        .collect()
}

/// Advertise the DHT-proxy role only while our own Mainline sees the swarm: a friend that picks a
/// proxy whose engine is dead (the same blocked network, a laptop asleep) only waits out a deadline.
/// `OMNI_DHT_PROXY` (a volunteer relay) keeps it on. Read on every sweep — an instant loopback read.
fn refresh_dht_proxy_role(runtime: &Arc<MeshRuntime>, db_path: &str) {
    let for_anyone = std::env::var("OMNI_DHT_PROXY").map(|v| !v.is_empty()).unwrap_or(false);
    let swarm_seen = load_dht_rendezvous_enabled(db_path)
        && crate::dht_helper_endpoint(db_path)
            .and_then(|endpoint| crate::dht_helper_nodes_count(&endpoint))
            .is_some_and(|count| {
                crate::note_dht_node_count(db_path, count, current_unix_ms());
                count > 0
            });
    runtime.set_dht_proxy_advertised(for_anyone || swarm_seen);
}

/// The mesh nodes of the devices of ours we already have a link with.
fn linked_own_device_nodes(storage: &Storage, runtime: &Arc<MeshRuntime>) -> Vec<[u8; 20]> {
    let Ok(Some(account)) = storage.get_local_account() else {
        return Vec::new();
    };
    storage
        .list_account_devices(&account.descriptor.account_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| node_address_from_ed25519_pubkey(record.chain.device.identity_signing_public_key.0).ok())
        .filter(|node| runtime.has_link(node))
        .map(|node| *node.as_bytes())
        .collect()
}

/// Meet a device of ours announced on the account topic now (a push wake-up).
fn rendezvous_own_account_now(storage: &Storage, runtime: &Arc<MeshRuntime>, db_path: &str) {
    let my_node = *runtime.node_address().as_bytes();
    let my_relay_bundle = build_my_relay_bundle_token(storage, db_path).ok().flatten();
    let peers = rendezvous_peers(storage);
    rendezvous_own_account(storage, runtime, db_path, my_node, &peers, my_relay_bundle.as_deref());
}

/// Rendezvous the approved contact with mesh node `node` now (a push wake-up) — not thinned by silence.
fn rendezvous_contact_by_node(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    node: [u8; 20],
) {
    let my_node = *runtime.node_address().as_bytes();
    let my_relay_bundle = build_my_relay_bundle_token(storage, db_path).ok().flatten();
    if let Some(peer) = rendezvous_peers(storage)
        .into_iter()
        .find(|peer| *peer.node.as_bytes() == node)
    {
        rendezvous_dial_one_peer(
            runtime,
            db_path,
            &peer,
            my_node,
            my_relay_bundle.as_deref(),
            None,
        );
    }
}

/// One contact's run of rendezvous attempts since it last had a direct link.
#[derive(Clone, Copy, Debug)]
struct RendezvousEpisode {
    attempts: usize,
    last: std::time::Instant,
}

/// Count a rendezvous dial started for `node` (by the sweep, an event or a retry) into its episode.
fn record_rendezvous_attempt(
    episodes: &mut std::collections::HashMap<[u8; 20], RendezvousEpisode>,
    node: [u8; 20],
    now: std::time::Instant,
) {
    episodes
        .entry(node)
        .and_modify(|episode| {
            episode.attempts += 1;
            episode.last = now;
        })
        .or_insert(RendezvousEpisode { attempts: 1, last: now });
}

/// Is an early re-rendezvous due for a contact still not directly linked, `attempts` into its
/// episode and `since_last` after the latest one? Pure + unit-tested.
fn rendezvous_retry_due(attempts: usize, since_last: std::time::Duration) -> bool {
    attempts >= 1
        && RENDEZVOUS_RETRY_AFTER
            .get(attempts - 1)
            .is_some_and(|wait| since_last >= *wait)
}

/// Early rendezvous, between periodic sweeps, for approved contacts without a direct link. Two
/// triggers, one per-contact dial:
///
/// - **a degrade** (direct→relay, losing the link, gaining only a relay link): rendezvous THAT peer
///   at once instead of waiting out the ~90 s sweep — the UDP-hostile case where UDP-direct is
///   throttled and TCP-direct is the fastest recovery. Per-peer flap-floored
///   (`EVENT_RENDEZVOUS_PER_PEER_FLOOR`);
/// - **an unfinished episode**: the last attempt didn't bring a direct link, so look again on the
///   `RENDEZVOUS_RETRY_AFTER` schedule. A direct link ends the episode.
///
/// Grades, timestamps and episodes are the worker's own state, updated in place.
fn early_rendezvous_contacts(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    grades: &mut std::collections::HashMap<[u8; 20], ContactLinkGrade>,
    last_event: &mut std::collections::HashMap<[u8; 20], std::time::Instant>,
    episodes: &mut std::collections::HashMap<[u8; 20], RendezvousEpisode>,
) {
    // Cross-network only (a loopback mesh has no internet peer to rendezvous with), matching the sweep.
    if runtime.quic_addr().ip().is_loopback() {
        return;
    }
    let now = std::time::Instant::now();
    let my_node = *runtime.node_address().as_bytes();
    let my_relay_bundle = build_my_relay_bundle_token(storage, db_path).ok().flatten();
    for peer in rendezvous_peers(storage) {
        let node_addr = peer.node;
        let node = *node_addr.as_bytes();
        let mut curr = contact_link_grade(runtime.has_link(&node_addr), runtime.peer_link_quality(&node_addr));
        // Direct off: a relay link is where we are going, not a step on the way.
        if curr == ContactLinkGrade::Relay && !runtime.allows_direct_punch() {
            curr = ContactLinkGrade::Direct;
        }
        if curr == ContactLinkGrade::Direct {
            episodes.remove(&node);
        }
        let since = last_event.get(&node).map(|t| now.duration_since(*t));
        // A contact rendezvoused moments ago (by the sweep in this very tick, say) was just looked
        // for: its fresh beacon was read and dialed. A degrade seen now is that same moment; the
        // retries of its episode look again. Stand 2026-10-17: each contact fetched twice in 150 ms.
        let just_looked = episodes
            .get(&node)
            .is_some_and(|episode| now.duration_since(episode.last) < RENDEZVOUS_JUST_LOOKED);
        let degraded = !just_looked && should_event_rendezvous(grades.get(&node).copied(), curr, since);
        let retry = episodes
            .get(&node)
            .filter(|_| curr != ContactLinkGrade::Direct)
            .filter(|episode| rendezvous_retry_due(episode.attempts, now.duration_since(episode.last)))
            .copied();
        if (degraded || retry.is_some())
            && rendezvous_dial_one_peer(
                runtime,
                db_path,
                &peer,
                my_node,
                my_relay_bundle.as_deref(),
                None,
            )
            .is_some()
        {
            if degraded {
                last_event.insert(node, now);
            }
            if let Some(episode) = retry {
                omni_app_trace_log(
                    "rendezvous/retry",
                    &format!(
                        "peer={} attempt={} after_s={} grade={curr:?}",
                        MeshNodeAddress::from_bytes(node),
                        episode.attempts + 1,
                        now.duration_since(episode.last).as_secs()
                    ),
                );
            }
            record_rendezvous_attempt(episodes, node, now);
        }
        grades.insert(node, curr);
    }
}

/// Phase C: is ANY configured relay node currently reachable (a live verified mesh
/// link)? Used to decide whether a symmetric×symmetric pair (direct impossible) can
/// still ride a relay backstop or is genuinely `NoRouteBothUnreachable`. A relay row
/// with no known node address (endpoint-only, not yet dialed) can't be link-checked,
/// so it doesn't count as reachable here.
fn any_relay_reachable(storage: &Storage, runtime: &Arc<MeshRuntime>) -> bool {
    let Ok(nodes) = storage.list_network_nodes() else {
        return false;
    };
    nodes
        .iter()
        .filter_map(|node| node.node_address.as_ref())
        .filter_map(|bytes| <[u8; 20]>::try_from(bytes.as_slice()).ok())
        .any(|addr| runtime.has_link(&MeshNodeAddress::from_bytes(addr)))
}

/// Relay-less retry for add-by-Omni-ID outgoing Pendings that have no session yet (2026-07-20).
/// The per-pair sweep needs an Approved contact + a pair secret, so a find-by-id Pending is never
/// retried there; its one-shot first dial in `find_contact_by_omni_id` can miss (UPnP/TCP front door
/// not mapped when the peer's beacon first published; a mobile carrier dropping the UDP burst). Each
/// throttled sweep, for every such peer still unlinked, re-fetch its FRESH identity beacon (now with
/// the optimistic UPnP-TCP front door) and re-run the shared `dial_identity_beacon_target`. Stops for
/// a peer the moment a session exists (pairing under way) or a direct link is up.
fn retry_unlinked_outgoing_id_pendings(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    pairing_targets: &Arc<Mutex<std::collections::HashSet<MeshNodeAddress>>>,
    hello_maker: &MeshHelloMaker,
) {
    if !storage.is_profile_signed_in().unwrap_or(false) {
        return;
    }
    if runtime.quic_addr().ip().is_loopback() {
        return; // same-host / loopback has no cross-network peer to rendezvous with
    }
    let contacts = match storage.list_contacts() {
        Ok(contacts) => contacts,
        Err(_) => return,
    };
    // Any candidate at all before we pay for a rendezvous runtime + courier fetches.
    let has_candidate = contacts.iter().any(|c| {
        c.relationship_state == omni_identity::ContactRelationshipState::Pending
            && c.pending_request_direction == Some(omni_identity::ContactRequestDirection::Outgoing)
            && c.remote_account_id.is_some()
    });
    if !has_candidate {
        return;
    }
    let allow_direct = crate::app_pinned_relay::allows_direct(db_path);
    let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
        return;
    };
    for contact in contacts {
        if contact.relationship_state != omni_identity::ContactRelationshipState::Pending
            || contact.pending_request_direction
                != Some(omni_identity::ContactRequestDirection::Outgoing)
        {
            continue;
        }
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        // Pairing already under way (a session exists) → the normal handshake owns it now.
        if storage
            .get_latest_session_for_remote_account(account_id)
            .ok()
            .flatten()
            .is_some()
        {
            continue;
        }
        // The peer's stored prekey bundle gives, through its certificate chain, the account master
        // key the Omni-ID commits to — enough to re-fetch fresh beacons, one per device.
        let Ok(Some(bundle)) = storage.get_latest_remote_prekey_bundle_for_account(account_id) else {
            continue;
        };
        let Some(chain) = bundle.device_certificate_chain.as_ref() else {
            continue;
        };
        let omni_id = omni_crypto::omni_id_bytes(&chain.working.master_public_key);
        let beacons = match fetch_identity_beacons(rendezvous.helper.as_ref(), &omni_id) {
            Ok(beacons) if !beacons.is_empty() => beacons,
            Ok(_) => {
                omni_app_trace_log(
                    "find_by_id/retry",
                    &format!("no fresh beacon for account={} (peer offline / not findable)", account_id.0),
                );
                continue;
            }
            Err(_) => continue,
        };
        // Each beacon is dialed at its OWN device's node: endpoints of one device must not be
        // tried for another.
        for beacon in beacons {
            let node = MeshNodeAddress::from_bytes(beacon.node);
            if runtime.has_link(&node) {
                continue; // some path already brought the link up
            }
            omni_app_trace_log(
                "find_by_id/retry",
                &format!("re-dialing account={} node={node}", account_id.0),
            );
            let hello_bytes = hello_maker(node);
            dial_identity_beacon_target(
                runtime,
                storage,
                pairing_targets,
                &beacon,
                node,
                &hello_bytes,
                allow_direct,
            );
        }
    }
}

/// Periodic rendezvous of every approved contact that isn't DIRECTLY linked. Returns the nodes a
/// rendezvous dial was started for — each starts (or continues) that contact's early-retry episode.
fn rendezvous_sweep_unlinked_contacts(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    silence: &mut ContactSilence,
) -> Vec<[u8; 20]> {
    if !storage.is_profile_signed_in().unwrap_or(false) {
        return Vec::new();
    }
    // Cross-network reach only: a loopback / same-host mesh pairs over loopback multicast and has no
    // internet peer to rendezvous with (also keeps same-host unit tests off STUN/ntfy regardless of
    // the sweep grace — which is why the first-sweep grace can be short).
    if runtime.quic_addr().ip().is_loopback() {
        return Vec::new();
    }
    let my_node = *runtime.node_address().as_bytes();
    // Обслуживание сегмента DHT — ПЕРИОДИЧЕСКОЕ, не привязанное к дозвону.
    //
    // Раньше и обучение сидам, и кэш живых узлов висели внутри `rendezvous_dial_with` и получали
    // контекст профиля только на «своём» дозвоне. В обычной работе дозванивается sweep, который
    // передаёт `None`, — и оба механизма не выполнялись вовсе: живой прогон с включённым DHT
    // (2026-08-13) дал 8 дозвонов, 14 успешных публикаций в DHT и НИ ОДНОЙ записи кэша.
    // А нужен он именно ноде, которая часами стоит слинкованной и никому не дозванивается: после
    // перезапуска ей нечем вернуться в рой без глобальных роутеров — ровно то, ради чего кэш и есть.
    // См. docs/decisions/dht-bootstrap-mesh-propagation.md.
    crate::maybe_seed_dht_bootstrap_from_peers(runtime, db_path);
    crate::maybe_harvest_dht_node_cache(db_path);
    refresh_dht_proxy_role(runtime, db_path);
    // R1c inc.3b: MY current signed relay bundle, attached to each approved contact's per-pair beacon
    // below so a friend's mesh auto-learns my (possibly rotated) relay set. Computed ONCE per sweep
    // (stable between changes); `None` when I share no relay. Approved-contact path only — bootstrap
    // beacons never carry it (pre-approval). See docs/decisions/r1c-rotatable-white-relay.md.
    let my_relay_bundle = build_my_relay_bundle_token(storage, db_path).ok().flatten();
    // Model A (MA2): if I opted into findable-by-ID, publish my signed identity beacon
    // under the Omni-ID-keyed topic this sweep (independent of the per-contact loop below).
    if load_discoverability_mode(db_path).is_public() {
        publish_identity_beacon(storage, runtime, db_path, my_relay_bundle.clone());
    } else if identity_withdrawal_pending(db_path)
        && publish_identity_beacon_withdrawal(storage, runtime, db_path)
    {
        clear_identity_withdrawal(db_path);
    }
    // Find by name: published, re-published, or withdrawn — the module decides from the switch.
    if let Ok(rendezvous) = profile_rendezvous_runtime(db_path) {
        crate::app_name_discovery::sweep_name_beacon(storage, rendezvous.helper.as_ref(), db_path);
        crate::app_relay_directory::sweep_relay_directory(storage, rendezvous.helper.as_ref(), runtime, db_path);
    }
    let peers = rendezvous_peers(storage);
    let (now, now_unix_ms) = (std::time::Instant::now(), current_unix_ms());
    if let Ok(last_heard) = storage.contact_last_activity_by_account() {
        for peer in &peers {
            if let Some(at) = last_heard.get(&peer.account_id) {
                silence.seed_from_storage(*peer.node.as_bytes(), *at, now, now_unix_ms);
            }
        }
    }
    // Our own other devices have no contact row, so their silence is kept per device: noted while
    // linked, seeded from it at start. Without it a long-gone copy of this profile got a full-cadence
    // quarter hour of rendezvous after every restart, and a thinned one after (stand 2026-10-15: one
    // unrevoked dead copy looked for every ~100 s).
    if let Ok(Some(account)) = storage.get_local_account() {
        let own = &account.descriptor.account_id;
        for peer in peers.iter().filter(|peer| &peer.account_id == own) {
            if runtime.has_link(&peer.node) {
                let _ = storage.note_own_device_heard(own, &peer.device_id, now_unix_ms);
            }
        }
        if let Ok(last_heard) = storage.own_devices_last_heard(own) {
            for peer in peers.iter().filter(|peer| &peer.account_id == own) {
                if let Some(at) = last_heard.get(&peer.device_id) {
                    silence.seed_from_storage(*peer.node.as_bytes(), *at, now, now_unix_ms);
                }
            }
        }
    }
    rendezvous_own_account(storage, runtime, db_path, my_node, &peers, my_relay_bundle.as_deref());
    peers
        .iter()
        .filter_map(|peer| {
            rendezvous_dial_one_peer(
                runtime,
                db_path,
                peer,
                my_node,
                my_relay_bundle.as_deref(),
                Some(&mut *silence),
            )
        })
        .collect()
}

/// Rendezvous ONE approved contact: publish my beacon under our pair topic + dial their advertised
/// UDP-direct and TCP-direct front doors (a relay→direct upgrade). Returns `true` if a dial was
/// spawned, `false` if skipped (not approved, no bundle/session/secret, or already DIRECTLY linked —
/// a relay link is a fallback we keep upgrading, a direct link is the destination). The one per-peer
/// body shared by the periodic sweep and the event-triggered upgrade below. Best-effort; the dial
/// runs on a background thread.
/// Our own devices that have never met — restored from the phrase, or a folder copied to another
/// network — have no session and so no pair topic. They meet on the account's own topic, derived
/// from the working key that only the account's devices hold: each publishes its beacon there every
/// sweep and dials a device it does not have yet. The link's hello carries the device certificate,
/// which makes it one of ours and starts the session; from then on its pair topic takes over
/// (docs/decisions/multi-device-equal-nodes.md, stage 2).
/// The mesh nodes of the devices our account took back.
fn revoked_own_device_nodes(storage: &Storage) -> Vec<[u8; 20]> {
    let Ok(Some(account)) = storage.get_local_account() else {
        return Vec::new();
    };
    storage
        .list_account_devices(&account.descriptor.account_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| record.revoked_at_unix_ms.is_some())
        .filter_map(|record| node_address_from_ed25519_pubkey(record.chain.device.identity_signing_public_key.0).ok())
        .map(|node| *node.as_bytes())
        .collect()
}

fn rendezvous_own_account(
    storage: &Storage,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    my_node: [u8; 20],
    peers: &[RendezvousPeer],
    my_relay_bundle: Option<&str>,
) {
    let Ok(Some(secret)) = storage.get_account_rendezvous_secret() else {
        return;
    };
    // Passed over: devices with a session (their own pair topic keeps them) and own devices already
    // linked — so the beacon dialed is one of a device not met yet.
    let mut skip: Vec<[u8; 20]> = peers.iter().map(|peer| *peer.node.as_bytes()).collect();
    skip.extend(linked_own_device_nodes(storage, runtime));
    // A device we took back still holds the working key and so still posts on this topic: it is not
    // one "not met yet", and dialing it is a failed dial every sweep. It hears its revocation from
    // the account mailbox.
    skip.extend(revoked_own_device_nodes(storage));
    let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
        return;
    };
    omni_app_trace_log(
        "rendezvous/account",
        &format!("own topic: passing over {} known device(s)", skip.len()),
    );
    account_mailbox_round(storage, true);
    account_passport_courier_round(storage, runtime, db_path);
    let _ = spawn_rendezvous_dial(
        rendezvous,
        Arc::clone(runtime),
        secret,
        PairCouriers::default(),
        my_node,
        skip,
        my_relay_bundle.map(str::to_string),
        None,
        Some(db_path.to_string()),
        None,
    );
}

/// One device rendezvous keeps reachable: its account (for traces and connection steps), its mesh
/// node and the pair secret of our session with it — the blinded topic both ends derive.
#[derive(Debug, Clone)]
pub(crate) struct RendezvousPeer {
    pub(crate) account_id: AccountId,
    device_id: DeviceId,
    node: MeshNodeAddress,
    pub(crate) secret: [u8; 32],
    /// The couriers this pair goes through (see [`pair_couriers`]).
    couriers: PairCouriers,
    /// A device revocation of ours this account may not have heard of (see `revocation_notice_for`).
    revocation_notice: Option<RevocationNotice>,
}

/// Every device rendezvous is for: each device of each approved contact we have a session with —
/// a contact is an account and each of its devices is its own node with its own session and topic —
/// and each of our own other devices we have a session with (docs/decisions/multi-device-equal-nodes.md).
/// A device without a session (or with a pre-rendezvous one) is skipped, with a trace saying why.
pub(crate) fn rendezvous_peers(storage: &Storage) -> Vec<RendezvousPeer> {
    let mut peers: Vec<RendezvousPeer> = Vec::new();
    let mut skips: Vec<String> = Vec::new();
    let mut add = |account_id: &AccountId, device_id: &DeviceId, signing_key: &[u8; 32], skips: &mut Vec<String>| {
        match rendezvous_peer(storage, account_id, device_id, signing_key) {
            Ok(peer) => {
                if !peers.iter().any(|known| known.node == peer.node) {
                    peers.push(peer);
                }
            }
            // Observability: a silent skip here is exactly what made R1c propagation hard to
            // diagnose live (the friend never rendezvouses, no trace why).
            Err(reason) => skips.push(format!("skip {} device {}: {reason}", account_id.0, device_id.0)),
        }
    };
    for contact in storage.list_contacts().unwrap_or_default() {
        if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
            continue;
        }
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        let bundles = storage
            .list_remote_prekey_bundles_for_account(account_id)
            .unwrap_or_default();
        if bundles.is_empty() {
            skips.push(format!("skip {}: no remote prekey bundle", account_id.0));
        }
        for bundle in bundles {
            add(account_id, &bundle.device_id, &bundle.identity_signing_key.0, &mut skips);
        }
    }
    if let (Ok(Some(account)), Ok(Some(device))) = (storage.get_local_account(), storage.get_local_device()) {
        let account_id = account.descriptor.account_id;
        for record in storage.list_account_devices(&account_id).unwrap_or_default() {
            let own = &record.chain.device;
            if own.device_id == device.device_id || record.revoked_at_unix_ms.is_some() {
                continue;
            }
            add(&account_id, &own.device_id, &own.identity_signing_public_key.0, &mut skips);
        }
    }
    crate::trace_rendezvous_skips_on_change(&storage.db_path().to_string_lossy(), skips);
    peers
}

/// The devices of one account rendezvous is for — a contact's, or our own other devices when
/// `account_id` is ours. Each carries its own node and pair secret: dial a device with ITS secret.
fn rendezvous_peers_for_account(storage: &Storage, account_id: &AccountId) -> Vec<RendezvousPeer> {
    rendezvous_peers(storage)
        .into_iter()
        .filter(|peer| &peer.account_id == account_id)
        .collect()
}

fn rendezvous_peer(
    storage: &Storage,
    account_id: &AccountId,
    device_id: &DeviceId,
    signing_key: &[u8; 32],
) -> Result<RendezvousPeer, &'static str> {
    let node = node_address_from_ed25519_pubkey(*signing_key).map_err(|_| "node derive failed")?;
    let session = match storage.get_session_record(&omni_crypto::SessionAddress {
        remote_account_id: account_id.clone(),
        remote_device_id: device_id.clone(),
    }) {
        Ok(Some(session)) => session,
        Ok(None) | Err(_) => return Err("no session"),
    };
    match omni_crypto::session_rendezvous_pair_secret(&session) {
        Ok(Some(secret)) => Ok(RendezvousPeer {
            account_id: account_id.clone(),
            device_id: device_id.clone(),
            node,
            secret,
            couriers: pair_couriers(storage, account_id, device_id),
            revocation_notice: revocation_notice_for(storage, account_id, current_unix_ms()),
        }),
        // Pre-rendezvous session (no blinded pair secret) or a transient read error.
        Ok(None) | Err(_) => Err("no rendezvous pair secret"),
    }
}

fn rendezvous_dial_one_peer(
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    peer: &RendezvousPeer,
    my_node: [u8; 20],
    my_relay_bundle: Option<&str>,
    // The periodic sweep thins out rendezvous for silent contacts; the early paths (a degrade, the
    // bounded retry) pass `None` and are not thinned — they are bounded on their own.
    silence: Option<&mut ContactSilence>,
) -> Option<[u8; 20]> {
    let remote_account_id = &peer.account_id;
    let (node, secret) = (peer.node, peer.secret);
    // Skip rendezvous only when the link is already DIRECT. A relay-only link is a FALLBACK, not the
    // destination: keep rendezvousing so we learn the peer's advertised reflexive/UPnP endpoints and
    // dial THEM (dial_at on the beacon endpoints) — an inbound-reachable peer (UPnP / port-forward)
    // lets even a CGNAT/4G side connect OUT to a DIRECT link (the way torrents reach seeded peers).
    // Mutual hole-punch for two unreachable sides is handled separately by upgrade_relay_links_via_punch.
    let now = std::time::Instant::now();
    if runtime.has_link(&node) {
        if let Some(silence) = silence.as_deref() {
            silence.heard_now(*node.as_bytes(), now);
        }
    }
    // A link on the peer's TCP DOOR is direct but NOT the destination: the UDP upgrade
    // (`upgrade_peer_to_udp` inside the dial) lives only on this path, so skipping it here pinned a
    // door-carried link to TCP for good. Live 2026-10-06: Omni firewalled on the PC for a minute,
    // the link came back as `online_direct/tcp_door` and stayed there 6+ minutes with UDP open.
    // A pair whose beacons were not read since the start is met once regardless: what is queued for
    // that account waits for them (a revocation it made while we were off — `contact_revocations_settled`).
    let beacons_read = pair_beacons_read(db_path, &secret);
    if rendezvous_already_there(
        beacons_read,
        rendezvous_link_is_the_destination(runtime, &node),
        runtime.peer_on_door(&node),
    ) {
        return None; // as reachable as allowed (direct over UDP, or any link when direct is off)
    }
    if let Some(silence) = silence.as_deref().filter(|_| beacons_read) {
        let silent_for = silence.silent_for(node.as_bytes(), now);
        let since_last_sweep = silence
            .last_sweep
            .get(node.as_bytes())
            .map(|at| now.saturating_duration_since(*at));
        if !silent_contact_sweep_due(silent_for, since_last_sweep) {
            omni_app_trace_log(
                "rendezvous/quiet",
                &format!(
                    "skip {}: silent {}s, last rendezvous {}s ago",
                    remote_account_id.0,
                    silent_for.as_secs(),
                    since_last_sweep.map_or(0, |s| s.as_secs())
                ),
            );
            return None;
        }
    }
    omni_app_trace_log(
        "rendezvous/sweep",
        &format!(
            "rendezvousing {} (relay_bundle={})",
            remote_account_id.0,
            if my_relay_bundle.is_some() { "yes" } else { "no" }
        ),
    );
    let rendezvous = match profile_rendezvous_runtime(db_path) {
        Ok(rendezvous) => rendezvous,
        Err(_) => return None,
    };
    // A contact already linked (over its TCP door, a relay, or met once after the start for its
    // beacons) is only being upgraded: not a step of its journey, which ended at «linked». Recording it
    // made the journal alternate «linked» / «rendezvous» every sweep, like a link that kept dropping.
    if !runtime.has_link(&node) {
        record_connection_step(
            db_path,
            &remote_account_id.0,
            ConnectionStage::Rendezvous,
            ConnectionStepStatus::Started,
            Some("публикую зашифрованный маячок и дозваниваюсь".to_string()),
        );
    }
    // Approved contact → attach MY relay bundle AND consume the friend's (per-pair path), so each
    // side auto-learns the other's relay set on the sweep (R1c inc.3b).
    let spawned = spawn_rendezvous_dial(
        rendezvous,
        Arc::clone(runtime),
        secret,
        peer.couriers.clone(),
        my_node,
        Vec::new(),
        my_relay_bundle.map(str::to_string),
        peer.revocation_notice.clone(),
        Some(db_path.to_string()),
        silence
            .as_deref()
            .map(|silence| (Arc::clone(&silence.heard), *node.as_bytes())),
    )
    .is_ok();
    if spawned {
        if let Some(silence) = silence {
            silence.last_sweep.insert(*node.as_bytes(), now);
        }
    }
    spawned.then_some(*node.as_bytes())
}

/// FIRST-contact bootstrap sweep for the autoconnect worker: for every outstanding invite, publish
/// our endpoints to its nonce-derived blinded bootstrap topic and dial the other side over the
/// rendezvous providers (each via a windowless background dial, off this loop). This is what lets a
/// brand-new contact on ANOTHER network connect before any X3DH session exists. Prunes entries that
/// expired (past the bootstrap window / invite TTL) or — on the importer side, which knows the
/// inviter's node — whose link is already up. The inviter side has no node to match, so it self-expires
/// at the window. See docs/decisions/invite-bootstrap-rendezvous.md.
fn bootstrap_rendezvous_sweep(
    bootstrap_invites: &Arc<Mutex<std::collections::HashMap<String, MeshBootstrapInvite>>>,
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
) {
    // Cross-network bootstrap only: a loopback / same-host mesh pairs over loopback multicast and
    // has no internet peer to rendezvous with (also keeps same-host unit tests off STUN/ntfy).
    if runtime.quic_addr().ip().is_loopback() {
        return;
    }
    let now = current_unix_ms();
    let my_node = *runtime.node_address().as_bytes();
    // Prune dead entries + snapshot the live secrets under a brief lock; run the slow dials after.
    let secrets: Vec<[u8; 32]> = {
        let Ok(mut map) = bootstrap_invites.lock() else {
            return;
        };
        map.retain(|_, entry| {
            if now >= entry.expires_at_ms {
                return false;
            }
            // Importer entry: drop it once the inviter's link is up (pairing can finish from here).
            !matches!(entry.inviter_node, Some(node) if runtime.has_link(&node))
        });
        map.values().map(|entry| entry.secret).collect()
    };
    for secret in secrets {
        let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
            continue;
        };
        // No relay bundle on a BOOTSTRAP beacon (R1c): this is FIRST contact (invite-nonce keyed, the
        // peer isn't an approved contact yet), and relay-set propagation is approved-only — leaking my
        // relays to whoever holds the invite would leak relay addresses to whoever holds it. The bundle
        // rides only the per-pair (approved) beacon. See docs/decisions/r1c-rotatable-white-relay.md.
        let _ = spawn_rendezvous_dial(
            rendezvous,
            Arc::clone(runtime),
            secret,
            PairCouriers::default(),
            my_node,
            Vec::new(),
            None,
            None,
            None,
            None,
        );
    }
}

#[cfg(test)]
mod invite_provider_tests {
    use super::{encode_mesh_pairing_invite, parse_mesh_pairing_invite};

    /// A friend's bundle listing US as a relay must not make our own door our relay carrier.
    #[test]
    fn our_own_node_is_never_our_relay_tcp_carrier() {
        let node = |address: u8, tcp: &str| omni_storage::NetworkNodeRecord {
            id: 0,
            endpoint: None,
            node_address: Some(vec![address; 20]),
            label: String::new(),
            source: String::new(),
            added_at_unix_ms: 0,
            tcp_endpoint: Some(tcp.to_string()),
            cert_spki_pin: None,
            bundle_id: None,
            bundle_version: None,
            shared: false,
        };
        let own = [7u8; 20];
        let nodes = [node(7, "203.0.113.1:60577"), node(9, "198.51.100.2:443")];
        assert_eq!(
            super::relay_tcp_endpoint_of(&nodes, &own),
            Some("198.51.100.2:443".parse().unwrap())
        );
        assert_eq!(super::relay_tcp_endpoint_of(&nodes[..1], &own), None, "only ourselves: no relay");
    }

    #[test]
    fn a_link_on_the_peers_door_is_never_where_the_sweep_stops() {
        use super::rendezvous_already_there;
        assert!(rendezvous_already_there(true, true, false), "direct over UDP: nothing to do");
        // Direct off, the link to our relay on its door: still tried on UDP (it used to stay on TCP).
        assert!(!rendezvous_already_there(true, true, true));
        assert!(!rendezvous_already_there(false, true, false), "beacons not read yet");
        assert!(!rendezvous_already_there(true, false, false), "no link worth keeping");
    }

    #[test]
    fn a_lost_contact_is_redialed_at_once_then_less_and_less_often_until_linked() {
        use super::LastGoodRedial;
        use std::time::{Duration, Instant};
        let node = omni_mesh_runtime::MeshNodeAddress::from_bytes([7; 20]);
        let mut redial = LastGoodRedial::default();
        let t0 = Instant::now();
        assert!(redial.due(&node, t0), "first try right after the drop");
        assert!(!redial.due(&node, t0 + Duration::from_secs(2)));
        assert!(redial.due(&node, t0 + Duration::from_secs(3)));
        // Pauses double: 6 s after the second try, not 3.
        assert!(!redial.due(&node, t0 + Duration::from_secs(8)));
        assert!(redial.due(&node, t0 + Duration::from_secs(9)));
        // A link resets it: the next drop is tried at once again.
        redial.linked(&node);
        assert!(redial.due(&node, t0 + Duration::from_secs(10)));
    }

    #[test]
    fn rendezvous_keeps_sweeping_a_relay_only_link_but_stops_on_direct() {
        use super::rendezvous_link_is_direct;
        use omni_mesh_runtime::MeshLinkQuality;
        // Direct (or no path snapshot) → terminal, stop rendezvousing.
        assert!(rendezvous_link_is_direct(Some(MeshLinkQuality::Direct)));
        assert!(rendezvous_link_is_direct(None));
        // Relay-only → NOT terminal: keep rendezvousing to learn + dial the peer's endpoints (so an
        // inbound-reachable peer upgrades relay→direct, e.g. a 4G side dialing OUT to a UPnP home peer).
        assert!(!rendezvous_link_is_direct(Some(MeshLinkQuality::Relay { hops: 1 })));
    }

    #[test]
    fn event_rendezvous_fires_only_on_a_witnessed_degrade_past_the_flap_floor() {
        use super::{
            contact_link_grade, should_event_rendezvous, ContactLinkGrade,
            EVENT_RENDEZVOUS_PER_PEER_FLOOR,
        };
        use omni_mesh_runtime::MeshLinkQuality;
        use std::time::Duration;

        // Grade mapping: no link → None, relay hop → Relay, anything else linked → Direct.
        assert_eq!(contact_link_grade(false, None), ContactLinkGrade::None);
        assert_eq!(
            contact_link_grade(true, Some(MeshLinkQuality::Relay { hops: 2 })),
            ContactLinkGrade::Relay
        );
        assert_eq!(
            contact_link_grade(true, Some(MeshLinkQuality::Direct)),
            ContactLinkGrade::Direct
        );
        assert_eq!(contact_link_grade(true, None), ContactLinkGrade::Direct);

        let long_ago = Some(EVENT_RENDEZVOUS_PER_PEER_FLOOR + Duration::from_secs(1));
        let just_now = Some(Duration::from_secs(1));

        // First sighting is left to the periodic sweep — no startup burst.
        assert!(!should_event_rendezvous(None, ContactLinkGrade::Relay, None));
        // A real degrade we witnessed, floor elapsed → fire.
        assert!(should_event_rendezvous(
            Some(ContactLinkGrade::Direct),
            ContactLinkGrade::Relay,
            long_ago
        ));
        assert!(should_event_rendezvous(
            Some(ContactLinkGrade::Relay),
            ContactLinkGrade::None,
            None
        ));
        // Gaining a relay link (none→relay) is also an upgrade opportunity → fire.
        assert!(should_event_rendezvous(
            Some(ContactLinkGrade::None),
            ContactLinkGrade::Relay,
            long_ago
        ));
        // Already direct, or unchanged → nothing to upgrade.
        assert!(!should_event_rendezvous(
            Some(ContactLinkGrade::Relay),
            ContactLinkGrade::Direct,
            long_ago
        ));
        assert!(!should_event_rendezvous(
            Some(ContactLinkGrade::Relay),
            ContactLinkGrade::Relay,
            long_ago
        ));
        // A degrade WITHIN the flap floor is suppressed — a flapping peer can't hammer providers.
        assert!(!should_event_rendezvous(
            Some(ContactLinkGrade::Direct),
            ContactLinkGrade::Relay,
            just_now
        ));
    }

    /// A new live direct endpoint is written at once; an unchanged set only on the refresh interval.
    #[test]
    fn a_new_peer_endpoint_is_saved_at_once_and_the_rest_on_the_interval() {
        use super::{peer_book_save_due, MeshNodeAddress, PEER_BOOK_SNAPSHOT_INTERVAL_SECS};
        use std::collections::HashSet;
        use std::time::Duration;
        let pc = MeshNodeAddress::from_bytes([0xD4; 20]);
        let port = |p: u16| std::net::SocketAddr::from(([192, 0, 2, 144], p)).to_string();
        let saved: HashSet<_> = [(pc, port(49_297))].into_iter().collect();
        let interval = Duration::from_secs(PEER_BOOK_SNAPSHOT_INTERVAL_SECS);

        // Negative half: nothing new, interval not up → no write (no churn on the 3 s tick); nothing
        // live at all → no write either.
        assert!(!peer_book_save_due(&saved, &saved, Duration::from_secs(3)));
        assert!(!peer_book_save_due(&HashSet::new(), &saved, Duration::from_secs(3)));

        // A link came up on an address the disk doesn't have: write now, not 30 s later.
        let moved: HashSet<_> = [(pc, port(61_380))].into_iter().collect();
        assert!(peer_book_save_due(&moved, &saved, Duration::from_secs(1)));
        // And the very first link after start, with nothing saved yet.
        assert!(peer_book_save_due(&saved, &HashSet::new(), Duration::ZERO));
        // The unchanged set still gets its periodic refresh (entries age out by TTL otherwise).
        assert!(peer_book_save_due(&saved, &saved, interval));
    }

    /// A contact heard from recently is swept at the normal cadence; one silent for long is swept
    /// ever more rarely, but never less than every `SILENT_SWEEP_MAX_INTERVAL`.
    #[test]
    fn a_silent_contact_is_rendezvoused_ever_more_rarely_but_never_given_up() {
        use super::{silent_contact_sweep_due, SILENCE_BEFORE_BACKOFF, SILENT_SWEEP_MAX_INTERVAL};
        use std::time::Duration;
        let min = |m: u64| Duration::from_secs(m * 60);

        // Negative half: within the grace the cadence is untouched, however recent the last sweep.
        assert!(silent_contact_sweep_due(Duration::ZERO, Some(Duration::ZERO)));
        assert!(silent_contact_sweep_due(
            SILENCE_BEFORE_BACKOFF - Duration::from_secs(1),
            Some(Duration::from_secs(1))
        ));

        // Silent for an hour: every quarter of it — 15 min.
        assert!(!silent_contact_sweep_due(min(60), Some(min(14))));
        assert!(silent_contact_sweep_due(min(60), Some(min(15))));
        // Silent for days: capped, still swept every 2 h.
        assert!(!silent_contact_sweep_due(min(3 * 24 * 60), Some(min(119))));
        assert!(silent_contact_sweep_due(min(3 * 24 * 60), Some(SILENT_SWEEP_MAX_INTERVAL)));
        // Never swept in this run: due.
        assert!(silent_contact_sweep_due(min(3 * 24 * 60), None));
    }

    /// A restart does not make a long-silent contact "just heard": its silence starts from the stored
    /// last activity. A stored time newer than the start, or a contact heard in this run, is left as is.
    #[test]
    fn a_restart_remembers_how_long_a_contact_has_been_silent() {
        use super::ContactSilence;
        use std::time::{Duration, Instant};
        let hour = Duration::from_secs(3600);
        let Some(started) = Instant::now().checked_sub(Duration::from_secs(60)) else {
            return; // the clock cannot go back a minute (just booted): nothing to check
        };
        let silence = ContactSilence::new(started);
        let now = Instant::now();
        let now_unix_ms = 10_000_000_000_i64;
        let (gone, fresh, heard) = ([1u8; 20], [2u8; 20], [3u8; 20]);

        silence.seed_from_storage(gone, now_unix_ms - 2 * 3_600_000, now, now_unix_ms);
        assert!(silence.silent_for(&gone, now) >= hour * 2 - Duration::from_secs(1));
        // Six days ago — further back than this machine has been up, as on a laptop booted this
        // morning: still counted (an `Instant` could not hold it, and the seed used to vanish).
        let week_gone = [4u8; 20];
        silence.seed_from_storage(week_gone, now_unix_ms - 6 * 24 * 3_600_000, now, now_unix_ms);
        assert!(silence.silent_for(&week_gone, now) >= hour * 24 * 6 - Duration::from_secs(1));

        // Negative half: news newer than the start changes nothing; neither does an old stored time
        // for a contact already heard in this run.
        silence.seed_from_storage(fresh, now_unix_ms - 1_000, now, now_unix_ms);
        assert!(silence.silent_for(&fresh, now) <= Duration::from_secs(61));
        silence.heard_now(heard, now);
        silence.seed_from_storage(heard, now_unix_ms - 2 * 3_600_000, now, now_unix_ms);
        assert_eq!(silence.silent_for(&heard, now), Duration::ZERO);
    }

    /// Targets go into groups of one subscription each (sixteen contacts — two topics apiece — per
    /// group); a group that is gone stops its subscription; a target that keeps publishing wakes the
    /// worker at most once per floor.
    #[test]
    fn beacon_watches_group_the_targets_and_floor_repeated_wakes() {
        use super::{BeaconWake, BeaconWatchTarget, BeaconWatches, BEACON_WAKE_FLOOR};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let target = |n: u8| BeaconWatchTarget { secret: [n; 32], node: Some([n; 20]), exclude: Vec::new(), read_from: None };
        let mut watches = BeaconWatches::new();
        assert_eq!(watches.set_targets((0..40).map(target).collect()), 3, "40 targets, 16 per group");
        assert_eq!(watches.groups.lock().unwrap()[2].len(), 8);
        let stops: Vec<Arc<AtomicBool>> = (0..3).map(|_| Arc::new(AtomicBool::new(false))).collect();
        watches.running = stops.clone();
        assert_eq!(watches.set_targets((0..10).map(target).collect()), 1);
        assert_eq!(watches.running.len(), 1);
        assert!(!stops[0].load(Ordering::Relaxed), "the first group still runs");
        assert!(stops[1].load(Ordering::Relaxed) && stops[2].load(Ordering::Relaxed), "the gone groups are stopped");

        let now = std::time::Instant::now();
        let b = BeaconWake::Contact([0xBB; 20]);
        watches.wake_tx.send(b).unwrap();
        watches.wake_tx.send(b).unwrap();
        watches.wake_tx.send(BeaconWake::OwnAccount).unwrap();
        assert_eq!(watches.woken(now), vec![b, BeaconWake::OwnAccount], "two arrivals, one wake each");
        watches.wake_tx.send(b).unwrap();
        assert!(watches.woken(now + BEACON_WAKE_FLOOR / 2).is_empty(), "within the floor: no wake");
        watches.wake_tx.send(b).unwrap();
        assert_eq!(watches.woken(now + BEACON_WAKE_FLOOR), vec![b]);
        watches.stop_all();
        assert!(stops[0].load(Ordering::Relaxed));
    }

    /// Hearing a contact (a link, its beacon) ends its silence; an unheard one counts from start.
    #[test]
    fn hearing_a_contact_ends_its_silence() {
        use super::ContactSilence;
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let silence = ContactSilence::new(start);
        let peer = [0x5A; 20];
        let later = start + Duration::from_secs(3600);
        assert_eq!(silence.silent_for(&peer, later), Duration::from_secs(3600));
        silence.heard_now(peer, later - Duration::from_secs(10));
        assert_eq!(silence.silent_for(&peer, later), Duration::from_secs(10));
        // Another contact is not affected.
        assert_eq!(silence.silent_for(&[0x5B; 20], later), Duration::from_secs(3600));
    }

    /// The QUIC port survives a restart through the profile's side file; nothing that isn't a
    /// usable port comes back out of it.
    #[test]
    fn the_mesh_quic_port_is_remembered_per_profile() {
        use super::{load_remembered_port, save_remembered_port, DOOR_PORT_FILE, QUIC_PORT_FILE};
        let dir = std::env::temp_dir().join(format!("omni-quic-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("omni.db").to_string_lossy().to_string();
        let other = dir.join("other.db").to_string_lossy().to_string();

        // Negative half: nothing remembered yet, and junk or a zero never becomes a port.
        assert_eq!(load_remembered_port(&db, QUIC_PORT_FILE), None);
        for junk in ["", "0", "-1", "70000", "port"] {
            std::fs::write(crate::profile_sidecar_path(&db, QUIC_PORT_FILE), junk).unwrap();
            assert_eq!(load_remembered_port(&db, QUIC_PORT_FILE), None, "{junk:?}");
        }

        save_remembered_port(&db, QUIC_PORT_FILE, 61_600);
        assert_eq!(load_remembered_port(&db, QUIC_PORT_FILE), Some(61_600));
        assert_eq!(load_remembered_port(&other, QUIC_PORT_FILE), None, "per profile, not per machine");
        // Each socket has its own file: the door's port is not the QUIC one.
        assert_eq!(load_remembered_port(&db, DOOR_PORT_FILE), None);
        save_remembered_port(&db, DOOR_PORT_FILE, 61_601);
        assert_eq!(load_remembered_port(&db, DOOR_PORT_FILE), Some(61_601));
        assert_eq!(load_remembered_port(&db, QUIC_PORT_FILE), Some(61_600));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rendezvous that leaves a contact without a direct link is followed by two early looks
    /// (30 s, then 60 s after the previous one) instead of waiting out the 90–120 s sweep — live,
    /// both sides starting together, the first look read the peer's beacon from its previous run.
    /// And no more than two: an offline contact must not turn into a faster sweep forever.
    #[test]
    fn an_unlinked_contact_is_looked_at_again_twice_then_left_to_the_sweep() {
        use super::{record_rendezvous_attempt, rendezvous_retry_due, RENDEZVOUS_RETRY_AFTER};
        use std::time::{Duration, Instant};

        let [first, second] = RENDEZVOUS_RETRY_AFTER;
        assert!(first < Duration::from_secs(90), "the first look must beat the sweep floor");

        // Negative half first: no attempt yet → no retry; too early → no retry; the schedule
        // exhausted → no retry, however long it has been.
        assert!(!rendezvous_retry_due(0, Duration::from_secs(3600)));
        assert!(!rendezvous_retry_due(1, first - Duration::from_secs(1)));
        assert!(!rendezvous_retry_due(2, second - Duration::from_secs(1)));
        assert!(!rendezvous_retry_due(3, Duration::from_secs(3600)));

        // Positive half: each due exactly on its own wait.
        assert!(rendezvous_retry_due(1, first));
        assert!(rendezvous_retry_due(2, second));

        // An episode counts every attempt, whoever started it (sweep, event or retry).
        let mut episodes = std::collections::HashMap::new();
        let node = [7u8; 20];
        let t0 = Instant::now();
        record_rendezvous_attempt(&mut episodes, node, t0);
        let t1 = t0 + first;
        assert!(rendezvous_retry_due(episodes[&node].attempts, t1.duration_since(episodes[&node].last)));
        record_rendezvous_attempt(&mut episodes, node, t1);
        assert_eq!(episodes[&node].attempts, 2);
        assert!(!rendezvous_retry_due(episodes[&node].attempts, first), "the second wait is longer");
        record_rendezvous_attempt(&mut episodes, node, t1 + second);
        assert!(!rendezvous_retry_due(episodes[&node].attempts, Duration::from_secs(3600)));
    }

    /// Честная подпись несущей в состоянии контакта: QUIC/UDP (быстрый) против TCP-over-TLS
    /// фолбэка (UDP зарезан) против LAN — и ОТДЕЛЬНО прямая TCP-дверь пира.
    ///
    /// Дверь добавлена 2026-08-31. `peer_best_carrier` отдаёт ИНТЕРФЕЙС (`QuicOverUdp`) и тогда,
    /// когда mux под ним уехал на дверь, — один и тот же ответ на «идём по UDP» и «идём по TCP».
    /// В `mesh_link_quality` это уже было замечено и исправлено (`direct:door` вместо лгущего
    /// `direct:udp`), но исправлено ТОЛЬКО там: состояние контакта — то, что пользователь читает
    /// в шапке чата, — про дверь не знало и писало «· QUIC» поверх TCP-соединения. Правило теперь
    /// одно, и оба вывода зовут его.
    #[test]
    fn link_carrier_maps_each_mesh_interface_kind_including_the_tcp_door() {
        use super::LinkCarrier;
        use omni_mesh_runtime::MeshInterfaceKind;

        assert_eq!(
            LinkCarrier::classify(MeshInterfaceKind::QuicOverUdp, true),
            LinkCarrier::TcpDoor,
            "байты идут по TCP — подписывать их QUIC значит врать"
        );
        // Отрицательные половины: без двери UDP остаётся UDP, а НАСТОЯЩУЮ TCP-несущую признак
        // двери перебивать не имеет права — фолбэк на TCP и миграция на дверь это разные вещи, и
        // ради их различения всё и заведено.
        assert_eq!(
            LinkCarrier::classify(MeshInterfaceKind::QuicOverUdp, false),
            LinkCarrier::Quic
        );
        assert_eq!(
            LinkCarrier::classify(MeshInterfaceKind::QuicOverTcp, true),
            LinkCarrier::TcpFallback
        );
        assert_eq!(LinkCarrier::classify(MeshInterfaceKind::Mdns, true), LinkCarrier::Lan);

        // Короткие имена для `mesh_link_quality` — сложившийся контракт daemon/CLI и живых
        // прогонов (`direct:udp` / `direct:door`), меняться не должны.
        assert_eq!(LinkCarrier::TcpDoor.wire_name(), "door");
        assert_eq!(LinkCarrier::Quic.wire_name(), "udp");
        assert_eq!(LinkCarrier::TcpFallback.wire_name(), "tcp");
        assert_eq!(LinkCarrier::Lan.wire_name(), "mdns");

        // И имя в DTO, по которому оболочка выбирает подпись в шапке.
        assert_eq!(
            serde_json::to_string(&LinkCarrier::TcpDoor).unwrap(),
            "\"tcp_door\""
        );
    }

    fn invite_with(providers: Vec<String>) -> omni_types::MeshPairingInviteV1 {
        omni_types::MeshPairingInviteV1 {
            version: omni_types::MESH_PAIRING_INVITE_VERSION_V1,
            source_instance_id: "inst-1".into(),
            signing_pubkey_hex: "ab".repeat(32),
            bootstrap_nonce_hex: "cd".repeat(32),
            rendezvous_providers: providers,
            dht_bootstrap_nodes: Vec::new(),
            exported_at_unix_ms: 1_700_000_000_000,
            expires_at_unix_ms: 1_700_000_900_000,
        }
    }

    #[test]
    fn invite_round_trips_the_rendezvous_provider_list() {
        let providers = vec!["https://ntfy.sh".to_string(), "wss://offchain.pub".to_string()];
        let token = encode_mesh_pairing_invite(&invite_with(providers.clone())).expect("encode");
        let parsed = parse_mesh_pairing_invite(&token).expect("parse");
        assert_eq!(parsed.rendezvous_providers, providers);
    }

    #[test]
    fn invite_round_trips_the_dht_bootstrap_nodes() {
        let mut invite = invite_with(vec![]);
        invite.dht_bootstrap_nodes =
            vec!["10.20.30.40:6881".to_string(), "10.20.30.41:6881".to_string()];
        let token = encode_mesh_pairing_invite(&invite).expect("encode");
        let parsed = parse_mesh_pairing_invite(&token).expect("parse");
        assert_eq!(parsed.dht_bootstrap_nodes, invite.dht_bootstrap_nodes);
    }

    #[test]
    fn a_legacy_invite_without_the_field_parses_to_an_empty_provider_list() {
        // An older `om3mesh:` token minted before R7 has no `rendezvous_providers` key; serde(default)
        // must yield an empty Vec (the importer then falls back to its own configured set), never an error.
        use base64::Engine as _;
        let legacy_json = serde_json::json!({
            "version": omni_types::MESH_PAIRING_INVITE_VERSION_V1,
            "source_instance_id": "inst-1",
            "signing_pubkey_hex": "ab".repeat(32),
            "bootstrap_nonce_hex": "cd".repeat(32),
            "exported_at_unix_ms": 1_700_000_000_000i64,
            "expires_at_unix_ms": 1_700_000_900_000i64,
        });
        let token = format!(
            "om3mesh:{}",
            base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&legacy_json).unwrap())
        );
        let parsed = parse_mesh_pairing_invite(&token).expect("legacy parse");
        assert!(parsed.rendezvous_providers.is_empty());
        assert!(parsed.dht_bootstrap_nodes.is_empty());
    }
}

/// Which `(feed, seq)` entries a durable snapshot holds — the periodic persist skips an unchanged set.
pub(crate) fn durable_rows_marks(rows: &[(Vec<u8>, i64, Vec<u8>)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut keys: Vec<(&[u8], i64)> = rows.iter().map(|(feed, seq, _)| (feed.as_slice(), *seq)).collect();
    keys.sort_unstable();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    keys.hash(&mut hasher);
    hasher.finish()
}
