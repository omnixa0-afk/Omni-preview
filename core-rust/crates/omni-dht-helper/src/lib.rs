//! The DHT helper process — the untrusted-by-design, key-free engine that owns the BitTorrent
//! Mainline DHT (privilege + proxy separation; see docs/decisions/omni-dht-helper.md, R7 Phase 2).
//!
//! One process = the ONLY thing that touches the DHT wire. It speaks a tiny line protocol over a
//! local socket; a client (today `BtDhtChannel` inside `omni-rendezvous-helper`) drives it. It holds
//! NO keys and never seals/opens beacons — it is a dumb **signed BEP44 mutable put/get primitive**:
//! given the 32-byte per-bucket seed (`key`) it derives the ed25519 keypair and reads/writes a slot.
//! Commands (one request line in → one `OK …` / `ERR …` line out):
//!   * `put <key_hex> <salt_hex> <value_hex>`  — sign + publish `value` to the slot named by
//!     `salt`. The slot picks the keypair — `sha256(domain ‖ key ‖ salt)` — and the item carries NO
//!     salt, with `value` inside a pkarr-shaped DNS packet and `seq` in microseconds: what lands in
//!     the DHT looks like an iroh endpoint record (`pkarr_shape.rs`,
//!     docs/decisions/libp2p-kad-vs-mainline.md). The helper reads the slot's current `seq` locally
//!     and bumps past it (BEP44 = last-writer-wins), so the client needs no extra round-trip. `OK`.
//!   * `get <key_hex> <salt_hex>`              — most-recent value for that slot. Reply
//!     `OK <value_hex>` (empty `OK` = slot empty / nothing of ours there).
//!   * `rput <key_hex> <salt_hex> <value_hex>` / `rget <key_hex> <salt_hex>` — the same record, the
//!     same slot, through the public pkarr relays over HTTPS instead of our own engine: the last way
//!     into Mainline on a network that drops DHT traffic (`relay.rs`,
//!     docs/decisions/dht-over-pkarr-relays.md). `ERR` when no relay answered.
//!   * `announce <info_hash_hex> <port>` — BEP 5 `announce_peer`: put THIS process's IP (as the swarm
//!     sees the packet — the protocol takes no other) with `port` under the 20-byte `info_hash`. The
//!     relay directory's Mainline shelves (docs/decisions/public-relay-directory.md, stage 2). `OK`.
//!   * `peers <info_hash_hex>` — BEP 5 `get_peers`: the `ip:port`s announced under it, comma-separated
//!     (`OK` alone — none). Bare addresses: who sits there is learned by dialing it.
//!   * `quit` — close this connection.
//!
//! Rendezvous policy (dual-slot-claim, the size cap, dropping one's own echo) stays in the CLIENT —
//! this process is generic so a future `announce`/`get_peers(infohash)` peer-discovery consumer can
//! reuse the same engine. A compromise of this process is contained to "a malicious DHT engine" =
//! DoS / a bad slot read; it can't reach the core's keys and can't forge a beacon (AEAD-sealed).
//!
//! Sync client, `node` feature, `#![allow(deprecated)]`: identical rationale to `btdht.rs` — no
//! async/tokio in an untrusted helper; sync `put/get_mutable` stay throughout `mainline` 7.x.
#![allow(deprecated)]

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use mainline::{Dht, MutableItem, SigningKey};
use sha2::Digest as _;

mod pkarr_shape;
mod relay;

/// The largest value a put takes: what fits one pkarr-shaped record (BEP44 caps the packet at 1000
/// bytes; base64url and the DNS framing take the rest). `btdht.rs` guards the same number.
fn max_value() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(pkarr_shape::max_payload)
}

/// A slot name's cap (it was BEP44's salt cap; the name now only feeds the key derivation).
const MAX_SLOT_NAME: usize = 64;

/// The keypair of one slot under a per-bucket seed: pkarr records have no salt, so the two slots of
/// a pair are two keys.
fn slot_signer(seed: &[u8; 32], slot: &[u8]) -> SigningKey {
    let mut hash = sha2::Sha256::new();
    hash.update(b"omni-dht-slot-key-v1\0");
    hash.update(seed);
    hash.update(slot);
    SigningKey::from_bytes(&hash.finalize().into())
}

/// The Mainline engine: one sync `Dht` client (its own actor thread) + a one-shot bootstrap gate.
/// The signed put/get primitive; rendezvous semantics live in the client.
struct DhtEngine {
    dht: Dht,
    /// One-shot bootstrap gate (see `ensure_bootstrapped`).
    boot: std::sync::Once,
}

impl DhtEngine {
    /// Production engine: a DHT client bootstrapped from the standard public Mainline routers.
    fn client() -> Result<Self, String> {
        let dht = Dht::client().map_err(|e| format!("dht client: {e}"))?;
        Ok(Self::from_dht(dht))
    }

    /// Engine bootstrapped from CUSTOM nodes instead of the public Mainline routers — the key to a
    /// DHT that works INSIDE an isolated network segment, where the global routers
    /// (router.bittorrent.com …) are unreachable but local DHT nodes are. Kademlia doesn't care
    /// whether the swarm is global or local; it only needs a reachable entry node to join. An empty
    /// list means "no override" → the standard global `client()`. See
    /// docs/decisions/serverless-connectivity-masterplan.md §3.
    fn with_bootstrap(nodes: &[String], extra: &[String]) -> Result<Self, String> {
        if nodes.is_empty() && extra.is_empty() {
            return Self::client();
        }
        let mut builder = Dht::builder();
        if !nodes.is_empty() {
            builder.bootstrap(nodes);
        }
        if !extra.is_empty() {
            // Поверх набора по умолчанию (или поверх кастомного, если он задан) — см. поле
            // `extra_bootstrap`: кэш дополняет точки входа, а не подменяет их.
            builder.extra_bootstrap(extra);
        }
        let dht = builder
            .build()
            .map_err(|e| format!("dht client (custom bootstrap): {e}"))?;
        Ok(Self::from_dht(dht))
    }

    fn from_dht(dht: Dht) -> Self {
        Self { dht, boot: std::sync::Once::new() }
    }

    /// Build the engine from a full [`EngineConfig`] — covers BOTH roles: a plain JOINER (custom
    /// `bootstrap`, or empty → global routers) AND an ENTRY/GENESIS node that others bootstrap through.
    /// The entry-node knobs (`server_mode` + fixed `port` + `public_ip` + `genesis`) are the server side
    /// of an isolated-segment DHT: without them no node — not even ours — can be a reachable in-segment
    /// bootstrap. See docs/decisions/dht-bootstrap-mesh-propagation.md.
    fn build(config: &EngineConfig) -> Result<Self, String> {
        // Fast path: a plain joiner with no entry-node knobs → byte-identical to the proven path.
        if config.port.is_none()
            && !config.server_mode
            && config.public_ip.is_none()
            && !config.genesis
        {
            return Self::with_bootstrap(&config.bootstrap, &config.extra_bootstrap);
        }
        let mut builder = Dht::builder();
        if config.genesis {
            // First node of a NEW isolated segment: NO bootstrap AND no global fallback, so this node's
            // routing table can never leak into the global swarm.
            builder.no_bootstrap();
        } else if !config.bootstrap.is_empty() {
            builder.bootstrap(&config.bootstrap);
        }
        // else: leave the default → the global Mainline routers.
        if !config.genesis && !config.extra_bootstrap.is_empty() {
            // Genesis намеренно исключён: он не должен видеть ничего, кроме себя, иначе таблица
            // нового сегмента протечёт в глобальный рой.
            builder.extra_bootstrap(&config.extra_bootstrap);
        }
        if let Some(port) = config.port {
            builder.port(port);
        }
        if config.server_mode {
            builder.server_mode();
        }
        if let Some(ip) = config.public_ip {
            builder.public_ip(ip);
        }
        let dht = builder
            .build()
            .map_err(|e| format!("dht client (entry-node config): {e}"))?;
        Ok(Self::from_dht(dht))
    }

    /// Block (once, lazily) until the first bootstrap query completes so the first op has a routing
    /// table (else `NoClosestNodes` on a fresh client); an empty/failed bootstrap just lets the op
    /// fail best-effort (the client's N-of-many covers a missing DHT leg). Lazy (not in the ctor) so
    /// construction never blocks on a slow/restricted network.
    fn ensure_bootstrapped(&self) {
        self.boot.call_once(|| {
            let _ = self.dht.bootstrapped();
        });
    }

    /// Signed BEP44 mutable put of `value` to the slot `salt` names, in the shape of a pkarr record:
    /// the slot's own keypair, no salt, the value inside a DNS packet, `seq` = `max(unix_micros,
    /// existing_seq + 1)` — computed HERE (the engine owns the `Dht`, so the read is local), strictly
    /// greater than any prior value so the swarm accepts our own refresh.
    fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]) -> Result<(), String> {
        if value.len() > max_value() {
            return Err(format!("{}B value exceeds the {}B a pkarr-shaped record holds", value.len(), max_value()));
        }
        if salt.is_empty() || salt.len() > MAX_SLOT_NAME {
            return Err(format!("slot name must be 1..={MAX_SLOT_NAME} bytes"));
        }
        let signer = slot_signer(key, salt);
        let pubkey = signer.verifying_key().to_bytes();
        let packet = pkarr_shape::wrap(&pubkey, value).ok_or("value does not fit a pkarr-shaped record")?;
        self.ensure_bootstrapped();
        let existing_seq = self
            .dht
            .get_mutable_most_recent(&pubkey, None)
            .map(|m| m.seq())
            .unwrap_or(0);
        let seq = (unix_micros() as i64).max(existing_seq + 1);
        let item = MutableItem::new(signer, &packet, seq, None);
        self.dht
            .put_mutable(item, None)
            .map(|_| ())
            .map_err(|e| format!("put: {e:?}"))
    }

    /// Most-recent value for the slot, or `None` if nothing of ours is published there.
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Option<Vec<u8>> {
        self.ensure_bootstrapped();
        let pubkey = slot_signer(key, salt).verifying_key().to_bytes();
        let item = self.dht.get_mutable_most_recent(&pubkey, None)?;
        pkarr_shape::unwrap(&pubkey, item.value())
    }

    /// The live routing-table nodes as `ip:port` strings (stale entries already filtered by the crate).
    /// A pure read of the CURRENT table — deliberately does NOT force a bootstrap (empty when the engine
    /// hasn't joined yet). The core persists this as a node cache so that after a restart it re-knocks on
    /// peers it actually saw online, re-joining the swarm without the well-known bootstrap routers — the
    /// key to a isolated-network DHT surviving a reboot without global entry points. See
    /// docs/decisions/dht-bootstrap-mesh-propagation.md.
    fn nodes(&self) -> Vec<String> {
        self.dht.to_bootstrap()
    }

    /// BEP 5 `announce_peer`: our IP as the swarm sees it, `port` given.
    fn announce(&self, info_hash: &[u8; 20], port: u16) -> Result<(), String> {
        let id = mainline::Id::from_bytes(info_hash).map_err(|e| format!("info_hash: {e}"))?;
        self.ensure_bootstrapped();
        self.dht.announce_peer(id, Some(port)).map(|_| ()).map_err(|e| format!("announce: {e}"))
    }

    /// BEP 5 `get_peers`: every address announced under `info_hash` (deduplicated, at most `cap`).
    fn peers(&self, info_hash: &[u8; 20], cap: usize) -> Result<Vec<String>, String> {
        let id = mainline::Id::from_bytes(info_hash).map_err(|e| format!("info_hash: {e}"))?;
        self.ensure_bootstrapped();
        let mut found: Vec<String> = Vec::new();
        for batch in self.dht.get_peers(id) {
            for peer in batch {
                let peer = peer.to_string();
                if !found.contains(&peer) {
                    found.push(peer);
                }
            }
            if found.len() >= cap {
                found.truncate(cap);
                break;
            }
        }
        Ok(found)
    }
}

/// At most this many addresses a `peers` answer carries (the directory shows at most 100 relays).
const PEERS_MAX: usize = 200;

fn parse_info_hash(hex: &str) -> Option<[u8; 20]> {
    from_hex(hex).and_then(|bytes| bytes.try_into().ok())
}

fn handle_announce(info_hash_hex: Option<&str>, port: Option<&str>, engine: &DhtEngine) -> String {
    let (Some(info_hash), Some(port)) = (
        info_hash_hex.and_then(parse_info_hash),
        port.and_then(|port| port.trim().parse::<u16>().ok()).filter(|port| *port != 0),
    ) else {
        return "ERR announce needs <info_hash_hex 20 bytes> <port>".to_string();
    };
    match engine.announce(&info_hash, port) {
        Ok(()) => "OK".to_string(),
        Err(error) => format!("ERR {error}"),
    }
}

fn handle_peers(info_hash_hex: Option<&str>, engine: &DhtEngine) -> String {
    let Some(info_hash) = info_hash_hex.and_then(parse_info_hash) else {
        return "ERR peers needs <info_hash_hex 20 bytes>".to_string();
    };
    match engine.peers(&info_hash, PEERS_MAX) {
        Ok(found) => format!("OK {}", found.join(",")).trim_end().to_string(),
        Err(error) => format!("ERR {error}"),
    }
}

/// pkarr's `seq`: the time in microseconds.
fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Engine configuration assembled from CLI args + env (the core spawns us with env; a standalone test
/// sets the same vars). Covers both roles — a JOINER (`bootstrap`) and an ENTRY/GENESIS node
/// (`server_mode` + `port` + `public_ip` + `genesis`). See [`DhtEngine::build`].
#[derive(Default)]
struct EngineConfig {
    /// Custom bootstrap nodes (`ip:port`) to JOIN through. Empty (and not genesis) ⇒ global routers.
    /// ЗАМЕНЯЕТ глобальные роутеры — это осознанный выбор оператора «работаем в изолированном
    /// сегменте», где глобальные всё равно недостижимы.
    bootstrap: Vec<String>,
    /// ДОПОЛНИТЕЛЬНЫЕ узлы поверх набора по умолчанию — не замена.
    ///
    /// Сюда идёт авто-собранный кэш живых узлов. Разница принципиальная: кэш собран нами самими и
    /// протухает (узлы роя ротируются быстро), поэтому подменять им глобальные роутеры нельзя —
    /// узел, вернувшийся через неделю, не смог бы войти в рой ВООБЩЕ и, что хуже, не попробовал бы
    /// запасной путь. Механизм выживания сегмента не должен становиться механизмом отрезания.
    extra_bootstrap: Vec<String>,
    /// Fixed UDP listen port — an entry node needs a KNOWN, firewall-openable port (else ephemeral).
    port: Option<u16>,
    /// Serve queries + be added to peers' routing tables — required to act as a bootstrap/entry node.
    server_mode: bool,
    /// A known public IPv4 for a secure node id (BEP42) — for a white-IP entry node.
    public_ip: Option<Ipv4Addr>,
    /// Genesis: the FIRST node of a NEW isolated segment (no bootstrap AND no global fallback).
    genesis: bool,
    /// Ask the local router (UPnP-IGD) to forward this node's DHT UDP `port` to us on the real WAN, so a
    /// home entry node is reachable without a manual port-forward. The mapped public IP becomes
    /// `public_ip` (unless one was given explicitly). Needs `port` set.
    upnp: bool,
}

impl EngineConfig {
    /// `args_bootstrap` = the `--bootstrap` arg (wins if given); otherwise `OMNI_DHT_BOOTSTRAP`. The
    /// entry-node knobs come from env: `OMNI_DHT_PORT`, `OMNI_DHT_SERVER_MODE`, `OMNI_DHT_PUBLIC_IP`,
    /// `OMNI_DHT_GENESIS`.
    fn from_env(args_bootstrap: Vec<String>) -> Self {
        let bootstrap = if !args_bootstrap.is_empty() {
            args_bootstrap
        } else {
            std::env::var("OMNI_DHT_BOOTSTRAP").map(|list| split_csv(&list)).unwrap_or_default()
        };
        EngineConfig {
            bootstrap,
            extra_bootstrap: std::env::var("OMNI_DHT_EXTRA_BOOTSTRAP")
                .map(|list| split_csv(&list))
                .unwrap_or_default(),
            port: std::env::var("OMNI_DHT_PORT").ok().and_then(|v| v.trim().parse().ok()),
            server_mode: env_flag("OMNI_DHT_SERVER_MODE"),
            public_ip: std::env::var("OMNI_DHT_PUBLIC_IP").ok().and_then(|v| v.trim().parse().ok()),
            genesis: env_flag("OMNI_DHT_GENESIS"),
            upnp: env_flag("OMNI_DHT_UPNP"),
        }
    }
}

/// UPnP-IGD lease for the DHT port mapping (seconds). The refresh loop re-maps before it expires.
const DHT_UPNP_LEASE_SECS: u32 = 3600;

/// Re-map at HALF the lease so a long-running entry node stays reachable — without this the router drops
/// the mapping after `DHT_UPNP_LEASE_SECS` and the node silently falls off the segment.
const DHT_UPNP_REFRESH_SECS: u64 = (DHT_UPNP_LEASE_SECS as u64) / 2;

/// Best-effort detect this host's LAN IPv4 on the interface with the default route to the internet.
/// Connecting a UDP socket sends nothing — it just resolves the local address the OS would use — so this
/// picks the REAL NIC, not an overlay adapter (e.g. a RadminVPN 26.x virtual NIC). Portable (no OS APIs).
fn detect_lan_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:53").ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// Ask the local router (UPnP-IGD) to forward the DHT UDP `port` to `lan_ip:port` and return the
/// resulting PUBLIC external endpoint. Requests the SAME external port as internal so the shared address
/// is STABLE across restarts (a peer can rely on `wan:port`); falls back to any external port if the
/// router won't grant the specific one. `None` if no IGD gateway answers, the request fails, or the
/// external IP isn't internet-public (CGNAT — inbound still blocked upstream). Idempotent — re-calling
/// refreshes the lease (see the refresh loop in `main`).
///
/// A near-duplicate of the mesh transport's `map_upnp`, kept separate ON PURPOSE: this one is SYNC (the
/// untrusted helper has no tokio) and diverges (fixed external port + lease refresh). Sharing it would
/// force a common crate + a heavier dep into the minimal helper AND touch the proven-live mesh UPnP path
/// — not worth it for ~20 lines. If they must converge, a sync primitive in `omni-net-primitives` is the
/// home; deferred deliberately (2026-07-10).
fn map_dht_port_upnp(lan_ip: Ipv4Addr, port: u16) -> Option<std::net::SocketAddr> {
    let options = igd_next::SearchOptions {
        bind_addr: std::net::SocketAddr::new(std::net::IpAddr::V4(lan_ip), 0),
        timeout: Some(std::time::Duration::from_secs(4)),
        ..Default::default()
    };
    let gateway = igd_next::search_gateway(options).ok()?;
    let local = std::net::SocketAddr::new(std::net::IpAddr::V4(lan_ip), port);
    // Prefer a stable public port (external == internal); fall back to any if the router won't grant it.
    let external = match gateway.add_port(
        igd_next::PortMappingProtocol::UDP,
        port,
        local,
        DHT_UPNP_LEASE_SECS,
        "Omni DHT",
    ) {
        Ok(()) => std::net::SocketAddr::new(gateway.get_external_ip().ok()?, port),
        Err(_) => gateway
            .get_any_address(igd_next::PortMappingProtocol::UDP, local, DHT_UPNP_LEASE_SECS, "Omni DHT")
            .ok()?,
    };
    match external.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_private() && !ip.is_loopback() => Some(external),
        _ => None,
    }
}

/// Split a comma-separated `ip:port` list into trimmed, non-empty entries.
fn split_csv(list: &str) -> Vec<String> {
    list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// An env var counts as set (`true`) when present and non-empty.
fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// Run the DHT engine helper with `args` (everything AFTER the program name) and return the process
/// exit code.
///
/// Two callers, one body: the thin `omni-dht-helper` bin, and the single-file host — which has no
/// sibling helper .exe on disk, so it re-execs ITSELF with `--run-helper dht …` and routes here via
/// the FFI (`omni_run_helper`). Never calls `process::exit`: the FFI caller must get the code back
/// rather than have the host process torn down.
/// See docs/decisions/omni-dht-helper.md.
pub fn run(args: Vec<String>) -> i32 {
    let mut args = args.into_iter();
    let mut listen: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut connect_cmd = String::new();
    // `--bootstrap` (comma-separated `ip:port`) wins if given; otherwise `EngineConfig::from_env` reads
    // `OMNI_DHT_BOOTSTRAP`. The entry-node knobs are env-only (see `EngineConfig::from_env`).
    let mut args_bootstrap: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next(),
            "--bootstrap" => {
                if let Some(list) = args.next() {
                    args_bootstrap = split_csv(&list);
                }
            }
            "--connect" => {
                connect = args.next();
                connect_cmd = args.by_ref().collect::<Vec<_>>().join(" ");
                break;
            }
            _ => {}
        }
    }

    if let Some(endpoint) = connect {
        return run_client(&endpoint, &connect_cmd);
    }

    let Some(addr) = listen else {
        eprintln!("usage: omni-dht-helper --listen <addr>  |  --connect <addr> <command>");
        return 2;
    };

    // Listener mode shows no terminal window — detach any inherited console (the app should also spawn
    // us with CreateNoWindow). Client mode keeps its console for stdout.
    #[cfg(windows)]
    detach_console();

    // Build the engine BEFORE binding so a `mainline` failure is reported on stderr and we exit rather
    // than accept connections we can't serve. The config picks the role: a JOINER (custom bootstrap, or
    // empty → global routers) or an ENTRY/GENESIS node for an isolated segment (server_mode + fixed port
    // + public_ip + genesis). See `EngineConfig` / `DhtEngine::build`.
    let mut config = EngineConfig::from_env(args_bootstrap);
    // Entry-node reachability: map our DHT UDP port on the home router so this node is dialable on the
    // real WAN without a manual port-forward — "Omni does its own UPnP", now for the DHT socket too. The
    // mapped PUBLIC endpoint is printed (a bootstrap peer dials THIS) and its IP feeds `public_ip`.
    if config.upnp {
        match config.port {
            Some(port) => match detect_lan_ipv4() {
                Some(lan) => match map_dht_port_upnp(lan, port) {
                    Some(external) => {
                        eprintln!("omni-dht-helper UPnP public endpoint {external}");
                        if config.public_ip.is_none() {
                            if let std::net::IpAddr::V4(ip) = external.ip() {
                                config.public_ip = Some(ip);
                            }
                        }
                        // Keep the mapping alive for the process lifetime: re-map before the lease
                        // expires, else a long-running entry node silently drops off after ~1h.
                        std::thread::spawn(move || loop {
                            std::thread::sleep(std::time::Duration::from_secs(DHT_UPNP_REFRESH_SECS));
                            let _ = map_dht_port_upnp(lan, port);
                        });
                    }
                    None => eprintln!(
                        "omni-dht-helper UPnP mapping failed (no IGD gateway / CGNAT) — this node may be unreachable as an entry point"
                    ),
                },
                None => eprintln!("omni-dht-helper UPnP: could not detect a LAN IPv4 to map from"),
            },
            None => eprintln!("omni-dht-helper: OMNI_DHT_UPNP set but OMNI_DHT_PORT missing — nothing to map"),
        }
    }
    let engine = match DhtEngine::build(&config) {
        Ok(e) => Arc::new(e),
        Err(error) => {
            eprintln!("cannot start DHT engine: {error}");
            return 1;
        }
    };

    // Proactively join the swarm so the routing table populates WITHOUT waiting for the first rendezvous
    // put/get — this is what makes the app's live "connected to N nodes" status meaningful shortly after
    // the user enables the DHT leg (the client otherwise bootstraps lazily on the first op). Background
    // thread: `bootstrapped()` blocks on a slow/restricted network and must not delay serving clients. The
    // `Once` gate in `ensure_bootstrapped` means a later op won't bootstrap twice.
    {
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || engine.ensure_bootstrapped());
    }

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(error) => {
            eprintln!("cannot listen on {addr}: {error}");
            return 1;
        }
    };
    // Print the RESOLVED bound address (not the arg) so a parent that passed `:0` can read the
    // ephemeral port from this line. Mirrors omni-rendezvous-helper / omni-node.
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    eprintln!("omni-dht-helper listening on {bound}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let engine = Arc::clone(&engine);
        std::thread::spawn(move || serve(stream, &engine));
    }
    0
}

fn serve(stream: TcpStream, engine: &DhtEngine) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.splitn(4, ' ');
        let cmd = parts.next().unwrap_or("");
        let response = match cmd {
            "put" => handle_put(parts.next(), parts.next(), parts.next(), engine),
            "get" => handle_get(parts.next(), parts.next(), engine),
            "rput" => handle_relay_put(parts.next(), parts.next(), parts.next()),
            "rget" => handle_relay_get(parts.next(), parts.next()),
            // Dump the live routing table as a comma-list of `ip:port` — the core persists it as a node
            // cache so a restart re-bootstraps from peers last seen online (segment DHT survives reboot
            // without global routers). Empty list ⇒ "OK " with no payload.
            "nodes" => format!("OK {}", engine.nodes().join(",")),
            "announce" => handle_announce(parts.next(), parts.next(), engine),
            "peers" => handle_peers(parts.next(), engine),
            "quit" => {
                let _ = writeln!(writer, "OK bye");
                break;
            }
            other => format!("ERR unknown command '{other}'"),
        };
        if writeln!(writer, "{response}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

fn handle_put(
    key_hex: Option<&str>,
    salt_hex: Option<&str>,
    value_hex: Option<&str>,
    engine: &DhtEngine,
) -> String {
    let (Some(key_hex), Some(salt_hex), Some(value_hex)) = (key_hex, salt_hex, value_hex) else {
        return "ERR put needs <key_hex> <salt_hex> <value_hex>".to_string();
    };
    let Some(key) = parse_key(key_hex) else {
        return "ERR bad key hex (need 32 bytes)".to_string();
    };
    let (Some(salt), Some(value)) = (from_hex(salt_hex), from_hex(value_hex)) else {
        return "ERR bad salt/value hex".to_string();
    };
    match engine.put(&key, &salt, &value) {
        Ok(()) => "OK".to_string(),
        Err(error) => format!("ERR {error}"),
    }
}

fn handle_get(key_hex: Option<&str>, salt_hex: Option<&str>, engine: &DhtEngine) -> String {
    let (Some(key_hex), Some(salt_hex)) = (key_hex, salt_hex) else {
        return "ERR get needs <key_hex> <salt_hex>".to_string();
    };
    let Some(key) = parse_key(key_hex) else {
        return "ERR bad key hex (need 32 bytes)".to_string();
    };
    let Some(salt) = from_hex(salt_hex) else {
        return "ERR bad salt hex".to_string();
    };
    match engine.get(&key, &salt) {
        Some(value) => format!("OK {}", to_hex(&value)),
        None => "OK".to_string(),
    }
}

/// The checks `put` makes, then the signed record through the pkarr relays.
fn handle_relay_put(key_hex: Option<&str>, salt_hex: Option<&str>, value_hex: Option<&str>) -> String {
    let (Some(key), Some(salt), Some(value)) = (
        key_hex.and_then(parse_key),
        salt_hex.and_then(from_hex),
        value_hex.and_then(from_hex),
    ) else {
        return "ERR rput needs <key_hex> <salt_hex> <value_hex>".to_string();
    };
    if value.len() > max_value() {
        return format!("ERR {}B value exceeds the {}B a pkarr-shaped record holds", value.len(), max_value());
    }
    if salt.is_empty() || salt.len() > MAX_SLOT_NAME {
        return format!("ERR slot name must be 1..={MAX_SLOT_NAME} bytes");
    }
    let signer = slot_signer(&key, &salt);
    let pubkey = signer.verifying_key().to_bytes();
    let Some(packet) = pkarr_shape::wrap(&pubkey, &value) else {
        return "ERR value does not fit a pkarr-shaped record".to_string();
    };
    let relays = relay::relays();
    // Past what a relay holds (BEP44 is last-writer-wins), like `put` past the swarm's.
    let existing_seq = match relay::fetch(&relays, &signer) {
        relay::Fetched::Item(item) => item.seq(),
        relay::Fetched::Empty => 0,
        relay::Fetched::Unreachable => return "ERR no pkarr relay answered".to_string(),
    };
    let seq = (unix_micros() as i64).max(existing_seq + 1);
    let item = MutableItem::new(signer, &packet, seq, None);
    if relay::publish(&relays, &item) {
        "OK".to_string()
    } else {
        "ERR no pkarr relay took the record".to_string()
    }
}

/// The slot's value through the pkarr relays: `OK <value_hex>`, `OK` for an empty slot, `ERR` when
/// none answered (the caller tells "empty" from "unreachable").
fn handle_relay_get(key_hex: Option<&str>, salt_hex: Option<&str>) -> String {
    let (Some(key), Some(salt)) = (key_hex.and_then(parse_key), salt_hex.and_then(from_hex)) else {
        return "ERR rget needs <key_hex> <salt_hex>".to_string();
    };
    if salt.is_empty() || salt.len() > MAX_SLOT_NAME {
        return format!("ERR slot name must be 1..={MAX_SLOT_NAME} bytes");
    }
    let signer = slot_signer(&key, &salt);
    let pubkey = signer.verifying_key().to_bytes();
    match relay::fetch(&relay::relays(), &signer) {
        relay::Fetched::Item(item) => match pkarr_shape::unwrap(&pubkey, item.value()) {
            Some(value) => format!("OK {}", to_hex(&value)),
            None => "OK".to_string(),
        },
        relay::Fetched::Empty => "OK".to_string(),
        relay::Fetched::Unreachable => "ERR no pkarr relay answered".to_string(),
    }
}

fn run_client(endpoint: &str, command: &str) -> i32 {
    let mut stream = match TcpStream::connect(endpoint) {
        Ok(s) => s,
        Err(error) => {
            eprintln!("connect {endpoint}: {error}");
            return 1;
        }
    };
    if writeln!(stream, "{command}").is_err() {
        eprintln!("write failed");
        return 1;
    }
    let _ = stream.flush();
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(_) => {
            let line = line.trim_end();
            println!("{line}");
            i32::from(line.starts_with("ERR"))
        }
        Err(error) => {
            eprintln!("read failed: {error}");
            1
        }
    }
}

/// Detach this process from any inherited console so the background service shows no terminal window
/// (Windows). Raw Win32 under `#[cfg(windows)]` (no Windows crates — Android-portability rule). A
/// no-op if there is no console. Mirrors omni-rendezvous-helper.
#[cfg(windows)]
fn detach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
    }
    // SAFETY: FreeConsole takes no args, returns a BOOL, and is safe to call with or without a console.
    unsafe {
        FreeConsole();
    }
}

/// Parse a 64-hex-char string into a 32-byte key, or `None` if it isn't exactly 32 bytes.
fn parse_key(hex: &str) -> Option<[u8; 32]> {
    let bytes = from_hex(hex)?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mainline::Testnet;
    use std::net::Ipv4Addr;

    /// An engine whose `Dht` is bound to LOCALHOST so it can join the in-process testnet (mainline's
    /// own tests bind this way; without it the client never populates a routing table for the
    /// localhost swarm → `NoClosestNodes`). Production binds a real interface via `client()`.
    fn engine_on(testnet: &Testnet) -> DhtEngine {
        let dht = Dht::builder()
            .bootstrap(&testnet.bootstrap)
            .bind_address(Ipv4Addr::LOCALHOST)
            .build()
            .expect("build testnet-bound client");
        DhtEngine::from_dht(dht)
    }

    /// Engine A publishes a value to a slot; engine B (a separate client sharing the seed) reads it
    /// back — the signed BEP44 put/get primitive round-tripping over an in-process Mainline testnet
    /// (no real network). This is the engine-level analog of `btdht.rs`'s dual-slot test.
    #[test]
    fn put_get_round_trip_through_testnet() {
        let testnet = Testnet::builder(10).build().expect("spin up an in-process testnet");
        let key = [0x5Au8; 32];
        let salt = b"0";
        let value = b"sealed-beacon-bytes".to_vec();

        let a = engine_on(&testnet);
        let b = engine_on(&testnet);

        a.put(&key, salt, &value).expect("A publishes the slot");
        let got = b.get(&key, salt).expect("B reads the slot back");
        assert_eq!(got, value, "B reads exactly what A published");

        // A distinct slot is a distinct, independent record — empty until written.
        assert!(b.get(&key, b"1").is_none(), "the other slot is empty");
    }

    /// What lands in the DHT is a pkarr record: no salt, `seq` in microseconds, the value a DNS
    /// packet of `_iroh` TXT records — and the two slots of a pair are two keys.
    #[test]
    fn the_stored_item_has_the_shape_of_a_pkarr_record() {
        let testnet = Testnet::builder(10).build().expect("spin up an in-process testnet");
        let engine = engine_on(&testnet);
        let key = [0x42u8; 32];
        let value = vec![0xA5u8; 540];
        engine.put(&key, b"0", &value).expect("publishes");

        let slot0 = slot_signer(&key, b"0").verifying_key().to_bytes();
        assert_ne!(slot0, slot_signer(&key, b"1").verifying_key().to_bytes(), "two slots, two keys");
        let item = engine.dht.get_mutable_most_recent(&slot0, None).expect("a salt-less item under the slot key");
        assert!(item.salt().is_none());
        assert!(item.seq() > 1_700_000_000_000_000, "seq is microseconds: {}", item.seq());
        assert_eq!(&item.value()[2..4], &[0x80, 0x00], "a DNS reply");
        assert!(item.value().windows(5).any(|w| w == b"_iroh"));
        assert!(engine.dht.get_mutable_most_recent(&slot0, Some(b"0")).is_none(), "nothing salted");
        assert_eq!(engine.get(&key, b"0"), Some(value));
    }

    /// Relay directory, Mainline shelves: two relays announce under one shelf, a reader sees both —
    /// many peers under one hash, which a BEP44 slot (one record, last writer wins) cannot hold.
    #[test]
    fn two_relays_announced_on_one_shelf_are_both_found() {
        let testnet = Testnet::builder(10).build().expect("spin up an in-process testnet");
        let shelf = [0x33u8; 20];
        let (relay_a, relay_b, reader) = (engine_on(&testnet), engine_on(&testnet), engine_on(&testnet));
        relay_a.announce(&shelf, 41_000).expect("relay A announces");
        relay_b.announce(&shelf, 42_000).expect("relay B announces");

        let found = reader.peers(&shelf, PEERS_MAX).expect("the reader gets peers");
        let ports: Vec<u16> = found.iter().filter_map(|peer| peer.rsplit(':').next()?.parse().ok()).collect();
        assert!(ports.contains(&41_000) && ports.contains(&42_000), "both relays: {found:?}");
        assert!(reader.peers(&[0x44u8; 20], PEERS_MAX).unwrap().is_empty(), "another shelf is empty");

        assert!(handle_announce(Some("zz"), Some("1"), &reader).starts_with("ERR"));
        assert!(handle_peers(Some(&to_hex(&shelf)), &reader).starts_with("OK 127.0.0.1:"));
    }

    /// After joining the testnet, `nodes()` dumps the routing table as `ip:port` strings — the node
    /// cache the core persists so a restart re-bootstraps from peers last seen online.
    #[test]
    fn nodes_dumps_the_routing_table_after_bootstrap() {
        let testnet = Testnet::builder(10).build().expect("spin up an in-process testnet");
        let engine = engine_on(&testnet);
        engine.ensure_bootstrapped(); // populate the routing table from the testnet
        let nodes = engine.nodes();
        assert!(!nodes.is_empty(), "routing table should hold testnet nodes after bootstrap");
        for node in &nodes {
            assert!(node.parse::<std::net::SocketAddr>().is_ok(), "{node} should be ip:port");
        }
    }

    #[test]
    fn split_csv_trims_and_drops_empties() {
        assert_eq!(split_csv("1.2.3.4:80, 5.6.7.8:90 ,,"), vec!["1.2.3.4:80", "5.6.7.8:90"]);
        assert!(split_csv("   ").is_empty());
    }

    /// The entry-node knobs (`server_mode` + fixed `port` + `genesis`/no-bootstrap) build a working
    /// engine — the server side of an isolated-segment DHT. A full genesis+join put/get is the live
    /// test's job; here we assert the config path constructs + serves, with an empty genesis table.
    #[test]
    fn entry_node_config_builds_a_genesis_server() {
        let config = EngineConfig {
            bootstrap: Vec::new(),
            // Genesis намеренно не получает и дополнительных узлов: таблица нового сегмента не
            // должна протечь в глобальный рой (см. `DhtEngine::build`).
            extra_bootstrap: Vec::new(),
            port: Some(0), // ephemeral loopback port for the test
            server_mode: true,
            public_ip: None,
            genesis: true,
            upnp: false, // no router discovery in a unit test
        };
        let engine = DhtEngine::build(&config).expect("genesis server-mode engine builds");
        // A fresh genesis node has an empty routing table (no bootstrap; nobody has contacted it yet).
        assert!(engine.nodes().is_empty(), "genesis routing table starts empty");
    }

    /// A value too big for one pkarr-shaped record is rejected before any put, and a bad/empty slot
    /// name too — no network touched (the guards return before `ensure_bootstrapped`).
    #[test]
    fn oversize_value_and_bad_slot_are_rejected_before_put() {
        // `with_bootstrap(&[])`-equivalent: a client with no bootstrap; the guards fire before any op.
        let dht = Dht::builder().bootstrap(&[] as &[String]).build().expect("client");
        let engine = DhtEngine::from_dht(dht);
        let key = [1u8; 32];
        assert!(engine.put(&key, b"0", &vec![0u8; max_value() + 1]).is_err(), "oversize rejected");
        assert!(engine.put(&key, b"", b"x").is_err(), "empty slot rejected");
        assert!(engine.put(&key, &[0u8; MAX_SLOT_NAME + 1], b"x").is_err(), "oversize slot rejected");
    }

    #[test]
    fn parse_key_requires_32_bytes() {
        assert!(parse_key(&"ab".repeat(32)).is_some(), "64 hex chars = 32 bytes");
        assert!(parse_key(&"ab".repeat(31)).is_none(), "31 bytes too short");
        assert!(parse_key("zz").is_none(), "non-hex rejected");
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none(), "odd length rejected");
    }
}
