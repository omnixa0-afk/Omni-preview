// Pinned relay (docs/decisions/public-relay-directory.md, stage 3): «Сидеть через него».
//
// The user picks a relay — from the directory or from their own list — and goes through it: the link
// to it is held and redialed, the direct path is forbidden (the same gate as «Только через релей»:
// beacons without my addresses, no punching), and the relay rides my relay bundle, so a contact learns
// from my beacon whom to dial to reach me and the mesh routes through it by announce.
//
// «До закрытия» lives in memory only; «всегда» in a profile file. One relay at a time — a new pin
// replaces the old.
//
// Losing it: the link down for `LOST_AFTER_MS` → lost. Unless the user asked to keep the address
// hidden, the profile's own mode applies again (direct allowed, as before the pin); either way the
// pin stays and the relay is redialed, slower; a pin «всегда» also looks for it in the directory now
// and then, in case it came back on another address.
//
// Held over its TCP door, the link climbs to UDP (`UPGRADE_EVERY_MS`): the direct gate is about
// contacts, and the relay itself is dialed direct either way. Rendezvous upgrades the other peers, but
// with direct forbidden it never dials, so the relay would stay on the door for good (live 2026-10-16:
// a whole evening at ~4 MB/s and 1-2 s RTT against 20 MB/s over UDP on the same two machines).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use omni_mesh_runtime::{MeshNodeAddress, MeshRuntime};
use omni_types::{OmniError, OmniResult};

use crate::{current_unix_ms, omni_app_trace_log, OmniApp};

/// The link down this long → the relay counts as lost (about three redials).
const LOST_AFTER_MS: u64 = 30_000;
/// Redial while the relay is held: three tries inside `LOST_AFTER_MS`.
const REDIAL_HELD_MS: u64 = 10_000;
/// Redial once lost: it may come back on the same address.
const REDIAL_LOST_MS: u64 = 60_000;
/// A pin «всегда» that is lost looks for its relay in the directory this often.
const DIRECTORY_LOOK_MS: u64 = 5 * 60_000;
/// Held over the door: try UDP this often (rendezvous retries its peers about as often).
const UPGRADE_EVERY_MS: u64 = 30_000;

/// The relay the profile goes through, as the user pinned it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedRelayV1 {
    /// The relay's mesh node address (hex, 40).
    pub node_id: String,
    pub name: String,
    /// `ip:port` — the QUIC/UDP address dialed.
    pub udp: String,
    #[serde(default)]
    pub tcp: Option<String>,
    /// Kept across restarts («всегда»), or only until the app closes («до закрытия»).
    pub always: bool,
    /// Once the relay is lost, still never go direct: messages wait, calls are off.
    #[serde(default)]
    pub keep_address_hidden: bool,
}

/// What the shell shows about the pin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedRelayStateV1 {
    pub relay: Option<PinnedRelayV1>,
    /// A live link to the relay right now.
    pub linked: bool,
    /// Lost this many seconds ago (`None` — held, or never linked yet and not overdue).
    pub lost_secs: Option<u64>,
}

struct Pin {
    relay: PinnedRelayV1,
    node: MeshNodeAddress,
    down_since_ms: Option<u64>,
    lost_since_ms: Option<u64>,
    last_dial_ms: u64,
    last_directory_look_ms: u64,
    last_upgrade_ms: u64,
    /// A dial or a door→UDP try under way, off the maintenance tick: either takes up to its timeout
    /// when the address is dead (a door dial held the tick 30 s, live 2026-10-16). Never two at once.
    connecting: Arc<AtomicBool>,
}

/// db_path → the pin (`None` — known to have none). Absent: not read from the profile yet.
static PINNED_RELAYS: OnceLock<Mutex<HashMap<String, Option<Pin>>>> = OnceLock::new();

fn pins() -> std::sync::MutexGuard<'static, HashMap<String, Option<Pin>>> {
    PINNED_RELAYS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Sign-out: a pin «до закрытия» ends here; «всегда» is read back from its file.
pub(crate) fn clear_pinned_relay_for_profile(db_path: &str) {
    pins().remove(db_path);
}

fn pinned_relay_path(db_path: &str) -> std::path::PathBuf {
    crate::profile_sidecar_path(db_path, "pinned-relay.json")
}

fn pin_from(relay: PinnedRelayV1) -> Option<Pin> {
    let node = crate::app_groups::node_from_hex(&relay.node_id)?;
    Some(Pin {
        relay,
        node,
        down_since_ms: None,
        lost_since_ms: None,
        last_dial_ms: 0,
        last_directory_look_ms: 0,
        last_upgrade_ms: 0,
        connecting: Arc::new(AtomicBool::new(false)),
    })
}

/// Run `f` on the profile's pin, reading the «всегда» file the first time.
fn with_pin<T>(db_path: &str, f: impl FnOnce(&mut Option<Pin>) -> T) -> T {
    let mut pins = pins();
    let pin = pins.entry(db_path.to_string()).or_insert_with(|| {
        std::fs::read_to_string(pinned_relay_path(db_path))
            .ok()
            .and_then(|raw| serde_json::from_str::<PinnedRelayV1>(&raw).ok())
            .and_then(pin_from)
    });
    f(pin)
}

pub(crate) fn pinned_relay(db_path: &str) -> Option<PinnedRelayV1> {
    with_pin(db_path, |pin| pin.as_ref().map(|pin| pin.relay.clone()))
}

/// Whether the profile may go direct: its mode allows it and no pin forbids it. A pin forbids it while
/// the relay is held — and after it is lost too, when the user asked to keep the address hidden.
pub(crate) fn allows_direct(db_path: &str) -> bool {
    let pin_forbids = with_pin(db_path, |pin| {
        pin.as_ref().is_some_and(|pin| pin.lost_since_ms.is_none() || pin.relay.keep_address_hidden)
    });
    crate::load_connection_privacy_mode(db_path).allows_direct_punch() && !pin_forbids
}

/// The relays a profile that does not go direct still links to directly: the pinned one and those in
/// «Узлы сети и релеи» (own and from contacts' bundles).
fn relay_nodes(storage: &omni_storage::Storage, db_path: &str) -> std::collections::HashSet<MeshNodeAddress> {
    let mut nodes: std::collections::HashSet<MeshNodeAddress> = storage
        .list_network_nodes()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|node| node.node_address)
        .filter_map(|bytes| <[u8; 20]>::try_from(bytes.as_slice()).ok())
        .map(MeshNodeAddress::from_bytes)
        .collect();
    if let Some(pin) = with_pin(db_path, |pin| pin.as_ref().map(|pin| pin.node)) {
        nodes.insert(pin);
    }
    nodes
}

/// Put the direct gate on the running mesh: whether to climb to direct, and — when not — direct links
/// kept only with the relays (the rest closed now and refused when they come). The one place the app
/// sets it: on start, on a mode or pin change, and every autoconnect tick (the relay list changes).
pub(crate) fn apply_direct_gate(storage: &omni_storage::Storage, runtime: &MeshRuntime, db_path: &str) {
    let allow = allows_direct(db_path);
    runtime.set_allow_direct_punch(allow);
    runtime.keep_direct_links_only_with((!allow).then(|| relay_nodes(storage, db_path)));
}

/// The pinned relay as an entry of my relay bundle — contacts dial it to reach me.
pub(crate) fn pinned_relay_bundle_entry(db_path: &str) -> Option<crate::RelayBundleEntryInput> {
    with_pin(db_path, |pin| {
        pin.as_ref().map(|pin| crate::RelayBundleEntryInput {
            node_address: Some(*pin.node.as_bytes()),
            endpoint: Some(pin.relay.udp.clone()),
            tcp_endpoint: pin.relay.tcp.clone(),
            label: pin.relay.name.clone(),
            cert_spki_pin_hex: None,
        })
    })
}

/// UDP first, then the relay's TCP door — straight away, not after the 30 s of a loss: with only
/// UDP tried, a profile whose hops are TCP (or whose UDP is dead) never reached its relay this way,
/// went «lost», went direct through rendezvous, came «back», and round again every half minute (live
/// 2026-10-16). Held over the door, the link climbs to UDP later (`upgrade_due`).
fn dial(runtime: &MeshRuntime, relay: &PinnedRelayV1, node: MeshNodeAddress) {
    let Ok(udp) = relay.udp.parse::<std::net::SocketAddr>() else {
        return;
    };
    let tcp = relay.tcp.as_deref().and_then(|tcp| tcp.parse::<std::net::SocketAddr>().ok());
    let udp_allowed = runtime.carrier_preference() != omni_mesh_runtime::CarrierPreference::Tcp;
    if udp_allowed {
        let _ = runtime.autoconnect_network_node(node, udp, tcp);
        if runtime.has_link(&node) {
            return;
        }
    }
    if let Some(door) = tcp {
        let up = runtime.dial_peer_tcp_direct(node, vec![door]);
        omni_app_trace_log("pinned-relay/door", &format!("door={door} udp_tried={udp_allowed} result={up:?}"));
    }
}

/// One maintenance tick: hold the link, notice a loss and a return, redial. Called from the
/// autoconnect loop.
pub(crate) fn tend_pinned_relay(storage: &omni_storage::Storage, runtime: &Arc<MeshRuntime>, db_path: &str) {
    let now = current_unix_ms() as u64;
    // Decide under the lock, dial and look outside it: a dial or a directory read can take seconds.
    let (to_dial, look, upgrade) = with_pin(db_path, |slot| {
        let Some(pin) = slot.as_mut() else {
            return (None, None, None);
        };
        if runtime.has_link(&pin.node) {
            pin.down_since_ms = None;
            if pin.lost_since_ms.take().is_some() {
                omni_app_trace_log("pinned-relay/back", &format!("relay={}", pin.relay.node_id));
            }
            let upgrade = upgrade_due(pin, runtime.peer_on_door(&pin.node), now);
            return (None, None, upgrade);
        }
        let down_since = *pin.down_since_ms.get_or_insert(now);
        if pin.lost_since_ms.is_none() && now.saturating_sub(down_since) >= LOST_AFTER_MS {
            pin.lost_since_ms = Some(now);
            omni_app_trace_log(
                "pinned-relay/lost",
                &format!("relay={} keep_address_hidden={}", pin.relay.node_id, pin.relay.keep_address_hidden),
            );
        }
        let every = if pin.lost_since_ms.is_some() { REDIAL_LOST_MS } else { REDIAL_HELD_MS };
        let to_dial = (now.saturating_sub(pin.last_dial_ms) >= every
            && !pin.connecting.swap(true, Ordering::AcqRel))
            .then(|| {
                pin.last_dial_ms = now;
                (pin.relay.clone(), pin.node, Arc::clone(&pin.connecting))
            });
        let look = (pin.lost_since_ms.is_some()
            && pin.relay.always
            && now.saturating_sub(pin.last_directory_look_ms) >= DIRECTORY_LOOK_MS)
            .then(|| {
                pin.last_directory_look_ms = now;
                pin.relay.node_id.clone()
            });
        (to_dial, look, None)
    });
    // Every tick, not only on a loss or a return: the relay list changes under it too.
    apply_direct_gate(storage, runtime, db_path);
    if let Some(node_id) = look {
        look_in_directory(runtime, db_path, &node_id);
    }
    if let Some((relay, node, connecting)) = to_dial {
        off_the_tick(runtime, connecting, "omni-pinned-relay-dial", move |runtime| dial(runtime, &relay, node));
    }
    if let Some((udp, node, connecting)) = upgrade {
        off_the_tick(runtime, connecting, "omni-pinned-relay-udp", move |runtime| {
            let up = runtime.upgrade_peer_to_udp(node, vec![udp]);
            omni_app_trace_log("pinned-relay/upgrade-udp", &format!("udp={udp} up={up}"));
        });
    }
}

/// Run `work` on its own thread and lower `connecting` when it ends (raised by the caller).
fn off_the_tick(
    runtime: &Arc<MeshRuntime>,
    connecting: Arc<AtomicBool>,
    name: &str,
    work: impl FnOnce(&MeshRuntime) + Send + 'static,
) {
    let runtime = Arc::clone(runtime);
    let flag = Arc::clone(&connecting);
    let spawned = std::thread::Builder::new().name(name.to_string()).spawn(move || {
        work(&runtime);
        flag.store(false, Ordering::Release);
    });
    if spawned.is_err() {
        connecting.store(false, Ordering::Release);
    }
}

/// Held over the door, no try under way and the last one long enough ago: the relay's UDP address
/// to try, its node and the in-flight flag (already raised).
fn upgrade_due(pin: &mut Pin, on_door: bool, now: u64) -> Option<(std::net::SocketAddr, MeshNodeAddress, Arc<AtomicBool>)> {
    if !on_door || now.saturating_sub(pin.last_upgrade_ms) < UPGRADE_EVERY_MS {
        return None;
    }
    let udp = pin.relay.udp.parse().ok()?;
    if pin.connecting.swap(true, Ordering::AcqRel) {
        return None;
    }
    pin.last_upgrade_ms = now;
    Some((udp, pin.node, Arc::clone(&pin.connecting)))
}

/// A lost pin «всегда»: is the relay in the directory again, maybe on another address? Take the new
/// address; the next redial goes there.
fn look_in_directory(runtime: &MeshRuntime, db_path: &str, node_id: &str) {
    let Ok(rendezvous) = crate::profile_rendezvous_runtime(db_path) else {
        return;
    };
    let mut sightings = HashMap::new();
    let mainline = crate::app_relay_directory::LiveMainline { db_path, runtime };
    let result = crate::app_relay_directory::search_public_relays_via(
        rendezvous.helper.as_ref(),
        Some(&mainline),
        None,
        &mut sightings,
        current_unix_ms() as u64,
    );
    let Some(found) = result.relays.into_iter().find(|relay| relay.node_id.eq_ignore_ascii_case(node_id)) else {
        return;
    };
    let Some(udp) = found.udp.first().cloned() else {
        return;
    };
    let relay = with_pin(db_path, |slot| {
        let pin = slot.as_mut().filter(|pin| pin.relay.node_id.eq_ignore_ascii_case(node_id))?;
        if pin.relay.udp == udp && pin.relay.tcp == found.tcp.first().cloned() {
            return None;
        }
        pin.relay.udp = udp;
        pin.relay.tcp = found.tcp.first().cloned();
        pin.last_dial_ms = 0;
        Some(pin.relay.clone())
    });
    if let Some(relay) = relay {
        omni_app_trace_log("pinned-relay/moved", &format!("relay={} udp={}", relay.node_id, relay.udp));
        let _ = store_pinned_relay(db_path, Some(&relay));
    }
}

fn store_pinned_relay(db_path: &str, relay: Option<&PinnedRelayV1>) -> OmniResult<()> {
    let path = pinned_relay_path(db_path);
    match relay.filter(|relay| relay.always) {
        Some(relay) => {
            let json = serde_json::to_string(relay).map_err(|error| OmniError::Internal(error.to_string()))?;
            std::fs::write(&path, json)
                .map_err(|error| OmniError::Storage(format!("failed to persist the pinned relay: {error}")))
        }
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(OmniError::Storage(format!("failed to forget the pinned relay: {error}"))),
        },
    }
}

impl OmniApp {
    /// Go through this relay (replaces a pin there was). `udp` — its `ip:port`; `tcp` — its TCP door.
    pub fn pin_relay(&self, relay: PinnedRelayV1) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("pin_relay")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mut relay = relay;
        relay.name = relay.name.trim().chars().take(omni_rendezvous::relay_directory::RELAY_NAME_MAX_CHARS).collect();
        relay.udp = relay.udp.trim().to_string();
        relay.tcp = relay.tcp.map(|tcp| tcp.trim().to_string()).filter(|tcp| !tcp.is_empty());
        relay.udp.parse::<std::net::SocketAddr>().map_err(|_| {
            OmniError::InvalidInput(format!("'{}' is not an ip:port address of the relay", relay.udp))
        })?;
        if let Some(tcp) = &relay.tcp {
            tcp.parse::<std::net::SocketAddr>()
                .map_err(|_| OmniError::InvalidInput(format!("'{tcp}' is not an ip:port TCP door")))?;
        }
        let Some(pin) = pin_from(relay.clone()) else {
            return Err(OmniError::InvalidInput(format!("'{}' is not a relay node id", relay.node_id)));
        };
        if crate::app_screen_control::local_signing_identity(&self.storage)
            .ok()
            .and_then(|(_, public)| crate::node_address_from_ed25519_pubkey(public).ok())
            .is_some_and(|mine| mine == pin.node)
        {
            return Err(OmniError::InvalidInput("this is this device itself".to_string()));
        }
        store_pinned_relay(&db_path, Some(&relay))?;
        let previous = with_pin(&db_path, |slot| slot.replace(pin).map(|old| old.relay));
        omni_app_trace_log(
            "pinned-relay/pin",
            &format!("relay={} udp={} always={} keep_address_hidden={}", relay.node_id, relay.udp, relay.always, relay.keep_address_hidden),
        );
        self.after_pin_change(previous.map(|old| (old.node_id, old.udp, old.tcp)) != Some((relay.node_id.clone(), relay.udp.clone(), relay.tcp.clone())));
        Ok(())
    }

    /// Stop going through the pinned relay: the profile's own mode applies again.
    pub fn unpin_relay(&self) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("unpin_relay")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        store_pinned_relay(&db_path, None)?;
        let had = with_pin(&db_path, |slot| slot.take().is_some());
        omni_app_trace_log("pinned-relay/unpin", &format!("had={had}"));
        self.after_pin_change(had);
        Ok(())
    }

    pub fn pinned_relay_state(&self) -> PinnedRelayStateV1 {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let runtime = self.mesh_runtime();
        let now = current_unix_ms() as u64;
        with_pin(&db_path, |slot| match slot.as_ref() {
            None => PinnedRelayStateV1 { relay: None, linked: false, lost_secs: None },
            Some(pin) => PinnedRelayStateV1 {
                relay: Some(pin.relay.clone()),
                linked: runtime.as_ref().is_some_and(|runtime| runtime.has_link(&pin.node)),
                lost_secs: pin.lost_since_ms.map(|since| now.saturating_sub(since) / 1000),
            },
        })
    }

    /// The gate follows the pin at once; the relay is dialed now; a changed relay set reaches contacts.
    fn after_pin_change(&self, relay_set_changed: bool) {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if let Some(runtime) = self.mesh_runtime() {
            apply_direct_gate(&self.storage, &runtime, &db_path);
            if let Some(relay) = pinned_relay(&db_path) {
                if let Some(node) = crate::app_groups::node_from_hex(&relay.node_id) {
                    with_pin(&db_path, |slot| {
                        if let Some(pin) = slot.as_mut() {
                            pin.last_dial_ms = current_unix_ms() as u64;
                        }
                    });
                    let _ = std::thread::Builder::new()
                        .name("omni-pinned-relay-dial".to_string())
                        .spawn(move || dial(&runtime, &relay, node));
                }
            }
        }
        if relay_set_changed {
            let _ = self.bump_my_relay_bundle_version();
            self.push_relay_set_update_to_linked_contacts();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(always: bool, keep_address_hidden: bool) -> PinnedRelayV1 {
        PinnedRelayV1 {
            node_id: "ab".repeat(20),
            name: "pc-stand".to_string(),
            udp: "203.0.113.5:51820".to_string(),
            tcp: Some("203.0.113.5:443".to_string()),
            always,
            keep_address_hidden,
        }
    }

    fn db(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("omni-pinned-relay-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("omni.db").to_string_lossy().to_string()
    }

    #[test]
    fn a_pin_forbids_direct_until_lost_unless_the_address_stays_hidden() {
        for (hidden, after_loss) in [(false, true), (true, false)] {
            let db = db(&format!("gate-{hidden}"));
            with_pin(&db, |slot| *slot = pin_from(relay(false, hidden)));
            assert!(!allows_direct(&db), "held: never direct");
            with_pin(&db, |slot| slot.as_mut().unwrap().lost_since_ms = Some(1));
            assert_eq!(allows_direct(&db), after_loss, "lost, keep_address_hidden={hidden}");
            clear_pinned_relay_for_profile(&db);
        }
    }

    /// Held over the door, the relay gets a UDP try every `UPGRADE_EVERY_MS`, one at a time; held
    /// over UDP, none.
    #[test]
    fn a_relay_held_over_the_door_is_tried_on_udp_now_and_then() {
        let mut pin = pin_from(relay(true, false)).unwrap();
        assert!(upgrade_due(&mut pin, false, 1_000_000).is_none(), "over UDP: nothing to climb");
        let (udp, node, connecting) = upgrade_due(&mut pin, true, 1_000_000).expect("over the door: try");
        assert_eq!(udp, "203.0.113.5:51820".parse().unwrap());
        assert_eq!(node, pin.node);
        assert!(upgrade_due(&mut pin, true, 1_000_000 + UPGRADE_EVERY_MS).is_none(), "one try at a time");
        connecting.store(false, Ordering::Release);
        assert!(upgrade_due(&mut pin, true, 1_000_000 + UPGRADE_EVERY_MS - 1).is_none(), "not before its time");
        assert!(upgrade_due(&mut pin, true, 1_000_000 + UPGRADE_EVERY_MS).is_some(), "the next one");
    }

    #[test]
    fn only_an_always_pin_outlives_the_session() {
        let db = db("persist");
        store_pinned_relay(&db, Some(&relay(true, false))).unwrap();
        assert_eq!(pinned_relay(&db), Some(relay(true, false)));
        clear_pinned_relay_for_profile(&db);
        assert_eq!(pinned_relay(&db), Some(relay(true, false)), "read back from the profile");

        store_pinned_relay(&db, Some(&relay(false, false))).unwrap();
        clear_pinned_relay_for_profile(&db);
        assert_eq!(pinned_relay(&db), None, "a session pin leaves no file");
        clear_pinned_relay_for_profile(&db);
    }

    #[test]
    fn the_pinned_relay_rides_the_bundle_with_its_node() {
        let db = db("bundle");
        with_pin(&db, |slot| *slot = pin_from(relay(false, false)));
        let entry = pinned_relay_bundle_entry(&db).unwrap();
        assert_eq!(entry.node_address, Some([0xab; 20]));
        assert_eq!(entry.endpoint.as_deref(), Some("203.0.113.5:51820"));
        assert_eq!(entry.tcp_endpoint.as_deref(), Some("203.0.113.5:443"));
        clear_pinned_relay_for_profile(&db);
    }
}
