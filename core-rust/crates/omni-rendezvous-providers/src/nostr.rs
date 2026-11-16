//! Nostr (NIP-01) rendezvous courier — a public WebSocket relay used as a [`RendezvousChannel`].
//!
//! Unlike a single ntfy broker, Nostr is a DIFFERENT protocol with MANY independent public relays, so
//! adding it means it takes ALL of them failing, not one host (R7 — "nothing to block wholesale"). It
//! maps cleanly onto our model: publish a sealed beacon as one NIP-01 event tagged with the blinded
//! topic (a `d` tag); fetch = subscribe by that tag and collect the events' `content`.
//!
//! Security is UNCHANGED and lives where it always did — the AEAD-sealed beacon under a blinded topic
//! (`omni-rendezvous`). The Nostr event is signed by an **ephemeral throwaway BIP340 key** purely to
//! satisfy the relay's "events must be signed" rule; it carries NO Omni identity, and we fetch by the
//! topic tag, not by author. See docs/decisions/serverless-bootstrap-rendezvous.md.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k256::schnorr::{Signature, SigningKey};
use omni_rendezvous::{ChannelError, RendezvousChannel, Topic};
use sha2::{Digest, Sha256};

use crate::nostr_link::{LinkConfig, RelayLink, MAX_EVENTS, TIMINGS};
#[cfg(test)]
use crate::from_hex;
use crate::to_hex;

/// NIP-78 application-data kind (parameterized-replaceable range) the sealed beacon rides under. The
/// blinded topic is the `d` tag, so a fetch filters `{"kinds":[30078],"#d":[<topic>]}` and gets EVERY
/// writer's beacon back (multi-writer, fetch-all — the courier contract).
pub(crate) const RENDEZVOUS_KIND: u16 = 30078;
/// Default free public relay (write-permissive). One of several — operator/user-addable like all
/// providers; the resilience comes from N-of-many across independent relays, not this single default.
const DEFAULT_RELAY: &str = "wss://nos.lol";

/// A [`RendezvousChannel`] over a Nostr relay (default [`DEFAULT_RELAY`]).
pub struct NostrChannel {
    relay_url: String,
    /// Connect only to a public address — a courier someone else named (V-13).
    public_only: bool,
    /// Connections opened so far ([`RendezvousChannel::connections_opened`]).
    connections: Arc<AtomicU64>,
    /// The relay connection, started on first use (`nostr_link`).
    link: OnceLock<RelayLink>,
}

impl NostrChannel {
    /// Default free public relay.
    pub fn new() -> Self {
        Self::with_relay(DEFAULT_RELAY)
    }

    /// Point at a specific relay (`wss://…` or `ws://…`) — user/operator-addable, like every provider.
    pub fn with_relay(relay_url: impl Into<String>) -> Self {
        Self { relay_url: relay_url.into(), public_only: false, connections: Arc::default(), link: OnceLock::new() }
    }

    /// The same relay, reached only at a public address (checked where the name is resolved): for
    /// a courier a contact named, which must not make us reach into our own network (V-13).
    pub fn public_only(mut self) -> Self {
        self.public_only = true;
        self
    }

    /// The relay's connection, started the first time this courier is used.
    fn link(&self) -> &RelayLink {
        self.link.get_or_init(|| {
            RelayLink::start(LinkConfig {
                url: self.relay_url.clone(),
                public_only: self.public_only,
                timings: TIMINGS,
                connections: Arc::clone(&self.connections),
            })
        })
    }
}

impl Default for NostrChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl NostrChannel {
    /// Publish under `topic`, signed by `signing`, and wait for the relay's `OK`. A relay that refuses
    /// anonymous writes (NIP-42 auth / payment) answers `OK false` with its reason — surfaced so the
    /// fan-out moves to the next courier and the breaker tells a quota from a breakdown.
    fn publish_signed_by(&self, signing: &SigningKey, topic: &Topic, blob: &[u8], ttl_secs: u32) -> Result<(), ChannelError> {
        let event = signed_event_by(signing, &topic.id(), &to_hex(blob), now_unix_secs() + u64::from(ttl_secs));
        let id = event["id"].as_str().unwrap_or_default().to_string();
        self.link().publish(event, id)
    }
}

impl RendezvousChannel for NostrChannel {
    fn publish(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>, blob: &[u8], ttl_secs: u32) -> Result<(), ChannelError> {
        self.publish_signed_by(&ephemeral_signing_key(), topic, blob, ttl_secs)
    }

    /// The same key for every beacon this process writes under `topic` — the relay replaces the
    /// previous one (NIP-33: one event per kind + pubkey + `d`). The key is the topic's own: it links
    /// nothing the topic does not already link, and differs from topic to topic.
    fn publish_replacing(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>, blob: &[u8], ttl_secs: u32) -> Result<(), ChannelError> {
        self.publish_signed_by(&writer_signing_key(&topic.id()), topic, blob, ttl_secs)
    }

    fn fetch(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
        self.fetch_many(std::slice::from_ref(topic))
    }

    /// One `REQ` per topic in turn, on the relay's one connection: each topic keeps its own `limit`
    /// (the relay directory spreads over shelves exactly because of it).
    fn fetch_many(&self, topics: &[Topic]) -> Result<Vec<Vec<u8>>, ChannelError> {
        let mut out = Vec::new();
        for topic in topics {
            let filter = serde_json::json!({
                "kinds": [RENDEZVOUS_KIND],
                "#d": [topic.id()],
                "limit": MAX_EVENTS,
            });
            out.extend(self.link().query(filter)?);
        }
        Ok(out)
    }

    fn supports_watch(&self) -> bool {
        true
    }

    /// A NIP-01 `REQ` held open past `EOSE` on the relay's connection — and between calls: what is
    /// published while no call waits is kept for the next one (`RelayLink::watch`).
    fn watch(&self, topics: &[Topic], wait: Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
        let ids: Vec<String> = topics.iter().map(Topic::id).collect();
        self.link().watch(&ids, wait)
    }

    fn release_watch(&self, topics: &[Topic]) {
        if let Some(link) = self.link.get() {
            let ids: Vec<String> = topics.iter().map(Topic::id).collect();
            link.release(&ids);
        }
    }

    fn connections_opened(&self) -> Option<u64> {
        Some(self.connections.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
fn signed_event(topic_d: &str, content: &str, expires_at: u64) -> serde_json::Value {
    signed_event_by(&ephemeral_signing_key(), topic_d, content, expires_at)
}

/// Build + sign a NIP-01 event carrying `content` under the `d`-tag `topic_d`, with a FRESH ephemeral
/// BIP340 keypair. `content`/`topic_d` are hex (ASCII), so serde_json's compact array is byte-exact to
/// NIP-01's canonical id pre-image — no string-escaping edge cases.
///
/// `expires_at` (unix seconds) goes into a NIP-40 `expiration` tag: a relay that supports it stops
/// serving the event then and may delete it — a beacon does not pile up on the relay, or in its
/// public archive, after it stopped meaning anything.
fn signed_event_by(signing: &SigningKey, topic_d: &str, content: &str, expires_at: u64) -> serde_json::Value {
    let pubkey = to_hex(&signing.verifying_key().to_bytes());
    let created_at = now_unix_secs();
    let tags = serde_json::json!([["d", topic_d], ["expiration", expires_at.to_string()]]);
    let preimage = serde_json::to_string(&serde_json::json!([
        0, pubkey, created_at, RENDEZVOUS_KIND, tags, content
    ]))
    .expect("serialize NIP-01 id pre-image");
    let id = Sha256::digest(preimage.as_bytes());
    // Nostr signs the 32-byte id DIRECTLY as the BIP340 message — `sign_raw`, NOT the `Signer::sign`
    // convenience (which pre-hashes the input and so produces a sig libsecp256k1 relays reject as
    // "bad signature"). aux_rand=0 = the BIP340 deterministic variant.
    let sig: Signature = signing
        .sign_raw(id.as_slice(), &[0u8; 32])
        .expect("BIP340 sign of a 32-byte message");
    serde_json::json!({
        "id": to_hex(id.as_slice()),
        "pubkey": pubkey,
        "created_at": created_at,
        "kind": RENDEZVOUS_KIND,
        "tags": tags,
        "content": content,
        "sig": to_hex(&sig.to_bytes()),
    })
}

/// This process's writer key for `topic_d`: a hash of a per-process random salt and the topic. A
/// restart gets a new salt, so its first beacon lands beside the old one, which expires (NIP-40).
fn writer_signing_key(topic_d: &str) -> SigningKey {
    static SALT: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let salt = SALT.get_or_init(|| {
        let mut salt = [0u8; 32];
        getrandom::getrandom(&mut salt).expect("OS RNG available");
        salt
    });
    let mut counter = 0u8;
    loop {
        let seed = Sha256::new()
            .chain_update(b"omni/nostr-writer/v1")
            .chain_update(salt)
            .chain_update(topic_d.as_bytes())
            .chain_update([counter])
            .finalize();
        if let Ok(key) = SigningKey::from_bytes(seed.as_slice()) {
            return key;
        }
        counter = counter.wrapping_add(1);
    }
}

/// A throwaway BIP340 keypair (retry past the astronomically-rare invalid scalar). The Nostr identity
/// is meaningless to us — it only signs the relay envelope; security is the AEAD beacon.
fn ephemeral_signing_key() -> SigningKey {
    loop {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("OS RNG available");
        if let Ok(key) = SigningKey::from_bytes(&seed) {
            return key;
        }
    }
}

fn now_unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Parse `wss://host[:port][/path]` / `ws://…` into `(host, port, is_tls)` (default port 443/80).
pub(crate) fn parse_ws_url(url: &str) -> Result<(String, u16, bool), ChannelError> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(ChannelError(format!("nostr url must be ws:// or wss://: {url}")));
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        // rsplit on ':' is safe here — IPv6 literals would need brackets, which public relays use as
        // hostnames anyway; a hostname has no ':'.
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| ChannelError(format!("nostr bad port: {authority}")))?,
        ),
        None => (authority.to_string(), if tls { 443 } else { 80 }),
    };
    if host.is_empty() {
        return Err(ChannelError(format!("nostr url has no host: {url}")));
    }
    Ok((host, port, tls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::schnorr::VerifyingKey;

    #[test]
    fn signed_event_is_well_formed_and_verifies() {
        // Offline: the event id matches the NIP-01 pre-image hash and the BIP340 sig verifies under the
        // event's own pubkey — proving the envelope a relay will accept is built correctly. (No network.)
        let event = signed_event("deadbeef", "cafe01", 1_800_000_000);
        let pubkey_hex = event["pubkey"].as_str().unwrap();
        let id_hex = event["id"].as_str().unwrap();
        let sig_hex = event["sig"].as_str().unwrap();

        // Recompute the id from the canonical pre-image.
        let preimage = serde_json::to_string(&serde_json::json!([
            0, pubkey_hex, event["created_at"], RENDEZVOUS_KIND, event["tags"], "cafe01"
        ]))
        .unwrap();
        let recomputed = to_hex(Sha256::digest(preimage.as_bytes()).as_slice());
        assert_eq!(recomputed, id_hex, "event id = sha256 of the NIP-01 pre-image");

        // The schnorr signature verifies under the event's x-only pubkey over the 32-byte id.
        let pk = VerifyingKey::from_bytes(&from_hex(pubkey_hex).unwrap()).unwrap();
        let sig = Signature::try_from(from_hex(sig_hex).unwrap().as_slice()).unwrap();
        let id = from_hex(id_hex).unwrap();
        assert!(pk.verify_raw(&id, &sig).is_ok(), "BIP340 sig verifies over the raw id");
        assert_eq!(event["kind"], RENDEZVOUS_KIND);
        assert_eq!(event["tags"], serde_json::json!([["d", "deadbeef"], ["expiration", "1800000000"]]));
    }

    #[test]
    fn a_writer_keeps_one_key_per_topic_and_a_different_one_on_another() {
        let a = writer_signing_key("aaaa").verifying_key().to_bytes();
        assert_eq!(a, writer_signing_key("aaaa").verifying_key().to_bytes(), "the relay can replace");
        assert_ne!(a, writer_signing_key("bbbb").verifying_key().to_bytes(), "topics stay unlinked");
    }

    #[test]
    fn parses_ws_urls() {
        assert_eq!(parse_ws_url("wss://relay.damus.io").unwrap(), ("relay.damus.io".into(), 443, true));
        assert_eq!(parse_ws_url("ws://localhost:7000/x").unwrap(), ("localhost".into(), 7000, false));
        assert!(parse_ws_url("https://nope").is_err());
    }

    #[test]
    #[ignore = "network: hits a real public Nostr relay"]
    fn publish_then_fetch_through_a_real_relay() {
        // The whole NIP-01 publish → fetch chain against a live relay (gated like the ntfy test). Run:
        //   cargo test -p omni-rendezvous-providers -- --ignored
        use omni_rendezvous::{rendezvous_topics, seal_beacon, Beacon, NatType};
        let secret = [7u8; 32];
        let now = now_unix_secs() * 1000;
        let topic = rendezvous_topics(&secret, now)[0];
        let beacon = Beacon {
            forwarded: vec![],
            node: [3u8; 20],
            reflexive: vec!["198.51.100.9:7777".into()],
            tcp_direct: vec![],
            tcp_punch: None,
            nat_type: NatType::Public,
            ts_ms: now,
            ttl_secs: 300,
            relay_bundle: None,
            revoked_through_ms: None,
        };
        let blob = seal_beacon(&secret, &topic, &beacon).to_wire();

        // Try a few independent public relays — pass if ANY round-trips (the spirit of N-of-many; a
        // single public relay may 503 / require auth / rate-limit at any moment).
        let relays = [
            "wss://nostr.mom",
            "wss://offchain.pub",
            "wss://relay.nostr.bg",
            "wss://nostr-pub.wellorder.net",
            "wss://nos.lol",
        ];
        let mut ok = false;
        for relay in relays {
            let ch = NostrChannel::with_relay(relay);
            if ch.publish(&topic, None, &blob, 600).is_err() {
                continue;
            }
            std::thread::sleep(Duration::from_secs(2));
            match ch.fetch(&topic, None) {
                Ok(got) if got.iter().any(|b| b == &blob) => {
                    println!("round-trip via {relay}");
                    ok = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(ok, "our beacon round-trips through at least one public Nostr relay");
    }

    /// Same as the ntfy one: a `REQ` held past `EOSE` receives an event published after it opened.
    /// Any one of a few public relays is enough (N-of-many). Run with `-- --ignored --nocapture`.
    #[test]
    #[ignore = "network: hits real public Nostr relays"]
    fn a_replacing_writer_leaves_one_event_per_topic_on_a_real_relay() {
        // Three beacons by one writer under one fresh topic: a relay that honours NIP-33 keeps the last.
        use omni_rendezvous::rendezvous_topics;
        let mut secret = [0u8; 32];
        secret[..8].copy_from_slice(&now_unix_secs().to_le_bytes());
        secret[8] = 0x33;
        let topic = rendezvous_topics(&secret, now_unix_secs() * 1000)[0];
        for relay in ["wss://nos.lol", "wss://relay.nostr.net", "wss://nostr.oxtr.dev"] {
            let channel = NostrChannel::with_relay(relay);
            let mut published = 0;
            for blob in [[1u8, 1], [2, 2], [3, 3]] {
                published += usize::from(channel.publish_replacing(&topic, None, &blob, 600).is_ok());
                // NIP-33 keeps the newer `created_at`; a second's step makes "newer" unambiguous.
                std::thread::sleep(Duration::from_millis(1100));
            }
            let fetched = channel.fetch(&topic, None);
            eprintln!("{relay}: published={published} fetched={fetched:?}");
            if published == 3 {
                assert_eq!(fetched.unwrap(), vec![vec![3u8, 3]], "{relay} kept more than the writer's last");
            }
        }
    }

    /// A relay-directory entry goes onto its real Nostr shelf and comes back whole. The entry is a
    /// withdrawal (no endpoints) of a throwaway key: readers drop it, so the shared shelf is not soiled.
    #[test]
    #[ignore = "network: hits real public Nostr relays"]
    fn a_relay_directory_entry_round_trips_on_its_real_shelf() {
        use omni_rendezvous::relay_directory::{
            relay_publish_topic, relay_read_topics, RelayDirectoryEntry, RelayLoad, RelayShelfKind,
        };
        let now_ms = now_unix_secs() * 1000;
        let mut pubkey = [0u8; 32];
        pubkey[..8].copy_from_slice(&now_ms.to_le_bytes());
        let entry = RelayDirectoryEntry {
            node_pubkey: pubkey,
            udp: vec![],
            tcp: vec![],
            name: "live-test".into(),
            load: RelayLoad::Free,
            since_ms: now_ms,
            ts_ms: now_ms,
            ttl_secs: 600,
            seq: now_ms,
            pow_nonce: 0,
            signature: vec![0; 64],
        };
        let (topic, _) = relay_publish_topic(RelayShelfKind::Nostr, &pubkey, now_ms);
        assert!(relay_read_topics(RelayShelfKind::Nostr, now_ms).iter().any(|(t, _)| *t == topic));
        let mut any = false;
        for relay in ["wss://nos.lol", "wss://relay.nostr.net", "wss://nostr.oxtr.dev"] {
            let channel = NostrChannel::with_relay(relay);
            let published = channel.publish_replacing(&topic, None, &entry.to_wire(), 600);
            let fetched = channel.fetch(&topic, None).unwrap_or_default();
            let ours = fetched
                .iter()
                .filter_map(|blob| RelayDirectoryEntry::from_wire(blob))
                .filter(|found| found.node_pubkey == pubkey)
                .count();
            eprintln!("{relay}: publish={published:?} on the shelf={} ours={ours}", fetched.len());
            if published.is_ok() {
                assert_eq!(ours, 1, "{relay}");
                any = true;
            }
        }
        assert!(any, "no relay took the entry");
    }

    #[test]
    #[ignore = "network: hits real public Nostr relays"]
    fn nostr_watch_live_sees_a_publication_made_after_it_opened() {
        use omni_rendezvous::rendezvous_topics;
        let mut secret = [0u8; 32];
        secret[..8].copy_from_slice(&now_unix_secs().to_le_bytes());
        secret[8] = 0x77;
        let topic = rendezvous_topics(&secret, now_unix_secs() * 1000)[0];
        let mut seen = Vec::new();
        for relay in ["wss://offchain.pub", "wss://nos.lol", "wss://nostr.mom", "wss://relay.nostr.bg"] {
            let publisher = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(2000));
                NostrChannel::with_relay(relay).publish(&topic, None, &[0xBE, 0xEF], 600)
            });
            let started = std::time::Instant::now();
            let got = NostrChannel::with_relay(relay).watch(&[topic], Duration::from_secs(20));
            let published = publisher.join().unwrap();
            println!("{relay}: watch={got:?} publish={published:?} after {:?}", started.elapsed());
            if let Ok(got) = got {
                if got == vec![vec![0xBE, 0xEF]] {
                    seen.push(relay);
                }
            }
        }
        assert!(!seen.is_empty(), "no relay pushed the event to the open subscription");
    }
}

