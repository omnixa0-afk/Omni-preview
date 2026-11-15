//! TCP hole punch between two nodes behind NATs: one side listens on its door and keeps a pinhole
//! open towards the other with low-TTL SYNs, the other connects from its door's port to the
//! listener's published address. Measured on a real pair: data flows only when exactly one side
//! answers SYN-ACK, so there is no simultaneous open here. See docs/decisions/tcp-hole-punch.md.
//!
//! Both sides start a window when the rendezvous opens the other's beacon and finds a punch address
//! in it; the roles are computed the same way on both, without an exchange, and swap every epoch so
//! that a pair whose NATs carry only one direction gets to try the other.
//!
//! A NAT may give the door's port the same external port for every destination and still hand out
//! a NEW one once the port has been idle a while (stand 2026-11-09: the laptop's CGNAT moved the door
//! from one port to another within four minutes). So the own address is asked again the moment a
//! window opens and every 15 s while one runs — those queries and the window's own traffic keep the
//! mapping alive meanwhile — and a window takes the peer's address from each fresh beacon, not only
//! the first.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_net_primitives::tcp_punch::{
    connect_from_door_port, send_pinhole_syn, tcp_stun_probe_from_port,
};
use omni_network::{CarrierPreference, Network};
use parking_lot::Mutex;

use crate::{register_link, LinkMap, MeshCallbacks, MeshLinkQuality, MeshNodeAddress};

/// How long one role lasts. Longer than a window's worth of tries in one role, short enough that a
/// pair whose NATs carry only one direction reaches the other within a couple of rendezvous sweeps.
/// The two sides' clocks may differ by seconds; that costs a few wasted tries at each boundary.
pub const TCP_PUNCH_EPOCH_SECS: u64 = 60;
/// A window started (or extended) by a rendezvous sweep runs this long. Covers more than one epoch,
/// so one window tries both roles, and is longer than the gap between sweeps of an unlinked contact,
/// so the two sides' windows overlap even though their sweeps are not in step.
const TCP_PUNCH_WINDOW: Duration = Duration::from_secs(150);
/// The listener's pinhole SYN interval (the probe that measured the scheme used 1 s).
const PINHOLE_EVERY: Duration = Duration::from_secs(1);
/// How long the pinhole socket is kept before it is dropped: enough for the SYN to leave.
const PINHOLE_HOLD: Duration = Duration::from_millis(150);
/// IP TTLs of the pinhole SYN, in turn: enough to cross our own NATs (two layers on a CGNAT line),
/// too few to reach the peer's. All three carried data in the measurements.
const PINHOLE_TTLS: [u32; 3] = [3, 4, 5];
/// One connect attempt of the dialer.
const DIAL_ATTEMPT: Duration = Duration::from_millis(2500);
/// Pause after an attempt that was refused or reset at once (the peer's NAT holds no pinhole yet).
const DIAL_RETRY_PAUSE: Duration = Duration::from_millis(200);
/// QUIC handshake + identity challenge over a connected punch.
const PUNCHED_HANDSHAKE_LIMIT: Duration = Duration::from_secs(15);
/// Re-learn our punch address this often when no window runs (a NAT may remap).
const REPROBE_EVERY: Duration = Duration::from_secs(5 * 60);
/// … and this often while a window runs: the address the peer is given must be the one the NAT uses
/// now. It also keeps that address: a full connection from the door's port renews the mapping, which
/// the listener's low-TTL SYNs may not reach. Measured on a CGNAT line (2026-11-09): a TCP mapping
/// idle 20 s kept its port, idle 40 s got a new one.
const REPROBE_WHILE_PUNCHING: Duration = Duration::from_secs(15);
/// One STUN-over-TCP query.
const STUN_QUERY_LIMIT: Duration = Duration::from_secs(4);

/// Which side of the punch a node takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpPunchRole {
    /// Listens on its door and sends pinhole SYNs to the peer.
    Listener,
    /// Connects from its door's port to the peer's punch address.
    Dialer,
}

/// The epoch a moment falls in (seconds since the unix epoch).
pub fn tcp_punch_epoch(unix_secs: u64) -> u64 {
    unix_secs / TCP_PUNCH_EPOCH_SECS
}

/// The role `local` takes towards `peer` in `epoch`: the lower address dials in even epochs, the
/// higher in odd ones. Both sides compute the same thing, so exactly one of them dials.
pub fn tcp_punch_role(local: &MeshNodeAddress, peer: &MeshNodeAddress, epoch: u64) -> TcpPunchRole {
    if (local < peer) == epoch.is_multiple_of(2) {
        TcpPunchRole::Dialer
    } else {
        TcpPunchRole::Listener
    }
}

/// What [`crate::MeshRuntime::start_tcp_punch`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpPunchStart {
    /// A new window is running.
    Started,
    /// A window was already running for this peer; it now ends later, aimed at the address given.
    Extended,
    /// Already linked directly: nothing to punch.
    AlreadyLinked,
    /// No door listener this run.
    NoDoor,
    /// We have no punch address of our own (the peer could not reach us or be reached from it).
    NoOwnAddress,
    /// Direct links are off (relay-only privacy, or QUIC-only carrier).
    NotAllowed,
}

#[derive(Clone, Copy)]
struct Window {
    until: Instant,
    punch: SocketAddr,
}

/// The punch state of one mesh: our own punch address and the running windows.
#[derive(Default)]
pub(crate) struct TcpPunchState {
    /// Our door's external TCP address, when two STUN servers agree on it.
    pub(crate) own_address: Mutex<Option<SocketAddr>>,
    /// Peer → its running window.
    windows: Mutex<HashMap<MeshNodeAddress, Window>>,
    /// Wakes the own-address loop when a window opens.
    window_opened: tokio::sync::Notify,
    /// What [`Self::address_to_publish`] last gave out.
    published: Mutex<Option<SocketAddr>>,
    /// Own-address probes finished so far: lets a caller wait for the one a new window asked for.
    pub(crate) probes: std::sync::atomic::AtomicU64,
}

impl TcpPunchState {
    /// Start a window for `peer` aimed at `punch`, or extend the running one and re-aim it (the
    /// peer's NAT may have moved). `true` if a new one has to be spawned.
    pub(crate) fn open_window(&self, peer: MeshNodeAddress, punch: SocketAddr) -> bool {
        let until = Instant::now() + TCP_PUNCH_WINDOW;
        let fresh = self.windows.lock().insert(peer, Window { until, punch }).is_none();
        if fresh {
            self.window_opened.notify_one();
        }
        fresh
    }

    pub(crate) fn window_open(&self, peer: &MeshNodeAddress) -> bool {
        self.windows.lock().contains_key(peer)
    }

    fn window(&self, peer: &MeshNodeAddress) -> Option<Window> {
        self.windows.lock().get(peer).copied()
    }

    fn close_window(&self, peer: &MeshNodeAddress) {
        self.windows.lock().remove(peer);
    }

    fn punching(&self) -> bool {
        !self.windows.lock().is_empty()
    }

    /// The punch address to put in our beacons. A beacon whose content changed is posted to the
    /// couriers at once, and behind a CGNAT the door's port changes every few minutes while it is idle
    /// (stand 2026-11-09: a new port after 40 s): handing out every fresh port would turn each
    /// rendezvous sweep of an unlinked contact into a publication. So the port follows the NAT only
    /// while a window runs — that is when the peer needs it, and the window asks again and republishes
    /// anyway — or when the public IP itself changed (another network). Otherwise the address given
    /// out last stays.
    pub(crate) fn address_to_publish(&self) -> Option<SocketAddr> {
        let own = *self.own_address.lock();
        let mut published = self.published.lock();
        let keep = match (own, *published) {
            (Some(own), Some(last)) => !self.punching() && last.ip() == own.ip(),
            _ => false,
        };
        if !keep {
            *published = own;
        }
        *published
    }
}

fn punch_trace(message: &str) {
    omni_net_primitives::trace_line("net/tcp-punch", message);
}

fn unix_now_secs() -> u64 {
    unix_now_ms() / 1000
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long the epoch `unix_ms` falls in still runs.
fn epoch_left(unix_ms: u64) -> Duration {
    let epoch_ms = TCP_PUNCH_EPOCH_SECS * 1000;
    Duration::from_millis(epoch_ms - unix_ms % epoch_ms)
}

fn linked_directly(network: &Network, links: &LinkMap, peer: &MeshNodeAddress) -> bool {
    links.lock().contains_key(peer) && matches!(network.peer_link_quality(peer), Some(MeshLinkQuality::Direct))
}

/// What one role's stretch of a window did — written when the role changes or the window ends, so
/// the trace says whether the dialer's connects timed out (nothing came back) or were refused.
#[derive(Default)]
struct Tally {
    holes: u32,
    connects: u32,
    timed_out: u32,
    refused: u32,
    connected: u32,
}

impl Tally {
    fn describe(&self) -> String {
        format!(
            "pinhole SYNs {}, connects {} (timed out {}, refused/reset {}, connected {})",
            self.holes, self.connects, self.timed_out, self.refused, self.connected
        )
    }
}

/// Everything a window needs, owned (it runs as its own task).
pub(crate) struct TcpPunchWindow {
    pub(crate) network: Arc<Network>,
    pub(crate) links: LinkMap,
    pub(crate) callbacks: MeshCallbacks,
    pub(crate) state: Arc<TcpPunchState>,
    pub(crate) door_port: u16,
    pub(crate) local: MeshNodeAddress,
    pub(crate) peer: MeshNodeAddress,
}

impl TcpPunchWindow {
    /// Run until the peer is linked directly or the window ends. Never blocks the caller: spawned.
    pub(crate) async fn run(self) {
        let Self { network, links, callbacks, state, door_port, local, peer } = self;
        let mut tally = Tally::default();
        let mut ttl_turn = 0usize;
        let mut last_role = None;
        let mut last_punch = None;
        loop {
            let Some(window) = state.window(&peer) else {
                return;
            };
            let punch = window.punch;
            if last_punch != Some(punch) {
                punch_trace(&format!("window peer={peer:?} punch={punch} door_port={door_port}"));
                last_punch = Some(punch);
            }
            if linked_directly(&network, &links, &peer) {
                state.close_window(&peer);
                punch_trace(&format!("window peer={peer:?} ended: linked ({})", tally.describe()));
                return;
            }
            if Instant::now() >= window.until || network.carrier_preference() == CarrierPreference::Quic {
                state.close_window(&peer);
                punch_trace(&format!("window peer={peer:?} ended without a link ({})", tally.describe()));
                return;
            }
            let role = tcp_punch_role(&local, &peer, tcp_punch_epoch(unix_now_secs()));
            if last_role != Some(role) {
                if let Some(previous) = last_role {
                    punch_trace(&format!("window peer={peer:?} as {previous:?}: {}", tally.describe()));
                }
                tally = Tally::default();
                punch_trace(&format!("window peer={peer:?} role={role:?}"));
                last_role = Some(role);
            }
            match role {
                TcpPunchRole::Listener => {
                    let ttl = PINHOLE_TTLS[ttl_turn % PINHOLE_TTLS.len()];
                    ttl_turn += 1;
                    match send_pinhole_syn(door_port, punch, ttl, PINHOLE_HOLD).await {
                        Ok(false) => tally.holes += 1,
                        // The peer's SYN met ours: a simultaneous open, over which this scheme's NATs
                        // carry no data (the measured rule). Dropped; the peer's next connect is
                        // accepted by the door instead.
                        Ok(true) => punch_trace(&format!(
                            "window peer={peer:?} pinhole socket connected (simultaneous open) — closed"
                        )),
                        Err(error) => punch_trace(&format!("window peer={peer:?} pinhole SYN failed: {error}")),
                    }
                    tokio::time::sleep(PINHOLE_EVERY.saturating_sub(PINHOLE_HOLD)).await;
                }
                TcpPunchRole::Dialer => {
                    let left = epoch_left(unix_now_ms());
                    if left < DIAL_ATTEMPT {
                        // An attempt now would still be connecting when the peer starts dialing in the
                        // next epoch: two connections on one pair of addresses, and both handshakes fail
                        // (stand 2026-11-09: an attempt begun 1.8 s before the boundary connected 0.5 s
                        // after it, the peer's own connect 0.2 s later, both links failed).
                        tokio::time::sleep(left).await;
                        continue;
                    }
                    tally.connects += 1;
                    match connect_from_door_port(door_port, punch, DIAL_ATTEMPT).await {
                        Ok(stream) => {
                            tally.connected += 1;
                            punch_trace(&format!(
                                "window peer={peer:?} TCP connected {} -> {punch}; QUIC handshake",
                                stream.local_addr().map(|a| a.to_string()).unwrap_or_default()
                            ));
                            let dial = network.dial_over_punched(peer, punch, stream, door_port);
                            match tokio::time::timeout(PUNCHED_HANDSHAKE_LIMIT, dial).await {
                                Ok(Ok(link)) => {
                                    register_link(link, &links, &callbacks, true);
                                    state.close_window(&peer);
                                    punch_trace(&format!(
                                        "window peer={peer:?} LINKED over the punch ({})",
                                        tally.describe()
                                    ));
                                    return;
                                }
                                Ok(Err(error)) => punch_trace(&format!(
                                    "window peer={peer:?} connected, but no link: {error}"
                                )),
                                Err(_) => punch_trace(&format!(
                                    "window peer={peer:?} connected, but the QUIC handshake timed out"
                                )),
                            }
                            network.forget_punched_route(punch);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => tally.timed_out += 1,
                        Err(_) => {
                            tally.refused += 1;
                            tokio::time::sleep(DIAL_RETRY_PAUSE).await;
                        }
                    }
                }
            }
        }
    }
}

/// Keep `state.own_address` current: STUN over TCP from the door's port on a network change, at once
/// when a window opens, every [`REPROBE_WHILE_PUNCHING`] while one runs and every [`REPROBE_EVERY`]
/// otherwise. Published only when two servers saw exactly the same public address — the NAT gives
/// the door's port one mapping for every destination, so a peer can use it.
pub(crate) async fn keep_own_address(
    network: Arc<Network>,
    state: Arc<TcpPunchState>,
    door_port: u16,
    every: Duration,
) {
    let mut last_lan = None;
    let mut probed_at: Option<Instant> = None;
    loop {
        if network.carrier_preference() == CarrierPreference::Quic {
            *state.own_address.lock() = None;
            probed_at = None;
        } else {
            let lan = crate::detect_primary_lan_ipv4();
            let due = if state.punching() { REPROBE_WHILE_PUNCHING } else { REPROBE_EVERY };
            if lan != last_lan || probed_at.is_none_or(|at| at.elapsed() >= due) {
                last_lan = lan;
                probed_at = Some(Instant::now());
                let servers = resolve_tcp_stun_servers().await;
                let probe = tcp_stun_probe_from_port(door_port, &servers, STUN_QUERY_LIMIT).await;
                let agreed = omni_net_primitives::stun::tcp_punch_consensus(&probe.answers)
                    .filter(|addr| omni_net_primitives::is_public_ip(addr.ip()));
                let before = std::mem::replace(&mut *state.own_address.lock(), agreed);
                state.probes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if before != agreed || agreed.is_none() {
                    punch_trace(&format!(
                        "own address door_port={door_port} servers={} asked={} answers={:?} -> {}",
                        servers.len(),
                        probe.asked,
                        probe.answers,
                        agreed.map(|a| a.to_string()).unwrap_or_else(|| "none (no agreement)".into())
                    ));
                }
            }
        }
        let pause = if state.punching() { REPROBE_WHILE_PUNCHING.min(every) } else { every };
        tokio::select! {
            _ = state.window_opened.notified() => probed_at = None,
            _ = tokio::time::sleep(crate::jittered(pause)) => {}
        }
    }
}

async fn resolve_tcp_stun_servers() -> Vec<SocketAddr> {
    let mut resolved = Vec::new();
    for server in omni_net_primitives::stun::DEFAULT_TCP_STUN_SERVERS {
        if let Ok(addrs) = tokio::net::lookup_host(*server).await {
            if let Ok(v4) = omni_net_primitives::stun::first_ipv4(server, addrs) {
                resolved.push(v4);
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8) -> MeshNodeAddress {
        MeshNodeAddress::from_bytes([byte; 20])
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, 7], port))
    }

    /// Exactly one side dials in every epoch, and the roles swap from one epoch to the next.
    #[test]
    fn exactly_one_side_dials_and_the_roles_swap_every_epoch() {
        let (a, b) = (node(1), node(2));
        for epoch in 0..6 {
            let (role_a, role_b) = (tcp_punch_role(&a, &b, epoch), tcp_punch_role(&b, &a, epoch));
            assert_ne!(role_a, role_b, "epoch {epoch}: one dials, the other listens");
            assert_ne!(
                role_a,
                tcp_punch_role(&a, &b, epoch + 1),
                "epoch {epoch}: the role changes in the next epoch"
            );
        }
        assert_eq!(tcp_punch_role(&a, &b, 0), TcpPunchRole::Dialer, "the lower address dials first");
    }

    /// The epoch is the same for clocks inside one epoch and changes at its boundary.
    #[test]
    fn the_epoch_changes_every_epoch_length() {
        assert_eq!(tcp_punch_epoch(0), tcp_punch_epoch(TCP_PUNCH_EPOCH_SECS - 1));
        assert_eq!(tcp_punch_epoch(TCP_PUNCH_EPOCH_SECS), 1);
    }

    /// A second start for the same peer extends the running window instead of spawning another, and
    /// re-aims it at the address of the fresh beacon (the peer's NAT moved its door).
    #[test]
    fn a_second_start_extends_and_reaims_the_window() {
        let state = TcpPunchState::default();
        assert!(state.open_window(node(7), addr(1000)), "first start spawns");
        assert!(state.punching());
        let first_end = state.window(&node(7)).unwrap().until;
        assert!(!state.open_window(node(7), addr(2000)), "second start only extends");
        let window = state.window(&node(7)).unwrap();
        assert!(window.until >= first_end);
        assert_eq!(window.punch, addr(2000), "aimed at the newest address");
        state.close_window(&node(7));
        assert!(!state.punching());
        assert!(state.open_window(node(7), addr(2000)), "after the window ends a start spawns again");
    }

    /// The dialer's guard at the end of its epoch: what is left of the epoch, never zero (a moment on
    /// the boundary is the start of the next epoch, with all of it left).
    #[test]
    fn the_time_left_in_an_epoch_counts_down_to_the_boundary() {
        let epoch_ms = TCP_PUNCH_EPOCH_SECS * 1000;
        let start = 29_903_255 * epoch_ms;
        assert_eq!(epoch_left(start), Duration::from_secs(TCP_PUNCH_EPOCH_SECS));
        assert_eq!(epoch_left(start + epoch_ms - 1_800), Duration::from_millis(1_800));
        assert!(epoch_left(start + epoch_ms - 1_800) < DIAL_ATTEMPT, "too late for one more attempt");
        assert!(epoch_left(start + 1_000) >= DIAL_ATTEMPT);
        assert_eq!(epoch_left(start + epoch_ms), Duration::from_secs(TCP_PUNCH_EPOCH_SECS));
    }

    /// Outside a window a new port of the same public IP does not reach the beacon (it would make every
    /// sweep a publication); inside one it does at once, and so does a new IP at any time.
    #[test]
    fn the_published_port_follows_the_nat_only_while_punching_or_on_a_new_ip() {
        let state = TcpPunchState::default();
        assert_eq!(state.address_to_publish(), None, "nothing learned, nothing published");
        *state.own_address.lock() = Some(addr(1000));
        assert_eq!(state.address_to_publish(), Some(addr(1000)));

        *state.own_address.lock() = Some(addr(1001));
        assert_eq!(state.address_to_publish(), Some(addr(1000)), "idle remap: the beacon stays the same");

        state.open_window(node(7), addr(5000));
        assert_eq!(state.address_to_publish(), Some(addr(1001)), "a window gives the peer the live port");
        state.close_window(&node(7));
        *state.own_address.lock() = Some(addr(1002));
        assert_eq!(state.address_to_publish(), Some(addr(1001)), "after the window it holds again");

        let moved = SocketAddr::from(([203, 0, 113, 9], 1002));
        *state.own_address.lock() = Some(moved);
        assert_eq!(state.address_to_publish(), Some(moved), "another network is news at once");

        *state.own_address.lock() = None;
        assert_eq!(state.address_to_publish(), None, "no agreement, no address");
    }
}
