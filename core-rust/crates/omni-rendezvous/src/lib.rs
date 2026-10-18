//! Serverless rendezvous core.
//!
//! Two NAT'd peers that already share a pairing secret (from X3DH) need to exchange their
//! current reflexive endpoints through an untrusted third-party channel (Nostr/BT-DHT/ntfy/…)
//! without that channel being able to **deceive** them or learn **who** is talking to **whom**.
//!
//! This crate is the dependency-free, fully-testable foundation for that:
//!   * [`rendezvous_topics`] — a **rotating, blinded** topic id `HKDF(pair_secret, time_bucket)`.
//!     Only a peer holding the pair secret can compute it ⇒ a third party sees an opaque key, and
//!     **only your contacts can publish to "your" topic** (the structural collapse of the DoS
//!     surface, TM-011).
//!   * [`seal_beacon`] / [`open_beacon`] — the presence beacon is **AEAD-sealed** (ChaCha20-Poly1305)
//!     under a key derived from the same pair secret, bound to the topic as associated data. A forged
//!     or tampered beacon fails authentication (TM-009); `ts`/`ttl` reject stale/replayed-late beacons;
//!     the AEAD nonce doubles as the **anti-replay token** ([`ReplayGuard`]).
//!   * [`RendezvousChannel`] — the trait interchangeable providers implement; [`InMemoryChannel`] is
//!     the test double. The real Nostr/BT-DHT/… providers + the untrusted helper process sit on top.
//!
//! Trust model: the channel is an **untrusted courier** of opaque blobs under opaque topics — it can
//! drop/replay/observe (DoS / metadata), but never deceive (AEAD auth + blinded topic + identity-bound
//! trust completed later by the QUIC connect-time challenge).

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Mutex;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// The 32-byte pairing secret two contacts share (derived from their X3DH handshake). Injected by
/// the caller — this crate never establishes it.
pub type PairSecret = [u8; 32];

/// How long one blinded topic is valid before it rotates. Both ends compute the same bucket from
/// wall-clock time; a reader checks the current + previous bucket.
///
/// Twelve hours, not the ten minutes it used to be (docs/decisions/courier-metadata-and-quota.md).
/// A short window bought almost nothing — a courier links the two ends of a pair by their IPs in any
/// window, and a reader without IPs learns nothing from a topic either way — and cost a lot: a
/// beacon that lives one window has to be posted again every window, which spent the 250-a-day
/// per-IP quota of ntfy.sh on beacons that said nothing new. Meeting is driven by events (start,
/// a new address, a peer's fresh beacon over a subscription), not by this clock. Twelve hours is how
/// long ntfy.sh keeps a message.
pub const TOPIC_WINDOW_SECS: u64 = 12 * 60 * 60;

/// The default cross-protocol courier set used when no operator override is configured. Deliberately
/// PLURAL and DISTINCT-protocol (HTTP ⟂ WS ⟂ BitTorrent-DHT) so one blocked/rate-limited substrate can't
/// sink rendezvous — see `serverless-bootstrap-rendezvous.md` (N-of-many). The single source of truth
/// shared by the helper process AND the app (which embeds it into mesh-pairing invites, R7).
/// `dht://mainline` is the BEP44 courier (`bt-dht-rendezvous-courier.md`, live-verified) — a serverless,
/// operator-less, host/domain-less leg; the helper bootstraps it from the public Mainline routers.
/// The Nostr relays are [`NOSTR_RELAY_POOL`]: each topic uses only a few of them.
pub const DEFAULT_RENDEZVOUS_PROVIDERS: &[&str] = &[
    "https://ntfy.sh",
    "wss://nos.lol",
    "wss://nostr.mom",
    "wss://nostr.oxtr.dev",
    "wss://relay.primal.net",
    "wss://relay.nostr.net",
    "wss://offchain.pub",
    "dht://mainline",
];

/// Public Nostr relays shared out among topics (docs/decisions/courier-metadata-and-quota.md, step 4).
/// Checked live 2026-10-10 with Omni's own courier: each takes a kind-30078 event from a throwaway key,
/// serves it back, and honours NIP-40 `expiration`. A topic goes to [`POOL_RELAYS_TO_PUBLISH`] of them,
/// picked by [`pool_rank`] — so no single relay sees every pair, and losing one relay is not losing
/// the Nostr leg. A relay the user (or an invite) adds outside this list is used for every topic.
pub const NOSTR_RELAY_POOL: [&str; 6] = [
    "wss://nos.lol",
    "wss://nostr.mom",
    "wss://nostr.oxtr.dev",
    "wss://relay.primal.net",
    "wss://relay.nostr.net",
    "wss://offchain.pub",
];

/// Pool relays a topic is published to: the first ones by [`pool_rank`] that are not cooling down.
pub const POOL_RELAYS_TO_PUBLISH: usize = 2;

/// Pool relays a topic is read from — one more than it is published to, so a relay that is cooling
/// down on one side of the pair only does not make the two sides miss each other.
pub const POOL_RELAYS_TO_READ: usize = 3;

/// How a courier of the set handed to the helper (`OMNI_RENDEZVOUS_PROVIDERS`) is used
/// (docs/decisions/courier-metadata-and-quota.md, step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CourierUse {
    /// We post our beacons there and look for others'.
    PublishRead,
    /// We look for others' beacons there, but post nothing (`#read`).
    ReadOnly,
    /// Not one of ours: a courier a device we talk to uses. Used for that device's pairs only — when
    /// the core names it for a topic — never for the rest (`#pair`).
    PairOnly,
}

const READ_ONLY_MARK: &str = "#read";
const PAIR_ONLY_MARK: &str = "#pair";

/// One entry of the courier set handed to the helper: the URL, marked by how it is used.
pub fn courier_entry(url: &str, courier_use: CourierUse) -> String {
    match courier_use {
        CourierUse::PublishRead => url.to_string(),
        CourierUse::ReadOnly => format!("{url}{READ_ONLY_MARK}"),
        CourierUse::PairOnly => format!("{url}{PAIR_ONLY_MARK}"),
    }
}

/// The URL of a courier entry and how it is used.
pub fn parse_courier_entry(entry: &str) -> (&str, CourierUse) {
    if let Some(url) = entry.strip_suffix(READ_ONLY_MARK) {
        (url, CourierUse::ReadOnly)
    } else if let Some(url) = entry.strip_suffix(PAIR_ONLY_MARK) {
        (url, CourierUse::PairOnly)
    } else {
        (entry, CourierUse::PublishRead)
    }
}

/// A compact human label for a courier URL — the KIND (from the scheme) plus its host, e.g.
/// `ntfy:ntfy.sh`, `nostr:offchain.pub`, `dht:mainline`. The helper names couriers by it in its
/// per-courier report, and the core matches that report back to the courier list by it.
pub fn courier_label(url: &str) -> String {
    let (kind, rest) = if let Some(r) = url.strip_prefix("wss://").or_else(|| url.strip_prefix("ws://")) {
        ("nostr", r)
    } else if let Some(r) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        ("ntfy", r)
    } else if let Some(r) = url.strip_prefix("dht://") {
        ("dht", r)
    } else {
        ("courier", url)
    };
    let host = rest.split('/').next().unwrap_or(rest);
    format!("{kind}:{host}")
}

/// Where `url` stands for `topic` in the pool (lower first). Both sides of a pair hold the same topic,
/// so they rank the relays alike without agreeing on anything; the order changes with the topic window.
pub fn pool_rank(topic: &Topic, url: &str) -> [u8; 32] {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(b"omni-courier-pool-v1");
    hasher.update(topic.as_bytes());
    hasher.update(url.as_bytes());
    hasher.finalize().into()
}

/// Parse the comma-separated `OMNI_RENDEZVOUS_PROVIDERS` URL list (each trimmed, empties dropped); falls
/// back to [`DEFAULT_RENDEZVOUS_PROVIDERS`] when unset/blank. Operator/user-configurable, no recompile —
/// the seam for N-of-many. Order is preserved (it's the fan-out set).
pub fn parse_provider_urls(env_value: Option<String>) -> Vec<String> {
    let urls: Vec<String> = env_value
        .unwrap_or_default()
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    if urls.is_empty() {
        DEFAULT_RENDEZVOUS_PROVIDERS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        urls
    }
}

const HKDF_TOPIC_INFO: &[u8] = b"omni-rendezvous-topic-v1";
const HKDF_AEAD_INFO: &[u8] = b"omni-rendezvous-aead-v1";
/// HKDF info for the BT-DHT BEP44 keypair seed (R7 BT-DHT courier). DISTINCT from the topic info, so
/// the per-bucket DHT signing key is NOT derivable from the (semi-public) `Topic::id` — an observer of
/// the ntfy URL / Nostr `d`-tag can't compute it and thus can't overwrite (grief) the pair's DHT slots
/// (forge is already impossible — the value is AEAD-sealed). Only a holder of the pair secret derives
/// it. See docs/decisions/bt-dht-rendezvous-courier.md.
const HKDF_DHT_KEY_INFO: &[u8] = b"omni-btdht-key-v1";

/// Max clock skew tolerated when judging a beacon's `ts` as "from the future".
const FUTURE_SKEW_MS: u64 = 120_000; // 2 min

/// R7 metadata hygiene — sealed beacons are padded so their wire size is quantized to this bucket. An
/// observer of the rendezvous channel then learns only a COARSE size class, never the exact size: a bare
/// presence beacon and one carrying a relay-set rotation are indistinguishable within a bucket, and the
/// relay count can't be read off the length. (A single fixed size would hide even the coarse bucket but
/// balloon every presence beacon × N providers × every sweep; bucketing bounds the waste. See R7.)
const BEACON_PAD_BUCKET: usize = 256;

/// Pad a beacon plaintext to the next [`BEACON_PAD_BUCKET`] multiple (floor one bucket). Layout:
/// `u32-LE real-length || json || zero-pad`. Reversed by [`unpad_plaintext`]. Applied BEFORE AEAD so the
/// ciphertext (and the length-preserving wire) carry only the bucketed length.
fn pad_plaintext(json: &[u8]) -> Vec<u8> {
    let body = 4 + json.len();
    let target = body.div_ceil(BEACON_PAD_BUCKET).max(1) * BEACON_PAD_BUCKET;
    let mut out = Vec::with_capacity(target);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(json);
    out.resize(target, 0);
    out
}

/// The smallest padded beacon: 512 bytes. A beacon is ~80 bytes bare and ~300 with a relay bundle of a
/// few relays — one floor for both, so the length does not tell a bundle is aboard; 540 bytes sealed,
/// within a BT-DHT record (`DHT_MAX_BLOB`).
const BEACON_PAD_FLOOR: usize = 2 * BEACON_PAD_BUCKET;

/// [`pad_plaintext`] to at least `floor` bytes.
fn pad_plaintext_from(payload: &[u8], floor: usize) -> Vec<u8> {
    let mut padded = pad_plaintext(payload);
    if padded.len() < floor {
        padded.resize(floor, 0);
    }
    padded
}

/// Recover the real json from a padded plaintext (`None` if the length prefix is implausible).
fn unpad_plaintext(padded: &[u8]) -> Option<&[u8]> {
    let len = u32::from_le_bytes(padded.get(0..4)?.try_into().ok()?) as usize;
    padded.get(4..4 + len)
}

/// A blinded rendezvous topic id (opaque to providers). Providers key blobs by its [`Topic::id`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Topic([u8; 32]);

impl Topic {
    /// Lowercase hex — the stable string a provider uses as its key/tag.
    pub fn id(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Reconstruct a topic from its 64-char hex [`Topic::id`]. The core computes the blinded topic
    /// (it has the pair secret) and hands the helper process only this opaque id — the helper never
    /// sees the secret. Returns `None` if the string isn't exactly 32 hex-encoded bytes.
    pub fn from_hex(hex: &str) -> Option<Topic> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            bytes[i] = (hi * 16 + lo) as u8;
        }
        Some(Topic(bytes))
    }
}

/// A peer's NAT class (informs whether a direct punch is feasible or a relay is needed).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NatType {
    Unknown,
    Public,
    Cone,
    Symmetric,
}

/// The presence beacon payload (plaintext, before sealing): who I am + where I am. The
/// whole struct is AEAD-sealed under the pair secret, so the `node` is visible only to
/// the contact (who already knows it from pairing) — it carries no metadata to the
/// untrusted courier.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Beacon {
    /// The publisher's mesh NodeAddress (20 bytes, opaque here). Both contacts publish to
    /// the SAME blinded topic (the secret is symmetric), so a reader gets its OWN beacon
    /// back too — `node` lets it tell which beacon is the peer's (vs its own) and which
    /// node to dial. Raw bytes keep this crate dependency-free.
    pub node: [u8; 20],
    /// UDP QUIC endpoints (`host:port`) that are inbound-dialable REGARDLESS of the publisher's NAT
    /// class: a UPnP port-forward and a native global IPv6. The publisher knows which of its
    /// endpoints are like this; the dialer cannot tell them apart from a STUN address by looking.
    ///
    /// Split out of `reflexive` on 2026-09-24. They used to share one list, and the dialer's
    /// symmetric×symmetric rule — correct for a STUN address, which is a per-destination mapping —
    /// threw the whole list away, forwards included. Live: a laptop and a PC that had talked over
    /// UDP three weeks earlier were pushed onto the TCP door the day the laptop's NAT started
    /// classifying as symmetric. See docs/bugs/a-forwarded-port-was-dropped-as-symmetric.md.
    ///
    /// Additive: skipped on the wire when empty, defaults to empty for an older beacon (whose
    /// forwards still arrive inside `reflexive`, as before).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwarded: Vec<String>,
    /// The STUN-observed endpoint(s) (`host:port`) the contact can try to dial / punch to. On a
    /// symmetric NAT this mapping is valid only towards the STUN server, which is exactly why the
    /// dialer may skip it for a symmetric×symmetric pair — and why forwards live in `forwarded`.
    pub reflexive: Vec<String>,
    /// TCP-direct front doors (`host:port`) the contact can dial when UDP-direct is throttled/blocked
    /// (UDP-hostile carriers leave TCP alone). Each is a `serve_tcp_bridge` listening in front of the
    /// publisher's quinn — the dialer wraps QUIC-over-TLS-over-TCP to it (same carrier as the relay
    /// fallback, but DIRECT). Additive + back-compatible: empty/skipped on the wire for older beacons
    /// and for publishers without a direct-TCP front door. See docs/decisions/tcp-direct-peer-fallback.md.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_direct: Vec<String>,
    pub nat_type: NatType,
    /// Unix epoch ms when published.
    pub ts_ms: u64,
    /// Validity in seconds after `ts_ms`.
    pub ttl_secs: u32,
    /// R1c: an optional signed `om3relay:` relay-bundle the publisher propagates to this approved
    /// contact (FIRST-PARTY only — the publisher's own set). `None` for a plain presence beacon;
    /// additive + back-compatible (skipped on the wire when absent, defaults to `None` for older
    /// beacons). The whole beacon is already AEAD-sealed under the pair secret, so only the contact
    /// can read it; the receiver still imports it only if the bundle's issuer == this beacon's `node`.
    /// See docs/decisions/r1c-rotatable-white-relay.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_bundle: Option<String>,
    /// When the publisher's account last took a device back, if this contact may not have heard: the
    /// signed revocation itself is too big for a beacon (a BT-DHT value holds ~1000 bytes, one
    /// revocation is ~960), so it waits in the pair's notice under the same secret, and this mark
    /// tells the contact to read it — once per new mark, so the notice costs no reads otherwise.
    /// See docs/decisions/multi-device-equal-nodes.md, «Отзыв через маячок».
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_through_ms: Option<u64>,
}

/// A sealed beacon as it travels through an untrusted channel: an AEAD nonce + ciphertext. Opaque
/// to the channel.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SealedBeacon {
    /// 12-byte AEAD nonce (also the anti-replay token).
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl SealedBeacon {
    /// Compact, **length-preserving** wire bytes for a provider: `nonce(12) || ciphertext`. Raw bytes
    /// (not the old JSON-array codec, whose per-byte decimal width leaked the size and defeated the
    /// padding). With the plaintext padded to a [`BEACON_PAD_BUCKET`] bucket, the wire length is now a
    /// quantized function of the bucket alone.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.nonce.len() + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    pub fn from_wire(bytes: &[u8]) -> Option<Self> {
        // The nonce is always 12 bytes; the rest is the AEAD ciphertext (≥16-byte tag). A short blob
        // (e.g. a stale beacon from the old JSON codec) returns None and is skipped by the reader.
        if bytes.len() < 12 + 16 {
            return None;
        }
        Some(Self {
            nonce: bytes[..12].to_vec(),
            ciphertext: bytes[12..].to_vec(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RvError {
    /// AEAD authentication failed — forged/tampered, or wrong pair secret (TM-009).
    Forged,
    /// Past its `ttl`, or `ts` implausibly in the future.
    Stale,
    /// This exact sealed beacon (by nonce) was already accepted on this topic (TM-011 replay).
    Replay,
    /// Plaintext didn't decode as a `Beacon`.
    Malformed,
}

fn hkdf32(secret: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, secret);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out).expect("32 is a valid HKDF-SHA256 length");
    out
}

/// The blinded topic for a given time bucket.
fn topic_for_bucket(secret: &PairSecret, bucket: u64) -> Topic {
    let mut info = Vec::with_capacity(HKDF_TOPIC_INFO.len() + 8);
    info.extend_from_slice(HKDF_TOPIC_INFO);
    info.extend_from_slice(&bucket.to_le_bytes());
    Topic(hkdf32(secret, &info))
}

/// The per-bucket BT-DHT BEP44 keypair SEED (32 bytes), `HKDF(pair_secret, DHT_KEY_INFO ‖ bucket)`.
/// Rotates with the topic bucket. Secret-derived + DISTINCT from the topic (see [`HKDF_DHT_KEY_INFO`]),
/// so only a pair-secret holder can sign/overwrite the pair's DHT slots — the grief-proof keying the
/// BT-DHT courier needs (the courier can't be handed the raw secret — it's an untrusted process — but
/// this per-bucket seed leaks nothing beyond the ability to write to THIS pair's THIS-bucket slots).
pub fn derive_dht_key(secret: &PairSecret, bucket: u64) -> [u8; 32] {
    let mut info = Vec::with_capacity(HKDF_DHT_KEY_INFO.len() + 8);
    info.extend_from_slice(HKDF_DHT_KEY_INFO);
    info.extend_from_slice(&bucket.to_le_bytes());
    hkdf32(secret, &info)
}

/// The topics to publish-to / subscribe-on right now: the current bucket plus the previous one
/// (so a beacon published just before a rotation is still found across the boundary).
pub fn rendezvous_topics(secret: &PairSecret, now_ms: u64) -> Vec<Topic> {
    rendezvous_topics_with_keys(secret, now_ms).into_iter().map(|(t, _)| t).collect()
}

/// Like [`rendezvous_topics`], but each topic paired with its per-bucket BT-DHT key seed
/// ([`derive_dht_key`]). The BT-DHT courier needs both (the blinded topic for the slot namespace +
/// the secret-derived signing seed); other couriers (ntfy/Nostr) use only the topic. Order matches
/// `rendezvous_topics` (current bucket first, then previous).
pub fn rendezvous_topics_with_keys(secret: &PairSecret, now_ms: u64) -> Vec<(Topic, [u8; 32])> {
    let bucket = now_ms / 1000 / TOPIC_WINDOW_SECS;
    [bucket, bucket.saturating_sub(1)]
        .into_iter()
        .map(|b| (topic_for_bucket(secret, b), derive_dht_key(secret, b)))
        .collect()
}

// --- Global identity discovery (Model A, MA2) --------------------------------
// A SECOND keying of the same courier machinery, for public "find me by ID". The
// topic is derived from the target's PUBLIC Omni-ID (not a pair secret), so anyone
// holding the ID computes it; the beacon is SIGNED (Ed25519, by the app — this
// crate stays key-free) rather than pair-sealed, so it is publicly readable and
// verifiable against the ID. См. docs/decisions/global-identity-discovery.md.

const HKDF_IDENTITY_TOPIC_INFO: &[u8] = b"omni.identity.topic.v1";

fn identity_topic_for_bucket(omni_id: &[u8; 20], bucket: u64) -> Topic {
    let mut info = Vec::with_capacity(HKDF_IDENTITY_TOPIC_INFO.len() + 8);
    info.extend_from_slice(HKDF_IDENTITY_TOPIC_INFO);
    info.extend_from_slice(&bucket.to_le_bytes());
    // HKDF ikm = the 20-byte Omni-ID (public). Publisher and looker-up both hold it.
    Topic(hkdf32(omni_id, &info))
}

/// The identity-discovery topics to publish-to / poll-on right now for an Omni-ID:
/// current bucket + previous (rotation-boundary tolerance), mirroring
/// [`rendezvous_topics`].
pub fn identity_topics(omni_id: &[u8; 20], now_ms: u64) -> Vec<Topic> {
    identity_topics_with_keys(omni_id, now_ms).into_iter().map(|(t, _)| t).collect()
}

const HKDF_IDENTITY_DHT_KEY_INFO: &[u8] = b"omni.identity.dht.v1";

/// The per-bucket BT-DHT slot seed for an identity topic. Unlike the per-pair
/// [`derive_dht_key`], this is derived from the PUBLIC Omni-ID, so it grants no
/// secrecy — anyone holding the ID computes the same value. It only namespaces the
/// BT-DHT slot; ntfy/Nostr ignore it. It exists because the courier-helper wire
/// protocol (`publish <topic> <dhtkey> <blob>` / `fetch <topic> <dhtkey>`) requires a
/// non-empty key token — passing `""` is a malformed command the helper rejects.
fn identity_dht_key_for_bucket(omni_id: &[u8; 20], bucket: u64) -> [u8; 32] {
    let mut info = Vec::with_capacity(HKDF_IDENTITY_DHT_KEY_INFO.len() + 8);
    info.extend_from_slice(HKDF_IDENTITY_DHT_KEY_INFO);
    info.extend_from_slice(&bucket.to_le_bytes());
    hkdf32(omni_id, &info)
}

/// Like [`identity_topics`], but each topic paired with its per-bucket BT-DHT key seed
/// ([`identity_dht_key_for_bucket`]). Mirrors [`rendezvous_topics_with_keys`]. Callers
/// that drive the real courier helper MUST use this (not [`identity_topics`] + an empty
/// key), or the helper rejects the command.
pub fn identity_topics_with_keys(omni_id: &[u8; 20], now_ms: u64) -> Vec<(Topic, [u8; 32])> {
    let bucket = now_ms / 1000 / TOPIC_WINDOW_SECS;
    [bucket, bucket.saturating_sub(1)]
        .into_iter()
        .map(|b| (identity_topic_for_bucket(omni_id, b), identity_dht_key_for_bucket(omni_id, b)))
        .collect()
}

/// A public, signed identity beacon (Model A): the serverless replacement for a
/// key server + locator. Carries the publisher's identity keys, a signed
/// `PrekeyBundle` (to start X3DH), and dialable endpoints. Verifiable against the
/// Omni-ID (`sha256(signing_pub ‖ dh_pub)[:20]`) + the Ed25519 `signature` over
/// [`identity_beacon_signing_bytes`] — a malicious courier can drop it, never forge it.
///
/// Binary, ~1 KB (2026-10-13): it used to be JSON of 4.5 KB — the certificate chain as JSON, and again
/// inside the prekey bundle JSON — and ntfy, which carries a blob as hex text up to 4096 characters,
/// turned it into an attachment nobody reads: find-by-Omni-ID and the account passport's marks rode
/// Nostr alone. Now the chain is compact and carried once; the bundle carries only what the chain
/// does not (omni-crypto `PrekeyBundle::to_compact`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdentityBeacon {
    /// 20-byte Omni-ID commitment (redundant with the master key in the chain, but lets a
    /// reader match the topic without recomputing the hash first).
    pub omni_id: [u8; 20],
    /// The publishing device's certificate chain, master → working key → device, compact (omni-crypto
    /// `DeviceCertificateChainV1::to_compact`; this crate holds no keys). The Omni-ID commits to its
    /// master key; the device key in it signs the beacon. See multi-device-equal-nodes.md.
    pub chain_compact: Vec<u8>,
    /// The publisher's mesh NodeAddress (to dial once endpoints resolve).
    pub node: [u8; 20],
    /// The X3DH bootstrap material beyond the chain (omni-crypto `PrekeyBundle::to_compact`). Empty —
    /// a withdrawal: the device is no longer findable by ID.
    pub prekey_compact: Vec<u8>,
    /// The device's label, as the prekey bundle names it.
    pub device_label: String,
    pub reflexive: Vec<String>,
    pub tcp_direct: Vec<String>,
    pub relay_bundle: Option<String>,
    pub ts_ms: u64,
    pub ttl_secs: u32,
    /// Monotone per-publisher sequence (freshest wins on fetch; anti-rollback).
    pub seq: u64,
    /// Ed25519 signature over `identity_beacon_signing_bytes` by the chain's device key.
    /// Produced/verified by the app (this crate holds no keys). Empty pre-signing.
    pub signature: Vec<u8>,
}

const IDENTITY_BEACON_MAGIC: &[u8; 4] = b"OIB2";
const IDENTITY_BEACON_SIGNING_CONTEXT: &[u8] = b"omni-identity-beacon-v2\0";
/// Endpoints a beacon lists at most, per kind.
const IDENTITY_BEACON_MAX_ENDPOINTS: usize = 16;

impl IdentityBeacon {
    pub fn is_withdrawal(&self) -> bool {
        self.prekey_compact.is_empty()
    }

    /// Layout: magic «OIB2» · omni_id 20 · chain (u16 len) · node 20 · bundle (u16 len) · label (u8 len)
    /// · reflexive (u8 count, each u8 len) · tcp_direct (same) · relay bundle (u16 len, 0 — none) · ts u64
    /// · ttl u32 · seq u64 · signature (u8 len). Little-endian.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256 + self.chain_compact.len() + self.prekey_compact.len());
        out.extend_from_slice(IDENTITY_BEACON_MAGIC);
        out.extend_from_slice(&self.omni_id);
        push_u16_len(&mut out, &self.chain_compact);
        out.extend_from_slice(&self.node);
        push_u16_len(&mut out, &self.prekey_compact);
        push_u8_len(&mut out, self.device_label.as_bytes());
        for list in [&self.reflexive, &self.tcp_direct] {
            let list: Vec<&String> = list.iter().take(IDENTITY_BEACON_MAX_ENDPOINTS).collect();
            out.push(list.len() as u8);
            for endpoint in list {
                push_u8_len(&mut out, endpoint.as_bytes());
            }
        }
        push_u16_len(&mut out, self.relay_bundle.as_deref().unwrap_or("").as_bytes());
        out.extend_from_slice(&self.ts_ms.to_le_bytes());
        out.extend_from_slice(&self.ttl_secs.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        push_u8_len(&mut out, &self.signature);
        out
    }

    pub fn from_wire(bytes: &[u8]) -> Option<Self> {
        let mut rest = bytes;
        if take_bytes(&mut rest, 4)? != IDENTITY_BEACON_MAGIC {
            return None;
        }
        let omni_id: [u8; 20] = take_bytes(&mut rest, 20)?.try_into().ok()?;
        let chain_compact = take_u16_len(&mut rest)?.to_vec();
        let node: [u8; 20] = take_bytes(&mut rest, 20)?.try_into().ok()?;
        let prekey_compact = take_u16_len(&mut rest)?.to_vec();
        let device_label = String::from_utf8(take_u8_len(&mut rest)?.to_vec()).ok()?;
        let mut lists: Vec<Vec<String>> = Vec::with_capacity(2);
        for _ in 0..2 {
            let count = take_bytes(&mut rest, 1)?[0] as usize;
            if count > IDENTITY_BEACON_MAX_ENDPOINTS {
                return None;
            }
            let mut list = Vec::with_capacity(count);
            for _ in 0..count {
                list.push(String::from_utf8(take_u8_len(&mut rest)?.to_vec()).ok()?);
            }
            lists.push(list);
        }
        let relay = String::from_utf8(take_u16_len(&mut rest)?.to_vec()).ok()?;
        let ts_ms = u64::from_le_bytes(take_bytes(&mut rest, 8)?.try_into().ok()?);
        let ttl_secs = u32::from_le_bytes(take_bytes(&mut rest, 4)?.try_into().ok()?);
        let seq = u64::from_le_bytes(take_bytes(&mut rest, 8)?.try_into().ok()?);
        let signature = take_u8_len(&mut rest)?.to_vec();
        if !rest.is_empty() {
            return None;
        }
        let tcp_direct = lists.pop()?;
        let reflexive = lists.pop()?;
        Some(Self {
            omni_id,
            chain_compact,
            node,
            prekey_compact,
            device_label,
            reflexive,
            tcp_direct,
            relay_bundle: (!relay.is_empty()).then_some(relay),
            ts_ms,
            ttl_secs,
            seq,
            signature,
        })
    }

    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms <= self.ts_ms.saturating_add((self.ttl_secs as u64).saturating_mul(1000))
    }
}

/// The canonical bytes the publisher's device key signs (and a looker-up verifies): the beacon's wire
/// form with an empty signature, behind a domain prefix.
pub fn identity_beacon_signing_bytes(beacon: &IdentityBeacon) -> Vec<u8> {
    let mut canonical = beacon.clone();
    canonical.signature = Vec::new();
    let mut bytes = IDENTITY_BEACON_SIGNING_CONTEXT.to_vec();
    bytes.extend_from_slice(&canonical.to_wire());
    bytes
}

fn push_u8_len(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len().min(u8::MAX as usize);
    out.push(len as u8);
    out.extend_from_slice(&bytes[..len]);
}

fn push_u16_len(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len().min(u16::MAX as usize);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&bytes[..len]);
}

fn take_bytes<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if rest.len() < len {
        return None;
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    Some(head)
}

fn take_u8_len<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = take_bytes(rest, 1)?[0] as usize;
    take_bytes(rest, len)
}

fn take_u16_len<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = u16::from_le_bytes(take_bytes(rest, 2)?.try_into().ok()?) as usize;
    take_bytes(rest, len)
}

/// The two BEP44 slots a contact-pair uses under their shared per-bucket keypair (distinct salts).
/// Two writers, one keypair: each side claims a slot and reads the other's (the dual-slot-claim of
/// `bt-dht-rendezvous-courier.md`). The bytes are arbitrary — only their being two stable, distinct
/// salts matters. The SINGLE source of truth for the slot layout, shared by every BT-DHT publisher
/// (the local `BtDhtChannel` IPC leg AND the core's DHT-over-peer leg) so both address the SAME slots.
pub const DHT_SLOT_SALTS: [&[u8]; 2] = [b"0", b"1"];

/// The largest sealed blob one BT-DHT record carries. `omni-dht-helper` stores it as a pkarr-shaped
/// record (base64url in `_iroh` TXT `user-data` strings of a DNS packet, BEP44's 1000-byte cap on the
/// packet), which leaves 628 bytes. A binary beacon pads to 512 (540 B sealed) even with a relay bundle of a
/// few relays; one over the limit goes to the DHT as its slim copy, without the bundle
/// (`RendezvousChannel::max_blob`). See docs/decisions/libp2p-kad-vs-mainline.md.
pub const DHT_MAX_BLOB: usize = 628;

/// Decides + remembers which of the two [`DHT_SLOT_SALTS`] slots THIS publisher writes for a given
/// per-bucket key, so two contacts sharing one keypair settle on DISTINCT slots with no identity
/// exchange. On the first publish under a key it probes both slots and takes an empty one (its own
/// random preference first, so two simultaneous newcomers tend to split), then stays **sticky** to
/// that slot. Lives here (not in a single courier) so every BT-DHT publish path — the local IPC leg
/// and the DHT-over-peer leg — shares ONE claim policy and can't drift. The rare same-slot
/// first-contact race is covered by bucket-rotation + the ntfy/Nostr N-of-many legs (ADR residual note).
pub struct DhtSlotClaimer {
    pref: usize,
    claimed: Mutex<HashMap<[u8; 32], usize>>,
}

impl Default for DhtSlotClaimer {
    fn default() -> Self {
        Self::new()
    }
}

impl DhtSlotClaimer {
    /// A claimer with a random slot preference.
    pub fn new() -> Self {
        let mut b = [0u8; 1];
        OsRng.fill_bytes(&mut b);
        Self { pref: (b[0] & 1) as usize, claimed: Mutex::new(HashMap::new()) }
    }

    /// The slot index (0/1) this publisher writes for `key` this bucket: sticky if already claimed,
    /// else claim an empty one (preference first). `slot_occupied(salt) -> Ok(true)` probes whether a
    /// slot already holds a value; its error `E` propagates so a probe failure fails the claim (the
    /// caller's publish then fails best-effort and N-of-many covers it).
    pub fn claim<E>(
        &self,
        key: &[u8; 32],
        mut slot_occupied: impl FnMut(&[u8]) -> Result<bool, E>,
    ) -> Result<usize, E> {
        if let Some(&s) = self.claimed.lock().unwrap_or_else(|p| p.into_inner()).get(key) {
            return Ok(s);
        }
        // Probe both slots WITHOUT holding the lock (a probe is a slow IPC / mesh round-trip).
        let full = [slot_occupied(DHT_SLOT_SALTS[0])?, slot_occupied(DHT_SLOT_SALTS[1])?];
        let other = 1 - self.pref;
        let slot = if !full[self.pref] {
            self.pref
        } else if !full[other] {
            other
        } else {
            self.pref // both occupied (race) — best-effort, see the ADR residual note
        };

        let mut map = self.claimed.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(&s) = map.get(key) {
            return Ok(s); // a concurrent publish claimed first — honour it
        }
        if map.len() >= 256 {
            map.clear(); // hygiene: bounded growth across the many buckets of a long session
        }
        map.insert(*key, slot);
        Ok(slot)
    }
}

pub mod wire;

// Find by name — signed pointers from a name to an Omni-ID (docs/decisions/find-by-name.md).
pub mod name_discovery;
pub mod relay_directory;

const BEACON_WIRE_VERSION: u8 = 1;
const BEACON_HAS_RELAY_BUNDLE: u8 = 1;
const BEACON_HAS_REVOCATION_MARK: u8 = 2;
/// The relay bundle as its raw bytes (`om3relay:` + base64url of them) …
const RELAY_BUNDLE_RAW: u8 = 1;
/// … or, if it is not such a token, as text.
const RELAY_BUNDLE_TEXT: u8 = 0;
const RELAY_TOKEN_PREFIX: &str = "om3relay:";

fn nat_byte(nat: NatType) -> u8 {
    match nat {
        NatType::Unknown => 0,
        NatType::Public => 1,
        NatType::Cone => 2,
        NatType::Symmetric => 3,
    }
}

fn nat_from_byte(byte: u8) -> Option<NatType> {
    Some(match byte {
        0 => NatType::Unknown,
        1 => NatType::Public,
        2 => NatType::Cone,
        3 => NatType::Symmetric,
        _ => return None,
    })
}

/// A beacon in its compact binary form ([`wire`]): what [`seal_beacon`] encrypts. `None` when a field
/// does not fit its length prefix (an endpoint string over 255 bytes, a relay bundle over 64 KiB).
pub fn encode_beacon(beacon: &Beacon) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let mut out = Vec::with_capacity(128);
    out.push(BEACON_WIRE_VERSION);
    out.extend_from_slice(&beacon.node);
    out.push(nat_byte(beacon.nat_type));
    out.extend_from_slice(&beacon.ts_ms.to_be_bytes());
    out.extend_from_slice(&beacon.ttl_secs.to_be_bytes());
    let mut flags = 0;
    if beacon.relay_bundle.is_some() {
        flags |= BEACON_HAS_RELAY_BUNDLE;
    }
    if beacon.revoked_through_ms.is_some() {
        flags |= BEACON_HAS_REVOCATION_MARK;
    }
    out.push(flags);
    for list in [&beacon.forwarded, &beacon.reflexive, &beacon.tcp_direct] {
        out.push(u8::try_from(list.len()).ok()?);
        for endpoint in list {
            wire::put_endpoint(&mut out, endpoint)?;
        }
    }
    if let Some(mark) = beacon.revoked_through_ms {
        out.extend_from_slice(&mark.to_be_bytes());
    }
    if let Some(token) = beacon.relay_bundle.as_deref() {
        let engine = &base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let raw = token
            .strip_prefix(RELAY_TOKEN_PREFIX)
            .and_then(|payload| engine.decode(payload).ok())
            .filter(|raw| format!("{RELAY_TOKEN_PREFIX}{}", engine.encode(raw)) == token);
        let (kind, bytes) = match &raw {
            Some(raw) => (RELAY_BUNDLE_RAW, raw.as_slice()),
            None => (RELAY_BUNDLE_TEXT, token.as_bytes()),
        };
        out.push(kind);
        out.extend_from_slice(&u16::try_from(bytes.len()).ok()?.to_be_bytes());
        out.extend_from_slice(bytes);
    }
    Some(out)
}

/// The beacon [`encode_beacon`] wrote; `None` for anything else — a short read, an unknown version,
/// trailing bytes.
pub fn decode_beacon(bytes: &[u8]) -> Option<Beacon> {
    use base64::Engine as _;
    let mut reader = wire::Reader::new(bytes);
    if reader.u8()? != BEACON_WIRE_VERSION {
        return None;
    }
    let node = reader.array::<20>()?;
    let nat_type = nat_from_byte(reader.u8()?)?;
    let ts_ms = reader.u64()?;
    let ttl_secs = reader.u32()?;
    let flags = reader.u8()?;
    let mut lists: [Vec<String>; 3] = Default::default();
    for list in lists.iter_mut() {
        for _ in 0..reader.u8()? {
            list.push(reader.endpoint()?);
        }
    }
    let revoked_through_ms = if flags & BEACON_HAS_REVOCATION_MARK != 0 { Some(reader.u64()?) } else { None };
    let relay_bundle = if flags & BEACON_HAS_RELAY_BUNDLE != 0 {
        let kind = reader.u8()?;
        let len = reader.u16()? as usize;
        let bytes = reader.take(len)?;
        Some(match kind {
            RELAY_BUNDLE_RAW => format!(
                "{RELAY_TOKEN_PREFIX}{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
            ),
            RELAY_BUNDLE_TEXT => String::from_utf8(bytes.to_vec()).ok()?,
            _ => return None,
        })
    } else {
        None
    };
    if !reader.is_empty() {
        return None;
    }
    let [forwarded, reflexive, tcp_direct] = lists;
    Some(Beacon { node, forwarded, reflexive, tcp_direct, nat_type, ts_ms, ttl_secs, relay_bundle, revoked_through_ms })
}

fn aead(secret: &PairSecret) -> ChaCha20Poly1305 {
    let key_bytes = hkdf32(secret, HKDF_AEAD_INFO);
    ChaCha20Poly1305::new(Key::from_slice(&key_bytes))
}

/// Seal a beacon for the given topic (the topic is bound in as AEAD associated data, so a sealed
/// beacon can't be replayed under a different topic). Returns the sealed blob to publish.
pub fn seal_beacon(secret: &PairSecret, topic: &Topic, beacon: &Beacon) -> SealedBeacon {
    // Pad to a uniform size bucket BEFORE encrypting (R7 metadata hygiene), so the sealed size leaks
    // only a coarse bucket — not "this beacon carries a relay-set rotation" / the relay count.
    let plaintext = pad_plaintext_from(&encode_beacon(beacon).unwrap_or_default(), BEACON_PAD_FLOOR);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = aead(secret)
        .encrypt(nonce, Payload { msg: &plaintext, aad: topic.as_bytes() })
        .expect("AEAD encryption of a bounded payload does not fail");
    SealedBeacon { nonce: nonce_bytes.to_vec(), ciphertext }
}

/// Open + fully validate a sealed beacon: AEAD auth (forgery), TTL/skew (staleness), and replay
/// (the nonce must be fresh on this topic). On success the beacon is recorded in `guard` so a
/// replay of the same blob is rejected next time.
pub fn open_beacon(
    secret: &PairSecret,
    topic: &Topic,
    sealed: &SealedBeacon,
    now_ms: u64,
    guard: &mut ReplayGuard,
) -> Result<Beacon, RvError> {
    if sealed.nonce.len() != 12 {
        return Err(RvError::Forged);
    }
    if guard.seen(topic, &sealed.nonce) {
        return Err(RvError::Replay);
    }
    let nonce = Nonce::from_slice(&sealed.nonce);
    let plaintext = aead(secret)
        .decrypt(nonce, Payload { msg: &sealed.ciphertext, aad: topic.as_bytes() })
        .map_err(|_| RvError::Forged)?;
    let encoded = unpad_plaintext(&plaintext).ok_or(RvError::Malformed)?;
    let beacon = decode_beacon(encoded).ok_or(RvError::Malformed)?;
    let expires = beacon.ts_ms.saturating_add(u64::from(beacon.ttl_secs) * 1000);
    if now_ms > expires || beacon.ts_ms > now_ms.saturating_add(FUTURE_SKEW_MS) {
        return Err(RvError::Stale);
    }
    guard.record(topic, &sealed.nonce);
    Ok(beacon)
}

/// Bounded per-topic set of accepted nonces — the cheap anti-replay memory (TM-011). Caps growth so
/// a flood of distinct nonces can't exhaust memory; oldest entries are evicted.
#[derive(Default)]
pub struct ReplayGuard {
    seen: HashMap<Topic, Vec<Vec<u8>>>,
    per_topic_cap: usize,
}

impl ReplayGuard {
    pub fn new() -> Self {
        Self { seen: HashMap::new(), per_topic_cap: 256 }
    }

    fn seen(&self, topic: &Topic, nonce: &[u8]) -> bool {
        self.seen.get(topic).is_some_and(|v| v.iter().any(|n| n == nonce))
    }

    fn record(&mut self, topic: &Topic, nonce: &[u8]) {
        let cap = self.per_topic_cap.max(1);
        let entry = self.seen.entry(*topic).or_default();
        if entry.len() >= cap {
            entry.remove(0); // evict oldest
        }
        entry.push(nonce.to_vec());
    }
}

/// An interchangeable rendezvous provider — the seam Nostr / BT-DHT / ntfy / a friend-run node
/// implement. Deliberately tiny (publish a small opaque blob under a topic; fetch blobs for a
/// topic), because the requirement is tiny — which is why so many third-party services can serve as
/// one even though it isn't their main purpose. Errors are the provider's business (rate-limit,
/// unreachable); the caller treats every provider as best-effort and uses several (N-of-many).
pub trait RendezvousChannel {
    /// Publish an opaque `blob` under `topic`. `dht_key` is the per-bucket BT-DHT signing seed
    /// ([`derive_dht_key`]) — `Some` only for couriers that key a write-authenticated slot by it
    /// (BT-DHT / BEP44); web couriers (ntfy/Nostr) ignore it (the blinded `topic` is their whole
    /// key). It is the SECRET-derived seed, never the raw pair secret, so handing it to an untrusted
    /// courier process leaks nothing beyond "can write THIS pair's THIS-bucket slots" (the value is
    /// still AEAD-sealed by the core).
    ///
    /// `ttl_secs` — how long the blob is worth keeping. A courier that can be told (a Nostr relay, by
    /// NIP-40 `expiration`) drops it after that instead of keeping a stale beacon forever; the others
    /// keep it as long as they keep anything (ntfy ~12 h, a BEP44 item ~2 h).
    fn publish(&self, topic: &Topic, dht_key: Option<&[u8; 32]>, blob: &[u8], ttl_secs: u32) -> Result<(), ChannelError>;
    /// Publish `blob` as THIS writer's current value under `topic`: a courier that can replace a
    /// writer's earlier value (a Nostr relay — NIP-33 parameterized-replaceable events) keeps only the
    /// latest instead of every copy. For a beacon, which has one current value per writer; never for
    /// a note (a mailbox note, a revocation notice), whose earlier copies must stay. Without it a
    /// relay kept every beacon of the topic's window and every fetch read them all back — ~50 blobs a
    /// topic, all but one or two superseded (stand, 2026-10-14). Default: [`publish`](Self::publish).
    fn publish_replacing(
        &self,
        topic: &Topic,
        dht_key: Option<&[u8; 32]>,
        blob: &[u8],
        ttl_secs: u32,
    ) -> Result<(), ChannelError> {
        self.publish(topic, dht_key, blob, ttl_secs)
    }
    /// Best-effort fetch of currently-stored blobs for `topic` (poll model; a push variant can be a
    /// richer trait later). `dht_key` as in [`publish`](RendezvousChannel::publish) — BT-DHT uses it
    /// to address the pair's slots; other couriers ignore it.
    fn fetch(&self, topic: &Topic, dht_key: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError>;

    /// [`fetch`](RendezvousChannel::fetch) under several topics at once, every blob together — for
    /// topics read as one set (the relay directory's shelves; no DHT key). A courier that can asks
    /// them over one connection: a Nostr relay answered 429 to 16 handshakes from one address at
    /// once (stand, 2026-10-14). Default: one fetch after another; the first failure fails the call.
    fn fetch_many(&self, topics: &[Topic]) -> Result<Vec<Vec<u8>>, ChannelError> {
        let mut out = Vec::new();
        for topic in topics {
            out.extend(self.fetch(topic, None)?);
        }
        Ok(out)
    }

    /// Push model: hold a subscription on `topics` for up to `wait` and return the blob(s) published
    /// under any of them AFTER the call began — early, on the first arrival; empty on timeout. Lets the
    /// core learn a contact's fresh beacon the moment it is published instead of on its next poll
    /// (90 s – 30 min). Only couriers with a native subscription implement it (ntfy's stream, a Nostr
    /// `REQ` held open); BT-DHT has none, so the default is "unsupported".
    fn watch(&self, topics: &[Topic], wait: std::time::Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
        let _ = (topics, wait);
        Err(ChannelError("watch: this courier has no subscription".to_string()))
    }

    /// Whether [`watch`](RendezvousChannel::watch) is implemented.
    fn supports_watch(&self) -> bool {
        false
    }

    /// Whether this courier meters publishing and little of the allowance is left (ntfy.sh: 250 a day
    /// per IP). The fan-out then keeps what is left for the beacons a contact is waiting for, and sends
    /// background posts (a find-by-ID beacon, a mailbox note) through the other couriers only.
    fn publish_allowance_low(&self) -> bool {
        false
    }

    /// The largest blob this courier can hold, if it has a limit below what we may publish (a BT-DHT
    /// record: [`DHT_MAX_BLOB`]). A beacon over it goes to this courier as its slim copy — the same
    /// beacon without the relay bundle — so the addresses always get there.
    fn max_blob(&self) -> Option<usize> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct ChannelError(pub String);

impl ChannelError {
    /// Отказ по КВОТЕ, а не по поломке. Отличать стоит по одной причине: у публичных брокеров
    /// такие лимиты бывают СУТОЧНЫЕ. Живой замер 2026-08-13: ntfy.sh отвечает
    /// `429 {"code":42908,"error":"limit reached: daily message quota reached"}`, то есть до утра
    /// не откроется. Обычный откат (потолок 5 минут) продолжал бы стучаться в закрытую дверь
    /// ~288 раз в сутки — ровно то, чего сам комментарий у `ProviderHealth` велит не делать.
    pub fn is_quota_exhausted(&self) -> bool {
        let text = self.0.to_ascii_lowercase();
        text.contains("429")
            || text.contains("too many requests")
            || text.contains("quota")
            || text.contains("rate limit")
            // NIP-01 пишет это через дефис — без отдельной строки отказ nostr-релея по лимиту
            // проходил бы как обычная поломка.
            || text.contains("rate-limited")
    }
}

/// First-failure cooldown for a flaky/rate-limited provider (doubles per consecutive failure).
pub const RV_BACKOFF_BASE_MS: u64 = 2_000; // ~2 s
/// Откат при исчерпанной квоте. Порядок величины — не «флап на секунды», а «лимит до конца суток»
/// (ntfy.sh на бесплатном тарифе даёт именно суточную квоту). Час выбран как компромисс: достаточно
/// редко, чтобы не жечь попытки впустую, и достаточно часто, чтобы подхватить сброс лимита.
pub const RV_QUOTA_COOLDOWN_MS: u64 = 3_600_000; // 1 час
/// Cap on the cooldown — order-of-magnitude of a public broker's rate-limit window (e.g. ntfy.sh).
pub const RV_BACKOFF_MAX_MS: u64 = 300_000; // 5 min

/// Exponential backoff for the `n`-th CONSECUTIVE failure: `BASE * 2^(n-1)`, capped at [`RV_BACKOFF_MAX_MS`].
/// `n == 0` → `0` (no cooldown). Deterministic (no jitter) so it's unit-testable; a 2-peer rendezvous
/// has no thundering-herd to spread, so jitter buys nothing here. Sequence from `BASE = 2 s`:
/// 2s, 4s, 8s, 16s, 32s, 64s, 128s, 256s, then the 5 min cap.
pub fn backoff_ms(consecutive_failures: u32) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    let shift = consecutive_failures.saturating_sub(1).min(31);
    RV_BACKOFF_BASE_MS
        .saturating_mul(1u64 << shift)
        .min(RV_BACKOFF_MAX_MS)
}

/// Per-provider circuit breaker for the N-of-many fan-out. PURE state machine — it carries no clock
/// (the caller passes `now_ms`), so it's fully unit-testable and reusable by the helper's provider
/// loop. A provider that errors (HTTP 429 rate-limit, timeout, unreachable — rendezvous
/// can't tell them apart and doesn't need to) is put in a GROWING cooldown so the fan-out stops
/// hammering it — re-POSTing into a 429 only prolongs the limit — and spends its effort on the
/// HEALTHY providers instead (the entire point of N-of-many). A single success resets the breaker.
/// This is what stops one rate-limited public broker (the confirmed live non-convergence cause; see
/// docs/bugs/relay-crossnat-delivery-investigation.md) from sinking the whole rendezvous plane.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProviderHealth {
    consecutive_failures: u32,
    cooldown_until_ms: Option<u64>,
}

impl ProviderHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Should the fan-out SKIP this provider right now (it's still cooling down)?
    pub fn is_cooling_down(&self, now_ms: u64) -> bool {
        matches!(self.cooldown_until_ms, Some(until) if now_ms < until)
    }

    /// When the cooldown expires (`None` = ready now). Used to pick the soonest-to-recover provider
    /// as a liveness probe when EVERY provider is cooling down — so rendezvous never goes fully dark.
    pub fn cooldown_until_ms(&self) -> Option<u64> {
        self.cooldown_until_ms
    }

    /// Record a successful publish/fetch — clears the breaker immediately.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.cooldown_until_ms = None;
    }

    /// Отказ по исчерпанной квоте. Отдельный, ДЛИННЫЙ откат: экспонента с потолком в 5 минут
    /// рассчитана на флап и таймаут, а суточную квоту она просто долбит до утра. Счётчик подряд
    /// идущих отказов при этом не растёт — это не деградация провайдера, а его расписание.
    pub fn on_quota_exhausted(&mut self, now_ms: u64) {
        self.cooldown_until_ms = Some(now_ms.saturating_add(RV_QUOTA_COOLDOWN_MS));
    }

    /// Record a failure at `now_ms` — grow the cooldown exponentially ([`backoff_ms`]).
    pub fn on_failure(&mut self, now_ms: u64) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.cooldown_until_ms = Some(now_ms.saturating_add(backoff_ms(self.consecutive_failures)));
    }
}

/// In-memory test double for `RendezvousChannel`. Not for production — exists so the beacon +
/// trait logic is fully unit-testable without any network.
#[derive(Default)]
pub struct InMemoryChannel {
    store: std::sync::Mutex<HashMap<String, Vec<Vec<u8>>>>,
}

impl InMemoryChannel {
    pub fn new() -> Self {
        Self::default()
    }
}

impl RendezvousChannel for InMemoryChannel {
    fn publish(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>, blob: &[u8], _ttl_secs: u32) -> Result<(), ChannelError> {
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        store.entry(topic.id()).or_default().push(blob.to_vec());
        Ok(())
    }

    fn fetch(&self, topic: &Topic, _dht_key: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
        let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        Ok(store.get(&topic.id()).cloned().unwrap_or_default())
    }
}

/// Publish my beacon to every provider, under the current topic. Best-effort across providers
/// (one failing doesn't stop the others — N-of-many). Returns how many providers accepted it.
pub fn publish_beacon(
    providers: &[&dyn RendezvousChannel],
    secret: &PairSecret,
    now_ms: u64,
    beacon: &Beacon,
) -> usize {
    let (topic, dht_key) = rendezvous_topics_with_keys(secret, now_ms)[0]; // current bucket
    let sealed = seal_beacon(secret, &topic, beacon).to_wire();
    providers
        .iter()
        .filter(|p| p.publish(&topic, Some(&dht_key), &sealed, beacon.ttl_secs).is_ok())
        .count()
}

/// Collect + validate a contact's beacons from every provider, across the current + previous
/// topic. Forged/stale/replayed/malformed blobs are silently dropped; only fresh, authentic
/// beacons are returned.
///
/// The anti-replay dedup is scoped to THIS poll (a fresh [`ReplayGuard`] per call): a polling /
/// caching courier (ntfy `since=all`, BT-DHT, …) legitimately RE-SERVES the same still-valid beacon
/// on every fetch, so the rendezvous layer must stay idempotent to that re-serve — a guard persisted
/// ACROSS polls would reject the re-served beacon as `Replay` and the friend's beacon would vanish
/// after the first poll (the R7 convergence bug). Within one poll the guard still dedups an identical
/// blob returned by several providers / both topic buckets. Rollback is handled separately: a caller
/// that keeps several results picks the freshest by `ts_ms`.
pub fn collect_beacons(
    providers: &[&dyn RendezvousChannel],
    secret: &PairSecret,
    now_ms: u64,
) -> Vec<Beacon> {
    let mut guard = ReplayGuard::new();
    let mut out = Vec::new();
    for (topic, dht_key) in rendezvous_topics_with_keys(secret, now_ms) {
        for provider in providers {
            let Ok(blobs) = provider.fetch(&topic, Some(&dht_key)) else { continue };
            for blob in blobs {
                let Some(sealed) = SealedBeacon::from_wire(&blob) else { continue };
                if let Ok(beacon) = open_beacon(secret, &topic, &sealed, now_ms, &mut guard) {
                    out.push(beacon);
                }
            }
        }
    }
    out
}

// --- The account's mailbox --------------------------------------------------
// A note the account's devices leave each other at the couriers, for a device that is offline
// when the change is made (docs/decisions/multi-device-equal-nodes.md, «Почтовый ящик аккаунта»).
// Presence beacons live minutes; a note has to outlive the device that wrote it being switched off,
// so its topic turns over by the day and a reader looks a few days back. Keyed by the account's
// rendezvous secret like the account topic, but with its own HKDF labels: a mailbox topic, its DHT
// slot and its sealing key are unrelated to the beacon ones. The content is opaque bytes here — what
// it says and who signed it is the app's business.

const HKDF_MAILBOX_TOPIC_INFO: &[u8] = b"omni-account-mailbox-topic-v1";
const HKDF_MAILBOX_AEAD_INFO: &[u8] = b"omni-account-mailbox-aead-v1";
const HKDF_MAILBOX_DHT_KEY_INFO: &[u8] = b"omni-account-mailbox-btdht-key-v1";

/// One mailbox topic lasts a day.
pub const MAILBOX_WINDOW_SECS: u64 = 24 * 60 * 60;

/// Days a reader looks back: today and the two before.
const MAILBOX_DAYS_READ: u64 = 3;

/// How long a note is worth keeping at a courier: as long as a reader still looks at its day.
pub const MAILBOX_NOTE_TTL_SECS: u32 = (MAILBOX_WINDOW_SECS * MAILBOX_DAYS_READ) as u32;

fn mailbox_bucket_info(label: &[u8], bucket: u64) -> Vec<u8> {
    let mut info = Vec::with_capacity(label.len() + 8);
    info.extend_from_slice(label);
    info.extend_from_slice(&bucket.to_le_bytes());
    info
}

/// The mailbox topics with their BT-DHT key seeds: today's first (the one to write to), then the
/// earlier days a reader also fetches.
pub fn mailbox_topics_with_keys(secret: &PairSecret, now_ms: u64) -> Vec<(Topic, [u8; 32])> {
    let today = now_ms / 1000 / MAILBOX_WINDOW_SECS;
    (0..MAILBOX_DAYS_READ)
        .filter_map(|back| today.checked_sub(back))
        .map(|bucket| {
            (
                Topic(hkdf32(secret, &mailbox_bucket_info(HKDF_MAILBOX_TOPIC_INFO, bucket))),
                hkdf32(secret, &mailbox_bucket_info(HKDF_MAILBOX_DHT_KEY_INFO, bucket)),
            )
        })
        .collect()
}

fn mailbox_aead(secret: &PairSecret) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(&hkdf32(secret, HKDF_MAILBOX_AEAD_INFO)))
}

/// Seal a mailbox note for `topic` (bound in as associated data), padded like a beacon so the size
/// says only a coarse bucket. Returns the wire bytes: `nonce(12) || ciphertext`.
pub fn seal_mailbox(secret: &PairSecret, topic: &Topic, content: &[u8]) -> Vec<u8> {
    let plaintext = pad_plaintext(content);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = mailbox_aead(secret)
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: &plaintext, aad: topic.as_bytes() })
        .expect("AEAD encryption of a bounded payload does not fail");
    SealedBeacon { nonce: nonce_bytes.to_vec(), ciphertext }.to_wire()
}

/// Open a mailbox note fetched under `topic`: `None` for anything not sealed by a holder of the
/// secret for this very topic.
pub fn open_mailbox(secret: &PairSecret, topic: &Topic, wire: &[u8]) -> Option<Vec<u8>> {
    let sealed = SealedBeacon::from_wire(wire)?;
    let plaintext = mailbox_aead(secret)
        .decrypt(Nonce::from_slice(&sealed.nonce), Payload { msg: &sealed.ciphertext, aad: topic.as_bytes() })
        .ok()?;
    unpad_plaintext(&plaintext).map(<[u8]>::to_vec)
}

// --- The account passport (docs/decisions/account-recovery-from-friends.md, Б1) -------------------
//
// The account's own small package — profile, contacts, groups — kept by a few contacts so a device
// restored from the phrase, with no other device of the account left, gets them back. Its secret is
// derived from the account rendezvous secret (the working key's), so every device of the account
// writes and a device restored from the phrase recomputes it. A holder sees the locator (what it is
// asked for by), the version and the size, never the content.
//
// The holders also post a mark at the couriers — sealed under the mark secret, which the owner hands
// them — saying how to reach them: a device that knows nothing but the phrase reads the marks to
// find its holders.

const HKDF_PASSPORT_LOCATOR_INFO: &[u8] = b"omni-account-passport-locator-v1";
const HKDF_PASSPORT_AEAD_INFO: &[u8] = b"omni-account-passport-aead-v1";
const HKDF_PASSPORT_MARK_INFO: &[u8] = b"omni-account-passport-mark-v1";

/// Sealed passports are padded to this bucket: the size says only a coarse class.
const PASSPORT_PAD_BUCKET: usize = 4096;

/// What a holder is asked for: the same for every device of the account, opaque to anyone else.
pub fn passport_locator(account_secret: &PairSecret) -> [u8; 32] {
    hkdf32(account_secret, HKDF_PASSPORT_LOCATOR_INFO)
}

/// The secret of the holders' marks (a mailbox secret: the marks live on its day topics).
pub fn passport_mark_secret(account_secret: &PairSecret) -> PairSecret {
    hkdf32(account_secret, HKDF_PASSPORT_MARK_INFO)
}

fn passport_aead(account_secret: &PairSecret) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(&hkdf32(account_secret, HKDF_PASSPORT_AEAD_INFO)))
}

/// Seal passport bytes (bound to the account's locator), padded to [`PASSPORT_PAD_BUCKET`]. Wire:
/// `nonce(12) || ciphertext`.
pub fn seal_passport(account_secret: &PairSecret, content: &[u8]) -> Vec<u8> {
    let body = 4 + content.len();
    let target = body.div_ceil(PASSPORT_PAD_BUCKET).max(1) * PASSPORT_PAD_BUCKET;
    let mut plaintext = Vec::with_capacity(target);
    plaintext.extend_from_slice(&(content.len() as u32).to_le_bytes());
    plaintext.extend_from_slice(content);
    plaintext.resize(target, 0);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let locator = passport_locator(account_secret);
    let ciphertext = passport_aead(account_secret)
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: &plaintext, aad: &locator })
        .expect("AEAD encryption of a bounded payload does not fail");
    let mut wire = nonce_bytes.to_vec();
    wire.extend_from_slice(&ciphertext);
    wire
}

/// Open a sealed passport: `None` for anything not sealed under this account secret.
pub fn open_passport(account_secret: &PairSecret, wire: &[u8]) -> Option<Vec<u8>> {
    if wire.len() < 12 + 16 {
        return None;
    }
    let (nonce, ciphertext) = wire.split_at(12);
    let locator = passport_locator(account_secret);
    let plaintext = passport_aead(account_secret)
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad: &locator })
        .ok()?;
    unpad_plaintext(&plaintext).map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read-only courier is one marked entry; the mark round-trips and never touches a plain URL.
    #[test]
    fn a_courier_entry_carries_the_read_only_mark() {
        assert_eq!(courier_entry("wss://nos.lol", CourierUse::ReadOnly), "wss://nos.lol#read");
        assert_eq!(parse_courier_entry("wss://nos.lol#read"), ("wss://nos.lol", CourierUse::ReadOnly));
        assert_eq!(parse_courier_entry("https://bob.example#pair"), ("https://bob.example", CourierUse::PairOnly));
        assert_eq!(parse_courier_entry("https://ntfy.sh"), ("https://ntfy.sh", CourierUse::PublishRead));
        assert_eq!(courier_label("wss://nostr.mom/"), "nostr:nostr.mom");
        assert_eq!(courier_label("dht://mainline"), "dht:mainline");
    }

    /// The pool ranking is a pure function of the topic and the relay: both sides of a pair get the
    /// same order, a different topic shuffles it, and over many topics every relay gets its share.
    #[test]
    fn the_pool_rank_is_shared_by_both_sides_and_spreads_topics_over_relays() {
        let topic = rendezvous_topics(&secret(1), 1_700_000_000_000)[0];
        let order = |topic: &Topic| {
            let mut relays = NOSTR_RELAY_POOL.to_vec();
            relays.sort_by_key(|url| pool_rank(topic, url));
            relays
        };
        assert_eq!(order(&topic), order(&rendezvous_topics(&secret(1), 1_700_000_000_000)[0]));
        let mut firsts = std::collections::HashMap::new();
        for seed in 0..120u8 {
            let topic = rendezvous_topics(&secret(seed), 1_700_000_000_000)[0];
            *firsts.entry(order(&topic)[0]).or_insert(0) += 1;
        }
        assert_eq!(firsts.len(), NOSTR_RELAY_POOL.len(), "every relay comes first for some topic");
        assert!(NOSTR_RELAY_POOL.iter().all(|url| DEFAULT_RENDEZVOUS_PROVIDERS.contains(url)));
    }

    /// A passport opens only under the account secret it was sealed with; its size says only a
    /// 4 KB class; garbage and a truncated blob are refused without a panic.
    #[test]
    fn a_passport_opens_only_under_its_own_account_secret() {
        let s = secret(21);
        let wire = seal_passport(&s, b"passport");
        assert_eq!(wire.len(), 12 + PASSPORT_PAD_BUCKET + 16);
        assert_eq!(open_passport(&s, &wire).as_deref(), Some(&b"passport"[..]));
        assert_eq!(open_passport(&secret(22), &wire), None);
        assert_eq!(open_passport(&s, &wire[..wire.len() - 1]), None);
        assert_eq!(open_passport(&s, &[]), None);
        assert_eq!(open_passport(&s, &[0u8; 40]), None);
        assert_ne!(passport_locator(&s), passport_locator(&secret(22)));
        assert_ne!(passport_mark_secret(&s), s);
        let big = vec![7u8; 5000];
        assert_eq!(seal_passport(&s, &big).len(), 12 + 2 * PASSPORT_PAD_BUCKET + 16);
    }

    /// The mailbox lives by the day, apart from the beacon topics: today's topic is the first, the
    /// reader also gets the two days before, and a note opens only under the secret and topic it was
    /// sealed for — a presence beacon sealed with the same secret is not a note.
    #[test]
    fn a_mailbox_note_opens_only_under_its_own_secret_and_day() {
        let s = secret(9);
        let day_ms = MAILBOX_WINDOW_SECS * 1000;
        let now = 1_700_000_000_000;
        let topics = mailbox_topics_with_keys(&s, now);
        assert_eq!(topics.len(), 3);
        assert_eq!(mailbox_topics_with_keys(&s, now + day_ms)[1].0, topics[0].0, "yesterday's topic is read tomorrow");
        assert_eq!(mailbox_topics_with_keys(&s, now + 60_000)[0].0, topics[0].0, "the topic holds within a day");
        assert!(rendezvous_topics(&s, now).iter().all(|beacon_topic| topics.iter().all(|(t, _)| t != beacon_topic)));
        assert_ne!(&topics[0].1[..], topics[0].0.as_bytes());

        let today = topics[0].0;
        let wire = seal_mailbox(&s, &today, b"note");
        assert_eq!(open_mailbox(&s, &today, &wire).as_deref(), Some(&b"note"[..]));
        assert_eq!(open_mailbox(&secret(10), &today, &wire), None, "another account's secret");
        assert_eq!(open_mailbox(&s, &topics[1].0, &wire), None, "moved to another day");
        let beacon = seal_beacon(&s, &today, &sample_beacon(now)).to_wire();
        assert_eq!(open_mailbox(&s, &today, &beacon), None, "a beacon is not a note");
    }

    fn secret(seed: u8) -> PairSecret {
        [seed; 32]
    }

    /// Every field comes back from the binary form; a relay token rides as its raw bytes, anything
    /// else as text; a cut, a trailing byte or another version reads as nothing.
    #[test]
    fn a_beacon_round_trips_through_its_binary_form() {
        use base64::Engine as _;
        let token = format!("om3relay:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9u8; 217]));
        let full = Beacon {
            node: [4u8; 20],
            forwarded: vec!["[2001:db8::1]:51820".into(), "203.0.113.9:6000".into()],
            reflexive: vec!["198.51.100.7:51820".into()],
            tcp_direct: vec!["door.example:443".into()],
            nat_type: NatType::Symmetric,
            ts_ms: 1_791_000_000_123,
            ttl_secs: 46_800,
            relay_bundle: Some(token.clone()),
            revoked_through_ms: Some(1_790_000_000_000),
        };
        let encoded = encode_beacon(&full).unwrap();
        assert_eq!(decode_beacon(&encoded), Some(full.clone()));
        assert!(encoded.len() < 217 + 120, "{} B — the token's 217 bytes are most of it", encoded.len());
        let text = Beacon { relay_bundle: Some("not a token".into()), ..full.clone() };
        assert_eq!(decode_beacon(&encode_beacon(&text).unwrap()), Some(text));
        let bare = sample_beacon(1_000);
        assert_eq!(decode_beacon(&encode_beacon(&bare).unwrap()), Some(bare));
        assert_eq!(decode_beacon(&encoded[..encoded.len() - 1]), None, "cut");
        assert_eq!(decode_beacon(&[encoded.as_slice(), &[0]].concat()), None, "trailing byte");
        let mut other_version = encoded.clone();
        other_version[0] = 9;
        assert_eq!(decode_beacon(&other_version), None);
    }

    /// A beacon with a bundle of three relays with pins (~217 raw bytes) seals within what a BT-DHT
    /// record holds — and into the same padding bucket as one without a bundle.
    #[test]
    fn a_beacon_with_a_relay_bundle_seals_within_a_dht_record() {
        use base64::Engine as _;
        let secret = [5u8; 32];
        let topic = rendezvous_topics(&secret, 0)[0];
        let mut beacon = sample_beacon(1_000);
        let bare = seal_beacon(&secret, &topic, &beacon).to_wire().len();
        beacon.relay_bundle = Some(format!("om3relay:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([1u8; 217])));
        let with_bundle = seal_beacon(&secret, &topic, &beacon).to_wire().len();
        assert!(with_bundle <= DHT_MAX_BLOB, "{with_bundle} B");
        assert_eq!(with_bundle, bare, "one bucket: the length does not tell a bundle is aboard");
    }

    fn sample_beacon(now: u64) -> Beacon {
        Beacon {
            node: [1u8; 20],
            forwarded: vec!["[2001:db8::1]:51820".into()],
            reflexive: vec!["203.0.113.7:51820".into()],
            tcp_direct: vec![],
            nat_type: NatType::Cone,
            ts_ms: now,
            ttl_secs: 300,
            relay_bundle: None,
            revoked_through_ms: None,
        }
    }

    /// BT-DHT courier (R7): the per-bucket DHT key seed is deterministic, secret-derived, rotates per
    /// bucket, and is DISTINCT from the topic bytes — so an observer of the (semi-public) topic can't
    /// derive it (grief-proof keying). See docs/decisions/bt-dht-rendezvous-courier.md.
    #[test]
    fn dht_key_is_deterministic_secret_derived_and_distinct_from_topic() {
        let s = secret(7);
        let now = 1_700_000_000_000;
        let bucket = now / 1000 / TOPIC_WINDOW_SECS;
        // Deterministic for the same (secret, bucket).
        assert_eq!(derive_dht_key(&s, bucket), derive_dht_key(&s, bucket));
        // Distinct from the topic bytes (different HKDF info) — a topic observer can't get the key.
        assert_ne!(&derive_dht_key(&s, bucket)[..], topic_for_bucket(&s, bucket).as_bytes());
        // Secret-derived: a different secret yields a different key.
        assert_ne!(derive_dht_key(&s, bucket), derive_dht_key(&secret(8), bucket));
        // Rotates per bucket.
        assert_ne!(derive_dht_key(&s, bucket), derive_dht_key(&s, bucket + 1));
        // `rendezvous_topics_with_keys` aligns with `rendezvous_topics` and carries the right keys.
        let twk = rendezvous_topics_with_keys(&s, now);
        let topics = rendezvous_topics(&s, now);
        assert_eq!(twk.len(), topics.len());
        assert_eq!(twk[0].0, topics[0]);
        assert_eq!(twk[0].1, derive_dht_key(&s, bucket));
        assert_eq!(twk[1].1, derive_dht_key(&s, bucket.saturating_sub(1)));
    }

    /// Dual-slot-claim: two publishers sharing one per-bucket key settle on DISTINCT slots (the
    /// second sees the first's slot occupied and takes the other), and a claimer is sticky to its slot
    /// without re-probing. See docs/decisions/bt-dht-rendezvous-courier.md.
    #[test]
    fn dht_slot_claimer_splits_two_writers_and_is_sticky() {
        let key = [7u8; 32];
        let occupied = Mutex::new(std::collections::HashSet::<Vec<u8>>::new());

        // A claims first (both slots empty) → takes its preference; we mark it occupied.
        let a = DhtSlotClaimer::new();
        let sa = a
            .claim(&key, |salt: &[u8]| Ok::<_, ()>(occupied.lock().unwrap().contains(salt)))
            .unwrap();
        occupied.lock().unwrap().insert(DHT_SLOT_SALTS[sa].to_vec());

        // B claims (A's slot occupied) → takes the OTHER slot.
        let b = DhtSlotClaimer::new();
        let sb = b
            .claim(&key, |salt: &[u8]| Ok::<_, ()>(occupied.lock().unwrap().contains(salt)))
            .unwrap();
        assert_ne!(sa, sb, "second writer settles on the other slot");

        // Sticky: A re-claims the same key without probing (the closure must never run).
        let sa2 = a.claim::<()>(&key, |_| panic!("sticky claim must not re-probe")).unwrap();
        assert_eq!(sa, sa2, "claimer stays sticky to its slot for the bucket");
    }

    /// Task 3 — the `tcp_direct` front doors survive the seal→open round-trip, and a beacon WITHOUT
    /// the field (an older publisher) opens with an empty list (additive + back-compatible). See
    /// docs/decisions/tcp-direct-peer-fallback.md.
    #[test]
    fn beacon_carries_tcp_direct_endpoints_and_defaults_empty_for_old_beacons() {
        let s = secret(11);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        // (1) the tcp_direct front doors survive seal → open.
        let beacon = Beacon {
            tcp_direct: vec!["198.51.100.4:8443".into()],
            ..sample_beacon(now)
        };
        let sealed = seal_beacon(&s, &topic, &beacon);
        let mut guard = ReplayGuard::new();
        let opened = open_beacon(&s, &topic, &sealed, now, &mut guard).expect("opens");
        assert_eq!(opened.tcp_direct, vec!["198.51.100.4:8443".to_string()]);
        // (2) an older publisher's beacon (no tcp_direct key) decodes to an empty list.
        let legacy = r#"{"node":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"reflexive":[],"nat_type":"Cone","ts_ms":1,"ttl_secs":300}"#;
        let parsed: Beacon = serde_json::from_str(legacy).expect("legacy beacon decodes");
        assert!(parsed.tcp_direct.is_empty(), "missing field defaults to empty");
    }

    /// `forwarded` survives seal -> open, and a beacon WITHOUT the key (an older publisher, whose
    /// forwards still ride inside `reflexive`) opens with an empty list — so an old peer is treated
    /// exactly as before, never as "has a forward it doesn't have".
    #[test]
    fn beacon_carries_forwarded_endpoints_separately_and_defaults_empty_for_old_beacons() {
        let s = secret(12);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let beacon = Beacon {
            forwarded: vec!["198.51.100.8:40123".into()],
            reflexive: vec!["198.51.100.8:61001".into()],
            ..sample_beacon(now)
        };
        let sealed = seal_beacon(&s, &topic, &beacon);
        let mut guard = ReplayGuard::new();
        let opened = open_beacon(&s, &topic, &sealed, now, &mut guard).expect("opens");
        assert_eq!(opened.forwarded, vec!["198.51.100.8:40123".to_string()]);
        assert_eq!(opened.reflexive, vec!["198.51.100.8:61001".to_string()]);

        let legacy = r#"{"node":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"reflexive":["198.51.100.8:40123"],"nat_type":"Symmetric","ts_ms":1,"ttl_secs":300}"#;
        let parsed: Beacon = serde_json::from_str(legacy).expect("legacy beacon decodes");
        assert!(parsed.forwarded.is_empty(), "missing field defaults to empty");
        assert_eq!(parsed.reflexive.len(), 1, "an old publisher's list stays where it was");
    }

    /// The iroh spike is gone (docs/decisions/iroh-connectivity-evaluation.md), but a peer still on a
    /// spike build may send a beacon with its `iroh` key: it must still open (the unknown key is
    /// ignored), and our own beacon never carries the key.
    #[test]
    fn a_beacon_from_an_iroh_spike_build_still_opens_and_ours_has_no_iroh_key() {
        let spike = r#"{"node":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"reflexive":["198.51.100.8:40123"],"nat_type":"Cone","ts_ms":1,"ttl_secs":300,"iroh":"iroh:ab;relay=https://relay.example/"}"#;
        let parsed: Beacon = serde_json::from_str(spike).expect("a spike-build beacon decodes");
        assert_eq!(parsed.reflexive, vec!["198.51.100.8:40123".to_string()]);
        assert!(!serde_json::to_string(&sample_beacon(1_700_000_000_000)).unwrap().contains("iroh"));
    }

    #[test]
    fn provider_list_defaults_to_two_protocols_and_parses_a_csv_override() {
        // Unset / blank → the default cross-protocol set (ntfy + Nostr).
        assert_eq!(parse_provider_urls(None), DEFAULT_RENDEZVOUS_PROVIDERS.to_vec());
        assert_eq!(
            parse_provider_urls(Some("   ".into())),
            DEFAULT_RENDEZVOUS_PROVIDERS.to_vec()
        );
        // CSV → trimmed, empties dropped, order preserved (the N-of-many fan-out set).
        assert_eq!(
            parse_provider_urls(Some(
                "https://ntfy.sh, wss://relay.example , ,https://b.example".into()
            )),
            vec!["https://ntfy.sh", "wss://relay.example", "https://b.example"]
        );
    }

    #[test]
    fn beacon_round_trips_through_a_channel() {
        let s = secret(1);
        let now = 1_700_000_000_000;
        let ch = InMemoryChannel::new();
        let published = publish_beacon(&[&ch], &s, now, &sample_beacon(now));
        assert_eq!(published, 1);

        let got = collect_beacons(&[&ch], &s, now + 1000);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reflexive[0], "203.0.113.7:51820");
        assert_eq!(got[0].nat_type, NatType::Cone);
    }

    #[test]
    fn padding_quantizes_the_sealed_wire_size_so_content_size_does_not_leak() {
        // R7 metadata hygiene: the sealed wire size is quantized to BEACON_PAD_BUCKET, so a channel
        // observer reads only a COARSE size class — never the exact content size or the relay count.
        let s = secret(7);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];

        // Two presence beacons differing by a few plaintext bytes (one extra reflexive endpoint, still
        // within one 256B bucket) seal to the IDENTICAL wire size — no fine-grained size leak.
        let small = sample_beacon(now);
        let mut slightly_bigger = sample_beacon(now);
        slightly_bigger.reflexive.push("198.51.100.9:7777".into());
        let small_wire = seal_beacon(&s, &topic, &small).to_wire().len();
        assert_eq!(
            small_wire,
            seal_beacon(&s, &topic, &slightly_bigger).to_wire().len(),
            "same bucket → identical wire size"
        );
        // Wire = nonce(12) + ciphertext(padded-to-bucket + 16-byte AEAD tag) → padded part is a bucket.
        assert_eq!((small_wire - 12 - 16) % BEACON_PAD_BUCKET, 0, "padded plaintext is a full bucket");

        // A beacon carrying a relay bundle is bigger → a HIGHER bucket (coarse class only), and still
        // round-trips (the unpad recovers the exact json).
        let with_bundle = Beacon {
            relay_bundle: Some("om3relay:".to_string() + &"A".repeat(600)),
            ..sample_beacon(now)
        };
        let sealed = seal_beacon(&s, &topic, &with_bundle);
        assert!(sealed.to_wire().len() > small_wire, "a larger beacon occupies a higher bucket");
        let mut guard = ReplayGuard::new();
        let opened = open_beacon(&s, &topic, &sealed, now + 1000, &mut guard).expect("opens after unpad");
        assert_eq!(opened.relay_bundle, with_bundle.relay_bundle);
    }

    #[test]
    fn wrong_pair_secret_finds_a_different_topic_and_cannot_open() {
        // Blinded topics: a different secret derives a different topic, so the beacon isn't even
        // found — and if forced onto the right topic, AEAD auth fails. (TM-011 + TM-009.)
        let now = 1_700_000_000_000;
        let alice = secret(1);
        let mallory = secret(2);
        assert_ne!(rendezvous_topics(&alice, now)[0], rendezvous_topics(&mallory, now)[0]);

        let ch = InMemoryChannel::new();
        publish_beacon(&[&ch], &alice, now, &sample_beacon(now));
        // Mallory, scanning her own topics, finds nothing of Alice's.
        assert!(collect_beacons(&[&ch], &mallory, now).is_empty());
    }

    #[test]
    fn relay_bundle_field_round_trips_under_seal_and_is_back_compatible() {
        // R1c: the optional relay_bundle rides inside the AEAD-sealed beacon (so only the contact
        // reads it), survives seal→open, and an older beacon JSON without the field decodes to None.
        let s = secret(9);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let beacon = Beacon { relay_bundle: Some("om3relay:abc".into()), ..sample_beacon(now) };
        let sealed = seal_beacon(&s, &topic, &beacon);
        let mut guard = ReplayGuard::new();
        let opened = open_beacon(&s, &topic, &sealed, now, &mut guard).expect("opens");
        assert_eq!(opened.relay_bundle.as_deref(), Some("om3relay:abc"));

        // Back-compat: a pre-R1c beacon (no relay_bundle key) still deserializes, field = None.
        let legacy = r#"{"node":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"reflexive":[],"nat_type":"Cone","ts_ms":1,"ttl_secs":300}"#;
        let parsed: Beacon = serde_json::from_str(legacy).expect("legacy beacon decodes");
        assert_eq!(parsed.relay_bundle, None);
    }

    #[test]
    fn forged_or_tampered_beacon_is_rejected() {
        let s = secret(3);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let mut sealed = seal_beacon(&s, &topic, &sample_beacon(now));
        // Flip a ciphertext byte.
        sealed.ciphertext[0] ^= 0xff;
        let mut guard = ReplayGuard::new();
        assert_eq!(
            open_beacon(&s, &topic, &sealed, now, &mut guard),
            Err(RvError::Forged)
        );
    }

    #[test]
    fn expired_beacon_is_stale() {
        let s = secret(4);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let beacon = Beacon { ttl_secs: 10, ..sample_beacon(now) };
        let sealed = seal_beacon(&s, &topic, &beacon);
        let mut guard = ReplayGuard::new();
        let later = now + 11_000; // 11s > 10s ttl
        assert_eq!(
            open_beacon(&s, &topic, &sealed, later, &mut guard),
            Err(RvError::Stale)
        );
    }

    #[test]
    fn future_dated_beacon_beyond_skew_is_stale() {
        let s = secret(5);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let beacon = Beacon { ts_ms: now + 10 * 60 * 1000, ..sample_beacon(now) }; // 10 min ahead
        let sealed = seal_beacon(&s, &topic, &beacon);
        let mut guard = ReplayGuard::new();
        assert_eq!(open_beacon(&s, &topic, &sealed, now, &mut guard), Err(RvError::Stale));
    }

    #[test]
    fn replayed_beacon_is_rejected_on_second_open() {
        let s = secret(6);
        let now = 1_700_000_000_000;
        let topic = rendezvous_topics(&s, now)[0];
        let sealed = seal_beacon(&s, &topic, &sample_beacon(now));
        let mut guard = ReplayGuard::new();
        assert!(open_beacon(&s, &topic, &sealed, now, &mut guard).is_ok());
        assert_eq!(
            open_beacon(&s, &topic, &sealed, now, &mut guard),
            Err(RvError::Replay)
        );
    }

    #[test]
    fn a_caching_courier_reserving_a_valid_beacon_must_not_vanish_across_polls() {
        // R7 REPRO: a polling/caching courier (ntfy `since=all`; the InMemoryChannel here keeps every
        // published blob and re-serves it on each fetch) returns the SAME valid peer beacon on every
        // sweep. The rendezvous layer MUST stay idempotent to that re-serve. With a PERSISTENT replay
        // guard, sweep 1 opens the beacon (records its nonce) but sweep 2 rejects the re-served blob as
        // `Replay` → the friend's beacon VANISHES → endless `NoPeerBeacon`. (This is the live
        // R7-step-0 bug.) FAILS before the fix; the fix scopes the anti-replay dedup to a single poll.
        let s = secret(11);
        let now = 1_700_000_000_000;
        let ch = InMemoryChannel::new();
        assert_eq!(publish_beacon(&[&ch], &s, now, &sample_beacon(now)), 1);
        // Two consecutive polls (sweeps) over the SAME caching courier — each is independent; both
        // must find the re-served valid beacon. (Was: a persisted guard dropped sweep 2 as Replay.)
        assert_eq!(collect_beacons(&[&ch], &s, now).len(), 1, "sweep 1 finds it");
        assert_eq!(
            collect_beacons(&[&ch], &s, now).len(),
            1,
            "sweep 2 must STILL find the re-served valid beacon (idempotent to a caching courier)"
        );
    }

    #[test]
    fn topic_bound_as_aad_blocks_cross_topic_replay() {
        // A beacon sealed for the current topic must not authenticate under the previous topic.
        let s = secret(7);
        let now = 1_700_000_000_000;
        let topics = rendezvous_topics(&s, now);
        let sealed = seal_beacon(&s, &topics[0], &sample_beacon(now));
        let mut guard = ReplayGuard::new();
        assert_eq!(
            open_beacon(&s, &topics[1], &sealed, now, &mut guard),
            Err(RvError::Forged)
        );
    }

    #[test]
    fn topic_hex_round_trips() {
        let s = secret(9);
        let topic = rendezvous_topics(&s, 1_700_000_000_000)[0];
        let restored = Topic::from_hex(&topic.id()).expect("valid hex");
        assert_eq!(restored, topic);
        assert!(Topic::from_hex("xyz").is_none());
        assert!(Topic::from_hex(&"ab".repeat(31)).is_none()); // 62 chars, too short
    }

    #[test]
    fn backoff_doubles_per_failure_and_caps() {
        // n==0 → no cooldown; then BASE * 2^(n-1), capped at RV_BACKOFF_MAX_MS.
        assert_eq!(backoff_ms(0), 0);
        assert_eq!(backoff_ms(1), RV_BACKOFF_BASE_MS); // 2s
        assert_eq!(backoff_ms(2), RV_BACKOFF_BASE_MS * 2); // 4s
        assert_eq!(backoff_ms(3), RV_BACKOFF_BASE_MS * 4); // 8s
        assert_eq!(backoff_ms(8), RV_BACKOFF_BASE_MS * 128); // 256s, still under the cap
        // Far enough out it saturates at the cap, and never overflows for a huge failure count.
        assert_eq!(backoff_ms(9), RV_BACKOFF_MAX_MS);
        assert_eq!(backoff_ms(1000), RV_BACKOFF_MAX_MS);
        assert_eq!(backoff_ms(u32::MAX), RV_BACKOFF_MAX_MS);
    }

    #[test]
    fn provider_health_breaker_cools_down_then_recovers_and_resets_on_success() {
        let mut h = ProviderHealth::new();
        let t0 = 1_000_000u64;
        // Fresh → never cooling down.
        assert!(!h.is_cooling_down(t0));
        assert_eq!(h.cooldown_until_ms(), None);

        // One failure → cooling down for BASE ms; ready again right after it expires.
        h.on_failure(t0);
        assert!(h.is_cooling_down(t0));
        assert!(h.is_cooling_down(t0 + RV_BACKOFF_BASE_MS - 1));
        assert!(!h.is_cooling_down(t0 + RV_BACKOFF_BASE_MS), "cooldown is exclusive at expiry");

        // A SECOND consecutive failure grows the window (no success reset between them).
        let t1 = t0 + RV_BACKOFF_BASE_MS;
        h.on_failure(t1);
        assert_eq!(h.cooldown_until_ms(), Some(t1 + RV_BACKOFF_BASE_MS * 2));

        // A success clears the breaker entirely — next failure starts from the base again.
        h.on_success();
        assert!(!h.is_cooling_down(t1));
        assert_eq!(h.cooldown_until_ms(), None);
        let t2 = t1 + 10_000;
        h.on_failure(t2);
        assert_eq!(h.cooldown_until_ms(), Some(t2 + RV_BACKOFF_BASE_MS), "reset → back to base backoff");
    }

    #[test]
    fn a_quota_refusal_is_told_apart_from_a_broken_provider() {
        // Живой ответ ntfy.sh (2026-08-13). Провайдер жив и отвечает — просто не пускает.
        assert!(ChannelError(
            r#"ntfy publish: status 429"#.to_string()
        )
        .is_quota_exhausted());
        assert!(ChannelError(
            "ntfy publish: limit reached: daily message quota reached".to_string()
        )
        .is_quota_exhausted());

        // А это настоящие поломки, и длинный откат им не положен.
        assert!(!ChannelError("ntfy publish: io error: connection refused".to_string())
            .is_quota_exhausted());
        assert!(!ChannelError("ntfy fetch: status 500".to_string()).is_quota_exhausted());
        assert!(!ChannelError("nostr publish: timed out".to_string()).is_quota_exhausted());
        // NIP-01 формулирует лимит через дефис.
        assert!(ChannelError("nostr publish: rate-limited: slow down".to_string())
            .is_quota_exhausted());
        assert!(!ChannelError("nostr publish: auth-required: please sign in".to_string())
            .is_quota_exhausted());
    }

    #[test]
    fn a_daily_quota_is_not_retried_every_five_minutes() {
        // Экспонента с потолком 5 минут рассчитана на флап. Суточную квоту она долбила бы ~288 раз
        // в сутки — ровно то, чего комментарий у ProviderHealth велит не делать.
        let t0 = 1_000_000;
        let mut quota = ProviderHealth::new();
        quota.on_quota_exhausted(t0);

        let mut flapping = ProviderHealth::new();
        for _ in 0..12 {
            flapping.on_failure(t0);
        }

        assert!(quota.is_cooling_down(t0 + RV_BACKOFF_MAX_MS));
        assert!(
            !flapping.is_cooling_down(t0 + RV_BACKOFF_MAX_MS),
            "обычный откат обязан истечь на своём потолке — иначе тест ничего не различает"
        );
        assert!(!quota.is_cooling_down(t0 + RV_QUOTA_COOLDOWN_MS));
    }

    #[test]
    fn a_quota_refusal_does_not_count_as_provider_degradation() {
        // Квота — это расписание провайдера, а не его деградация: счётчик подряд идущих отказов
        // расти не должен, иначе один суточный лимит навсегда сместит откат в потолок.
        let t0 = 5_000;
        let mut health = ProviderHealth::new();
        health.on_quota_exhausted(t0);
        health.on_success();
        assert!(!health.is_cooling_down(t0));

        health.on_failure(t0);
        assert!(
            !health.is_cooling_down(t0 + RV_BACKOFF_BASE_MS),
            "после успеха откат должен начинаться с базы, а не с накопленного за квоту"
        );
    }

    #[test]
    fn n_of_many_one_provider_suffices() {
        // Published to two providers; fetching from only the second still finds it.
        let s = secret(8);
        let now = 1_700_000_000_000;
        let a = InMemoryChannel::new();
        let b = InMemoryChannel::new();
        assert_eq!(publish_beacon(&[&a, &b], &s, now, &sample_beacon(now)), 2);
        assert_eq!(collect_beacons(&[&b], &s, now).len(), 1);
    }

    // --- Model A identity discovery (MA2) ---
    fn sample_identity_beacon(now: u64) -> IdentityBeacon {
        IdentityBeacon {
            omni_id: [1u8; 20],
            chain_compact: vec![2u8; 470],
            node: [4u8; 20],
            prekey_compact: vec![3u8; 180],
            device_label: "Ноутбук".to_string(),
            reflexive: vec!["203.0.113.5:51820".to_string()],
            tcp_direct: vec![],
            relay_bundle: None,
            ts_ms: now,
            ttl_secs: 600,
            seq: 7,
            signature: vec![],
        }
    }

    #[test]
    fn identity_topics_are_id_keyed_and_rotate() {
        let now = 1_700_000_000_000;
        let id_a = [0xAAu8; 20];
        let id_b = [0xBBu8; 20];
        let a = identity_topics(&id_a, now);
        // Two buckets (current + previous), distinct.
        assert_eq!(a.len(), 2);
        assert_ne!(a[0].id(), a[1].id());
        // A different ID yields different topics (find-by-ID isolation).
        let b = identity_topics(&id_b, now);
        assert_ne!(a[0].id(), b[0].id());
        // The publisher and a looker-up with the same ID compute the SAME topic.
        assert_eq!(identity_topics(&id_a, now)[0].id(), a[0].id());
    }

    #[test]
    fn identity_topics_with_keys_are_deterministic_and_nonzero() {
        let now = 1_700_000_000_000;
        let id = [0xAAu8; 20];
        let keyed = identity_topics_with_keys(&id, now);
        // Same shape as identity_topics (current + previous bucket), topics agree.
        let plain = identity_topics(&id, now);
        assert_eq!(keyed.len(), 2);
        assert_eq!(keyed[0].0.id(), plain[0].id());
        // Each DHT key is a real, non-zero 32-byte seed (the helper wire protocol rejects
        // an empty token — see identity_dht_key_for_bucket) and the two buckets differ.
        assert_ne!(keyed[0].1, [0u8; 32]);
        assert_ne!(keyed[0].1, keyed[1].1);
        // Publisher and looker-up with the same ID derive the SAME key (BT-DHT slots align).
        assert_eq!(identity_topics_with_keys(&id, now)[0].1, keyed[0].1);
        // A different ID yields a different key (find-by-ID isolation).
        assert_ne!(identity_topics_with_keys(&[0xBBu8; 20], now)[0].1, keyed[0].1);
    }

    #[test]
    fn identity_beacon_wire_roundtrip_and_signing_bytes_exclude_sig() {
        let now = 1_700_000_000_000;
        let beacon = sample_identity_beacon(now);
        let wire = beacon.to_wire();
        assert_eq!(IdentityBeacon::from_wire(&wire).as_ref(), Some(&beacon));
        // Within what ntfy reads (4096 hex characters), with room for a relay bundle.
        assert!(wire.len() < 1_000, "identity beacon is {} bytes", wire.len());
        assert!(IdentityBeacon::from_wire(&wire[..wire.len() - 1]).is_none());
        assert!(beacon.is_fresh(now));
        assert!(!beacon.is_fresh(now + 601 * 1000));

        // Signing bytes are stable regardless of the signature field.
        let bytes_a = identity_beacon_signing_bytes(&beacon);
        let mut signed = beacon.clone();
        signed.signature = vec![9u8; 64];
        let bytes_b = identity_beacon_signing_bytes(&signed);
        assert_eq!(bytes_a, bytes_b);
        // But a changed payload field changes them (binds the content).
        let mut tampered = beacon.clone();
        tampered.seq = 8;
        assert_ne!(identity_beacon_signing_bytes(&tampered), bytes_a);
    }
}
