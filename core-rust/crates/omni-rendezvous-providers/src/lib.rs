//! Real `RendezvousChannel` providers over FREE PUBLIC brokers — no owned infrastructure.
//!
//! The first provider is **ntfy.sh**: a free public HTTP pub/sub. We `POST` an opaque sealed
//! beacon to `https://ntfy.sh/<topic>` and `GET https://ntfy.sh/<topic>/json?poll=1` to read the
//! recent ones back. ntfy never learns anything useful: the topic is the blinded
//! `HKDF(pair_secret)` (only the two contacts compute it) and the body is the AEAD-sealed beacon
//! (omni-rendezvous). So even though message-broking isn't "rendezvous", it serves perfectly —
//! the requirement is tiny. This is the lightest way to prove the serverless-rendezvous seam live.
//! The second provider is **Nostr** ([`NostrChannel`], NIP-01 over WebSocket) — a DIFFERENT protocol
//! with many independent public relays, so one host going down can't take out the rendezvous plane
//! host (R7, N-of-many). BT-DHT (more decentralised still) follows.

mod nostr;
pub use nostr::NostrChannel;

mod btdht;
pub use btdht::BtDhtChannel;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use omni_rendezvous::{ChannelError, RendezvousChannel, Topic};

/// How often the broker is asked what is left of the publishing allowance (`GET /v1/account` — a
/// read, it costs no message). Between asks every accepted post counts one off.
const ALLOWANCE_CHECK_EVERY: Duration = Duration::from_secs(10 * 60);

/// Below this many messages left, background posts stop going to this broker.
const ALLOWANCE_RESERVE: u32 = 50;

/// A `RendezvousChannel` backed by an ntfy-style HTTP pub/sub broker (default `https://ntfy.sh`).
pub struct NtfyChannel {
    base_url: String,
    agent: ureq::Agent,
    /// Messages left to publish today, as the broker last said (and counted down since), and when it
    /// said it. `None` — not asked yet, or the broker does not say (a self-hosted one without limits).
    allowance: Mutex<Option<(u32, Instant)>>,
}

/// `stats.messages_remaining` of an ntfy `GET /v1/account` reply.
fn messages_remaining(account_json: &str) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_str(account_json).ok()?;
    value.get("stats")?.get("messages_remaining")?.as_u64().map(|n| n.min(u64::from(u32::MAX)) as u32)
}

impl NtfyChannel {
    /// Default public broker (`https://ntfy.sh`).
    pub fn new() -> Self {
        Self::with_base("https://ntfy.sh")
    }

    /// Point at a specific broker base URL (self-hosted ntfy, or another instance — user-addable,
    /// like all providers).
    pub fn with_base(base_url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(8))
            .build();
        Self { base_url: base_url.into(), agent, allowance: Mutex::new(None) }
    }

    /// Messages left to publish, asking the broker when the last answer is older than
    /// [`ALLOWANCE_CHECK_EVERY`]. `None` when the broker does not say.
    fn messages_left(&self) -> Option<u32> {
        let mut allowance = self.allowance.lock().unwrap_or_else(|poison| poison.into_inner());
        if allowance.is_none_or(|(_, at)| at.elapsed() >= ALLOWANCE_CHECK_EVERY) {
            let asked = self
                .agent
                .get(&format!("{}/v1/account", self.base_url))
                .call()
                .ok()
                .and_then(|response| response.into_string().ok())
                .and_then(|body| messages_remaining(&body));
            *allowance = asked.map(|left| (left, Instant::now()));
        }
        allowance.map(|(left, _)| left)
    }

    fn count_one_published(&self) {
        if let Some((left, _)) = self.allowance.lock().unwrap_or_else(|poison| poison.into_inner()).as_mut() {
            *left = left.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod allowance_tests {
    use super::*;

    /// The allowance comes from the broker's own account reply (as ntfy.sh answered on 2026-10-09) and
    /// is counted down by every accepted post until the broker is asked again.
    #[test]
    fn the_allowance_is_read_from_the_account_reply_and_counted_down() {
        let reply = r#"{"username":"*","role":"anonymous","limits":{"basis":"ip","messages":250},"stats":{"messages":201,"messages_remaining":49}}"#;
        assert_eq!(messages_remaining(reply), Some(49));
        assert_eq!(messages_remaining(r#"{"stats":{}}"#), None);
        assert_eq!(messages_remaining("not json"), None);

        let ntfy = NtfyChannel::with_base("http://127.0.0.1:1");
        *ntfy.allowance.lock().unwrap() = Some((ALLOWANCE_RESERVE, Instant::now()));
        assert!(!ntfy.publish_allowance_low(), "at the reserve: still fine");
        ntfy.count_one_published();
        assert!(ntfy.publish_allowance_low(), "below the reserve: keep the rest for contacts");
        *ntfy.allowance.lock().unwrap() = None;
        assert!(!ntfy.publish_allowance_low(), "a broker that does not say is not low");
    }
}

impl Default for NtfyChannel {
    fn default() -> Self {
        Self::new()
    }
}


/// Превратить ошибку ureq в `ChannelError`, СОХРАНИВ код состояния в тексте.
///
/// Без этого «кончилась квота» неотличимо от «брокер лёг»: у ureq 2.x `Display` для
/// `Error::Status` не обязан содержать код, а именно по коду видно, что провайдер жив и просто
/// не пускает нас до сброса лимита. Живой замер 2026-08-13: ntfy.sh отдаёт
/// `429 {"code":42908,"error":"limit reached: daily message quota reached"}`.
fn channel_error(context: &str, error: ureq::Error) -> ChannelError {
    match error {
        ureq::Error::Status(code, _) => ChannelError(format!("{context}: status {code}")),
        other => ChannelError(format!("{context}: {other}")),
    }
}

impl RendezvousChannel for NtfyChannel {
    fn publish(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>, blob: &[u8], _ttl_secs: u32) -> Result<(), ChannelError> {
        let url = format!("{}/{}", self.base_url, topic.id());
        // ntfy is a TEXT broker: it strips/mangles control bytes (a raw beacon starting with 0x0A
        // would lose its leading newline). Hex-encode so any opaque blob survives byte-exact.
        self.agent
            .post(&url)
            .send_string(&to_hex(blob))
            .map(|_| self.count_one_published())
            .map_err(|e| channel_error("ntfy publish", e))
    }

    fn publish_allowance_low(&self) -> bool {
        self.messages_left().is_some_and(|left| left < ALLOWANCE_RESERVE)
    }

    fn fetch(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
        // `poll=1` returns the cached messages then closes; `since=all` includes the (short-lived)
        // history. The response is newline-delimited JSON, one object per message.
        let url = format!("{}/{}/json?poll=1&since=all", self.base_url, topic.id());
        let body = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| channel_error("ntfy fetch", e))?
            .into_string()
            .map_err(|e| ChannelError(format!("ntfy body: {e}")))?;

        Ok(body.lines().filter_map(ntfy_message_blob).collect())
    }

    fn supports_watch(&self) -> bool {
        true
    }

    /// ntfy's own subscription: `GET /<t1>,<t2>/json` (no `since` = new messages only) stays open and writes one JSON line
    /// per event (`open`, a `keepalive` every ~45 s, `message`). Free on the server side — the quota
    /// counts publishes, not subscribers.
    fn watch(&self, topics: &[Topic], wait: Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
        use std::io::BufRead;
        let ids: Vec<String> = topics.iter().map(Topic::id).collect();
        let url = format!("{}/{}/json", self.base_url, ids.join(","));
        let started = std::time::Instant::now();
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(wait)
            .build();
        let response = agent.get(&url).call().map_err(|e| channel_error("ntfy watch", e))?;
        for line in std::io::BufReader::new(response.into_reader()).lines() {
            // A read timeout ends the wait like a deadline does: nothing arrived.
            let Ok(line) = line else { break };
            if let Some(blob) = ntfy_message_blob(&line) {
                return Ok(vec![blob]);
            }
            if started.elapsed() >= wait {
                break;
            }
        }
        Ok(Vec::new())
    }
}

/// The blob of one ntfy JSON event line, if it is a `message` carrying one (ntfy also emits
/// `open`/`keepalive` events).
fn ntfy_message_blob(line: &str) -> Option<Vec<u8>> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("event").and_then(|e| e.as_str()) != Some("message") {
        return None;
    }
    from_hex(value.get("message")?.as_str()?)
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
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
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
    use omni_rendezvous::{collect_beacons, publish_beacon, Beacon, NatType};

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// A fresh per-run pair secret so each test run uses a unique blinded topic (no collision with
    /// prior runs, and nothing stale).
    fn fresh_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        s[0..8].copy_from_slice(&now_ms().to_le_bytes());
        s[8..16].copy_from_slice(&(std::process::id() as u64).to_le_bytes());
        s[16] = 0xA5; // marker so it's obviously a test secret
        s
    }

    /// Live end-to-end serverless rendezvous through the real public ntfy.sh — NO owned
    /// infrastructure, NO account, NO second machine: seal a beacon, publish it to ntfy under our
    /// blinded topic, then fetch + open it back. Ignored by default (needs HTTPS egress).
    /// Run: `cargo test -p omni-rendezvous-providers ntfy_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ntfy_live_round_trip() {
        let secret = fresh_secret();
        let now = now_ms();
        let channel = NtfyChannel::new();
        let beacon = Beacon {
            forwarded: vec![],
            node: [0u8; 20],
            reflexive: vec!["203.0.113.7:51820".into()],
            tcp_direct: vec![],
            nat_type: NatType::Symmetric,
            relay_bundle: None,
            revoked_through_ms: None,
            ts_ms: now,
            ttl_secs: 300,
        };

        let published = publish_beacon(&[&channel], &secret, now, &beacon);
        println!("published to {published} provider(s)");
        assert_eq!(published, 1, "ntfy.sh should accept the beacon");

        // Give the broker a moment to store it.
        std::thread::sleep(Duration::from_secs(2));

        let got = collect_beacons(&[&channel], &secret, now + 1000);
        println!("fetched {} beacon(s) back: {:?}", got.len(), got);
        assert!(!got.is_empty(), "should fetch our own beacon back through real ntfy.sh");
        assert_eq!(got[0].reflexive[0], "203.0.113.7:51820");
    }

    /// The subscription really pushes: `watch` is open BEFORE the publication and returns the blob
    /// published a moment later — not a cached one (a stream without `since`). Ignored by default (HTTPS egress).
    /// Run: `cargo test -p omni-rendezvous-providers watch_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn ntfy_watch_live_sees_a_publication_made_after_it_opened() {
        let topic = omni_rendezvous::rendezvous_topics(&fresh_secret(), now_ms())[0];
        let publisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            NtfyChannel::new().publish(&topic, None, &[0xC0, 0xFF, 0xEE], 600)
        });
        let started = std::time::Instant::now();
        let got = NtfyChannel::new().watch(&[topic], Duration::from_secs(25)).expect("watch");
        publisher.join().unwrap().expect("publish");
        println!("ntfy watch: {:?} after {:?}", got, started.elapsed());
        assert_eq!(got, vec![vec![0xC0, 0xFF, 0xEE]]);
        assert!(started.elapsed() < Duration::from_secs(15), "pushed, not timed out");
    }
}

