// The knock of the TCP hole punch, for find-by-Omni-ID (docs/decisions/tcp-hole-punch.md, «Стук»).
//
// The two sides of a pair learn each other's punch addresses from the pair's beacons. A finder and the
// device it found by Omni-ID do not: the finder reads the device's identity beacon, and the device
// learns nothing of the finder — while a punch needs both sides to know where the other is in the same
// minute. So the finder knocks: it posts its own punch address on the Omni-ID's knock topic, sealed to
// the knock key the device's identity beacon carries, and starts its window. The device, while it is
// findable, holds a subscription on that topic and starts its window on a knock.
//
// A NAT may give the door a new port once it has been idle a while, so the address in the device's
// beacon can be stale by the time someone knocks. The device asks its own address again when its
// window opens and republishes its beacon with it; the finder follows the device's identity topic
// through its window and re-aims at the new address. The finder knocks again when its own address
// moves.
//
// Anyone who has the Omni-ID can knock, so what a knock can make the device do is bounded: a window
// only towards a public address, a limited number of new windows an hour, and a republished beacon at
// most once a minute and never to the metered ntfy.

/// New punch windows knocks may start on this device per hour.
const KNOCK_WINDOWS_PER_HOUR: usize = 12;
/// A knock republishes our identity beacon at most this often: the couriers meter publishing.
const KNOCK_REPUBLISH_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// The kinds of courier a knock-driven publication never goes to: ntfy.sh allows 250 messages a day
/// per IP, shared by everyone behind the router, and someone else's knocks must not spend them.
const KNOCK_NEVER_VIA: &[&str] = &["ntfy"];
/// A knock asks for one punch window; it is worth nothing after that.
const KNOCK_TTL_SECS: u32 = 150;
/// How long the own-address probe a new window asks for is waited for before the address is read.
const KNOCK_PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// One wait on the found device's identity topic while following it.
const KNOCK_FOLLOW_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// After a look at the identity topic that failed at once, wait this long before the next.
const KNOCK_FOLLOW_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
/// Knocks one window of the finder sends at most: the first, and again when its own address moved.
const KNOCKS_PER_WINDOW: u32 = 4;

/// This device's knock key `(secret, public)` — the public half rides the identity beacon.
pub(crate) fn own_knock_key(storage: &Storage) -> Option<([u8; 32], [u8; 32])> {
    let (seed, _) = crate::app_screen_control::local_signing_identity(storage).ok()?;
    omni_crypto::derive_knock_key(&seed).ok()
}

fn own_omni_id(storage: &Storage) -> Option<[u8; 20]> {
    let account = storage.get_local_account().ok()??;
    let keys = storage.get_account_keys(&account.descriptor.account_id).ok()??;
    Some(omni_crypto::omni_id_bytes(&keys.master_public_key))
}

/// The subscription on our own knock topic: while we are findable by ID and have a punch address to
/// publish — exactly when our identity beacon carries a knock key.
pub(crate) fn own_knock_watch(storage: &Storage, runtime: &MeshRuntime) -> Option<BeaconWatchTarget> {
    if !load_discoverability_mode(storage).is_public() || runtime.tcp_punch_endpoint().is_none() {
        return None;
    }
    let omni_id = own_omni_id(storage)?;
    let (reply, _) = own_knock_key(storage)?;
    Some(BeaconWatchTarget {
        secret: omni_rendezvous::knock_secret(&omni_id),
        awaits: Awaited::Knock(InviteSeal::Inviter { reply }),
        exclude: Vec::new(),
        read_from: None,
    })
}

/// What knocks have cost this device lately (lives in the mesh worker's loop, per running profile).
#[derive(Default)]
pub(crate) struct KnockLimit {
    windows_started: std::collections::VecDeque<std::time::Instant>,
    republished: Option<std::time::Instant>,
}

impl KnockLimit {
    fn may_start_window(&mut self, now: std::time::Instant) -> bool {
        let hour = std::time::Duration::from_secs(60 * 60);
        while self.windows_started.front().is_some_and(|at| now.saturating_duration_since(*at) >= hour) {
            self.windows_started.pop_front();
        }
        self.windows_started.len() < KNOCK_WINDOWS_PER_HOUR
    }

    fn window_started(&mut self, now: std::time::Instant) {
        self.windows_started.push_back(now);
    }

    fn may_republish(&mut self, now: std::time::Instant) -> bool {
        let due = self.republished.is_none_or(|at| now.saturating_duration_since(at) >= KNOCK_REPUBLISH_EVERY);
        if due {
            self.republished = Some(now);
        }
        due
    }
}

/// Whether a knock's address is one we may send SYNs to: a public one. A knock is anyone's to post,
/// and must not turn this device into a prober of its own network.
pub(crate) fn knock_target_allowed(punch: &std::net::SocketAddr) -> bool {
    punch.port() != 0 && omni_net_primitives::is_public_ip(punch.ip())
}

/// Wait until our own punch address has been asked once more after `before` probes (a window that just
/// opened asks at once), or [`KNOCK_PROBE_WAIT`] — so the address read next is the NAT's current one.
fn wait_for_own_probe(runtime: &MeshRuntime, before: u64) {
    let until = std::time::Instant::now() + KNOCK_PROBE_WAIT;
    while runtime.tcp_punch_probes() == before && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// A knock arrived on our subscription: punch towards the finder, within [`KnockLimit`], and give it
/// our current punch address by republishing our identity beacon.
pub(crate) fn on_knock(
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    limit: &mut KnockLimit,
    node: [u8; 20],
    punch: std::net::SocketAddr,
) {
    let peer = MeshNodeAddress::from_bytes(node);
    if !knock_target_allowed(&punch) {
        omni_app_trace_log("rendezvous/knock", &format!("from={peer} punch={punch} refused: not a public address"));
        return;
    }
    let now = std::time::Instant::now();
    let running = runtime.tcp_punch_window_open(&peer);
    if !running && !limit.may_start_window(now) {
        omni_app_trace_log(
            "rendezvous/knock",
            &format!("from={peer} refused: {KNOCK_WINDOWS_PER_HOUR} knock windows this hour already"),
        );
        return;
    }
    let probes = runtime.tcp_punch_probes();
    let started = runtime.start_tcp_punch(peer, punch);
    omni_app_trace_log("rendezvous/knock", &format!("from={peer} punch={punch} -> {started:?}"));
    if started != omni_mesh_runtime::TcpPunchStart::Started {
        return;
    }
    limit.window_started(now);
    if !limit.may_republish(now) {
        return;
    }
    let runtime = Arc::clone(runtime);
    let db_path = db_path.to_string();
    let _ = std::thread::Builder::new().name("omni-knock-republish".to_string()).spawn(move || {
        wait_for_own_probe(&runtime, probes);
        let Ok(storage) = Storage::open(&db_path) else {
            return;
        };
        let my_relay_bundle = build_my_relay_bundle_token(&storage, &db_path).ok().flatten();
        let posted = post_identity_beacon_avoiding(&storage, &runtime, &db_path, my_relay_bundle, KNOCK_NEVER_VIA);
        omni_app_trace_log(
            "rendezvous/knock",
            &format!("republished identity beacon={posted} punch={:?}", runtime.tcp_punch_endpoint()),
        );
    });
}

/// Our knock: our punch address `own`, sealed on `omni_id`'s knock topic to the found device's
/// `knock_key` with a key of its own (nothing ties two knocks of ours together). Posted to Nostr
/// only — the courier that delivers at once to a held subscription; a knock that arrives minutes late
/// is worth nothing, and ntfy's daily quota is not spent on it. Returns whether a courier took it.
fn post_knock(
    helper: &RendezvousHelper,
    omni_id: &[u8; 20],
    knock_key: &[u8; 32],
    my_node: [u8; 20],
    own: std::net::SocketAddr,
) -> bool {
    let Ok(ephemeral) = omni_crypto::random_bytes(32).map(|bytes| <[u8; 32]>::try_from(bytes.as_slice())) else {
        return false;
    };
    let Ok(ephemeral) = ephemeral else {
        return false;
    };
    let now = current_unix_ms() as u64;
    let secret = omni_rendezvous::knock_secret(omni_id);
    let mut knock = build_self_beacon(my_node, Vec::new(), NatType::Unknown, None);
    knock.tcp_punch = Some(own.to_string());
    knock.ttl_secs = KNOCK_TTL_SECS;
    let Some((topic, dht_key)) = omni_rendezvous::rendezvous_topics_with_keys(&secret, now).into_iter().next() else {
        return false;
    };
    let Some(wire) = omni_rendezvous::seal_beacon_to(&secret, &topic, &knock, &ephemeral, knock_key) else {
        return false;
    };
    let route = PublishRoute {
        key: format!("knock:{}", hex::encode(omni_id)),
        steps: vec![vec!["nostr".to_string()]],
        reach: 1,
        priority: PublishPriority::Contact,
    };
    helper
        .publish_routed_among(&topic.id(), &hex::encode(dht_key), &hex::encode(wire), None, KNOCK_TTL_SECS, &route, None)
        .is_ok()
}

/// The punch address in the freshest valid identity beacon of `node` among `hex_blobs` newer than
/// `seen_seq`, with that beacon's `seq`. Verified like a lookup: a forged beacon must not re-aim us.
fn fresher_punch_address(
    omni_id: &[u8; 20],
    node: [u8; 20],
    hex_blobs: &[String],
    seen_seq: u64,
    now_ms: u64,
) -> Option<(u64, Option<std::net::SocketAddr>)> {
    hex_blobs
        .iter()
        .filter_map(|hex_blob| hex::decode(hex_blob).ok())
        .filter_map(|bytes| omni_rendezvous::IdentityBeacon::from_wire(&bytes))
        .filter(|beacon| {
            beacon.node == node
                && &beacon.omni_id == omni_id
                && beacon.seq > seen_seq
                && beacon.is_fresh(now_ms)
                && verify_identity_beacon(omni_id, beacon).is_ok()
        })
        .max_by_key(|beacon| beacon.seq)
        .map(|beacon| (beacon.seq, beacon.tcp_punch.as_deref().and_then(|addr| addr.parse().ok())))
}

/// The finder's side, from a dial of a found device: punch towards the address its identity beacon
/// published, and when that starts a new window, knock and follow the device's beacon in the
/// background until the window ends or the device is linked.
pub(crate) fn punch_found_device(
    runtime: &Arc<MeshRuntime>,
    db_path: &str,
    beacon: &omni_rendezvous::IdentityBeacon,
    node: MeshNodeAddress,
) {
    // Public only, as a knock's (blind audit 2026-11-11, G3): a beacon is its owner's to fill.
    let (Some(punch), Some(knock_key)) = (
        beacon
            .tcp_punch
            .as_deref()
            .and_then(|addr| addr.parse::<std::net::SocketAddr>().ok())
            .filter(knock_target_allowed),
        beacon.knock_key,
    ) else {
        return;
    };
    let probes = runtime.tcp_punch_probes();
    let started = runtime.start_tcp_punch(node, punch);
    omni_app_trace_log("find_by_id/punch", &format!("node={node} punch={punch} -> {started:?}"));
    if started != omni_mesh_runtime::TcpPunchStart::Started {
        return;
    }
    let runtime = Arc::clone(runtime);
    let db_path = db_path.to_string();
    let omni_id = beacon.omni_id;
    let seen_seq = beacon.seq;
    let _ = std::thread::Builder::new().name("omni-knock".to_string()).spawn(move || {
        knock_and_follow(&runtime, &db_path, omni_id, knock_key, node, punch, probes, seen_seq);
    });
}

#[allow(clippy::too_many_arguments)] // one window's state, handed to its thread
fn knock_and_follow(
    runtime: &MeshRuntime,
    db_path: &str,
    omni_id: [u8; 20],
    knock_key: [u8; 32],
    node: MeshNodeAddress,
    mut aim: std::net::SocketAddr,
    probes: u64,
    mut seen_seq: u64,
) {
    wait_for_own_probe(runtime, probes);
    let Ok(rendezvous) = profile_rendezvous_runtime(db_path) else {
        return;
    };
    let my_node = *runtime.node_address().as_bytes();
    let target = *node.as_bytes();
    let mut knocked: Option<std::net::SocketAddr> = None;
    let mut knocks = 0u32;
    while runtime.tcp_punch_window_open(&node) && !runtime.has_link(&node) {
        // While a window runs our published address is the NAT's current one.
        match runtime.tcp_punch_endpoint() {
            Some(own) if Some(own) != knocked && knocks < KNOCKS_PER_WINDOW => {
                let taken = post_knock(rendezvous.helper.as_ref(), &omni_id, &knock_key, my_node, own);
                omni_app_trace_log("find_by_id/knock", &format!("node={node} own={own} posted={taken}"));
                if taken {
                    knocked = Some(own);
                    knocks += 1;
                }
            }
            None if knocked.is_none() => {
                omni_app_trace_log("find_by_id/knock", &format!("node={node} not sent: no punch address of our own"));
            }
            _ => {}
        }
        let topics: Vec<String> = omni_rendezvous::identity_topics(&omni_id, current_unix_ms() as u64)
            .iter()
            .map(|topic| topic.id())
            .collect();
        let started = std::time::Instant::now();
        match rendezvous.helper.watch(&topics, KNOCK_FOLLOW_WAIT) {
            Ok(hex_blobs) if !beacon_watch_needs_pause(&hex_blobs, started.elapsed()) => {
                if let Some((seq, punch)) =
                    fresher_punch_address(&omni_id, target, &hex_blobs, seen_seq, current_unix_ms() as u64)
                {
                    seen_seq = seq;
                    if let Some(punch) = punch.filter(|punch| *punch != aim && knock_target_allowed(punch)) {
                        aim = punch;
                        let started = runtime.start_tcp_punch(node, punch);
                        omni_app_trace_log("find_by_id/punch", &format!("node={node} re-aimed punch={punch} -> {started:?}"));
                    }
                }
            }
            _ => std::thread::sleep(KNOCK_FOLLOW_RETRY_AFTER),
        }
    }
    omni_app_trace_log(
        "find_by_id/knock",
        &format!("node={node} window over linked={} knocks={knocks}", runtime.has_link(&node)),
    );
}

#[cfg(test)]
mod tcp_knock_tests {
    use super::*;

    /// Knocks start at most `KNOCK_WINDOWS_PER_HOUR` windows in any hour, and the hour slides.
    #[test]
    fn knocks_start_a_bounded_number_of_windows_an_hour() {
        let mut limit = KnockLimit::default();
        let start = std::time::Instant::now();
        for i in 0..KNOCK_WINDOWS_PER_HOUR {
            let at = start + std::time::Duration::from_secs(i as u64);
            assert!(limit.may_start_window(at), "window {i}");
            limit.window_started(at);
        }
        assert!(!limit.may_start_window(start + std::time::Duration::from_secs(60)), "the hour is full");
        assert!(limit.may_start_window(start + std::time::Duration::from_secs(60 * 60)), "the first one aged out");
    }

    /// A knock republishes our beacon at most once a minute, whoever knocks.
    #[test]
    fn knocks_republish_at_most_once_a_minute() {
        let mut limit = KnockLimit::default();
        let start = std::time::Instant::now();
        assert!(limit.may_republish(start));
        assert!(!limit.may_republish(start + std::time::Duration::from_secs(30)));
        assert!(limit.may_republish(start + KNOCK_REPUBLISH_EVERY));
    }

    /// A knock may aim the punch only at a public address: not at our own network, not at loopback.
    #[test]
    fn a_knock_aims_only_at_a_public_address() {
        assert!(knock_target_allowed(&"203.0.114.7:40001".parse().unwrap()));
        assert!(!knock_target_allowed(&"192.168.1.10:40001".parse().unwrap()));
        assert!(!knock_target_allowed(&"10.0.0.5:40001".parse().unwrap()));
        assert!(!knock_target_allowed(&"127.0.0.1:40001".parse().unwrap()));
        assert!(!knock_target_allowed(&"203.0.114.7:0".parse().unwrap()));
    }

    /// Following the found device: only its own, verified, newer beacon re-aims the punch.
    #[test]
    fn only_a_newer_verified_beacon_of_the_device_reaims_the_finder() {
        let omni_id = [7u8; 20];
        let unsigned = omni_rendezvous::IdentityBeacon {
            omni_id,
            chain_compact: vec![1],
            node: [8u8; 20],
            prekey_compact: vec![1],
            device_label: String::new(),
            reflexive: vec![],
            tcp_direct: vec![],
            relay_bundle: None,
            tcp_punch: Some("203.0.114.7:40002".into()),
            knock_key: Some([3u8; 32]),
            ts_ms: 1_700_000_000_000,
            ttl_secs: 900,
            seq: 5,
            signature: vec![0; 64],
        };
        let blobs = vec![hex::encode(unsigned.to_wire()), "zz".to_string()];
        assert_eq!(
            fresher_punch_address(&omni_id, [8u8; 20], &blobs, 1, 1_700_000_000_000),
            None,
            "a beacon that does not verify is ignored"
        );
    }
}
