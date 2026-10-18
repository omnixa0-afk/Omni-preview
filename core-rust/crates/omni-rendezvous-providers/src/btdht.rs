//! BT-DHT rendezvous courier — an IPC CLIENT to the `omni-dht-helper` process (R7 Phase 2).
//!
//! A third, MORE-decentralised `RendezvousChannel` (after ntfy/Nostr): the BitTorrent Mainline DHT,
//! a substrate of millions of nodes with no operator to pressure and no host/domain to block, so a
//! an outage has to take down the whole swarm. Design + rationale: `docs/decisions/bt-dht-rendezvous-courier.md`
//! (the BEP44 mechanics) + `docs/decisions/omni-dht-helper.md` (this client/engine split).
//!
//! ## Where the Mainline engine lives (Phase 2)
//! The `mainline` engine — its UDP socket, routing table, and hostile-wire parsing — runs in a
//! SEPARATE process, `omni-dht-helper`, so the DHT's UDP can be tunnelled independently of the web
//! couriers and its blast radius is isolated (see that ADR). This type no longer holds a `Dht`; it is
//! a thin **IPC client** to that helper over a local line protocol:
//!   * `put <key_hex> <salt_hex> <value_hex>` — the helper derives the keypair from `key`, bumps the
//!     slot's `seq`, signs + publishes. Reply `OK`.
//!   * `get <key_hex> <salt_hex>` — reply `OK <value_hex>` (empty `OK` = slot empty).
//!
//! The helper endpoint is handed to the rendezvous process via `OMNI_DHT_HELPER_ENDPOINT`; without it
//! the courier isn't constructed at all (`channel_for_url` returns `None`) and N-of-many covers the
//! missing DHT leg. The `seq` bump is the HELPER's job now (it owns the `Dht`), so this client carries
//! no seq logic — only the rendezvous policy below.
//!
//! ## Keying — `dht_key`, NOT the topic
//! BEP44 mutable slots are addressed by `target = sha1(pubkey ‖ salt)`, so the namespace is the
//! ed25519 keypair, derived (in the helper) from the per-bucket, secret-derived seed the seam threads
//! (`dht_key` = `derive_dht_key(secret, bucket)`). Because the seed already encodes *this pair, this
//! topic-window bucket* (12 h since 2026-10-09), the `topic` argument is redundant and ignored. The seed is grief-proof (only a
//! pair-secret holder can derive it) and is NOT the raw pair secret — handing it to the untrusted
//! helper leaks nothing beyond "can write THIS pair's THIS-bucket slots" (the value is AEAD-sealed by
//! the core). Without a `dht_key` the courier can't address a slot and fails best-effort.
//!
//! ## Dual-slot-claim (two writers, one shared keypair, no identity)
//! BEP44 keeps only the highest-`seq` value per slot (last-writer-wins), so two contacts sharing one
//! keypair would clobber each other in a single slot. We give the pair **two slots** under the same
//! key — `salt = b"0"` and `salt = b"1"` — and each side claims one: on first publish this bucket it
//! reads both (via `get`) and takes an empty one (its own random preference first, so two simultaneous
//! newcomers tend to split), then stays **sticky** to that slot for the bucket. `fetch` reads BOTH
//! slots and returns both values; the caller drops its own echo by `beacon.node`. This policy stays
//! client-side (it is rendezvous semantics, not DHT mechanics) so the helper can stay a generic
//! put/get primitive. The rare same-slot first-contact race is covered by bucket-rotation + the
//! ntfy/Nostr legs, exactly as before (see the courier ADR's residual note).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omni_rendezvous::{ChannelError, DhtSlotClaimer, RendezvousChannel, Topic, DHT_MAX_BLOB, DHT_SLOT_SALTS};

/// How long a single put/get IPC round-trip may take before we give up (the helper itself may be
/// mid-DHT-query). A stuck helper must not wedge the fan-out — the breaker then parks this leg.
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a published item is put again while it is still worth keeping. The swarm forgets a
/// BEP44 item after about two hours; a beacon lives half a day (`omni_rendezvous::TOPIC_WINDOW_SECS`)
/// and is posted once per window, so without this it would be gone from the DHT for most of its life.
const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(45 * 60);

/// One item this courier keeps alive in the swarm until `expires_at_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeptItem {
    key: [u8; 32],
    salt: &'static [u8],
    value: Vec<u8>,
    expires_at_ms: u64,
}

/// A `RendezvousChannel` over the BitTorrent Mainline DHT, as an IPC client to `omni-dht-helper`.
/// Holds the helper's loopback endpoint + the per-bucket slot it claimed; no DHT engine in-process.
pub struct BtDhtChannel {
    /// `omni-dht-helper` loopback endpoint (host:port), from `OMNI_DHT_HELPER_ENDPOINT`.
    endpoint: String,
    /// The shared dual-slot-claim policy (per-bucket-seed → sticky slot). Lives in `omni-rendezvous`
    /// so this local IPC leg and the core's DHT-over-peer leg address the SAME slots.
    claimer: DhtSlotClaimer,
    /// What we published and keep alive — put again every [`KEEP_ALIVE_EVERY`] until it expires.
    /// No operator and no quota: keeping our items in the swarm is our job, not a courier's.
    kept: Arc<Mutex<Vec<KeptItem>>>,
    keeper: OnceLock<()>,
}

impl BtDhtChannel {
    /// Build a courier that delegates raw BEP44 put/get to the `omni-dht-helper` at `endpoint`.
    pub fn connect(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            claimer: DhtSlotClaimer::new(),
            kept: Arc::new(Mutex::new(Vec::new())),
            keeper: OnceLock::new(),
        }
    }

    fn request(&self, line: &str) -> Result<String, ChannelError> {
        request(&self.endpoint, line)
    }

    /// Publish `value` to the slot `salt` under the keypair derived (helper-side) from `key`.
    fn put(&self, key: &[u8; 32], salt: &[u8], value: &[u8]) -> Result<(), ChannelError> {
        put(&self.endpoint, key, salt, value)
    }

    /// Put every kept item again; drop the expired. Returns how many went out. The keeper thread calls
    /// it every [`KEEP_ALIVE_EVERY`] through the free function.
    #[cfg(test)]
    fn refresh_kept(&self, now_ms: u64) -> usize {
        refresh_kept(&self.endpoint, &self.kept, now_ms)
    }

    /// Remember `value` in `salt` under `key` until `ttl_secs` from now (replacing what this slot held),
    /// and make sure the keeper runs.
    fn keep(&self, key: &[u8; 32], salt: &'static [u8], value: &[u8], ttl_secs: u32) {
        let expires_at_ms = now_ms().saturating_add(u64::from(ttl_secs) * 1000);
        {
            let mut kept = self.kept.lock().unwrap_or_else(|poison| poison.into_inner());
            kept.retain(|item| !(item.key == *key && item.salt == salt));
            kept.push(KeptItem { key: *key, salt, value: value.to_vec(), expires_at_ms });
        }
        self.keeper.get_or_init(|| {
            let (endpoint, kept) = (self.endpoint.clone(), Arc::clone(&self.kept));
            let _ = std::thread::Builder::new().name("omni-btdht-keeper".to_string()).spawn(move || loop {
                std::thread::sleep(KEEP_ALIVE_EVERY);
                refresh_kept(&endpoint, &kept, now_ms());
            });
        });
    }

    /// Most-recent value for the slot `salt`, or `None` if nothing is published there.
    fn get(&self, key: &[u8; 32], salt: &[u8]) -> Result<Option<Vec<u8>>, ChannelError> {
        let line = format!("get {} {}", to_hex(key), to_hex(salt));
        let reply = self.request(&line)?;
        let mut parts = reply.split_whitespace();
        match parts.next() {
            Some("OK") => Ok(parts.next().and_then(from_hex)),
            _ => Err(ChannelError(format!("btdht get: {reply}"))),
        }
    }

}

impl RendezvousChannel for BtDhtChannel {
    fn publish(&self, _topic: &Topic, dht_key: Option<&[u8; 32]>, blob: &[u8], ttl_secs: u32) -> Result<(), ChannelError> {
        let Some(key) = dht_key else {
            return Err(ChannelError("btdht: no dht_key (BT-DHT needs the secret-derived seed)".into()));
        };
        // The helper stores the blob as a pkarr-shaped record (see `DHT_MAX_BLOB`); a bigger one is
        // refused here with a clear reason rather than failing the put.
        if blob.len() > DHT_MAX_BLOB {
            return Err(ChannelError(format!(
                "btdht: {}B beacon exceeds the {DHT_MAX_BLOB}B a pkarr-shaped record holds",
                blob.len()
            )));
        }
        // Claim our slot via the shared policy, probing occupancy over the helper IPC.
        let slot = self.claimer.claim(key, |salt| self.get(key, salt).map(|v| v.is_some()))?;
        self.put(key, DHT_SLOT_SALTS[slot], blob)?;
        self.keep(key, DHT_SLOT_SALTS[slot], blob, ttl_secs);
        Ok(())
    }

    fn max_blob(&self) -> Option<usize> {
        Some(DHT_MAX_BLOB)
    }

    fn fetch(&self, _topic: &Topic, dht_key: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
        let Some(key) = dht_key else {
            return Err(ChannelError("btdht: no dht_key (BT-DHT needs the secret-derived seed)".into()));
        };
        let mut out = Vec::new();
        for salt in DHT_SLOT_SALTS {
            if let Some(value) = self.get(key, salt)? {
                out.push(value);
            }
        }
        Ok(out)
    }
}

/// One request line → one reply line, over a fresh loopback connection to the helper, bounded by
/// [`IPC_TIMEOUT`] so a stuck helper can't wedge the caller.
fn request(endpoint: &str, line: &str) -> Result<String, ChannelError> {
    let mut stream =
        TcpStream::connect(endpoint).map_err(|e| ChannelError(format!("btdht connect {endpoint}: {e}")))?;
    let _ = stream.set_read_timeout(Some(IPC_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IPC_TIMEOUT));
    writeln!(stream, "{line}").map_err(|e| ChannelError(format!("btdht write: {e}")))?;
    let _ = stream.flush();
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| ChannelError(format!("btdht read: {e}")))?;
    Ok(reply.trim().to_string())
}

fn put(endpoint: &str, key: &[u8; 32], salt: &[u8], value: &[u8]) -> Result<(), ChannelError> {
    let line = format!("put {} {} {}", to_hex(key), to_hex(salt), to_hex(value));
    let reply = request(endpoint, &line)?;
    if reply.starts_with("OK") {
        Ok(())
    } else {
        Err(ChannelError(format!("btdht put: {reply}")))
    }
}

fn refresh_kept(endpoint: &str, kept: &Mutex<Vec<KeptItem>>, now_ms: u64) -> usize {
    let items: Vec<KeptItem> = {
        let mut kept = kept.lock().unwrap_or_else(|poison| poison.into_inner());
        kept.retain(|item| item.expires_at_ms > now_ms);
        kept.clone()
    };
    items
        .iter()
        .filter(|item| put(endpoint, &item.key, item.salt, &item.value).is_ok())
        .count()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
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
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A `topic` the BT-DHT courier ignores (it keys off `dht_key`); the trait still requires one.
    fn ignored_topic() -> Topic {
        Topic::from_hex(&"cd".repeat(32)).unwrap()
    }

    /// A tiny in-process stand-in for `omni-dht-helper`: a loopback line server that speaks the same
    /// `put`/`get` protocol backed by an in-memory `(key_hex, salt_hex) -> value_hex` map. It exercises
    /// the client's dual-slot policy DETERMINISTICALLY with NO DHT and NO network — the real Mainline
    /// round-trip is covered by `omni-dht-helper`'s own testnet test + its live `#[ignore]` test.
    /// Returns the bound endpoint; the server thread is detached (reaped at test-process exit).
    fn spawn_mock_helper() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock helper");
        let endpoint = listener.local_addr().expect("addr").to_string();
        let store: Arc<Mutex<HashMap<(String, String), String>>> = Arc::new(Mutex::new(HashMap::new()));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let store = Arc::clone(&store);
                std::thread::spawn(move || serve_mock(stream, &store));
            }
        });
        endpoint
    }

    fn serve_mock(stream: std::net::TcpStream, store: &Mutex<HashMap<(String, String), String>>) {
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
            let reply = match parts.next() {
                Some("put") => {
                    let (k, s, v) = (parts.next(), parts.next(), parts.next());
                    if let (Some(k), Some(s), Some(v)) = (k, s, v) {
                        store
                            .lock()
                            .unwrap()
                            .insert((k.to_string(), s.to_string()), v.to_string());
                        "OK".to_string()
                    } else {
                        "ERR put args".to_string()
                    }
                }
                Some("get") => {
                    let (k, s) = (parts.next(), parts.next());
                    if let (Some(k), Some(s)) = (k, s) {
                        match store.lock().unwrap().get(&(k.to_string(), s.to_string())) {
                            Some(v) => format!("OK {v}"),
                            None => "OK".to_string(),
                        }
                    } else {
                        "ERR get args".to_string()
                    }
                }
                other => format!("ERR unknown '{}'", other.unwrap_or("")),
            };
            if writeln!(writer, "{reply}").is_err() {
                break;
            }
            let _ = writer.flush();
        }
    }

    /// Two channels sharing one per-bucket seed publish to DISTINCT slots and each reads the other's
    /// beacon back — the dual-slot-claim round-trip, against the in-process mock helper (no DHT).
    #[test]
    fn dual_slot_round_trip_through_mock_helper() {
        let endpoint = spawn_mock_helper();
        let dht_key = [0x5Au8; 32];
        let a = BtDhtChannel::connect(&endpoint);
        let b = BtDhtChannel::connect(&endpoint);

        let blob_a = b"beacon-from-A".to_vec();
        let blob_b = b"beacon-from-B".to_vec();

        // Sequential publish: B sees A's slot already taken and claims the other → distinct slots.
        a.publish(&ignored_topic(), Some(&dht_key), &blob_a, 600).expect("A publishes");
        b.publish(&ignored_topic(), Some(&dht_key), &blob_b, 600).expect("B publishes");

        let got_a = a.fetch(&ignored_topic(), Some(&dht_key)).expect("A fetches");
        let got_b = b.fetch(&ignored_topic(), Some(&dht_key)).expect("B fetches");

        assert!(got_a.iter().any(|v| v == &blob_b), "A reads B's beacon from the other slot: {got_a:?}");
        assert!(got_b.iter().any(|v| v == &blob_a), "B reads A's beacon from the other slot: {got_b:?}");
    }

    /// What we published is put again until it expires — the swarm forgets an item in about two
    /// hours, a beacon lives half a day — and a newer beacon in the same slot replaces the older one.
    #[test]
    fn a_published_item_is_kept_alive_until_it_expires() {
        let endpoint = spawn_mock_helper();
        let ch = BtDhtChannel::connect(&endpoint);
        let key = [0x6Bu8; 32];
        ch.publish(&ignored_topic(), Some(&key), b"old", 3_600).unwrap();
        ch.publish(&ignored_topic(), Some(&key), b"new", 3_600).unwrap();
        let now = now_ms();
        assert_eq!(ch.refresh_kept(now + 60_000), 1, "one slot of ours, its latest value");
        assert!(ch.fetch(&ignored_topic(), Some(&key)).unwrap().iter().any(|v| v == b"new"));
        assert_eq!(ch.refresh_kept(now + 3_601_000), 0, "expired: no longer put");
        assert!(ch.kept.lock().unwrap().is_empty());
    }

    /// No seed → the courier can't address a slot and fails best-effort BEFORE any IPC (so the bogus
    /// endpoint is never contacted). N-of-many covers it.
    #[test]
    fn without_dht_key_fails_best_effort() {
        let ch = BtDhtChannel::connect("127.0.0.1:1");
        assert!(ch.publish(&ignored_topic(), None, b"x", 600).is_err(), "publish needs a seed");
        assert!(ch.fetch(&ignored_topic(), None).is_err(), "fetch needs a seed");
    }

    /// A value over what one pkarr-shaped record holds is rejected before any IPC, not silently dropped.
    #[test]
    fn oversize_value_is_rejected_before_put() {
        let ch = BtDhtChannel::connect("127.0.0.1:1");
        let key = [1u8; 32];
        let too_big = vec![0u8; DHT_MAX_BLOB + 1];
        assert!(
            ch.publish(&ignored_topic(), Some(&key), &too_big, 600).is_err(),
            "{}B exceeds the record's cap",
            too_big.len()
        );
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("abc").is_none(), "odd length rejected");
    }
}
