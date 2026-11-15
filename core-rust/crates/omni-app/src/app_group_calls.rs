// Group-call orchestration (G8 control-plane + G9 peer-SFU media routing).
// Ties omni-protocol/group_call (topology + forwarder + failover + media
// envelope) and omni-crypto (per-sender SFrame media keys) to the app: start /
// join / leave / end a group call over the group feed (Control inners), maintain
// call state, and route media datagrams — leaves send one uplink to the
// forwarder, the forwarder re-addresses sealed frames without decrypting, the
// receiver opens with the sender's media key.
// Hearing about a call never joins it: Invite rings / lists the call, join is explicit.
// См. docs/decisions/group-crypto-mls.md («Групповой звонок: вход по согласию»).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;

use omni_protocol::{
    active_speaker_should_forward, call_tag_from_id, decide_call_topology, feedback_next_hop,
    activity_from_level, feedback_reporter, pack_media_meta, parse_group_feedback, parse_group_media,
    prepare_group_feedback_forward, prepare_group_media_forward,
    rtcp_repair_requests_only, sender_tag_from_pub, tree_send_targets, unpack_media_meta,
    wrap_group_feedback, wrap_group_media, CallParticipantV1, CallTopology, CallTree,
    GroupCallControlKindV1, GroupFeedback, GroupCallControlV1, GroupUpstreamKind, ScreenControlGrantV1,
    ScreenControlOutbox, ScreenControlReceiver, ScreenInputEvent, GROUP_CALL_FANOUT, GROUP_CALL_MAX,
    GROUP_MEDIA_DEFAULT_HOP_LIMIT, GROUP_MEDIA_TAG_LEN, GROUP_WIRE_VERSION_V2,
};
use omni_storage::Storage;
use omni_types::{OmniError, OmniResult};

use crate::app_screen_control::{decode_identity, open_batch, sign_batch, ScreenControlDelivery};
use crate::{current_unix_ms, OmniApp};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Uplink tier we self-report when we can't measure (advisory only).
const DEFAULT_UPLINK_TIER: u8 = 2;
/// How long (ms) a sender's last `media_meta` activity level keeps them "active" for cascade
/// active-speaker gating (C1). A sender silent longer than this drops out of the ranked set, so the
/// forwarder stops relaying their (now-quiet) stream. A few frame-intervals of audio.
const ACTIVE_WINDOW_MS: i64 = 600;
/// The sender tag the shell's media router reads as "feedback for OUR sender": a receiver's RTCP
/// about our stream is delivered to the group dialog under it (`omni_webrtc_media.cpp`, group branch
/// of `DeliverUnsafe`). No member can own it — real tags are SHA-256 prefixes, and this one is
/// reserved by both sides.
pub(crate) const GROUP_FEEDBACK_SHELL_TAG: [u8; GROUP_MEDIA_TAG_LEN] = [0xFF; GROUP_MEDIA_TAG_LEN];
/// Nonce domain of sealed feedback (the media seal uses "OMNA"); the counter is the call's one.
const GROUP_FEEDBACK_SEAL_TAG: u32 = 0x4F4D_4642; // "OMFB"
/// How long a member's pre-rekey media key is still accepted after a rekey.
const REKEY_GRACE_MS: i64 = 3_000;
/// After a member's removal, how long each next participant (by member_pub) waits for those before
/// it to rekey the call before doing it itself.
const REKEY_TURN_MS: i64 = 6_000;
/// How long after we started or joined a call another one, of an earlier id, may still take us over
/// to it ([`handle_group_call_control`]): two members who pressed «call» at once.
const CALL_FOLD_WINDOW_MS: i64 = 30_000;

/// How long after a rekey another one counts as its rival rather than a new change ([`takes_rekey`]).
const REKEY_ROUND_MS: i64 = 60_000;

/// Whether a `Rekey` carrying `offered`, said at `offered_at_ms`, replaces `current`, said at
/// `said_at_ms` (both by their senders' clocks — the time the `Rekey` itself carries). The first
/// after a removal does. One said a round or more after ours does: a later change. One said within
/// [`REKEY_ROUND_MS`] of ours is its rival — two participants each took the turn of one who was
/// silent — and the smaller secret wins, whichever order they came in, so everyone ends on the
/// same key. One said a round or more BEFORE ours is an old one that came late, and is not taken:
/// by the time it came, it used to count as «a later change» and took the call back to a secret
/// the others had left (blind audit 2026-11-11, pass 14). A time of 0 or less is no time: such a
/// `Rekey` is not taken. The differences saturate — `i64::MIN` from a member overflowed them, and a
/// wrapped difference took an old `Rekey` as «a later change» (blind audit 2026-11-12, pass 15).
fn takes_rekey(current: &[u8], said_at_ms: i64, offered: &[u8], offered_at_ms: i64) -> bool {
    if offered_at_ms <= 0 {
        return false;
    }
    if said_at_ms == 0 || offered_at_ms.saturating_sub(said_at_ms) >= REKEY_ROUND_MS {
        return true;
    }
    said_at_ms.saturating_sub(offered_at_ms) < REKEY_ROUND_MS && offered < current
}

/// How far ahead of our clock a `Rekey`'s own time may be: past it, the `Rekey` is not taken (it
/// would stand over every later one).
const REKEY_CLOCK_AHEAD_MS: i64 = 5 * 60 * 1000;

/// A member as tracked in a live call: identity + routing + media key material.
#[derive(Debug, Clone)]
struct CallMember {
    member_pub: String,
    node: [u8; 20],
    sender_tag: [u8; 8],
    /// Per-sender SFrame media key (HKDF of the call secret). Cached. Every member can derive it, so
    /// it does not tell who sealed a frame: [`Self::verifier`] does.
    media_key: [u8; 32],
    /// The member's public signing key: a frame counts as its only with its signature (security
    /// review 2026-11-09, F1). `None` — a member_pub that is no key: none of its frames count.
    verifier: Option<omni_crypto::GroupCallMediaVerifier>,
    /// The key before the last rekey and until when it is still accepted: frames sealed just before
    /// a participant learned the new secret keep playing for a moment. Bounded on purpose — the
    /// removed member knows the old secret and could forge frames under it.
    previous_media_key: Option<([u8; 32], i64)>,
    /// Self-reported mic-muted state (from this member's `MediaState` control).
    /// Defaults `false` — mic is live unless the member announces a mute.
    muted: bool,
    /// Self-reported camera-off state. Defaults `true` — the call is audio-first
    /// (camera is opt-in, same rule as 1:1), so a member has no live video until
    /// they enable the camera and announce `camera_off = false`.
    camera_off: bool,
    /// Self-reported screen-sharing state: the member is broadcasting their screen on the video track
    /// INSTEAD of the camera. Defaults `false`. Lets the shell label + auto-pin the sharer's tile.
    screen_sharing: bool,
    /// Who may control this member's shared screen (their last `MediaState`). `None` — nobody.
    screen_control: Option<ScreenControlGrantV1>,
    /// The member's video passport, from the call's own media (ours: what our media published).
    /// `None` until their media is up. See [`GROUP_PASSPORT_MAGIC`].
    video_capabilities: Option<omni_types::RealtimeMediaCapabilitiesV1>,
}

#[derive(Debug, Clone)]
struct GroupCallRuntime {
    call_id: String,
    /// The call secret (per-sender media keys derive from it) — kept to key members who join later.
    call_secret: Vec<u8>,
    group_id: String,
    dialog_key: String,
    my_pub: String,
    /// Our signing key: every frame we send is signed with it ([`omni_crypto::sign_group_call_media`]).
    signer: Option<omni_crypto::GroupCallMediaSigner>,
    topology: CallTopology,
    /// Cascade forwarder tree, `Some` in `Cascade` topology (N ≥ 4), `None` in full
    /// mesh (N ≤ 3, no forwarding). Deterministic function of the live member set —
    /// rebuilt identically by every participant on join/leave (the failover contract).
    tree: Option<CallTree>,
    /// Everyone in the call (incl. us), keyed by member_pub — participants, not all group members.
    members: HashMap<String, CallMember>,
    /// Counter of OUR sealed frames (the AEAD nonce): a random epoch in the high 32 bits, a count
    /// in the low ones ([`fresh_media_seq_epoch`]).
    next_media_seq: u64,
    /// Per co-member: the counters of its frames already opened, so a frame opens at most once (an
    /// on-path relay replaying recorded media). Kept when the member leaves the call — rejoining it
    /// keys its frames the same.
    media_replay: HashMap<String, MemberMediaReplay>,
    /// When our video passport last went out on the media plane ([`take_due_passport`]); 0 = owed.
    passport_sent_at_ms: i64,
    /// When the call's secret was last changed by a `Rekey` (ours or one received); 0 — a rekey is
    /// awaited (a member was just removed) or there was none. See [`takes_rekey`].
    rekeyed_at_ms: i64,
    /// A member was removed and the one whose turn it was to rekey has not: when it becomes ours
    /// ([`take_due_group_call_rekeys`]).
    rekey_owed_at_ms: Option<i64>,
    /// A `Rekey` of ours that changed the secret here and was not said to the group: said again as
    /// it is, not made anew ([`owe_group_call_rekey`]).
    rekey_unsaid: Option<GroupCallControlV1>,
    started_at_unix_ms: i64,
    /// Cascade active-speaker gating (C1): per-sender last-seen audio activity `(level, at_ms)`, read
    /// from the cleartext `media_meta` of inbound frames. A forwarder ranks by this to relay only the
    /// top-K loudest streams. Empty until real levels flow → gating fails open (un-gated cascade).
    activity: HashMap<String, (u8, i64)>,
    /// Remote screen control (docs/decisions/remote-screen-control.md): as sharer, a receiver per
    /// viewer; as viewer, an outbox per sharer. Both keyed by member_pub, both reset with the grant.
    screen_rx: HashMap<String, ScreenControlReceiver>,
    screen_out: HashMap<String, ScreenControlOutbox>,
    /// A `Rekey` from a member not in the call here yet (its Join still on the way): taken once that
    /// member is. Dropped, the member who rekeyed after a removal was heard joining later and we stayed
    /// on the old key (blind audit 2026-11-13, pass 22). The latest one only.
    held_rekey: Option<HeldRekey>,
}

/// A `Rekey` whose sender is not known to be in the call yet: (sender, secret, said at).
type HeldRekey = (String, Vec<u8>, i64);

impl GroupCallRuntime {
    fn participants(&self) -> Vec<CallParticipantV1> {
        self.members
            .values()
            .map(|m| CallParticipantV1 {
                member_pub: m.member_pub.clone(),
                is_live: true,
                uplink_tier: DEFAULT_UPLINK_TIER,
                can_forward: true,
            })
            .collect()
    }

    fn member_by_sender_tag(&self, tag: &[u8; 8]) -> Option<&CallMember> {
        self.members.values().find(|m| &m.sender_tag == tag)
    }

    /// Our own sender tag (the tag a co-member routes our media on; also the `relay_tag`
    /// we stamp when we forward). Defaults to zeros only if we're somehow absent.
    fn my_sender_tag(&self) -> [u8; GROUP_MEDIA_TAG_LEN] {
        self.members.get(&self.my_pub).map(|m| m.sender_tag).unwrap_or_default()
    }

    /// Recompute topology + cascade tree from the current live members. Every participant runs the
    /// same deterministic build, so all land on the same tree without a coordinator — this IS the
    /// join/leave/failover step (no forwarder-election signal on the wire).
    fn rebuild_topology(&mut self) {
        let participants = self.participants();
        self.topology = decide_call_topology(participants.len().max(1));
        self.tree = match self.topology {
            CallTopology::Cascade => Some(CallTree::build(&participants, GROUP_CALL_FANOUT)),
            CallTopology::FullMesh => None,
        };
    }

    /// Whom we send a media datagram to. `arrival == None` → our own media (all targets); `Some(pub)`
    /// → forwarding a frame that reached us from tree-neighbour `pub` (flood to the others).
    ///   - full mesh: every other member (own media only; full mesh never forwards).
    ///   - cascade: `tree_send_targets` — tree-neighbours minus the arrival.
    fn media_targets(&self, arrival: Option<&str>) -> Vec<String> {
        match (self.topology, &self.tree) {
            (CallTopology::FullMesh, _) => self
                .members
                .keys()
                .filter(|k| k.as_str() != self.my_pub)
                .cloned()
                .collect(),
            (CallTopology::Cascade, Some(tree)) => tree_send_targets(tree, &self.my_pub, arrival),
            (CallTopology::Cascade, None) => Vec::new(),
        }
    }

    /// Resolve member_pubs to their mesh node addresses (skipping any with no known node hint).
    fn nodes_for(&self, pubs: &[String]) -> Vec<[u8; 20]> {
        pubs.iter()
            .filter_map(|p| self.members.get(p).map(|m| m.node))
            .collect()
    }

    /// True if we relay for others in this call (internal/root of the cascade tree).
    fn is_forwarder(&self) -> bool {
        matches!(&self.tree, Some(tree) if tree.is_forwarder(&self.my_pub))
    }

    /// Our position in the cascade tree, for the shell snapshot: `mesh` (full mesh, no tree),
    /// `root` (top of the tree), `internal` (forwards but has a parent), or `leaf` (uplink only).
    fn tree_role(&self) -> &'static str {
        match (self.topology, &self.tree) {
            (CallTopology::FullMesh, _) | (CallTopology::Cascade, None) => "mesh",
            (CallTopology::Cascade, Some(tree)) => {
                if tree.root() == Some(self.my_pub.as_str()) {
                    "root"
                } else if tree.is_forwarder(&self.my_pub) {
                    "internal"
                } else {
                    "leaf"
                }
            }
        }
    }

    /// Record a sender's current audio activity (from the cleartext `media_meta` of an inbound frame)
    /// for cascade active-speaker gating (C1). `level == 0` clears any stale entry (silence).
    fn note_activity(&mut self, sender_pub: &str, level: u8, now_ms: i64) {
        if level == 0 {
            self.activity.remove(sender_pub);
        } else {
            self.activity.insert(sender_pub.to_string(), (level, now_ms));
        }
    }

    /// The call's currently-active senders (fresh, non-zero activity) ordered loudest-first, with a
    /// stable `member_pub` tie-break so every forwarder ranks identically. Stale entries (older than
    /// [`ACTIVE_WINDOW_MS`]) are treated as silent and excluded.
    fn ranked_active(&self, now_ms: i64) -> Vec<String> {
        let mut active: Vec<(&String, u8)> = self
            .activity
            .iter()
            .filter(|(_, (level, at))| *level > 0 && now_ms.saturating_sub(*at) <= ACTIVE_WINDOW_MS)
            .map(|(pk, (level, _))| (pk, *level))
            .collect();
        active.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        active.into_iter().map(|(pk, _)| pk.clone()).collect()
    }

    /// Whether we (a cascade forwarder) should relay a frame from `sender_pub` given its media kind,
    /// under active-speaker gating. Fails open when there is no real activity signal (see
    /// [`active_speaker_should_forward`]).
    fn forward_gate(&self, is_video: bool, sender_pub: &str, now_ms: i64) -> bool {
        active_speaker_should_forward(is_video, sender_pub, &self.ranked_active(now_ms))
    }

    /// Our own activity for the `media_meta` of our frames: the level the shell last reported
    /// ([`OmniApp::set_group_call_audio_level`]), silent once it is older than [`ACTIVE_WINDOW_MS`].
    fn own_activity(&self, now_ms: i64) -> u8 {
        self.activity
            .get(&self.my_pub)
            .filter(|(_, at)| now_ms.saturating_sub(*at) <= ACTIVE_WINDOW_MS)
            .map_or(0, |(level, _)| *level)
    }

    /// Whether `member_pub` fits in the call: someone already in it always does, a newcomer only
    /// while the call is under [`GROUP_CALL_MAX`]. Two newcomers taking the last seat at once may be
    /// let in by different participants; the cap is a sanity bound, not an agreed admission.
    fn has_room_for(&self, member_pub: &str) -> bool {
        self.members.contains_key(member_pub) || self.members.len() < GROUP_CALL_MAX
    }
}

/// One participant in the shell-facing group-call snapshot. `sender_tag_hex` is the
/// 8-byte demux key (lowercase hex) the shell uses to route each remote sender's media
/// to its own video tile — it matches the tag prefixed onto inbound group RTP by
/// `handle_incoming_group_media`. `member_pub` is the stable identity the shell maps to a
/// display name (group member / contact); `is_self` marks our own tile (rendered from the
/// local capture preview, not the network).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupCallMemberSnapshot {
    pub member_pub: String,
    pub sender_tag_hex: String,
    pub is_self: bool,
    /// Last-known mic-muted state for this member (self-reported via `MediaState`).
    /// The shell draws a mic-off badge on the member's tile. Self stays authoritative
    /// from the local toggle; remote reflects the last broadcast we ingested.
    pub is_muted: bool,
    /// Last-known camera-off state (audio-first default `true` until they enable it).
    pub is_camera_off: bool,
    /// Last-known screen-sharing state: this member is broadcasting their screen instead of camera.
    /// The shell labels ("демонстрирует экран") + auto-pins their tile (Zoom/Telemost UX).
    pub is_screen_sharing: bool,
    /// This member shares their screen and lets US control it.
    #[serde(default)]
    pub lets_me_control: bool,
}

/// Shell-facing snapshot of the current group call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupCallSnapshot {
    pub call_id: String,
    pub group_id: String,
    pub dialog_key: String,
    pub participant_count: usize,
    /// "full_mesh" | "cascade"
    pub topology: String,
    /// True if we relay for others (a cascade forwarder — root or internal node).
    pub is_forwarder: bool,
    /// Our place in the cascade tree: "mesh" | "root" | "internal" | "leaf". The shell
    /// labels our tile / call banner with this (honest indicator of our forwarding role).
    pub tree_role: String,
    pub started_at_unix_ms: i64,
    /// Everyone in the call (incl. us), so the shell can build one video tile per member
    /// and label it. Order is unspecified (shell sorts/pins as it likes).
    pub members: Vec<GroupCallMemberSnapshot>,
    /// Who may control the screen WE share. `None` — nobody.
    #[serde(default)]
    pub my_screen_control: Option<ScreenControlGrantV1>,
}

/// A call going on in a group that we are NOT in: someone started it (`Invite`) and members joined /
/// left since. Hearing about a call never puts us in it — joining is the user's choice. A fresh
/// `Invite` rings for [`RING_WINDOW_MS`]; after that (or once dismissed) the shell shows it as
/// "a call is on" with a join button.
#[derive(Debug, Clone)]
struct KnownCall {
    call_id: String,
    call_secret: Vec<u8>,
    started_by: String,
    started_at_unix_ms: i64,
    /// Members heard in the call (Invite / Join / MediaState senders minus Leave senders).
    participants: BTreeSet<String>,
    /// Learned from a fresh `Invite` (not a late `Join`) — only that rings.
    rings: bool,
    dismissed: bool,
    /// As [`GroupCallRuntime::rekeyed_at_ms`].
    rekeyed_at_ms: i64,
    /// As [`GroupCallRuntime::held_rekey`].
    held_rekey: Option<HeldRekey>,
}

/// How long a group-call `Invite` rings before it becomes a quiet "call is on" indicator.
const RING_WINDOW_MS: i64 = 45_000;

/// Shell-facing: a group call we're in or could join (the chat-list indicator + the ring prompt).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupCallNotice {
    pub group_id: String,
    pub group_name: String,
    pub call_id: String,
    pub started_by_name: String,
    pub started_at_unix_ms: i64,
    pub participant_count: usize,
    /// A fresh invitation we haven't answered — the shell prompts "Присоединиться".
    pub ringing: bool,
    /// We are in this call.
    pub joined: bool,
}

static GROUP_CALL_REGISTRY: OnceLock<Mutex<HashMap<String, HashMap<String, GroupCallRuntime>>>> =
    OnceLock::new();
static KNOWN_GROUP_CALLS: OnceLock<Mutex<HashMap<String, HashMap<String, KnownCall>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, HashMap<String, GroupCallRuntime>>> {
    GROUP_CALL_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn known_calls() -> &'static Mutex<HashMap<String, HashMap<String, KnownCall>>> {
    KNOWN_GROUP_CALLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Our part in the group's call ends here — we left, were removed, or the group is gone (A09, A10,
/// audit 2026-11-09 #6): its runtime and media keys go, and it is no longer listed to join. Nothing
/// is said to the group: we can no longer speak in it.
pub(crate) fn end_group_call_here(db_path: &str, group_id: &str) {
    let _ = take_runtime(db_path, group_id);
    if let Ok(mut known) = known_calls().lock() {
        if let Some(calls) = known.get_mut(db_path) {
            calls.remove(group_id);
        }
    }
}

/// The group whose call this profile is in, if any.
pub(crate) fn group_call_in_progress(db_path: &str) -> Option<String> {
    registry().lock().ok()?.get(db_path)?.keys().next().cloned()
}

pub(crate) fn clear_group_calls_for_profile(db_path: &str) {
    if let Ok(mut reg) = registry().lock() {
        reg.remove(db_path);
    }
    if let Ok(mut known) = known_calls().lock() {
        known.remove(db_path);
    }
    // The media counters too: kept, the next profile of this database reported the last one's
    // peers and failures as its own (blind audit 2026-11-12, pass 17).
    if let Ok(mut stats) = GROUP_MEDIA_STATS.get_or_init(Default::default).lock() {
        stats.remove(db_path);
    }
}

/// Media counters of a profile's group calls, flushed to the trace (`group/media-stats`) at most
/// every [`MEDIA_STATS_EVERY_MS`]. Without them a call that carries nothing looks like a healthy
/// one from the outside: every send and drop on these paths was silent (live pass 2026-10-04).
#[derive(Default)]
struct GroupMediaStats {
    sent: u64,
    /// Sends the mesh refused, by node (hex prefix) — `NoLink` means no direct link to that member.
    send_failed: HashMap<String, (u64, String)>,
    /// Opened frames, by sender tag (hex prefix).
    received: HashMap<String, u64>,
    forwarded: u64,
    feedback_sent: u64,
    feedback_received: u64,
    /// Other members' feedback relayed on toward its sender (cascade, we are on the media path).
    feedback_relayed: u64,
    /// Screen-control batches addressed to us and refused: not granted, bad signature, or replayed.
    screen_control_refused: u64,
    no_call: u64,
    unknown_sender: u64,
    open_failed: u64,
    last_flush_ms: i64,
}

const MEDIA_STATS_EVERY_MS: i64 = 5_000;

/// Group frames (media and feedback) that did not open with the sender's key or were replays; the
/// group half of `call_datagrams_unopened_total`. Cumulative, unlike `GroupMediaStats`.
static GROUP_MEDIA_UNOPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn group_media_unopened_total() -> u64 {
    GROUP_MEDIA_UNOPENED.load(std::sync::atomic::Ordering::Relaxed)
}

fn note_unopened(db_path: &str) {
    GROUP_MEDIA_UNOPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    note_media(db_path, |s| s.open_failed += 1);
}
static GROUP_MEDIA_STATS: OnceLock<Mutex<HashMap<String, GroupMediaStats>>> = OnceLock::new();

fn note_media(db_path: &str, update: impl FnOnce(&mut GroupMediaStats)) {
    let Ok(mut all) = GROUP_MEDIA_STATS.get_or_init(Default::default).lock() else {
        return;
    };
    let stats = all.entry(db_path.to_string()).or_default();
    update(stats);
    let now = current_unix_ms();
    if now - stats.last_flush_ms < MEDIA_STATS_EVERY_MS {
        return;
    }
    let mut failed: Vec<String> =
        stats.send_failed.iter().map(|(node, (n, why))| format!("{node}:{n}:{why}")).collect();
    failed.sort();
    let mut received: Vec<String> = stats.received.iter().map(|(tag, n)| format!("{tag}:{n}")).collect();
    received.sort();
    crate::omni_app_trace_log(
        "group/media-stats",
        &format!(
            "sent={} send_failed=[{}] received=[{}] forwarded={} feedback={}out/{}in/{}relayed screen_refused={} no_call={} unknown_sender={} open_failed={}",
            stats.sent,
            failed.join(","),
            received.join(","),
            stats.forwarded,
            stats.feedback_sent,
            stats.feedback_received,
            stats.feedback_relayed,
            stats.screen_control_refused,
            stats.no_call,
            stats.unknown_sender,
            stats.open_failed
        ),
    );
    *stats = GroupMediaStats { last_flush_ms: now, ..Default::default() };
}

fn note_send(db_path: &str, node: &[u8; 20], result: Result<(), omni_mesh_runtime::MeshRuntimeError>) {
    note_media(db_path, |s| match result {
        Ok(()) => s.sent += 1,
        Err(error) => {
            let entry = s.send_failed.entry(hex::encode(&node[..4])).or_insert((0, String::new()));
            entry.0 += 1;
            entry.1 = match error {
                omni_mesh_runtime::MeshRuntimeError::NoLink => "no_link".to_string(),
                other => other.to_string().chars().take(40).collect(),
            };
        }
    });
}

fn take_known(db_path: &str, group_id: &str) -> Option<KnownCall> {
    known_calls().lock().ok()?.get_mut(db_path)?.remove(group_id)
}

/// Whether the call going on in the group already has [`GROUP_CALL_MAX`] people besides us. Checked
/// before joining, so a full call stays listed instead of being taken out of the known calls.
fn known_call_is_full(db_path: &str, group_id: &str, my_pub: &str) -> bool {
    let Ok(known) = known_calls().lock() else { return false };
    known
        .get(db_path)
        .and_then(|calls| calls.get(group_id))
        .is_some_and(|call| call.participants.iter().filter(|p| p.as_str() != my_pub).count() >= GROUP_CALL_MAX)
}

/// Call ids sort by start time (zero-padded ms), then by the starter's tag — so when two members
/// start a call at once, everyone agrees the earlier one is THE call and the later one folds into it.
/// When a call was started, as its id says ([`new_call_id`]).
fn call_started_ms(call_id: &str) -> Option<i64> {
    call_id.strip_prefix("gcall-")?.split('-').next()?.parse().ok()
}

fn new_call_id(now_ms: i64, my_pub: &str) -> String {
    format!("gcall-{now_ms:013}-{}", hex::encode(sender_tag_from_pub(my_pub)))
}

impl OmniApp {
    /// Start a call in a group — or, if one is already going on there, join it (one call per group:
    /// pressing "call" while a call is on means "let me in", never a second parallel call).
    pub fn start_group_call(&self, group_id: &str) -> OmniResult<GroupCallSnapshot> {
        let _ = self.ensure_runtime_profile_active("start_group_call")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if let Some(existing) = self.group_call_snapshot(group_id)? {
            return Ok(existing);
        }
        if known_calls().lock().ok().is_some_and(|k| k.get(&db_path).is_some_and(|m| m.contains_key(group_id))) {
            return self.join_group_call(group_id);
        }
        let group = self.active_call_group(group_id)?;
        let now = current_unix_ms();
        let call_id = new_call_id(now, &group.my_member_pub);
        let call_secret = omni_crypto::generate_group_key().to_vec();
        let runtime = build_runtime(
            &self.storage,
            &group,
            &call_id,
            &call_secret,
            &BTreeSet::new(),
            self.own_mesh_node(),
        )?;
        let snapshot = snapshot_of(&runtime);
        self.store_runtime_if_no_other_call(&db_path, runtime)?;

        // The Invite carries the call secret inside the group's MLS application message, so only
        // members of the current epoch can read it. Not published, the call is not on: its runtime
        // goes, or it held the profile's one call slot after an error said it did not start (blind
        // audit 2026-11-11, J7).
        if let Err(error) =
            self.publish_group_call_control(group_id, &call_control(&call_id, GroupCallControlKindV1::Invite, Some(&call_secret)))
        {
            let _ = take_runtime(&db_path, group_id);
            return Err(error);
        }
        Ok(snapshot)
    }

    /// Join a call going on in the group (answering the ring, or the "join" button later).
    pub fn join_group_call(&self, group_id: &str) -> OmniResult<GroupCallSnapshot> {
        let _ = self.ensure_runtime_profile_active("join_group_call")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if let Some(existing) = self.group_call_snapshot(group_id)? {
            return Ok(existing);
        }
        let group = self.active_call_group(group_id)?;
        if known_call_is_full(&db_path, group_id, &group.my_member_pub) {
            return Err(OmniError::InvalidInput(format!("the call is full ({GROUP_CALL_MAX} people)")));
        }
        let known = take_known(&db_path, group_id)
            .ok_or_else(|| OmniError::InvalidInput("no call is going on in this group".into()))?;
        let runtime = build_runtime(
            &self.storage,
            &group,
            &known.call_id,
            &known.call_secret,
            &known.participants,
            self.own_mesh_node(),
        )?;
        let snapshot = snapshot_of(&runtime);
        if let Err(error) = self.store_runtime_if_no_other_call(&db_path, runtime) {
            // Still going on there: it stays listed to join later.
            if let Ok(mut calls) = known_calls().lock() {
                calls.entry(db_path.clone()).or_default().insert(group_id.to_string(), known);
            }
            return Err(error);
        }
        // Our Join carries the secret too, so a member who missed the Invite can still join later. Not
        // published, we are not in: the runtime goes and the call stays listed to join (J7).
        if let Err(error) = self.publish_group_call_control(
            group_id,
            &call_control(&known.call_id, GroupCallControlKindV1::Join, Some(&known.call_secret)),
        ) {
            let _ = take_runtime(&db_path, group_id);
            if let Ok(mut calls) = known_calls().lock() {
                calls.entry(db_path.clone()).or_default().insert(group_id.to_string(), known);
            }
            return Err(error);
        }
        Ok(snapshot)
    }

    /// Stop ringing for a call (the user declined) — it stays listed as "a call is on".
    pub fn dismiss_group_call(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("dismiss_group_call")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if let Ok(mut known) = known_calls().lock() {
            if let Some(call) = known.get_mut(&db_path).and_then(|m| m.get_mut(group_id)) {
                call.dismissed = true;
            }
        }
        Ok(())
    }

    /// Leave the group call. If others are still in it, it stays listed (quietly) so we can rejoin.
    pub fn leave_group_call(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("leave_group_call")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let Some(runtime) = take_runtime(&db_path, group_id) else {
            return Ok(());
        };
        let others: BTreeSet<String> = runtime.members.keys().filter(|p| **p != runtime.my_pub).cloned().collect();
        if !others.is_empty() {
            if let Ok(mut known) = known_calls().lock() {
                known.entry(db_path.clone()).or_default().insert(
                    group_id.to_string(),
                    KnownCall {
                        call_id: runtime.call_id.clone(),
                        call_secret: runtime.call_secret.clone(),
                        started_by: String::new(),
                        started_at_unix_ms: runtime.started_at_unix_ms,
                        participants: others,
                        rings: false,
                        dismissed: true,
                        rekeyed_at_ms: 0,
                        held_rekey: None,
                    },
                );
            }
        }
        // Best-effort signal; state is already cleared locally.
        let _ = self.publish_group_call_control(group_id, &call_control(&runtime.call_id, GroupCallControlKindV1::Leave, None));
        Ok(())
    }

    /// Announce our own media state (mic muted / camera off / screen sharing) to the group call so
    /// remote members can badge our tile. We send the full current state on every toggle and update
    /// our own entry locally. No-op if there's no active call for this group.
    /// (Speaking is deliberately NOT announced here — see `GroupCallControlKindV1`.)
    /// Our microphone's level (the capture's smoothed RMS, 0..1; 0 while muted) for active-speaker
    /// gating (C1c): the shell reads it on its call tick. It goes into the `media_meta` of our frames
    /// and ranks us in our own forwarding like any sender. Memory only — no storage, no network.
    pub fn set_group_call_audio_level(&self, group_id: &str, level: f32) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let mut reg = registry()
            .lock()
            .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
        if let Some(runtime) = reg.get_mut(&db_path).and_then(|m| m.get_mut(group_id)) {
            let my_pub = runtime.my_pub.clone();
            runtime.note_activity(&my_pub, activity_from_level(level), current_unix_ms());
        }
        Ok(())
    }

    pub fn set_group_call_media_state(
        &self,
        group_id: &str,
        muted: bool,
        camera_off: bool,
        screen_sharing: bool,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_group_call_media_state")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let control = {
            let mut reg = registry()
                .lock()
                .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
            let Some(runtime) = reg.get_mut(&db_path).and_then(|m| m.get_mut(group_id)) else {
                return Ok(()); // no active call for this group — nothing to announce
            };
            let my_pub = runtime.my_pub.clone();
            if let Some(me) = runtime.members.get_mut(&my_pub) {
                me.muted = muted;
                me.camera_off = camera_off;
                me.screen_sharing = screen_sharing;
                // Control is of a shared screen: no screen, no control.
                if !screen_sharing {
                    me.screen_control = None;
                }
            }
            let released: Vec<String> =
                if screen_sharing { Vec::new() } else { runtime.screen_rx.drain().map(|(viewer, _)| viewer).collect() };
            (own_media_state(runtime), released, runtime.dialog_key.clone())
        };
        let (control, released, dialog_key) = control;
        for viewer in released {
            self.screen_control_sink.release(&dialog_key, &viewer);
        }
        self.publish_group_call_control(group_id, &control)
    }

    /// Who may control the screen we share in this group call (`None` — nobody). Allowed only while we
    /// share; the shell offers it only for a whole monitor. Viewers who lose control are released.
    pub fn set_group_call_screen_control(&self, group_id: &str, grant: Option<ScreenControlGrantV1>) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_group_call_screen_control")?;
        crate::app_screen_control::local_signing_identity(&self.storage)?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let (control, released, dialog_key) = {
            let mut reg = registry()
                .lock()
                .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
            let runtime = reg
                .get_mut(&db_path)
                .and_then(|m| m.get_mut(group_id))
                .ok_or_else(|| OmniError::InvalidInput("there is no active call in this group".into()))?;
            let my_pub = runtime.my_pub.clone();
            let me = runtime
                .members
                .get_mut(&my_pub)
                .ok_or_else(|| OmniError::Internal("we are missing from our own call".into()))?;
            if grant.is_some() && !me.screen_sharing {
                return Err(OmniError::InvalidInput("screen control needs a shared screen".into()));
            }
            let grant = grant.filter(|g| g.everyone || !g.members.is_empty());
            me.screen_control = grant.clone();
            let still = |viewer: &str| grant.as_ref().is_some_and(|g| g.allows(viewer));
            let released: Vec<String> = runtime.screen_rx.keys().filter(|v| !still(v)).cloned().collect();
            for viewer in &released {
                runtime.screen_rx.remove(viewer);
            }
            (own_media_state(runtime), released, runtime.dialog_key.clone())
        };
        for viewer in released {
            self.screen_control_sink.release(&dialog_key, &viewer);
        }
        self.publish_group_call_control(group_id, &control)
    }

    /// Our input to `sharer`'s screen: signed, and sent toward the sharer along the media path
    /// backwards — the same way as feedback, because we need not have a link to the sharer.
    pub(crate) fn send_group_screen_control(
        &self,
        group_id: &str,
        sharer: &str,
        pointer: Option<(u16, u16)>,
        events: &[ScreenInputEvent],
    ) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let (seed, _) = crate::app_screen_control::local_signing_identity(&self.storage)?;
        let mesh = self.mesh_runtime().ok_or_else(|| OmniError::Transport("mesh is not running".into()))?;
        let (hop, wire) = {
            let mut reg = registry()
                .lock()
                .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
            let runtime = reg
                .get_mut(&db_path)
                .and_then(|m| m.get_mut(group_id))
                .ok_or_else(|| OmniError::InvalidInput("there is no active call in this group".into()))?;
            let (Some(target), Some(me)) = (runtime.members.get(sharer), runtime.members.get(&runtime.my_pub)) else {
                return Err(OmniError::InvalidInput("the sharer is not in the call".into()));
            };
            if !(target.screen_sharing && target.screen_control.as_ref().is_some_and(|g| g.allows(&runtime.my_pub))) {
                return Err(OmniError::InvalidInput("the sharer has not granted screen control".into()));
            }
            let sharer_pub = decode_identity(sharer).ok_or_else(|| OmniError::InvalidInput("bad sharer key".into()))?;
            let (target_tag, my_tag) = (target.sender_tag, me.sender_tag);
            let hop = feedback_next_hop(runtime.tree.as_ref(), &runtime.my_pub, sharer)
                .and_then(|hop| runtime.nodes_for(&[hop]).pop())
                .ok_or_else(|| OmniError::Transport("no way toward the sharer".into()))?;
            let call_id = runtime.call_id.clone();
            let batch = runtime.screen_out.entry(sharer.to_string()).or_default().batch(current_unix_ms(), pointer, events);
            let signed = sign_batch(&seed, &call_id, &sharer_pub, &batch)?;
            (
                hop,
                wrap_group_feedback(GroupUpstreamKind::ScreenControl, &call_tag_from_id(&call_id), &target_tag, &my_tag, &signed),
            )
        };
        mesh.send_media(omni_mesh_runtime::MeshNodeAddress::from_bytes(hop), &wire)
            .map_err(|error| OmniError::Transport(format!("screen control send failed: {error}")))
    }

    pub fn group_call_snapshot(&self, group_id: &str) -> OmniResult<Option<GroupCallSnapshot>> {
        let _ = self.ensure_runtime_profile_active("group_call_snapshot")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        Ok(registry()
            .lock()
            .ok()
            .and_then(|reg| reg.get(&db_path).and_then(|m| m.get(group_id)).map(snapshot_of)))
    }

    /// Every group call we're in or could join — the chat-list call indicator and the ring prompt.
    pub fn group_call_inbox(&self) -> OmniResult<Vec<GroupCallNotice>> {
        let _ = self.ensure_runtime_profile_active("group_call_inbox")?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let now = current_unix_ms();
        let joined: Vec<(String, String, i64, usize)> = registry()
            .lock()
            .ok()
            .and_then(|reg| {
                reg.get(&db_path).map(|m| {
                    m.values()
                        .map(|r| (r.group_id.clone(), r.call_id.clone(), r.started_at_unix_ms, r.members.len()))
                        .collect()
                })
            })
            .unwrap_or_default();
        let known: Vec<(String, KnownCall)> = known_calls()
            .lock()
            .ok()
            .and_then(|k| k.get(&db_path).map(|m| m.iter().map(|(g, c)| (g.clone(), c.clone())).collect()))
            .unwrap_or_default();

        let mut out = Vec::new();
        for (group_id, call_id, started_at_unix_ms, participant_count) in joined {
            let Some(group) = self.storage.get_group(&group_id)? else { continue };
            out.push(GroupCallNotice {
                group_id,
                group_name: group.name,
                call_id,
                started_by_name: String::new(),
                started_at_unix_ms,
                participant_count,
                ringing: false,
                joined: true,
            });
        }
        for (group_id, call) in known {
            let Some(group) = self.storage.get_group(&group_id)? else { continue };
            let started_by_name = self
                .storage
                .list_group_members(&group_id)?
                .into_iter()
                .find(|m| m.member_pub == call.started_by)
                .map(|m| m.display_name)
                .unwrap_or_default();
            out.push(GroupCallNotice {
                group_id,
                group_name: group.name,
                call_id: call.call_id,
                started_by_name,
                started_at_unix_ms: call.started_at_unix_ms,
                participant_count: call.participants.len(),
                ringing: call.rings && !call.dismissed && now.saturating_sub(call.started_at_unix_ms) < RING_WINDOW_MS,
                joined: false,
            });
        }
        out.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        Ok(out)
    }

    /// Send a batch of raw RTP media frames into a group call: seal each with our
    /// per-sender media key, wrap in the group-media routing envelope, and send to
    /// the topology's targets (the forwarder for a leaf, or every other member for
    /// full mesh / when we are the forwarder). Best-effort per target.
    pub fn send_group_media_batch(
        &self,
        group_id: &str,
        payloads: Vec<Vec<u8>>,
    ) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let Some(mesh) = self.mesh_runtime() else {
            return Ok(());
        };
        // Our video passport rides ahead of the media, only to the call's participants.
        let mut payloads = payloads;
        if let Some(passport) = take_due_passport(&db_path, group_id, current_unix_ms()) {
            payloads.insert(0, passport);
        }
        send_own_group_media(&db_path, group_id, &mesh, &payloads)
    }

    /// Send our RTCP about one sender's stream back to that sender — never flooded: in full mesh
    /// straight to it, in cascade along the media path backwards ([`feedback_next_hop`]). Only the
    /// sender's next hop ([`feedback_reporter`]) sends all of it: the sender's congestion controller
    /// must hear one link. Everyone else sends just its repair requests (NACK, PLI, FIR).
    pub fn send_group_feedback(
        &self,
        group_id: &str,
        sender_tag: &[u8; GROUP_MEDIA_TAG_LEN],
        rtcp: &[u8],
    ) -> OmniResult<()> {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let Some(runtime) = mesh_runtime_and_state(&db_path, group_id) else {
            return Ok(()); // the call is over
        };
        let (Some(mesh), Some(sender), Some(me)) = (
            self.mesh_runtime(),
            runtime.member_by_sender_tag(sender_tag).cloned(),
            runtime.members.get(&runtime.my_pub).cloned(),
        ) else {
            return Ok(());
        };
        let participants: Vec<String> = runtime.members.keys().cloned().collect();
        let reporter = feedback_reporter(runtime.tree.as_ref(), &participants, &sender.member_pub);
        let payload = if reporter.as_deref() == Some(runtime.my_pub.as_str()) {
            rtcp.to_vec()
        } else {
            match rtcp_repair_requests_only(rtcp) {
                Some(repairs) => repairs,
                None => return Ok(()),
            }
        };
        let Some(hop) = feedback_next_hop(runtime.tree.as_ref(), &runtime.my_pub, &sender.member_pub)
            .and_then(|hop| runtime.nodes_for(&[hop]).pop())
        else {
            return Ok(());
        };
        let Some(seq) = bump_media_seq(&db_path, group_id, 1) else {
            return Ok(());
        };
        let sealed = omni_crypto::seal_group_call_media(&me.media_key, GROUP_FEEDBACK_SEAL_TAG, seq, &payload)?;
        let Some(sealed) = signed_by_us(&runtime, sealed) else {
            return Ok(());
        };
        let wire =
            wrap_group_feedback(GroupUpstreamKind::Feedback, &call_tag_from_id(&runtime.call_id), &sender.sender_tag, &me.sender_tag, &sealed);
        let sent = mesh.send_media(omni_mesh_runtime::MeshNodeAddress::from_bytes(hop), &wire);
        note_media(&db_path, |s| s.feedback_sent += u64::from(sent.is_ok()));
        if sent.is_err() {
            note_send(&db_path, &hop, sent);
        }
        Ok(())
    }

    /// One call per profile (F5, audit 2026-11-09 #5): a group call goes in only while this profile
    /// is in no other call — no 1:1 call (ringing too) and no other group's. Checked and stored under
    /// the session lock the 1:1 calls take, so neither slips in between.
    fn store_runtime_if_no_other_call(&self, db_path: &str, runtime: GroupCallRuntime) -> OmniResult<()> {
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let busy = crate::current_call_runtime(db_path)?.is_some()
            || group_call_in_progress(db_path).is_some_and(|other| other != runtime.group_id);
        if busy {
            return Err(OmniError::InvalidInput("another call is already in progress".into()));
        }
        store_runtime(db_path, runtime);
        Ok(())
    }

    fn active_call_group(&self, group_id: &str) -> OmniResult<omni_storage::GroupRecord> {
        let group = self
            .storage
            .get_group(group_id)?
            .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        if group.my_status != "member" {
            return Err(OmniError::InvalidInput("cannot call: not an active member of the group".into()));
        }
        Ok(group)
    }

    fn own_mesh_node(&self) -> [u8; 20] {
        self.mesh_runtime().map(|rt| *rt.node_address().as_bytes()).unwrap_or([0u8; 20])
    }

    pub(crate) fn publish_group_call_control(
        &self,
        group_id: &str,
        control: &GroupCallControlV1,
    ) -> OmniResult<()> {
        crate::app_groups::publish_group_call_control_via(&self.storage, group_id, control)?;
        self.flush_group_feed();
        Ok(())
    }
}

fn call_control(call_id: &str, kind: GroupCallControlKindV1, secret: Option<&[u8]>) -> GroupCallControlV1 {
    GroupCallControlV1 {
        version: GROUP_WIRE_VERSION_V2,
        call_id: call_id.to_string(),
        control: kind,
        call_secret_b64: secret.map(|s| B64.encode(s)),
        muted: None,
        camera_off: None,
        screen_sharing: None,
        screen_control: None,
        at_unix_ms: current_unix_ms(),
    }
}

/// Our full current call state as a `MediaState` frame. Also our presence announcement: a joiner
/// learns who is in the call from these replies (control frames get compacted by durable GC, so a
/// joiner cannot rely on having seen earlier Joins).
fn own_media_state(runtime: &GroupCallRuntime) -> GroupCallControlV1 {
    let (muted, camera_off, screen_sharing, screen_control) = runtime
        .members
        .get(&runtime.my_pub)
        .map(|me| (me.muted, me.camera_off, me.screen_sharing, me.screen_control.clone()))
        .unwrap_or((false, true, false, None));
    GroupCallControlV1 {
        muted: Some(muted),
        camera_off: Some(camera_off),
        screen_sharing: Some(screen_sharing),
        screen_control,
        ..call_control(&runtime.call_id, GroupCallControlKindV1::MediaState, None)
    }
}

// --- inbound control (from the group feed ingest) ----------------------------

/// Ingest a group-call control frame and return the control frames WE should publish in response
/// (the caller, the feed ingest, publishes them).
///
/// - Not in a call: the frame updates what we know about the group's call (rings on a fresh
///   Invite). Nothing is sent — hearing about a call never joins it.
/// - In this call: a Join / Invite / MediaState from someone not yet tracked adds them; to a
///   Join we reply with our own state (presence for the joiner). Leave drops the sender.
/// - In a DIFFERENT call (two members started at once): the earlier call id wins everywhere; if
///   the incoming one is earlier we move over to it (Leave ours, Join theirs) — only while ours is
///   just started and the other started about when ours did, by both ids ([`CALL_FOLD_WINDOW_MS`]).
///   Any member of the group took a running call over by an Invite of a smaller id, «0»: everyone in
///   it left for a call of their own (blind audit 2026-11-13, pass 21).
pub(crate) fn handle_group_call_control(
    storage: &Storage,
    group_id: &str,
    sender_pub: &str,
    control: &GroupCallControlV1,
) -> OmniResult<Vec<GroupCallControlV1>> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Some(group) = storage.get_group(group_id)? else {
        return Ok(vec![]);
    };
    if group.my_status != "member" || sender_pub == group.my_member_pub {
        return Ok(vec![]);
    }
    let secret = match control.call_secret_b64.as_ref() {
        Some(b64) => Some(B64.decode(b64).map_err(|e| OmniError::InvalidInput(format!("call secret b64: {e}")))?),
        None => None,
    };

    let now = current_unix_ms();
    let ours = registry().lock().ok().and_then(|reg| {
        reg.get(&db_path).and_then(|m| m.get(group_id)).map(|r| {
            // Just started here, and the other call started about when ours did, by both ids: two
            // (or more) pressed «call» at once. Folding only while alone left three such starts in
            // two calls for good, once one had been joined (pass 22).
            let fresh = now.saturating_sub(r.started_at_unix_ms) < CALL_FOLD_WINDOW_MS;
            let concurrent = match (call_started_ms(&r.call_id), call_started_ms(&control.call_id)) {
                (Some(ours), Some(theirs)) => ours.abs_diff(theirs) < CALL_FOLD_WINDOW_MS as u64,
                _ => false,
            };
            (r.call_id.clone(), fresh && concurrent)
        })
    });
    match ours {
        Some((our_call, _)) if our_call == control.call_id => {
            let replies = in_call_control(storage, &db_path, &group, sender_pub, control)?;
            // Someone joined, left or told us what they decode: our one codec for all may change.
            crate::reselect_group_media(&db_path, &group.dialog_key);
            Ok(replies)
        }
        Some((our_call, may_fold)) => {
            let joins = matches!(control.control, GroupCallControlKindV1::Invite | GroupCallControlKindV1::Join);
            match secret {
                Some(secret) if joins && may_fold && control.call_id < our_call => {
                    // Fold into the earlier call.
                    let Some(old) = take_runtime(&db_path, group_id) else { return Ok(vec![]) };
                    let mut participants = BTreeSet::new();
                    participants.insert(sender_pub.to_string());
                    let mut runtime = build_runtime(storage, &group, &control.call_id, &secret, &participants, own_node_of(&old))?;
                    let my_pub = runtime.my_pub.clone();
                    if let (Some(me), Some(old_me)) = (runtime.members.get_mut(&my_pub), old.members.get(&old.my_pub)) {
                        me.muted = old_me.muted;
                        me.camera_off = old_me.camera_off;
                        me.screen_sharing = old_me.screen_sharing;
                    }
                    let state = own_media_state(&runtime);
                    store_runtime(&db_path, runtime);
                    Ok(vec![
                        call_control(&old.call_id, GroupCallControlKindV1::Leave, None),
                        call_control(&control.call_id, GroupCallControlKindV1::Join, Some(&secret)),
                        state,
                    ])
                }
                // The later call folds into ours on its side — nothing to do here.
                _ => Ok(vec![]),
            }
        }
        None => {
            note_known_call(&db_path, group_id, sender_pub, control, secret);
            crate::record_group_changed_event(&db_path, group_id);
            Ok(vec![])
        }
    }
}

/// A participant's Rekey for the call we are in. Someone took the turn — when its secret is taken:
/// a Rekey that is not (the secret we hold already, sent again; a larger rival) left the turn
/// nobody's, and the call on the secret the removed member held (blind audit 2026-11-11, pass 13).
fn take_received_rekey(runtime: &mut GroupCallRuntime, secret: Vec<u8>, said_at_ms: i64, now_ms: i64) -> OmniResult<()> {
    if said_at_ms > now_ms + REKEY_CLOCK_AHEAD_MS {
        return Ok(());
    }
    if secret != runtime.call_secret && takes_rekey(&runtime.call_secret, runtime.rekeyed_at_ms, &secret, said_at_ms) {
        rekey_runtime(runtime, secret, now_ms)?;
        runtime.rekeyed_at_ms = said_at_ms;
        runtime.rekey_owed_at_ms = None;
        runtime.rekey_unsaid = None;
    }
    Ok(())
}

/// A control frame for the call we are in.
fn in_call_control(
    storage: &Storage,
    db_path: &str,
    group: &omni_storage::GroupRecord,
    sender_pub: &str,
    control: &GroupCallControlV1,
) -> OmniResult<Vec<GroupCallControlV1>> {
    let group_id = group.group_id.as_str();
    let mut reg = registry()
        .lock()
        .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
    let Some(runtime) = reg.get_mut(db_path).and_then(|m| m.get_mut(group_id)) else {
        return Ok(vec![]);
    };
    // A leave takes only its sender out; the call ends when the last one leaves (no `End` in the
    // protocol: sent by any member, it ended the call for everyone — blind audit 2026-11-10).
    if matches!(control.control, GroupCallControlKindV1::Leave) {
        // Every remaining participant runs the same deterministic rebuild → the same new tree.
        runtime.members.remove(sender_pub);
        runtime.rebuild_topology();
        return Ok(vec![]);
    }
    if control.control == GroupCallControlKindV1::Rekey {
        // Only a participant may rekey; the secret rides the MLS epoch the removed member can't read.
        // One not in the call here yet is held until its Join comes.
        if let Some(b64) = control.call_secret_b64.as_ref() {
            let secret = B64.decode(b64).map_err(|e| OmniError::InvalidInput(format!("call secret b64: {e}")))?;
            if !runtime.members.contains_key(sender_pub) {
                runtime.held_rekey = Some((sender_pub.to_string(), secret, control.at_unix_ms));
                return Ok(vec![]);
            }
            take_received_rekey(runtime, secret, control.at_unix_ms, current_unix_ms())?;
        }
        return Ok(vec![]);
    }
    // Invite / Join / MediaState: the sender is in the call.
    if !runtime.has_room_for(sender_pub) {
        crate::omni_app_trace_log(
            "group/call-full",
            &format!("group={group_id} member={} not let in: {GROUP_CALL_MAX} in the call", short_pub(sender_pub)),
        );
        return Ok(vec![]);
    }
    if !runtime.members.contains_key(sender_pub) {
        if let Some(member) = call_member(storage, group_id, sender_pub, &runtime.call_secret, [0u8; 20])? {
            runtime.members.insert(sender_pub.to_string(), member);
            runtime.rebuild_topology();
            if runtime.held_rekey.as_ref().is_some_and(|(by, ..)| by == sender_pub) {
                if let Some((_, secret, said_at_ms)) = runtime.held_rekey.take() {
                    take_received_rekey(runtime, secret, said_at_ms, current_unix_ms())?;
                }
            }
        }
    }
    if let Some(member) = runtime.members.get_mut(sender_pub) {
        // `sender_pub` is the MLS-authenticated leaf, so a member can only set their own badge.
        if let Some(muted) = control.muted {
            member.muted = muted;
        }
        if let Some(camera_off) = control.camera_off {
            member.camera_off = camera_off;
        }
        if let Some(screen_sharing) = control.screen_sharing {
            member.screen_sharing = screen_sharing;
        }
        // `MediaState` is the sender's full state: no grant in it means nobody may control.
        if control.control == GroupCallControlKindV1::MediaState {
            member.screen_control = control.screen_control.clone();
        }
    }
    let lets_me = runtime.members.get(sender_pub).is_some_and(|m| {
        m.screen_sharing && m.screen_control.as_ref().is_some_and(|grant| grant.allows(&runtime.my_pub))
    });
    if !lets_me {
        runtime.screen_out.remove(sender_pub);
    }
    Ok(match control.control {
        GroupCallControlKindV1::Join | GroupCallControlKindV1::Invite => vec![own_media_state(runtime)],
        _ => vec![],
    })
}

/// The video passport travels the call's media plane, not the group feed (maintainers' choice
/// 2026-10-14: only the call's participants learn what a member's machine encodes and decodes; the
/// feed reaches every member of the group, called or not). A passport is a sealed group-media frame
/// like any RTP packet, its plaintext this magic and the deflated passport JSON — never valid RTP (version
/// bits 01), so it cannot be mistaken for media. It is resent every [`GROUP_PASSPORT_EVERY_MS`] for
/// whoever joined since and whatever datagram was lost. Known limit: in cascade (4+) the C1 gate
/// may hold back a quiet member's passport like its video; those watchers are H.264 to it until
/// it speaks — safe, every machine decodes H.264.
pub(crate) const GROUP_PASSPORT_MAGIC: &[u8; 8] = b"OMNIPSPT";
const GROUP_PASSPORT_EVERY_MS: i64 = 3_000;
/// A passport rides one datagram with the seal and the routing envelope around it.
const GROUP_PASSPORT_MAX_BYTES: usize = 900;
/// What a passport may inflate to: a bound on what a member makes us allocate.
const GROUP_PASSPORT_PLAIN_MAX_BYTES: u64 = 16 * 1024;

/// What a member signs in its passport frame: this call, its own identity key, the deflated passport.
fn group_passport_signed_bytes(call_id: &str, member_pub: &[u8; 32], deflated: &[u8]) -> Vec<u8> {
    let mut bytes = b"omni-group-passport-v1".to_vec();
    bytes.extend_from_slice(&(call_id.len() as u32).to_le_bytes());
    bytes.extend_from_slice(call_id.as_bytes());
    bytes.extend_from_slice(member_pub);
    bytes.extend_from_slice(deflated);
    bytes
}

/// The passport frame's plaintext: magic ‖ signature ‖ deflated passport JSON. The dialog and
/// announcement id stay home (the call names the dialog, the id is ours alone). `None` when it would
/// not fit one datagram.
///
/// Signed with the member's identity key (V-27, review 2026-10-18): the group media key proves
/// membership only — every member derives every other's — so without the signature any member could
/// seal a passport "as" another and steer the codecs sent to it.
pub(crate) fn encode_group_passport(
    capabilities: &omni_types::RealtimeMediaCapabilitiesV1,
    signing_seed: &[u8; 32],
    call_id: &str,
) -> Option<Vec<u8>> {
    use ed25519_dalek::Signer as _;
    let mut stripped = capabilities.clone();
    stripped.dialog_key.clear();
    stripped.capabilities_id.clear();
    let json = serde_json::to_vec(&stripped).ok()?;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    std::io::Write::write_all(&mut encoder, &json).ok()?;
    let deflated = encoder.finish().ok()?;
    let key = ed25519_dalek::SigningKey::from_bytes(signing_seed);
    let signature = key.sign(&group_passport_signed_bytes(call_id, &key.verifying_key().to_bytes(), &deflated));
    let frame = [GROUP_PASSPORT_MAGIC.as_slice(), &signature.to_bytes(), &deflated].concat();
    (frame.len() <= GROUP_PASSPORT_MAX_BYTES).then_some(frame)
}

/// A media plaintext that is a passport frame: the passport, or `Some(None)` when it is one but
/// unreadable or not signed by `member_pub` for `call_id`. `None` — it is media.
pub(crate) fn decode_group_passport(
    plaintext: &[u8],
    call_id: &str,
    member_pub: &str,
) -> Option<Option<omni_types::RealtimeMediaCapabilitiesV1>> {
    let signed = plaintext.strip_prefix(GROUP_PASSPORT_MAGIC.as_slice())?;
    let verified = (|| {
        let (signature, deflated) = signed.split_at_checked(ed25519_dalek::SIGNATURE_LENGTH)?;
        let public = decode_identity(member_pub)?;
        ed25519_dalek::VerifyingKey::from_bytes(&public)
            .ok()?
            .verify_strict(
                &group_passport_signed_bytes(call_id, &public, deflated),
                &ed25519_dalek::Signature::from_bytes(signature.try_into().ok()?),
            )
            .ok()?;
        Some(deflated)
    })();
    let Some(deflated) = verified else {
        return Some(None);
    };
    let mut json = Vec::new();
    let inflated = std::io::Read::read_to_end(
        &mut std::io::Read::take(flate2::read::DeflateDecoder::new(deflated), GROUP_PASSPORT_PLAIN_MAX_BYTES + 1),
        &mut json,
    );
    if inflated.is_err() || json.len() as u64 > GROUP_PASSPORT_PLAIN_MAX_BYTES {
        return Some(None);
    }
    Some(serde_json::from_slice(&json).ok())
}

/// Only what tells codecs apart: a fresh id or timestamp alone is no news.
fn same_video_passport(
    known: Option<&omni_types::RealtimeMediaCapabilitiesV1>,
    capabilities: &omni_types::RealtimeMediaCapabilitiesV1,
) -> bool {
    known.is_some_and(|known| {
        known.codecs == capabilities.codecs
            && known.power == capabilities.power
            && known.cpu_load == capabilities.cpu_load
            && known.codec_ceiling == capabilities.codec_ceiling
            && known.bitrate_level == capabilities.bitrate_level
            && known.content == capabilities.content
    })
}

/// Keep our own video passport on our member of the call; a changed one goes out with our next
/// media ([`take_due_passport`]). No-op when we are not in a call there.
pub(crate) fn store_own_group_video_capabilities(
    db_path: &str,
    group_id: &str,
    capabilities: &omni_types::RealtimeMediaCapabilitiesV1,
) {
    let Ok(mut reg) = registry().lock() else { return };
    let Some(runtime) = reg.get_mut(db_path).and_then(|calls| calls.get_mut(group_id)) else { return };
    let my_pub = runtime.my_pub.clone();
    let Some(me) = runtime.members.get_mut(&my_pub) else { return };
    if same_video_passport(me.video_capabilities.as_ref(), capabilities) {
        return;
    }
    me.video_capabilities = Some(capabilities.clone());
    runtime.passport_sent_at_ms = 0;
}

/// A member's passport as heard on the call's media. Returns whether it changed what we knew.
pub(crate) fn note_member_video_capabilities(
    db_path: &str,
    group_id: &str,
    member_pub: &str,
    capabilities: omni_types::RealtimeMediaCapabilitiesV1,
) -> bool {
    let Ok(mut reg) = registry().lock() else { return false };
    let Some(member) = reg
        .get_mut(db_path)
        .and_then(|calls| calls.get_mut(group_id))
        .and_then(|runtime| runtime.members.get_mut(member_pub))
    else {
        return false;
    };
    if same_video_passport(member.video_capabilities.as_ref(), &capabilities) {
        return false;
    }
    member.video_capabilities = Some(capabilities);
    true
}

/// Our passport frame when one is due: never sent, changed, or [`GROUP_PASSPORT_EVERY_MS`] since
/// the last. `None` before our media has published one.
fn take_due_passport(db_path: &str, group_id: &str, now_ms: i64) -> Option<Vec<u8>> {
    let mut reg = registry().lock().ok()?;
    let runtime = reg.get_mut(db_path).and_then(|calls| calls.get_mut(group_id))?;
    if now_ms - runtime.passport_sent_at_ms < GROUP_PASSPORT_EVERY_MS {
        return None;
    }
    let capabilities = runtime.members.get(&runtime.my_pub)?.video_capabilities.clone()?;
    // Loaded when our passport was published (`publish_group_media_capabilities`).
    let Some((seed, _)) = crate::app_screen_control::cached_signing_identity(db_path) else {
        crate::omni_app_trace_log("group/codec", "no identity key loaded to sign the passport; watchers stay on H.264");
        return None;
    };
    runtime.passport_sent_at_ms = now_ms;
    let frame = encode_group_passport(&capabilities, &seed, &runtime.call_id);
    if frame.is_none() {
        crate::omni_app_trace_log("group/codec", "passport does not fit one datagram; watchers stay on H.264");
    }
    frame
}

/// Seal our own media frames and send them to the call's targets — all tree-neighbours (cascade) or
/// every other participant (full mesh). Best-effort per target.
fn send_own_group_media(
    db_path: &str,
    group_id: &str,
    mesh: &omni_mesh_runtime::MeshRuntime,
    payloads: &[Vec<u8>],
) -> OmniResult<()> {
    let Some(runtime) = mesh_runtime_and_state(db_path, group_id) else {
        return Ok(()); // no active call for this group — drop
    };
    let my = runtime
        .members
        .get(&runtime.my_pub)
        .ok_or_else(|| OmniError::Internal("we are not in our own call".into()))?
        .clone();
    let call_tag = call_tag_from_id(&runtime.call_id);
    // Our own media → all tree-neighbours (cascade) or every other participant (full mesh).
    let target_nodes = runtime.nodes_for(&runtime.media_targets(None));
    if target_nodes.is_empty() {
        return Ok(());
    }

    // Advance the media counter under the registry lock so concurrent senders
    // don't reuse a nonce.
    let Some(first_seq) = bump_media_seq(db_path, group_id, payloads.len() as u64) else {
        return Ok(());
    };
    let own_activity = runtime.own_activity(current_unix_ms());
    for (seq, payload) in (first_seq..).zip(payloads) {
        let sealed = omni_crypto::seal_group_call_media(&my.media_key, 0x4F4D4E41, seq, payload)?;
        let Some(sealed) = signed_by_us(&runtime, sealed) else {
            return Ok(());
        };
        // We are the origin, so relay_tag == our sender_tag (we are our own last hop).
        // media_meta (C1 active-speaker gating): the frame's kind and our microphone's activity,
        // which forwarders rank us by.
        let media_meta = pack_media_meta(!is_own_audio_rtp(payload), own_activity);
        let wire = wrap_group_media(
            GROUP_MEDIA_DEFAULT_HOP_LIMIT,
            media_meta,
            &my.sender_tag,
            &call_tag,
            &my.sender_tag,
            &sealed,
        );
        for node in &target_nodes {
            let sent = mesh.send_media(omni_mesh_runtime::MeshNodeAddress::from_bytes(*node), &wire);
            note_send(db_path, node, sent);
        }
    }
    Ok(())
}

/// Whether an outgoing payload is our audio: an RTP packet of the native Opus sender (SSRC "OMNA",
/// PT 111). Everything else we send in a call is video (the screen too).
fn is_own_audio_rtp(payload: &[u8]) -> bool {
    payload.len() >= 12 && payload[1] & 0x7F == 111 && payload[8..12] == 0x4F4D_4E41u32.to_be_bytes()
}

/// The mesh nodes of everyone else in the calls we are in. Call media rides direct links only
/// (`MeshRuntime::send_media`), and two members of a group need not be each other's contacts — the
/// contact autoconnect would never link them, and they would not hear each other (stand, 2026-10-14:
/// Alice and the PC, members of one group, in one call, no media either way).
pub(crate) fn group_call_member_nodes(db_path: &str) -> Vec<[u8; 20]> {
    let Ok(reg) = registry().lock() else { return Vec::new() };
    let Some(calls) = reg.get(db_path) else { return Vec::new() };
    calls
        .values()
        .flat_map(|runtime| {
            runtime
                .members
                .iter()
                .filter(|(member_pub, _)| **member_pub != runtime.my_pub)
                .map(|(_, member)| member.node)
        })
        .collect()
}

/// Our passport to each call we are in that is owed one, on the mesh maintenance tick: a member who
/// sends nothing (mic muted, camera off) still tells the others what it decodes, or they would send
/// it H.264 for want of knowing. A member who sends media carries it there sooner.
pub(crate) fn send_due_group_passports(db_path: &str, mesh: &omni_mesh_runtime::MeshRuntime) {
    let group_ids: Vec<String> = registry()
        .lock()
        .ok()
        .and_then(|reg| reg.get(db_path).map(|calls| calls.keys().cloned().collect()))
        .unwrap_or_default();
    for group_id in group_ids {
        if let Some(passport) = take_due_passport(db_path, &group_id, current_unix_ms()) {
            let _ = send_own_group_media(db_path, &group_id, mesh, &[passport]);
        }
    }
}

/// The passports of everyone else in our call for `group_id`, and whether every one of them is
/// known. `None` when we are not in a call there.
pub(crate) fn group_call_watcher_capabilities(
    db_path: &str,
    group_id: &str,
) -> Option<(Vec<(String, omni_types::RealtimeMediaCapabilitiesV1)>, bool)> {
    let reg = registry().lock().ok()?;
    let runtime = reg.get(db_path).and_then(|calls| calls.get(group_id))?;
    let mut all_known = true;
    let mut watchers = Vec::new();
    for (member_pub, member) in &runtime.members {
        if *member_pub == runtime.my_pub {
            continue;
        }
        match &member.video_capabilities {
            Some(capabilities) => watchers.push((member_pub.clone(), capabilities.clone())),
            None => all_known = false,
        }
    }
    Some((watchers, all_known))
}

/// A participant's Rekey of a call we know of and are not in.
fn take_known_rekey(call: &mut KnownCall, secret: Vec<u8>, said_at_ms: i64) {
    if said_at_ms <= current_unix_ms() + REKEY_CLOCK_AHEAD_MS && takes_rekey(&call.call_secret, call.rekeyed_at_ms, &secret, said_at_ms) {
        call.call_secret = secret;
        call.rekeyed_at_ms = said_at_ms;
    }
}

/// Track a call we are not in. The earlier call id wins if two were started at once.
fn note_known_call(
    db_path: &str,
    group_id: &str,
    sender_pub: &str,
    control: &GroupCallControlV1,
    secret: Option<Vec<u8>>,
) {
    let Ok(mut known) = known_calls().lock() else { return };
    let calls = known.entry(db_path.to_string()).or_default();
    match control.control {
        GroupCallControlKindV1::Leave => {
            if let Some(call) = calls.get_mut(group_id).filter(|c| c.call_id == control.call_id) {
                call.participants.remove(sender_pub);
                if call.participants.is_empty() {
                    calls.remove(group_id);
                }
            }
        }
        GroupCallControlKindV1::Rekey => {
            if let (Some(call), Some(secret)) =
                (calls.get_mut(group_id).filter(|c| c.call_id == control.call_id), secret)
            {
                // Only someone heard in the call: any member of the group changed the secret of a
                // call we had not joined yet, and we joined on a key nobody in it held (pass 21).
                // One not heard yet is held until it is (pass 22).
                if !call.participants.contains(sender_pub) {
                    call.held_rekey = Some((sender_pub.to_string(), secret, control.at_unix_ms));
                    return;
                }
                take_known_rekey(call, secret, control.at_unix_ms);
            }
        }
        GroupCallControlKindV1::Invite | GroupCallControlKindV1::Join | GroupCallControlKindV1::MediaState => {
            let same = calls.get(group_id).is_some_and(|c| c.call_id == control.call_id);
            if !same {
                // An earlier id takes over only from a call started about when it was, by both ids:
                // any member's Join of an old, smaller id took the place of the call that rang, and
                // we joined a call nobody was in (blind audit 2026-11-13, pass 23).
                let replaces = calls.get(group_id).is_none_or(|c| {
                    let concurrent = match (call_started_ms(&c.call_id), call_started_ms(&control.call_id)) {
                        (Some(known), Some(theirs)) => known.abs_diff(theirs) < CALL_FOLD_WINDOW_MS as u64,
                        _ => false,
                    };
                    control.call_id < c.call_id && concurrent
                });
                // A call we can't join (no secret on this frame) is not worth listing.
                let Some(secret) = secret.filter(|_| replaces) else { return };
                let invite = control.control == GroupCallControlKindV1::Invite;
                calls.insert(
                    group_id.to_string(),
                    KnownCall {
                        call_id: control.call_id.clone(),
                        call_secret: secret,
                        started_by: if invite { sender_pub.to_string() } else { String::new() },
                        started_at_unix_ms: control.at_unix_ms,
                        participants: BTreeSet::new(),
                        rings: invite,
                        dismissed: false,
                        rekeyed_at_ms: 0,
                        held_rekey: None,
                    },
                );
            }
            if let Some(call) = calls.get_mut(group_id) {
                call.participants.insert(sender_pub.to_string());
                if call.held_rekey.as_ref().is_some_and(|(by, ..)| by == sender_pub) {
                    if let Some((_, secret, said_at_ms)) = call.held_rekey.take() {
                        take_known_rekey(call, secret, said_at_ms);
                    }
                }
            }
        }
    }
}

/// The secret of the call we are in (tests compare who holds which key).
#[cfg(test)]
pub(crate) fn call_secret_for_test(db_path: &str, group_id: &str) -> Option<Vec<u8>> {
    registry().lock().ok()?.get(db_path)?.get(group_id).map(|r| r.call_secret.clone())
}

/// Open a co-member's sealed frame: with their current key, or — for [`REKEY_GRACE_MS`] after a
/// rekey — with the previous one.
/// Open a co-member's frame and let it through once: `None` for a frame that does not open with the
/// member's key or that was opened before (a replay).
fn open_member_frame(runtime: &mut GroupCallRuntime, member_pub: &str, signed: &[u8], now_ms: i64) -> Option<Vec<u8>> {
    // Its sender's signature first: any member can seal under any member's key.
    let member = runtime.members.get(member_pub)?;
    let sealed = omni_crypto::verified_group_call_media(member.verifier.as_ref()?, &runtime.call_id, signed)?;
    let opened = open_member_media(member, sealed, now_ms)?;
    let (_, seq) = omni_crypto::peek_group_call_media_header(sealed)?;
    runtime
        .media_replay
        .entry(member_pub.to_string())
        .or_default()
        .accept(seq)
        .then_some(opened)
}

fn open_member_media(sender: &CallMember, sealed: &[u8], now_ms: i64) -> Option<Vec<u8>> {
    omni_crypto::open_group_call_media(&sender.media_key, sealed).ok().or_else(|| {
        sender
            .previous_media_key
            .filter(|(_, until)| now_ms <= *until)
            .and_then(|(key, _)| omni_crypto::open_group_call_media(&key, sealed).ok())
    })
}

/// Swap a call to a new secret: re-derive every participant's media key, keeping the old one for
/// [`REKEY_GRACE_MS`].
fn rekey_runtime(runtime: &mut GroupCallRuntime, secret: Vec<u8>, now_ms: i64) -> OmniResult<()> {
    for member in runtime.members.values_mut() {
        let fresh = omni_crypto::derive_group_call_media_key(&secret, &member.member_pub)?;
        member.previous_media_key = Some((member.media_key, now_ms + REKEY_GRACE_MS));
        member.media_key = fresh;
    }
    runtime.call_secret = secret;
    runtime.rekeyed_at_ms = now_ms;
    Ok(())
}

/// Members left the GROUP (a Commit merged). Drop them from the group's call and rotate its secret —
/// they had it from the Invite, in the call or not — so they can neither decrypt nor forge its media
/// any longer. One
/// remaining participant issues the new secret — the smallest member_pub, a choice every participant
/// makes identically, so one `Rekey` goes out. Returns that frame when it is ours to send (the
/// caller publishes it into the group feed, i.e. in the new epoch). The others take the turn one
/// after another if no `Rekey` comes ([`take_due_group_call_rekeys`]): the smallest member_pub may
/// be one that dropped out without a Leave, and the call kept the old secret while it was silent
/// (blind audit 2026-11-11, M3).
pub(crate) fn on_group_members_removed(
    db_path: &str,
    group_id: &str,
    removed: &[String],
) -> OmniResult<Option<GroupCallControlV1>> {
    if removed.is_empty() {
        return Ok(None);
    }
    if let Ok(mut known) = known_calls().lock() {
        if let Some(calls) = known.get_mut(db_path) {
            if let Some(call) = calls.get_mut(group_id) {
                call.participants.retain(|p| !removed.contains(p));
                // The next Rekey is a new change, not a rival of the last one.
                call.rekeyed_at_ms = 0;
                // Nobody left in it: it is over. Kept, «start a call» joined it — its old secret, the
                // removed one's too — and nobody rang (blind audit 2026-11-11, L3).
                if call.participants.is_empty() {
                    calls.remove(group_id);
                }
            }
        }
    }
    let mut reg = registry()
        .lock()
        .map_err(|_| OmniError::Internal("group call registry poisoned".into()))?;
    let Some(runtime) = reg.get_mut(db_path).and_then(|m| m.get_mut(group_id)) else {
        return Ok(None);
    };
    // Rekeyed whether or not they were in it: the Invite went to the whole group, so every member then
    // got the secret. Rekeyed only when one in the call left, a member removed while out of it kept a
    // secret that still opened the call (blind audit 2026-11-11, L3).
    runtime.members.retain(|p, _| !removed.contains(p));
    runtime.rebuild_topology();
    let now_ms = current_unix_ms();
    // The next Rekey is a new change, not a rival of the last one.
    runtime.rekeyed_at_ms = 0;
    let mut in_turn: Vec<&String> = runtime.members.keys().collect();
    in_turn.sort_unstable();
    let turn = in_turn.iter().position(|member| **member == runtime.my_pub);
    match turn {
        Some(0) => {
            runtime.rekey_owed_at_ms = None;
            let secret = omni_crypto::generate_group_key().to_vec();
            rekey_runtime(runtime, secret.clone(), now_ms)?;
            Ok(Some(call_control(&runtime.call_id, GroupCallControlKindV1::Rekey, Some(&secret))))
        }
        Some(turn) => {
            runtime.rekey_owed_at_ms = Some(now_ms + turn as i64 * REKEY_TURN_MS);
            Ok(None)
        }
        None => Ok(None),
    }
}

/// The calls whose rekey has become ours to make: a member was removed, and nobody whose turn came
/// before ours has rekeyed. Each is rekeyed here; the frames are the caller's to publish.
pub(crate) fn take_due_group_call_rekeys(db_path: &str, now_ms: i64) -> Vec<(String, GroupCallControlV1)> {
    let Ok(mut reg) = registry().lock() else {
        return Vec::new();
    };
    let Some(calls) = reg.get_mut(db_path) else {
        return Vec::new();
    };
    let mut due = Vec::new();
    for (group_id, runtime) in calls.iter_mut() {
        if runtime.rekey_owed_at_ms.is_none_or(|at| at > now_ms) {
            continue;
        }
        runtime.rekey_owed_at_ms = None;
        // One we made and could not say goes out again as it is: made anew each time, the secret
        // here moved on with every failed try while the others stayed where they were — past the
        // one key back that is still accepted (blind audit 2026-11-11, pass 14).
        if let Some(unsaid) = runtime.rekey_unsaid.take() {
            due.push((group_id.clone(), unsaid));
            continue;
        }
        let secret = omni_crypto::generate_group_key().to_vec();
        if rekey_runtime(runtime, secret.clone(), now_ms).is_ok() {
            let control = call_control(&runtime.call_id, GroupCallControlKindV1::Rekey, Some(&secret));
            runtime.rekeyed_at_ms = control.at_unix_ms;
            due.push((group_id.clone(), control));
        }
    }
    due
}

/// On the mesh maintenance tick: rekey the calls it has become our turn to rekey, and say so.
pub(crate) fn rekey_due_group_calls(storage: &Storage) {
    let db_path = storage.db_path().to_string_lossy().to_string();
    for (group_id, rekey) in take_due_group_call_rekeys(&db_path, current_unix_ms()) {
        if let Err(error) = crate::app_groups::publish_group_call_control_via(storage, &group_id, &rekey) {
            crate::omni_app_trace_log("group/call-rekey", &format!("group={group_id} our turn to rekey, not said: {error}"));
            owe_group_call_rekey(&db_path, &group_id, rekey);
        }
    }
}

/// The rekey of the call in `group_id` is ours to make again, on the next tick: the one we made was
/// not said to the group, and we alone held the new secret (blind audit 2026-11-11, pass 13).
pub(crate) fn owe_group_call_rekey(db_path: &str, group_id: &str, unsaid: GroupCallControlV1) {
    if let Ok(mut reg) = registry().lock() {
        if let Some(runtime) = reg.get_mut(db_path).and_then(|calls| calls.get_mut(group_id)) {
            // Only while the secret it carries is the one we hold: a `Rekey` taken since stands.
            let ours = unsaid.call_secret_b64.as_deref().and_then(|b64| B64.decode(b64).ok()).is_some_and(|secret| secret == runtime.call_secret);
            if ours {
                runtime.rekey_unsaid = Some(unsaid);
                runtime.rekey_owed_at_ms = Some(0);
            }
        }
    }
}

/// Handle an inbound mesh media datagram that MIGHT be group-call media. Returns
/// true if it was (handled), false if it's ordinary 1:1 media the caller should
/// route the normal way. Two things can happen (both, if we're a cascade forwarder
/// AND a participant): flood to our OTHER tree-neighbours without decrypting
/// (peer-SFU), and/or open with the sender's key + enqueue to the group thread's
/// media queue.
/// Mesh send of one datagram to a node — `MeshRuntime::send_media` in the app, a recorder in tests.
pub(crate) type GroupMediaSend<'a> = &'a dyn Fn(&[u8; 20], &[u8]) -> Result<(), omni_mesh_runtime::MeshRuntimeError>;

pub(crate) fn handle_incoming_group_media(
    db_path: &str,
    send: GroupMediaSend<'_>,
    payload: &[u8],
    enqueue: &dyn Fn(&str, Vec<u8>),
    screen: &dyn Fn(&ScreenControlDelivery),
) -> bool {
    if let Some(feedback) = parse_group_feedback(payload) {
        handle_incoming_group_feedback(db_path, send, payload, &feedback, enqueue, screen);
        return true;
    }
    let Some((_hop_limit, media_meta, relay_tag, call_tag, sender_tag, sealed)) =
        parse_group_media(payload)
    else {
        return false;
    };
    let (is_video, activity_level) = unpack_media_meta(media_meta);
    let Ok(mut reg) = registry().lock() else {
        return true; // it IS group media; a poisoned lock just means we drop it
    };
    let Some(profile_calls) = reg.get_mut(db_path) else {
        note_media(db_path, |s| s.no_call += 1);
        return true;
    };
    // Find the call this frame belongs to by `call_tag` (routing pseudonym), then take it mutably so
    // we can record the sender's activity for gating.
    let Some(group_id) = profile_calls
        .iter()
        .find(|(_, r)| call_tag_from_id(&r.call_id) == call_tag)
        .map(|(k, _)| k.clone())
    else {
        note_media(db_path, |s| s.no_call += 1);
        return true; // group media for a call we don't track — drop, don't misroute
    };
    let runtime = profile_calls
        .get_mut(&group_id)
        .expect("call just matched by call_tag");

    let my_sender_tag = runtime.my_sender_tag();
    let is_own = sender_tag == my_sender_tag;
    let sender_pub = runtime.member_by_sender_tag(&sender_tag).map(|m| m.member_pub.clone());

    // Opened first, before anything is relayed: a frame that does not open (an unknown sender, a
    // forged seal, a replay) went on to every downstream neighbour before it was found out — any
    // member who knew the call's tag could have the forwarders carry its junk (blind audit
    // 2026-11-10). Every forwarder is in the call and opens each frame for itself anyway.
    // A frame under our own tag that came in is not ours: we send our frames, they do not come back
    // to us. The tag is public (derived from our member key), so it was a free pass past the check
    // just below (blind audit 2026-11-10, a hole in the first version of that check).
    if is_own {
        note_media(db_path, |s| s.unknown_sender += 1);
        return true;
    }
    let opened = sender_pub.as_ref().map(|pk| open_member_frame(runtime, pk, sealed, current_unix_ms()));
    let authentic = matches!(opened, Some(Some(_)));
    // The sender's activity level for cascade active-speaker gating (C1), from a frame that opened
    // only: counted before the check, frames forged under other members' tags kept silent members on
    // top of the ranking and the one speaking off the forwarders (blind audit 2026-11-11, F5). Still
    // before the gate, so an actually-active sender is in the ranking for this frame.
    if authentic {
        if let Some(pk) = &sender_pub {
            runtime.note_activity(pk, activity_level, current_unix_ms());
        }
    }

    // Forwarder role (cascade only): flood the WHOLE datagram to our OTHER tree-neighbours — never
    // back toward the one it arrived from. Relayed sealed as it came — E2E holds. The arrival neighbour is read
    // from `relay_tag` (the immediate hop stamps its own tag), NOT the mesh source address: once the
    // tree is multi-hop the transport `from_node` is just a hop and doesn't identify the tree-
    // neighbour. `prepare_group_media_forward` decrements `hop_limit` (a spent frame is dropped — the
    // loop guard) and re-stamps `relay_tag` with our tag so the next hop excludes us. Leaves compute
    // an empty target set and forward nothing; full mesh never enters here.
    if runtime.topology == CallTopology::Cascade {
        // Active-speaker gate (C1): relay only the top-K loudest audio + the speaker's video onward, so
        // downstream downlink is bounded. Fails open until real activity flows (see `forward_gate`);
        // a frame that did not open is never relayed.
        let relay_ok = authentic
            && sender_pub
                .as_deref()
                .is_some_and(|pk| runtime.forward_gate(is_video, pk, current_unix_ms()));
        if relay_ok {
            let arrival = runtime.member_by_sender_tag(&relay_tag).map(|m| m.member_pub.clone());
            let targets = runtime.nodes_for(&runtime.media_targets(arrival.as_deref()));
            if !targets.is_empty() {
                if let Some(forwarded) = prepare_group_media_forward(payload, &my_sender_tag) {
                    for node in &targets {
                        let sent = send(node, &forwarded);
                        if sent.is_ok() {
                            note_media(db_path, |s| s.forwarded += 1);
                        } else {
                            note_send(db_path, node, sent);
                        }
                    }
                }
            }
        }
    }

    // Participant role: if this frame is from a co-member (not our own tag), open
    // it with that sender's media key and enqueue the RTP to the group thread —
    // PREFIXED with the 8-byte sender_tag so the shell can demux it to that sender's
    // own decoder/video tile. The group-mode receive loop strips the tag before feeding
    // the RTP to the native decoder; 1:1 media is never prefixed. See T1/T2.
    let mut passport = None;
    let delivery = match (sender_pub, opened) {
        (Some(sender_pub), Some(opened)) => match opened {
            Some(plaintext) if plaintext.starts_with(GROUP_PASSPORT_MAGIC) => {
                if let Some(Some(capabilities)) = decode_group_passport(&plaintext, &runtime.call_id, &sender_pub) {
                    passport = Some((group_id.clone(), sender_pub, capabilities));
                }
                None
            }
            Some(rtp) => {
                note_media(db_path, |s| *s.received.entry(hex::encode(&sender_tag[..4])).or_default() += 1);
                Some((runtime.dialog_key.clone(), tag_group_rtp_for_shell(&sender_tag, &rtp)))
            }
            None => {
                note_unopened(db_path);
                None
            }
        },
        _ => {
            note_media(db_path, |s| s.unknown_sender += 1);
            None
        }
    };
    // Hand the frame to the engine only AFTER the registry lock is released: the engine answers it
    // synchronously — a group receiver emits its RTCP from inside the push, and that RTCP comes
    // straight back here (`send_group_feedback`) for the same lock. Holding it deadlocked the
    // receiving member's mesh thread (stand, 2026-10-05).
    let dialog_key = runtime.dialog_key.clone();
    drop(reg);
    if let Some((dialog_key, bytes)) = delivery {
        enqueue(&dialog_key, bytes);
    }
    if let Some((group_id, member_pub, capabilities)) = passport {
        if note_member_video_capabilities(db_path, &group_id, &member_pub, capabilities) {
            crate::reselect_group_media(db_path, &dialog_key);
        }
    }
    true
}

/// A participant's RTCP about some sender's stream. About OURS: open it with the reporter's key and
/// hand it to our media engine under [`GROUP_FEEDBACK_SHELL_TAG`], where the router gives it to our
/// sending session — the group twin of 1:1 feedback. About someone else's (cascade, we are on the
/// media path between them): relay it unopened one tree hop on toward that sender.
fn handle_incoming_group_feedback(
    db_path: &str,
    send: GroupMediaSend<'_>,
    payload: &[u8],
    feedback: &GroupFeedback<'_>,
    enqueue: &dyn Fn(&str, Vec<u8>),
    screen: &dyn Fn(&ScreenControlDelivery),
) {
    enum Next {
        Ours(String, Vec<u8>),
        Screen(ScreenControlDelivery),
        Relay([u8; 20], Vec<u8>),
    }
    let next = {
        let Ok(mut reg) = registry().lock() else { return };
        let Some(runtime) = reg
            .get_mut(db_path)
            .and_then(|calls| calls.values_mut().find(|r| call_tag_from_id(&r.call_id) == feedback.call_tag))
        else {
            note_media(db_path, |s| s.no_call += 1);
            return;
        };
        let (Some(reporter), Some(target)) = (
            runtime.member_by_sender_tag(&feedback.reporter_tag),
            runtime.member_by_sender_tag(&feedback.target_tag),
        ) else {
            note_media(db_path, |s| s.unknown_sender += 1);
            return;
        };
        if target.member_pub == runtime.my_pub && feedback.kind == GroupUpstreamKind::ScreenControl {
            let viewer = reporter.member_pub.clone();
            match open_group_screen_control(runtime, &viewer, feedback.sealed) {
                Some(delivery) => Next::Screen(delivery),
                None => {
                    note_media(db_path, |s| s.screen_control_refused += 1);
                    return;
                }
            }
        } else if target.member_pub == runtime.my_pub {
            let reporter_pub = reporter.member_pub.clone();
            match open_member_frame(runtime, &reporter_pub, feedback.sealed, current_unix_ms()) {
                Some(rtcp) => Next::Ours(runtime.dialog_key.clone(), rtcp),
                None => {
                    note_unopened(db_path);
                    return;
                }
            }
        } else {
            // Relayed only when its reporter signed it — checked without opening it (the replay
            // window is the target's): unchecked, any member had the forwarders carry its junk under
            // other members' tags (blind audit 2026-11-11, G4). Screen control is signed by the viewer
            // for the sharer; the rest is media-signed like a frame.
            let signed = if feedback.kind == GroupUpstreamKind::ScreenControl {
                match (decode_identity(&reporter.member_pub), decode_identity(&target.member_pub)) {
                    (Some(viewer), Some(sharer)) => open_batch(&viewer, &runtime.call_id, &sharer, feedback.sealed).is_some(),
                    _ => false,
                }
            } else {
                reporter
                    .verifier
                    .as_ref()
                    .is_some_and(|verifier| omni_crypto::verified_group_call_media(verifier, &runtime.call_id, feedback.sealed).is_some())
            };
            if !signed {
                note_unopened(db_path);
                return;
            }
            let hop = feedback_next_hop(runtime.tree.as_ref(), &runtime.my_pub, &target.member_pub)
                .and_then(|hop| runtime.nodes_for(&[hop]).pop());
            match (hop, prepare_group_feedback_forward(payload)) {
                (Some(hop), Some(relayed)) => Next::Relay(hop, relayed),
                _ => return, // no way on, or the hop budget is spent
            }
        }
    };
    match next {
        Next::Ours(dialog_key, rtcp) => {
            note_media(db_path, |s| s.feedback_received += 1);
            enqueue(&dialog_key, tag_group_rtp_for_shell(&GROUP_FEEDBACK_SHELL_TAG, &rtcp));
        }
        Next::Screen(delivery) => screen(&delivery),
        Next::Relay(hop, relayed) => {
            let sent = send(&hop, &relayed);
            if sent.is_ok() {
                note_media(db_path, |s| s.feedback_relayed += 1);
            } else {
                note_send(db_path, &hop, sent);
            }
        }
    }
}

/// A viewer's screen-control batch addressed to us: delivered only if we share, `viewer` is granted,
/// and `viewer` itself signed it for this call and for us. A group media key proves membership only —
/// any member could seal "as" another — so the identity signature is the check.
fn open_group_screen_control(runtime: &mut GroupCallRuntime, viewer: &str, signed: &[u8]) -> Option<ScreenControlDelivery> {
    let me = runtime.members.get(&runtime.my_pub)?;
    if !(me.screen_sharing && me.screen_control.as_ref().is_some_and(|grant| grant.allows(viewer))) {
        return None;
    }
    let batch = open_batch(&decode_identity(viewer)?, &runtime.call_id, &decode_identity(&runtime.my_pub)?, signed)?;
    let step = runtime.screen_rx.entry(viewer.to_string()).or_default().accept(&batch)?;
    Some(ScreenControlDelivery {
        dialog_key: runtime.dialog_key.clone(),
        viewer: viewer.to_string(),
        pointer: step.pointer,
        events: step.events,
        release: step.gap,
    })
}

/// Prefix a co-member's opened RTP with its 8-byte `sender_tag`, the framing the group-mode
/// receive loop in the shell splits on to route each remote sender's media to its own decoder
/// + video tile. The inverse (`payload[..8]` = tag, `payload[8..]` = RTP) is the shell's
/// contract; 1:1 media never carries this prefix. Kept a pure fn so the format is unit-tested
/// without standing up a mesh (see the tagging test).
fn tag_group_rtp_for_shell(sender_tag: &[u8; GROUP_MEDIA_TAG_LEN], rtp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sender_tag.len() + rtp.len());
    out.extend_from_slice(sender_tag);
    out.extend_from_slice(rtp);
    out
}

// --- registry helpers --------------------------------------------------------

fn snapshot_of(runtime: &GroupCallRuntime) -> GroupCallSnapshot {
    GroupCallSnapshot {
        call_id: runtime.call_id.clone(),
        group_id: runtime.group_id.clone(),
        dialog_key: runtime.dialog_key.clone(),
        participant_count: runtime.members.len(),
        topology: match runtime.topology {
            CallTopology::FullMesh => "full_mesh".to_string(),
            CallTopology::Cascade => "cascade".to_string(),
        },
        is_forwarder: runtime.is_forwarder(),
        tree_role: runtime.tree_role().to_string(),
        started_at_unix_ms: runtime.started_at_unix_ms,
        members: runtime
            .members
            .values()
            .map(|m| GroupCallMemberSnapshot {
                member_pub: m.member_pub.clone(),
                sender_tag_hex: hex::encode(m.sender_tag),
                is_self: m.member_pub == runtime.my_pub,
                is_muted: m.muted,
                is_camera_off: m.camera_off,
                is_screen_sharing: m.screen_sharing,
                lets_me_control: m.member_pub != runtime.my_pub
                    && m.screen_sharing
                    && m.screen_control.as_ref().is_some_and(|grant| grant.allows(&runtime.my_pub)),
            })
            .collect(),
        my_screen_control: runtime.members.get(&runtime.my_pub).and_then(|me| me.screen_control.clone()),
    }
}

fn store_runtime(db_path: &str, runtime: GroupCallRuntime) {
    if let Ok(mut reg) = registry().lock() {
        reg.entry(db_path.to_string())
            .or_default()
            .insert(runtime.group_id.clone(), runtime);
    }
}

fn take_runtime(db_path: &str, group_id: &str) -> Option<GroupCallRuntime> {
    registry()
        .lock()
        .ok()
        .and_then(|mut reg| reg.get_mut(db_path).and_then(|m| m.remove(group_id)))
}

/// Clone the runtime for read (media send path). Named for clarity at the call site.
fn mesh_runtime_and_state(db_path: &str, group_id: &str) -> Option<GroupCallRuntime> {
    registry()
        .lock()
        .ok()
        .and_then(|reg| reg.get(db_path).and_then(|m| m.get(group_id)).cloned())
}

/// Take `by` consecutive counters for our next sealed frames. `None` — the call is gone: nothing is
/// sealed then (a fallback counter would repeat a nonce under a key that is still valid).
fn bump_media_seq(db_path: &str, group_id: &str, by: u64) -> Option<u64> {
    let mut reg = registry().lock().ok()?;
    let runtime = reg.get_mut(db_path).and_then(|m| m.get_mut(group_id))?;
    Some(take_media_seqs(&mut runtime.next_media_seq, by))
}

fn take_media_seqs(next: &mut u64, by: u64) -> u64 {
    const COUNT: u64 = u32::MAX as u64;
    let mut start = *next;
    if (start & COUNT).saturating_add(by) > COUNT {
        start = fresh_media_seq_epoch(); // the count would run into the epoch: a new epoch
    }
    *next = start + by;
    start
}

/// The first counter of a fresh run of our media: an epoch in the high 32 bits, a count from 0 in the
/// low ones. A member's media key comes from the call secret and its member key only, so leaving the
/// call and joining it again keys its frames the same — a counter restarting at 0 would reuse
/// ChaCha20-Poly1305 nonces (keystream and tag forgery). The epoch is the current second, and never
/// the same twice in this process (`LAST_MEDIA_EPOCH`): every run's (key, nonce) are apart, and a
/// receiver can tell a newer run from a recording of an older one.
fn fresh_media_seq_epoch() -> u64 {
    let now = u32::try_from(current_unix_ms().max(0) / 1000).unwrap_or(u32::MAX);
    let mut last = LAST_MEDIA_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
    loop {
        let epoch = now.max(last.saturating_add(1));
        match LAST_MEDIA_EPOCH.compare_exchange_weak(
            last,
            epoch,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return u64::from(epoch) << 32,
            Err(seen) => last = seen,
        }
    }
}

/// The last media epoch this process handed out; process-wide on purpose (a monotone clock, not
/// per-profile state — nothing to clear).
static LAST_MEDIA_EPOCH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Replay protection for one co-member's frames: a sliding window over the counters of its newest
/// epoch. A frame of a newer epoch (the member joined again) starts a fresh window; one of an older
/// epoch can only be a recording and is refused. A newer epoch cannot be forged — the frame opened.
#[derive(Debug, Clone, Default)]
struct MemberMediaReplay {
    epoch: Option<u32>,
    window: crate::MediaReplayWindow,
}

impl MemberMediaReplay {
    fn accept(&mut self, seq: u64) -> bool {
        let epoch = (seq >> 32) as u32;
        match self.epoch {
            Some(current) if epoch < current => return false,
            Some(current) if epoch == current => {}
            _ => {
                self.epoch = Some(epoch);
                self.window = crate::MediaReplayWindow::default();
            }
        }
        self.window.accept(seq & u64::from(u32::MAX))
    }
}

/// One call participant from the group's member projection: routing node + per-sender media key.
/// `None` for a non-member or a member we have no node for yet (we can't route to them).
fn call_member(
    storage: &Storage,
    group_id: &str,
    member_pub: &str,
    call_secret: &[u8],
    self_node: [u8; 20],
) -> OmniResult<Option<CallMember>> {
    let Some(record) = storage
        .list_group_members(group_id)?
        .into_iter()
        .find(|m| m.member_pub == member_pub && m.status == "member")
    else {
        return Ok(None);
    };
    let node = match crate::app_groups::node_from_hex(&record.node_hint_hex) {
        Some(n) => *n.as_bytes(),
        None if self_node != [0u8; 20] => self_node,
        None => {
            crate::omni_app_trace_log(
                "group/call-member",
                &format!("group={group_id} member={} skipped: no node hint", short_pub(member_pub)),
            );
            return Ok(None);
        }
    };
    crate::omni_app_trace_log(
        "group/call-member",
        &format!("group={group_id} member={} node={}", short_pub(member_pub), hex::encode(&node[..4])),
    );
    Ok(Some(CallMember {
        sender_tag: sender_tag_from_pub(member_pub),
        media_key: omni_crypto::derive_group_call_media_key(call_secret, member_pub)?,
        verifier: media_verifier(member_pub),
        previous_media_key: None,
        node,
        member_pub: member_pub.to_string(),
        muted: false,
        camera_off: true,
        screen_sharing: false,
        screen_control: None,
        video_capabilities: None,
    }))
}

/// A call runtime with us + the given participants (only people actually in the call — media goes
/// to them, not to every group member).
fn build_runtime(
    storage: &Storage,
    group: &omni_storage::GroupRecord,
    call_id: &str,
    call_secret: &[u8],
    participants: &BTreeSet<String>,
    self_node: [u8; 20],
) -> OmniResult<GroupCallRuntime> {
    let my_pub = group.my_member_pub.as_str();
    let mut members = HashMap::new();
    // Ourselves even without a node hint on our own row (the founder's row has none).
    members.insert(
        my_pub.to_string(),
        CallMember {
            sender_tag: sender_tag_from_pub(my_pub),
            media_key: omni_crypto::derive_group_call_media_key(call_secret, my_pub)?,
            verifier: media_verifier(my_pub),
            previous_media_key: None,
            node: self_node,
            member_pub: my_pub.to_string(),
            muted: false,
            camera_off: true,
            screen_sharing: false,
            screen_control: None,
            video_capabilities: None,
        },
    );
    for member_pub in participants.iter().filter(|p| p.as_str() != my_pub) {
        if let Some(member) = call_member(storage, &group.group_id, member_pub, call_secret, [0u8; 20])? {
            members.insert(member_pub.clone(), member);
        }
    }
    let mut runtime = GroupCallRuntime {
        call_id: call_id.to_string(),
        call_secret: call_secret.to_vec(),
        group_id: group.group_id.clone(),
        dialog_key: group.dialog_key.clone(),
        my_pub: my_pub.to_string(),
        signer: crate::app_screen_control::local_signing_identity(storage)
            .ok()
            .map(|(seed, _)| omni_crypto::GroupCallMediaSigner::from_seed(&seed)),
        topology: CallTopology::FullMesh,
        tree: None,
        members,
        next_media_seq: fresh_media_seq_epoch(),
        media_replay: HashMap::new(),
        passport_sent_at_ms: 0,
        rekeyed_at_ms: 0,
        rekey_owed_at_ms: None,
        rekey_unsaid: None,
        started_at_unix_ms: current_unix_ms(),
        activity: HashMap::new(),
        screen_rx: HashMap::new(),
        screen_out: HashMap::new(),
        held_rekey: None,
    };
    runtime.rebuild_topology();
    Ok(runtime)
}

/// A member's frame verifier, from its member_pub (its Ed25519 key, base64).
fn media_verifier(member_pub: &str) -> Option<omni_crypto::GroupCallMediaVerifier> {
    base64::engine::general_purpose::STANDARD
        .decode(member_pub)
        .ok()
        .and_then(|key| omni_crypto::GroupCallMediaVerifier::from_public_key(&key))
}

/// Our sealed datagram, signed as ours for this call. `None` without our signing key — nothing goes
/// out unsigned.
fn signed_by_us(runtime: &GroupCallRuntime, sealed: Vec<u8>) -> Option<Vec<u8>> {
    runtime
        .signer
        .as_ref()
        .map(|signer| omni_crypto::sign_group_call_media(signer, &runtime.call_id, sealed))
}

fn short_pub(member_pub: &str) -> &str {
    member_pub.get(..12).unwrap_or(member_pub)
}

fn own_node_of(runtime: &GroupCallRuntime) -> [u8; 20] {
    runtime.members.get(&runtime.my_pub).map(|m| m.node).unwrap_or([0u8; 20])
}

#[cfg(test)]
mod group_call_media_tests {
    use super::*;

    /// Injector for tests that never reach screen control: delivering to it is a bug.
    fn no_screen(_: &ScreenControlDelivery) {
        panic!("nothing in this test may reach the screen-control injector");
    }

    // A receiver holding a co-member's media key opens a wrapped+sealed frame and
    // enqueues the recovered RTP to the group thread — the participant-role path of
    // handle_incoming_group_media, exercised without a mesh (forwarder skipped).
    fn member(pub_key: &str, node: u8, secret: &[u8]) -> CallMember {
        CallMember {
            member_pub: pub_key.to_string(),
            node: [node; 20],
            sender_tag: sender_tag_from_pub(pub_key),
            media_key: omni_crypto::derive_group_call_media_key(secret, pub_key).unwrap(),
            verifier: Some(test_verifier(pub_key)),
            previous_media_key: None,
            muted: false,
            camera_off: true,
            screen_sharing: false,
            screen_control: None,
            video_capabilities: None,
        }
    }

    /// A test member's signing key, from its name (the tests name members, not keys).
    fn test_seed(name: &str) -> [u8; 32] {
        use sha2::Digest as _;
        sha2::Sha256::digest(name.as_bytes()).into()
    }

    fn test_signer(name: &str) -> omni_crypto::GroupCallMediaSigner {
        omni_crypto::GroupCallMediaSigner::from_seed(&test_seed(name))
    }

    fn test_verifier(name: &str) -> omni_crypto::GroupCallMediaVerifier {
        let key = ed25519_dalek::SigningKey::from_bytes(&test_seed(name));
        omni_crypto::GroupCallMediaVerifier::from_public_key(&key.verifying_key().to_bytes()).unwrap()
    }

    /// A frame sealed with `key` and signed as `who` for `call_id` — what a member sends.
    fn signed_frame(who: &str, call_id: &str, key: &[u8], tag: u32, seq: u64, payload: &[u8]) -> Vec<u8> {
        let sealed = omni_crypto::seal_group_call_media(key, tag, seq, payload).unwrap();
        omni_crypto::sign_group_call_media(&test_signer(who), call_id, sealed)
    }

    /// A participant's RTCP about our stream reaches OUR engine under the feedback tag; one from
    /// someone outside the call (unknown reporter tag) or sealed with a wrong key is dropped.
    #[test]
    fn feedback_from_a_participant_reaches_our_sender_and_a_stranger_does_not() {
        let db_path = "test-db-feedback";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        members.insert("me".to_string(), member("me", 1, &secret));
        members.insert("bob".to_string(), member("bob", 2, &secret));
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-fb".to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: "me".to_string(),
                signer: Some(test_signer("me")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let delivered = std::sync::Mutex::new(Vec::<(String, Vec<u8>)>::new());
        let enqueue = |dialog: &str, bytes: Vec<u8>| delivered.lock().unwrap().push((dialog.to_string(), bytes));
        let pli = vec![0x81u8, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7];
        let feedback_from = |who: &str, key_secret: &[u8]| {
            let key = omni_crypto::derive_group_call_media_key(key_secret, who).unwrap();
            let sealed = signed_frame(who, "gcall-fb", &key, GROUP_FEEDBACK_SEAL_TAG, 0, &pli);
            let me = sender_tag_from_pub("me");
            wrap_group_feedback(GroupUpstreamKind::Feedback, &call_tag_from_id("gcall-fb"), &me, &sender_tag_from_pub(who), &sealed)
        };

        handle_incoming_group_feedback_for_test(db_path, &feedback_from("bob", &secret), &enqueue);
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "group:gid");
        assert_eq!(&got[0].1[..8], &GROUP_FEEDBACK_SHELL_TAG);
        assert_eq!(&got[0].1[8..], &pli[..]);

        // Negative halves: a stranger's tag, and Bob's tag under a key from another call.
        handle_incoming_group_feedback_for_test(db_path, &feedback_from("mallory", &secret), &enqueue);
        let other_call = omni_crypto::generate_group_key().to_vec();
        handle_incoming_group_feedback_for_test(db_path, &feedback_from("bob", &other_call), &enqueue);
        assert_eq!(delivered.lock().unwrap().len(), 1, "nothing else reaches our engine");

        let _ = take_runtime(db_path, "gid");
    }

    /// The engine answers a pushed frame synchronously — a group receiver emits RTCP from inside
    /// the push, and that RTCP comes back for the call registry. So the frame must be handed over
    /// with the registry UNLOCKED; holding it deadlocked the receiving member (stand, 2026-10-05).
    /// Negative half: a frame from someone outside the call is not handed over at all.
    #[test]
    fn a_frame_is_handed_to_the_engine_with_the_registry_unlocked() {
        let db_path = "test-db-unlocked-enqueue";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        members.insert("me".to_string(), member("me", 1, &secret));
        members.insert("bob".to_string(), member("bob", 2, &secret));
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-unlocked".to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: "me".to_string(),
                signer: Some(test_signer("me")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let frame_from = |who: &str| {
            let key = omni_crypto::derive_group_call_media_key(&secret, who).unwrap();
            let sealed = signed_frame(who, "gcall-unlocked", &key, 0x4F4D4E41, 0, &[0x80, 102, 0, 1]);
            let tag = sender_tag_from_pub(who);
            wrap_group_media(GROUP_MEDIA_DEFAULT_HOP_LIMIT, 0, &tag, &call_tag_from_id("gcall-unlocked"), &tag, &sealed)
        };
        let handed = std::sync::Mutex::new(Vec::<bool>::new());
        // Other tests share the global registry for moments; only THIS thread holding it never ends.
        let registry_free = || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                if registry().try_lock().is_ok() {
                    return true;
                }
                if std::time::Instant::now() > deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        let enqueue = |_: &str, _: Vec<u8>| handed.lock().unwrap().push(registry_free());
        let send = |_: &[u8; 20], _: &[u8]| Ok(());

        assert!(handle_incoming_group_media(db_path, &send, &frame_from("bob"), &enqueue, &no_screen));
        assert_eq!(*handed.lock().unwrap(), vec![true], "handed over once, registry free");
        assert!(handle_incoming_group_media(db_path, &send, &frame_from("mallory"), &enqueue, &no_screen));
        assert_eq!(handed.lock().unwrap().len(), 1, "a stranger's frame is not handed over");

        let _ = take_runtime(db_path, "gid");
    }

    fn handle_incoming_group_feedback_for_test(db_path: &str, wire: &[u8], enqueue: &dyn Fn(&str, Vec<u8>)) {
        let send = |_: &[u8; 20], _: &[u8]| -> Result<(), omni_mesh_runtime::MeshRuntimeError> {
            panic!("feedback about our own stream is never relayed")
        };
        assert!(handle_incoming_group_media(db_path, &send, wire, enqueue, &no_screen));
    }

    /// Cascade: feedback about another member's stream is relayed unopened one tree hop on toward
    /// that sender — the far PC heard b only through a, and its repair requests straight to b
    /// failed `no_link` (live, 2026-10-05). Negative halves: a spent hop budget and a stranger's
    /// feedback go nowhere, and none of it reaches our own engine.
    #[test]
    fn feedback_about_another_senders_stream_is_relayed_toward_it() {
        let db_path = "test-db-feedback-relay";
        let secret = omni_crypto::generate_group_key().to_vec();
        let names = ["a", "b", "c", "d", "e"];
        let mut members = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            members.insert(name.to_string(), member(name, i as u8 + 1, &secret));
        }
        let live: Vec<CallParticipantV1> = names
            .iter()
            .map(|k| CallParticipantV1 {
                member_pub: k.to_string(),
                is_live: true,
                uplink_tier: 2,
                can_forward: true,
            })
            .collect();
        // fanout 2: a → (b, c); b → (d, e). We are a; d reports about c's stream.
        let tree = CallTree::build(&live, 2);
        assert_eq!(tree.root(), Some("a"));
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-relay".to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: "a".to_string(),
                signer: Some(test_signer("a")),
                topology: CallTopology::Cascade,
                tree: Some(tree),
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let sent = std::sync::Mutex::new(Vec::<([u8; 20], Vec<u8>)>::new());
        let send = |node: &[u8; 20], bytes: &[u8]| {
            sent.lock().unwrap().push((*node, bytes.to_vec()));
            Ok(())
        };
        let enqueued = std::sync::Mutex::new(0usize);
        let enqueue = |_: &str, _: Vec<u8>| *enqueued.lock().unwrap() += 1;
        let feedback = |reporter: &str| {
            let key = omni_crypto::derive_group_call_media_key(&secret, reporter).unwrap();
            let sealed = signed_frame(reporter, "gcall-relay", &key, GROUP_FEEDBACK_SEAL_TAG, 0, b"nack");
            let call = call_tag_from_id("gcall-relay");
            wrap_group_feedback(GroupUpstreamKind::Feedback, &call, &sender_tag_from_pub("c"), &sender_tag_from_pub(reporter), &sealed)
        };

        let wire = feedback("d");
        assert!(handle_incoming_group_media(db_path, &send, &wire, &enqueue, &no_screen));
        let got = sent.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "relayed once");
        assert_eq!(got[0].0, [3; 20], "to c's node — the next hop toward c");
        assert_eq!(got[0].1, prepare_group_feedback_forward(&wire).unwrap(), "one hop spent, body untouched");

        // Under d's tag, sealed with d's media key (any member derives it), signed by e: not d's, so
        // not relayed (blind audit 2026-11-11, G4).
        let forged = {
            let key = omni_crypto::derive_group_call_media_key(&secret, "d").unwrap();
            let sealed = signed_frame("e", "gcall-relay", &key, GROUP_FEEDBACK_SEAL_TAG, 1, b"nack");
            wrap_group_feedback(GroupUpstreamKind::Feedback, &call_tag_from_id("gcall-relay"), &sender_tag_from_pub("c"), &sender_tag_from_pub("d"), &sealed)
        };
        assert!(handle_incoming_group_media(db_path, &send, &forged, &enqueue, &no_screen));
        assert_eq!(sent.lock().unwrap().len(), 1, "a feedback its reporter did not sign is not relayed");

        let mut spent = wire.clone();
        spent[2] = 0;
        assert!(handle_incoming_group_media(db_path, &send, &spent, &enqueue, &no_screen));
        assert!(handle_incoming_group_media(db_path, &send, &feedback("mallory"), &enqueue, &no_screen));
        assert_eq!(sent.lock().unwrap().len(), 1, "a spent budget and a stranger go nowhere");
        assert_eq!(*enqueued.lock().unwrap(), 0, "someone else's feedback never reaches our engine");

        let _ = take_runtime(db_path, "gid");
    }

    /// We share our screen and let `viewer` control it: the viewer's signed batch reaches our
    /// injector with its pointer and events. Negative halves: a member we did not grant, a batch
    /// forged under the viewer's tag with another key, a replay, and anything after the grant is
    /// withdrawn — none of it reaches the injector.
    #[test]
    fn only_a_granted_viewers_own_signed_input_reaches_the_injector() {
        use omni_protocol::{ScreenControlOutbox, ScreenMouseButton};
        let db_path = "test-db-screen-control";
        let secret = omni_crypto::generate_group_key().to_vec();
        let identity = |seed: u8| {
            let seed = [seed; 32];
            let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().to_bytes();
            (seed, public, B64.encode(public))
        };
        let (_, me_pub, me) = identity(1);
        let (viewer_seed, _, viewer) = identity(2);
        let (other_seed, _, other) = identity(3);
        let mut members = HashMap::new();
        for (name, node) in [(&me, 1u8), (&viewer, 2), (&other, 3)] {
            members.insert(name.clone(), member(name, node, &secret));
        }
        let mine = members.get_mut(&me).unwrap();
        mine.screen_sharing = true;
        mine.screen_control = Some(ScreenControlGrantV1 { everyone: false, members: vec![viewer.clone()] });
        let call_id = "gcall-screen";
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: call_id.to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: me.clone(),
                signer: Some(test_signer(&me)),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let delivered = std::sync::Mutex::new(Vec::<ScreenControlDelivery>::new());
        let screen = |d: &ScreenControlDelivery| delivered.lock().unwrap().push(d.clone());
        let send = |_: &[u8; 20], _: &[u8]| -> Result<(), omni_mesh_runtime::MeshRuntimeError> {
            panic!("input addressed to us is never relayed")
        };
        let enqueue = |_: &str, _: Vec<u8>| panic!("input never reaches the media engine");
        let click = [ScreenInputEvent::Button { button: ScreenMouseButton::Left, down: true }];
        let wire = |seed: &[u8; 32], tagged_as: &str, outbox: &mut ScreenControlOutbox| {
            let batch = outbox.batch(1_000, Some((300, 400)), &click);
            let signed = sign_batch(seed, call_id, &me_pub, &batch).unwrap();
            wrap_group_feedback(
                GroupUpstreamKind::ScreenControl,
                &call_tag_from_id(call_id),
                &sender_tag_from_pub(&me),
                &sender_tag_from_pub(tagged_as),
                &signed,
            )
        };

        let granted = wire(&viewer_seed, &viewer, &mut ScreenControlOutbox::default());
        assert!(handle_incoming_group_media(db_path, &send, &granted, &enqueue, &screen));
        let got = delivered.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].viewer.as_str(), got[0].pointer, got[0].events.as_slice(), got[0].release),
            (viewer.as_str(), Some((300, 400)), &click[..], false)
        );

        // Negative halves.
        assert!(handle_incoming_group_media(db_path, &send, &granted, &enqueue, &screen), "replay");
        let ungranted = wire(&other_seed, &other, &mut ScreenControlOutbox::default());
        assert!(handle_incoming_group_media(db_path, &send, &ungranted, &enqueue, &screen), "not granted");
        let forged = wire(&other_seed, &viewer, &mut ScreenControlOutbox::default());
        assert!(handle_incoming_group_media(db_path, &send, &forged, &enqueue, &screen), "forged as the viewer");
        assert_eq!(delivered.lock().unwrap().len(), 1, "replay, ungranted and forged input never reach the injector");
        if let Ok(mut reg) = registry().lock() {
            let runtime = reg.get_mut(db_path).and_then(|m| m.get_mut("gid")).unwrap();
            runtime.members.get_mut(&me).unwrap().screen_control = None;
        }
        let mut later = ScreenControlOutbox::default();
        later.batch(0, None, &[]);
        let after_withdrawal = wire(&viewer_seed, &viewer, &mut later);
        assert!(handle_incoming_group_media(db_path, &send, &after_withdrawal, &enqueue, &screen));
        assert_eq!(delivered.lock().unwrap().len(), 1, "a withdrawn grant lets nothing through");

        let _ = take_runtime(db_path, "gid");
    }

    #[test]
    fn participant_opens_comember_frame_and_enqueues_rtp() {
        let db_path = "test-db-participant";
        let secret = omni_crypto::generate_group_key().to_vec();
        let call_id = "gcall-x";
        let mut members = HashMap::new();
        members.insert("me".to_string(), member("me", 1, &secret));
        members.insert("bob".to_string(), member("bob", 2, &secret));
        let runtime = GroupCallRuntime {
            call_id: call_id.to_string(),
            call_secret: secret.to_vec(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: "me".to_string(),
            signer: Some(test_signer("me")),
            topology: CallTopology::FullMesh, // no forwarder role
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        store_runtime(db_path, runtime);

        // Bob seals an RTP frame with HIS media key + wraps it (origin ⇒ relay_tag == his sender_tag).
        let bob_key = omni_crypto::derive_group_call_media_key(&secret, "bob").unwrap();
        let rtp = vec![0x80u8, 0x60, 9, 8, 7, 6];
        let sealed = signed_frame("bob", call_id, &bob_key, 0x4F4D4E41, 0, &rtp);
        let wire = wrap_group_media(
            GROUP_MEDIA_DEFAULT_HOP_LIMIT,
            pack_media_meta(false, 0),
            &sender_tag_from_pub("bob"),
            &call_tag_from_id(call_id),
            &sender_tag_from_pub("bob"),
            &sealed,
        );

        // Exercise the participant-role decision the fn makes: parse → not-our-tag →
        // resolve the sender by tag → open with their key → recover the RTP.
        let (_hop, _mm, _rt, ct, st, body) = parse_group_media(&wire).expect("group media");
        assert_eq!(ct, call_tag_from_id(call_id));
        let reg = registry().lock().unwrap();
        let rt = reg.get(db_path).unwrap().get("gid").unwrap();
        assert_ne!(st, rt.members["me"].sender_tag);
        let sender = rt.member_by_sender_tag(&st).expect("known sender");
        let verified = omni_crypto::verified_group_call_media(sender.verifier.as_ref().unwrap(), call_id, body);
        let opened = verified.and_then(|sealed| omni_crypto::open_group_call_media(&sender.media_key, sealed).ok());
        drop(reg);
        assert_eq!(opened, Some(rtp.clone()), "Bob's signature holds and his key opens it");

        let _ = take_runtime(db_path, "gid");
    }

    /// A co-member's frame reaches the engine once: a relay replaying it, or a recording from the
    /// member's earlier run in this call (it left and joined again, same key), is dropped.
    #[test]
    fn a_comembers_frame_reaches_the_engine_once_and_an_older_run_never() {
        let db_path = "test-db-media-replay";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        members.insert("me".to_string(), member("me", 1, &secret));
        members.insert("bob".to_string(), member("bob", 2, &secret));
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-replay".to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: "me".to_string(),
                signer: Some(test_signer("me")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let bob_key = omni_crypto::derive_group_call_media_key(&secret, "bob").unwrap();
        let frame = |seq: u64| {
            let sealed = signed_frame("bob", "gcall-replay", &bob_key, 0x4F4D4E41, seq, &[0x80, 102, 0, 1]);
            let tag = sender_tag_from_pub("bob");
            wrap_group_media(GROUP_MEDIA_DEFAULT_HOP_LIMIT, 0, &tag, &call_tag_from_id("gcall-replay"), &tag, &sealed)
        };
        let handed = std::sync::Mutex::new(0usize);
        let enqueue = |_: &str, _: Vec<u8>| *handed.lock().unwrap() += 1;
        let send = |_: &[u8; 20], _: &[u8]| Ok(());
        let earlier_run = (7u64 << 32) | 5;
        let this_run = 8u64 << 32;

        for wire in [frame(earlier_run), frame(this_run), frame(this_run), frame(earlier_run), frame(this_run + 1)] {
            assert!(handle_incoming_group_media(db_path, &send, &wire, &enqueue, &no_screen));
        }
        assert_eq!(*handed.lock().unwrap(), 3, "earlier run once, this run's two frames once each");

        let _ = take_runtime(db_path, "gid");
    }

    /// Our counter never repeats under one key: every run starts a newer epoch (leaving the call and
    /// joining it again keys our frames the same), and a count that would run into the epoch bits
    /// moves to a newer one.
    #[test]
    fn every_run_of_our_media_starts_a_newer_epoch() {
        let first = fresh_media_seq_epoch();
        let second = fresh_media_seq_epoch();
        assert!(second >> 32 > first >> 32);
        assert_eq!(second & u64::from(u32::MAX), 0);

        let mut next = second;
        assert_eq!(take_media_seqs(&mut next, 10), second);
        assert_eq!(next, second + 10);

        let mut near_end = second | (u64::from(u32::MAX) - 2);
        let moved = take_media_seqs(&mut near_end, 5);
        assert!(moved >> 32 > second >> 32, "a new, newer epoch");
        assert_eq!(moved & u64::from(u32::MAX), 0);
        assert_eq!(near_end, moved + 5);
    }

    /// After a rekey a frame sealed under the old key still opens for a moment, then never again
    /// (the removed member knows the old secret — an open-ended grace would let them forge media).
    #[test]
    fn previous_key_opens_only_within_the_rekey_grace() {
        let old_secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        members.insert("bob".to_string(), member("bob", 2, &old_secret));
        let mut runtime = GroupCallRuntime {
            call_id: "c".to_string(),
            call_secret: old_secret.clone(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: "me".to_string(),
            signer: Some(test_signer("me")),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        let old_key = runtime.members["bob"].media_key;
        let old_frame = omni_crypto::seal_group_call_media(&old_key, 0x4F4D4E41, 1, b"rtp").unwrap();

        rekey_runtime(&mut runtime, omni_crypto::generate_group_key().to_vec(), 10_000).unwrap();
        let bob = &runtime.members["bob"];
        assert_ne!(bob.media_key, old_key);
        let new_frame = omni_crypto::seal_group_call_media(&bob.media_key, 0x4F4D4E41, 2, b"rtp").unwrap();
        assert!(open_member_media(bob, &new_frame, 10_001).is_some(), "new key opens");
        assert!(open_member_media(bob, &old_frame, 10_000 + REKEY_GRACE_MS).is_some(), "old key, inside the grace");
        assert!(open_member_media(bob, &old_frame, 10_001 + REKEY_GRACE_MS).is_none(), "old key, after it — refused");
    }

    /// A member who holds the call secret can derive Bob's media key and seal a frame under his tag;
    /// without Bob's signature it does not reach the engine as Bob's (security review 2026-11-09, F1).
    #[test]
    fn a_frame_sealed_with_bobs_key_but_not_signed_by_bob_is_not_bobs() {
        let db_path = "test-db-forged-sender";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for (name, node) in [("me", 1), ("bob", 2), ("mallory", 3)] {
            members.insert(name.to_string(), member(name, node, &secret));
        }
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-forge".to_string(),
                call_secret: secret.clone(),
                group_id: "gid".to_string(),
                dialog_key: "group:gid".to_string(),
                my_pub: "me".to_string(),
                signer: Some(test_signer("me")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let bob_key = omni_crypto::derive_group_call_media_key(&secret, "bob").unwrap();
        let as_bob = |signed_by: &str, seq: u64| {
            let sealed = signed_frame(signed_by, "gcall-forge", &bob_key, 0x4F4D4E41, seq, &[0x80, 102, 0, 1]);
            let tag = sender_tag_from_pub("bob");
            wrap_group_media(GROUP_MEDIA_DEFAULT_HOP_LIMIT, 0, &tag, &call_tag_from_id("gcall-forge"), &tag, &sealed)
        };
        let handed = std::sync::Mutex::new(0usize);
        let enqueue = |_: &str, _: Vec<u8>| *handed.lock().unwrap() += 1;
        let send = |_: &[u8; 20], _: &[u8]| -> Result<(), omni_mesh_runtime::MeshRuntimeError> { Ok(()) };
        assert!(handle_incoming_group_media(db_path, &send, &as_bob("mallory", 1), &enqueue, &no_screen));
        assert_eq!(*handed.lock().unwrap(), 0, "Mallory's frame under Bob's name is dropped");
        assert!(handle_incoming_group_media(db_path, &send, &as_bob("bob", 2), &enqueue, &no_screen));
        assert_eq!(*handed.lock().unwrap(), 1, "Bob's own frame goes through");
        let _ = take_runtime(db_path, "gid");
    }

    #[test]
    fn tag_group_rtp_prefixes_sender_then_rtp_for_shell_demux() {
        // The framing the group-mode receive loop splits on: sender_tag(8) ‖ rtp. Locks the
        // T1 contract T2 (C# per-tile demux) depends on — no mesh needed.
        let tag = sender_tag_from_pub("bob");
        let rtp = vec![0x80u8, 0x60, 9, 8, 7, 6];
        let tagged = tag_group_rtp_for_shell(&tag, &rtp);
        assert_eq!(tagged.len(), GROUP_MEDIA_TAG_LEN + rtp.len());
        assert_eq!(&tagged[..GROUP_MEDIA_TAG_LEN], &tag[..], "first 8 bytes are the sender tag");
        assert_eq!(&tagged[GROUP_MEDIA_TAG_LEN..], &rtp[..], "remainder is the untouched RTP");
    }

    /// Blind audit 2026-11-10: a cascade forwarder relayed a frame to all its downstream neighbours
    /// before opening it — an unknown sender's, or one sealed under a member's tag but not by it. Only
    /// a frame that opens goes on.
    #[test]
    fn a_cascade_forwarder_relays_only_frames_that_open() {
        let db_path = "test-db-cascade-unopened";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for (name, node) in [("aaa", 1), ("bbb", 2), ("ccc", 3), ("ddd", 4)] {
            members.insert(name.to_string(), member(name, node, &secret));
        }
        let mut runtime = GroupCallRuntime {
            call_id: "gcall-cascade".to_string(),
            call_secret: secret.clone(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: "aaa".to_string(),
            signer: Some(test_signer("aaa")),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        runtime.rebuild_topology();
        assert!(runtime.is_forwarder());
        store_runtime(db_path, runtime);
        let bbb_key = omni_crypto::derive_group_call_media_key(&secret, "bbb").unwrap();
        let frame = |sender: &str, signed_by: &str, seq: u64| {
            let sealed = signed_frame(signed_by, "gcall-cascade", &bbb_key, 0x4F4D4E41, seq, &[0x80, 102, 0, 1]);
            let tag = sender_tag_from_pub(sender);
            wrap_group_media(GROUP_MEDIA_DEFAULT_HOP_LIMIT, 0, &tag, &call_tag_from_id("gcall-cascade"), &tag, &sealed)
        };
        let relayed = std::sync::Mutex::new(0usize);
        let send = |_: &[u8; 20], _: &[u8]| -> Result<(), omni_mesh_runtime::MeshRuntimeError> {
            *relayed.lock().unwrap() += 1;
            Ok(())
        };
        let enqueue = |_: &str, _: Vec<u8>| {};
        assert!(handle_incoming_group_media(db_path, &send, &frame("nobody", "nobody", 1), &enqueue, &no_screen));
        assert_eq!(*relayed.lock().unwrap(), 0, "an unknown sender's frame is not relayed");
        assert!(handle_incoming_group_media(db_path, &send, &frame("bbb", "ccc", 2), &enqueue, &no_screen));
        assert_eq!(*relayed.lock().unwrap(), 0, "a frame under bbb's tag not signed by bbb is not relayed");
        assert!(handle_incoming_group_media(db_path, &send, &frame("aaa", "aaa", 4), &enqueue, &no_screen));
        assert_eq!(*relayed.lock().unwrap(), 0, "a frame under our own tag that came in is not ours, and goes nowhere");
        assert!(handle_incoming_group_media(db_path, &send, &frame("bbb", "bbb", 3), &enqueue, &no_screen));
        assert!(*relayed.lock().unwrap() > 0, "bbb's own frame goes on");
        let _ = take_runtime(db_path, "gid");
    }

    /// Blind audit 2026-11-11, M3: the one whose turn it was to rekey after a removal was silent (it
    /// had dropped out without a Leave), and the call kept the secret the removed member held. The
    /// next in turn rekeys when its time comes; rival rekeys end on one secret, the smaller.
    #[test]
    fn a_call_is_rekeyed_by_the_next_in_turn_when_the_first_is_silent() {
        let db_path = "test-db-rekey-turn";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for (name, node) in [("aaa", 1), ("bbb", 2), ("ccc", 3)] {
            members.insert(name.to_string(), member(name, node, &secret));
        }
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-turn".to_string(),
                call_secret: secret.clone(),
                group_id: "gid-turn".to_string(),
                dialog_key: "group:gid-turn".to_string(),
                my_pub: "bbb".to_string(),
                signer: Some(test_signer("bbb")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let now = current_unix_ms();
        assert!(on_group_members_removed(db_path, "gid-turn", &["zzz".to_string()]).unwrap().is_none(), "aaa's turn first");
        assert!(take_due_group_call_rekeys(db_path, now).is_empty(), "not ours yet");
        let due = take_due_group_call_rekeys(db_path, now + REKEY_TURN_MS + 1_000);
        assert_eq!(due.len(), 1, "aaa was silent: ours");
        assert_eq!(due[0].1.control, GroupCallControlKindV1::Rekey);
        assert!(take_due_group_call_rekeys(db_path, now + 10 * REKEY_TURN_MS).is_empty(), "once");
        let runtime = take_runtime(db_path, "gid-turn").unwrap();
        assert_ne!(runtime.call_secret, secret, "a new secret");

        // Rivals within a round: the smaller secret, whichever came first.
        // Our turn is not taken by a Rekey whose secret is not: the old one, sent again.
        let mut owing = runtime;
        owing.rekey_owed_at_ms = Some(now);
        let held = owing.call_secret.clone();
        take_received_rekey(&mut owing, held.clone(), now, now).unwrap();
        assert_eq!(owing.rekey_owed_at_ms, Some(now), "still ours to make");
        // One whose secret is taken does take it.
        let later = owing.rekeyed_at_ms + REKEY_ROUND_MS;
        take_received_rekey(&mut owing, vec![5u8; 32], later, later).unwrap();
        assert_eq!(owing.rekey_owed_at_ms, None);
        assert_eq!(owing.call_secret, vec![5u8; 32]);
        // An old one that comes late — said a round before the one we hold — is not (pass 14):
        // it took the call back to a secret the others had left.
        take_received_rekey(&mut owing, vec![1u8; 32], later - REKEY_ROUND_MS, later + 10 * REKEY_ROUND_MS).unwrap();
        assert_eq!(owing.call_secret, vec![5u8; 32], "an old Rekey does not take the call back");
        // Nor one dated far ahead of our clock.
        take_received_rekey(&mut owing, vec![9u8; 32], later + 2 * REKEY_CLOCK_AHEAD_MS, later).unwrap();
        assert_eq!(owing.call_secret, vec![5u8; 32]);
        // A rekey we made and could not say is said again as it is — the same secret, not a new
        // one each time (pass 14).
        owing.rekey_owed_at_ms = Some(0);
        store_runtime(db_path, owing);
        let made = take_due_group_call_rekeys(db_path, later);
        assert_eq!(made.len(), 1);
        owe_group_call_rekey(db_path, "gid-turn", made[0].1.clone());
        let again = take_due_group_call_rekeys(db_path, later);
        assert_eq!(again.len(), 1, "said again on the next tick");
        assert_eq!(again[0].1.call_secret_b64, made[0].1.call_secret_b64, "the same secret");
        assert!(take_due_group_call_rekeys(db_path, later).is_empty(), "once");
        take_runtime(db_path, "gid-turn").unwrap();
        let (small, large) = (vec![1u8; 32], vec![2u8; 32]);
        assert!(takes_rekey(&secret, 0, &large, 100), "the first after a removal");
        assert!(takes_rekey(&large, 100, &small, 200), "a smaller rival");
        assert!(!takes_rekey(&small, 100, &large, 200), "a larger rival");
        assert!(takes_rekey(&small, 100, &large, 100 + REKEY_ROUND_MS), "a later change");
        // A time from far before ours wrapped the difference (pass 15), and no time is no Rekey.
        let ours = 1_800_000_000_000;
        assert!(!takes_rekey(&small, ours, &large, i64::MIN), "i64::MIN is not a later change");
        assert!(!takes_rekey(&large, ours, &small, i64::MIN), "nor a rival");
        assert!(!takes_rekey(&small, ours, &large, 0), "0 is no time");
        assert!(!takes_rekey(&secret, 0, &large, -5), "not even the first after a removal");
    }

    /// Blind audit 2026-11-11, L3: a call we knew of and had not joined stayed known when its last
    /// participant was removed from the group; «start a call» then joined it, under the secret the
    /// removed one held.
    #[test]
    fn a_known_call_whose_last_participant_was_removed_is_forgotten() {
        let db_path = "test-db-known-call-removed";
        let secret = omni_crypto::generate_group_key().to_vec();
        let invite = call_control("gcall-known", GroupCallControlKindV1::Invite, Some(&secret));
        note_known_call(db_path, "gid-known", "mmm", &invite, Some(secret));
        let known = || known_calls().lock().unwrap().get(db_path).is_some_and(|calls| calls.contains_key("gid-known"));
        assert!(known());
        on_group_members_removed(db_path, "gid-known", &["mmm".to_string()]).unwrap();
        assert!(!known(), "nobody left in it: it is over");
    }

    /// Blind audit 2026-11-11, L3: the Invite went to the whole group, so a member removed while out
    /// of the call still held its secret — and no rekey came, the call kept that secret.
    #[test]
    fn removing_a_member_who_was_not_in_the_call_rekeys_it() {
        let db_path = "test-db-rekey-outsider";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for (name, node) in [("aaa", 1), ("bbb", 2)] {
            members.insert(name.to_string(), member(name, node, &secret));
        }
        store_runtime(
            db_path,
            GroupCallRuntime {
                call_id: "gcall-rekey".to_string(),
                call_secret: secret.clone(),
                group_id: "gid-rekey".to_string(),
                dialog_key: "group:gid-rekey".to_string(),
                my_pub: "aaa".to_string(),
                signer: Some(test_signer("aaa")),
                topology: CallTopology::FullMesh,
                tree: None,
                members,
                next_media_seq: 0,
                media_replay: HashMap::new(),
                passport_sent_at_ms: 0,
                rekeyed_at_ms: 0,
                rekey_owed_at_ms: None,
                rekey_unsaid: None,
                started_at_unix_ms: 0,
                activity: HashMap::new(),
                screen_rx: HashMap::new(),
                screen_out: HashMap::new(),
                held_rekey: None,
            },
        );
        let rekey = on_group_members_removed(db_path, "gid-rekey", &["zzz-out-of-the-call".to_string()]).unwrap();
        assert!(rekey.is_some_and(|control| control.control == GroupCallControlKindV1::Rekey), "rekeyed");
        let runtime = take_runtime(db_path, "gid-rekey").unwrap();
        assert_ne!(runtime.call_secret, secret, "a new secret");
    }

    /// Blind audit 2026-11-11, F5: the activity a frame claims counted before the frame was checked —
    /// frames forged under other members' tags kept silent members on top of the active-speaker ranking.
    /// Only a frame that opens counts.
    #[test]
    fn a_frame_that_does_not_open_moves_no_one_in_the_speaker_ranking() {
        let db_path = "test-db-cascade-activity";
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for (name, node) in [("aaa", 1), ("bbb", 2), ("ccc", 3), ("ddd", 4)] {
            members.insert(name.to_string(), member(name, node, &secret));
        }
        let mut runtime = GroupCallRuntime {
            call_id: "gcall-activity".to_string(),
            call_secret: secret.clone(),
            group_id: "gid-activity".to_string(),
            dialog_key: "group:gid-activity".to_string(),
            my_pub: "aaa".to_string(),
            signer: Some(test_signer("aaa")),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        runtime.rebuild_topology();
        store_runtime(db_path, runtime);
        let frame = |sender: &str, signed_by: &str, seq: u64| {
            let key = omni_crypto::derive_group_call_media_key(&secret, sender).unwrap();
            let sealed = signed_frame(signed_by, "gcall-activity", &key, 0x4F4D4E41, seq, &[0x80, 111, 0, 1]);
            let tag = sender_tag_from_pub(sender);
            let loud = omni_protocol::pack_media_meta(false, 120);
            wrap_group_media(GROUP_MEDIA_DEFAULT_HOP_LIMIT, loud, &tag, &call_tag_from_id("gcall-activity"), &tag, &sealed)
        };
        let send = |_: &[u8; 20], _: &[u8]| -> Result<(), omni_mesh_runtime::MeshRuntimeError> { Ok(()) };
        let enqueue = |_: &str, _: Vec<u8>| {};
        assert!(handle_incoming_group_media(db_path, &send, &frame("ccc", "ddd", 1), &enqueue, &no_screen));
        assert!(handle_incoming_group_media(db_path, &send, &frame("bbb", "bbb", 2), &enqueue, &no_screen));
        let runtime = take_runtime(db_path, "gid-activity").unwrap();
        assert!(!runtime.activity.contains_key("ccc"), "a forged frame under ccc's tag counts nothing for ccc");
        assert!(runtime.activity.contains_key("bbb"), "bbb's own frame counts");
    }

    #[test]
    fn cascade_forwarder_floods_to_neighbours_except_arrival() {
        // A height-1 cascade tree: with equal tier/forward the root is the smallest member_pub, so
        // "aaa" is the root and "bbb"/"ccc"/"ddd" are its leaves. The root, relaying a frame that
        // arrived from child "bbb" (carried in relay_tag), floods to its OTHER children ccc,ddd —
        // never back to bbb, never self.
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        members.insert("aaa".to_string(), member("aaa", 1, &secret));
        members.insert("bbb".to_string(), member("bbb", 2, &secret));
        members.insert("ccc".to_string(), member("ccc", 3, &secret));
        members.insert("ddd".to_string(), member("ddd", 4, &secret));
        let mut runtime = GroupCallRuntime {
            call_id: "c".to_string(),
            call_secret: secret.to_vec(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: "aaa".to_string(),
            signer: Some(test_signer("aaa")),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        runtime.rebuild_topology();
        assert_eq!(runtime.topology, CallTopology::Cascade);
        assert_eq!(runtime.tree.as_ref().unwrap().root(), Some("aaa"));
        assert!(runtime.is_forwarder());
        assert_eq!(runtime.tree_role(), "root");

        // A frame relayed by "bbb" (relay_tag = bbb's sender_tag) → arrival resolves to "bbb".
        let arrival = runtime
            .member_by_sender_tag(&sender_tag_from_pub("bbb"))
            .map(|m| m.member_pub.clone());
        assert_eq!(arrival.as_deref(), Some("bbb"));
        let mut relay_targets = runtime.media_targets(arrival.as_deref());
        relay_targets.sort();
        assert_eq!(relay_targets, vec!["ccc".to_string(), "ddd".to_string()]);

        // Our own media (arrival None) floods to every child.
        let mut own = runtime.media_targets(None);
        own.sort();
        assert_eq!(own, vec!["bbb".to_string(), "ccc".to_string(), "ddd".to_string()]);
    }

    #[test]
    fn a_full_call_lets_nobody_new_in_and_keeps_those_already_in() {
        let secret = omni_crypto::generate_group_key().to_vec();
        let pubs: Vec<String> = (0..GROUP_CALL_MAX).map(|i| format!("m{i:02}")).collect();
        let mut members = HashMap::new();
        for pk in &pubs {
            members.insert(pk.clone(), member(pk, 1, &secret));
        }
        let mut rt = GroupCallRuntime {
            call_id: "c".to_string(),
            call_secret: secret.to_vec(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: pubs[0].clone(),
            signer: Some(test_signer(&pubs[0])),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        assert!(!rt.has_room_for("newcomer"), "the call is at the cap");
        assert!(rt.has_room_for(&pubs[5]), "someone already in is never turned away");
        rt.members.remove(&pubs[5]);
        assert!(rt.has_room_for("newcomer"), "a seat freed by a Leave is free again");

        // The joiner's side: a known call with the cap of others in it is full; us among them is not
        // counted (we are not in it yet, a stale entry of ours must not block us).
        let db_path = "test-db-full-call";
        let participants: BTreeSet<String> = pubs.iter().cloned().collect();
        let known = |participants: BTreeSet<String>| KnownCall {
            call_id: "c".to_string(),
            call_secret: secret.clone(),
            started_by: String::new(),
            started_at_unix_ms: 0,
            participants,
            rings: false,
            dismissed: false,
            rekeyed_at_ms: 0,
            held_rekey: None,
        };
        known_calls()
            .lock()
            .unwrap()
            .entry(db_path.to_string())
            .or_default()
            .insert("gid".to_string(), known(participants.clone()));
        assert!(known_call_is_full(db_path, "gid", "me"));
        assert!(!known_call_is_full(db_path, "gid", &pubs[0]), "only others count");
        let mut one_less = participants;
        one_less.remove(&pubs[0]);
        known_calls().lock().unwrap().get_mut(db_path).unwrap().insert("gid".to_string(), known(one_less));
        assert!(!known_call_is_full(db_path, "gid", "me"), "the last seat is ours to take");
        assert!(!known_call_is_full(db_path, "other-group", "me"));
        let _ = take_known(db_path, "gid");
    }

    #[test]
    fn active_speaker_activity_tracking_ranks_and_gates() {
        let secret = omni_crypto::generate_group_key().to_vec();
        let mut members = HashMap::new();
        for pk in ["aaa", "bbb", "ccc", "ddd", "eee", "fff"] {
            members.insert(pk.to_string(), member(pk, 1, &secret));
        }
        let mut rt = GroupCallRuntime {
            call_id: "c".to_string(),
            call_secret: secret.to_vec(),
            group_id: "gid".to_string(),
            dialog_key: "group:gid".to_string(),
            my_pub: "aaa".to_string(),
            signer: Some(test_signer("aaa")),
            topology: CallTopology::FullMesh,
            tree: None,
            members,
            next_media_seq: 0,
            media_replay: HashMap::new(),
            passport_sent_at_ms: 0,
            rekeyed_at_ms: 0,
            rekey_owed_at_ms: None,
            rekey_unsaid: None,
            started_at_unix_ms: 0,
            activity: HashMap::new(),
            screen_rx: HashMap::new(),
            screen_out: HashMap::new(),
            held_rekey: None,
        };
        rt.rebuild_topology();
        let now = 10_000i64;

        // No activity recorded → gate fails open (un-gated cascade): every sender relayed.
        assert!(rt.forward_gate(false, "bbb", now));
        assert!(rt.forward_gate(true, "bbb", now));

        // Five senders active with distinct levels (bbb loudest … fff quietest).
        rt.note_activity("bbb", 90, now);
        rt.note_activity("ccc", 70, now);
        rt.note_activity("ddd", 50, now);
        rt.note_activity("eee", 30, now);
        rt.note_activity("fff", 10, now);
        assert_eq!(rt.ranked_active(now), vec!["bbb", "ccc", "ddd", "eee", "fff"]);

        // Audio K=4: top-4 (bbb..eee) relayed, the 5th-loudest (fff) dropped.
        assert!(rt.forward_gate(false, "bbb", now));
        assert!(rt.forward_gate(false, "eee", now));
        assert!(!rt.forward_gate(false, "fff", now), "quietest of 5 active: audio dropped");
        // Video K=4 likewise: the four most active stay live, the fifth's video dropped.
        assert!(rt.forward_gate(true, "eee", now));
        assert!(!rt.forward_gate(true, "fff", now), "quietest of 5 active: video dropped");

        // Staleness: past the active window all entries expire → nobody active → fail open again.
        let later = now + ACTIVE_WINDOW_MS + 1;
        assert!(rt.ranked_active(later).is_empty());
        assert!(rt.forward_gate(false, "fff", later));

        // A level-0 update clears an entry (the sender went silent).
        rt.note_activity("bbb", 0, now);
        assert!(!rt.ranked_active(now).contains(&"bbb".to_string()));

        // Our own level (C1c) is what our frames carry, and silence once it is stale.
        let me = rt.my_pub.clone();
        assert_eq!(rt.own_activity(now), 0, "nothing reported yet");
        rt.note_activity(&me, 40, now);
        assert_eq!(rt.own_activity(now), 40);
        assert_eq!(rt.own_activity(later), 0, "a level the shell stopped reporting is silence");
    }

    /// Our frames are told apart by the native Opus sender's RTP: SSRC "OMNA", PT 111 — audio;
    /// anything else (camera, screen) — video.
    #[test]
    fn own_audio_rtp_is_told_from_video_by_its_ssrc_and_payload_type() {
        let rtp = |pt: u8, ssrc: u32| {
            let mut packet = vec![0x80, pt, 0, 1, 0, 0, 0, 0];
            packet.extend_from_slice(&ssrc.to_be_bytes());
            packet.extend_from_slice(&[0xAA; 20]);
            packet
        };
        assert!(is_own_audio_rtp(&rtp(111, 0x4F4D_4E41)));
        assert!(is_own_audio_rtp(&rtp(0x80 | 111, 0x4F4D_4E41)), "the marker bit is not the type");
        assert!(!is_own_audio_rtp(&rtp(96, 0x4F4D_4E41)), "another type is video");
        assert!(!is_own_audio_rtp(&rtp(111, 0x1234_5678)), "another stream is video");
        assert!(!is_own_audio_rtp(&[0x80, 111]), "too short");
    }
}
