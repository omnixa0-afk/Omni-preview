// Group-call topology + cascade forwarder tree (G5/G6) — pure, deterministic
// logic for the peer-SFU model. См. docs/decisions/group-call-cascade-sfu.md +
// docs/decisions/group-envelope-and-transport.md §5 +
// docs/architecture/group-chat-and-calls-plan.md §4.
//
// The rule the user set: NEVER full-mesh for larger calls. So:
//   - N ≤ FULL_MESH_MAX (3): full mesh — each sends to each. Cheap, no forwarder.
//   - N ≥ 4: a DETERMINISTIC cascade tree of peer forwarders (peer-SFU). Every
//     participant builds the SAME tree from the same member list, so each knows
//     its parent (uplink) and children (fan-out). Media floods the tree — forward
//     to every tree-neighbour EXCEPT the one it arrived from — so each frame
//     reaches everyone exactly once (tree ⇒ loop-free). Forwarders are keyless
//     mesh nodes (they read only routing tags) → E2E holds on every hop.
//
// The tree is a deterministic function of the live participant set, so there is
// no coordinator and no forwarder-election signal: on join/leave everyone
// rebuilds the same tree → the call self-heals. A single peer forwarder is just a
// height-1 tree, so the old single-forwarder path is fully subsumed by this.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Full mesh is fine up to this many participants; at/above +1 we cascade.
pub const GROUP_CALL_FULL_MESH_MAX: usize = 3;
/// Sanity cap on group-call size. Raised from 8 → 32 with cascade (deterministic E2E forwarder tree):
/// the single-forwarder O(N²) bottleneck is gone, so larger friend-groups are supported. 32 is a
/// sanity bound (not a webinar) — download stays N-1 streams, so audio scales to tens, video needs
/// active-speaker/SVC (C1). See docs/decisions/group-call-cascade-sfu.md.
pub const GROUP_CALL_MAX: usize = 32;
/// Max children per cascade-tree node (fanout). Bounds each forwarder's fan-out; deeper tree spreads
/// load across more nodes. 4 = balance of depth vs per-node load.
pub const GROUP_CALL_FANOUT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTopology {
    /// N ≤ 3: everyone sends to everyone directly.
    FullMesh,
    /// N ≥ 4: a deterministic cascade tree of peer forwarders relays encrypted media.
    Cascade,
}

/// A candidate for a forwarding role — what the cascade tree ranks on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallParticipantV1 {
    /// Identity signing pub (base64) — the participant's group identity + tie-break key.
    pub member_pub: String,
    /// Whether the participant currently has a live media path (dead ones can't forward).
    pub is_live: bool,
    /// Self-reported uplink tier: higher = better (e.g. wired/strong = 3, wifi = 2,
    /// cellular = 1, constrained = 0). Advisory; ties broken by member_pub.
    pub uplink_tier: u8,
    /// Whether this participant is willing to forward (opt-out for a weak device).
    pub can_forward: bool,
}

/// Decide the topology for a participant count.
pub fn decide_call_topology(participant_count: usize) -> CallTopology {
    if participant_count <= GROUP_CALL_FULL_MESH_MAX {
        CallTopology::FullMesh
    } else {
        CallTopology::Cascade
    }
}

// --- Cascade forwarder tree (peer-SFU) --------------------------------------
// Generalises a single peer-forwarder to a DETERMINISTIC tree so group calls scale past 8 without a
// coordinator or a dedicated server. Every participant builds the SAME tree from the same participant
// list, so each knows its parent (uplink target) and children (fan-out); media floods the tree —
// forward to every tree-neighbour EXCEPT the one it arrived from — so each frame reaches everyone
// exactly once (tree ⇒ loop-free). A single forwarder is just a height-1 tree. Forwarders stay keyless
// (they read only routing tags) → E2E holds on every hop.

/// The deterministic group-call forwarder tree. Built from the live participants via a fanout-`F`
/// array embedding over the rank order `(can_forward desc, uplink_tier desc, member_pub asc)`: strong
/// / willing nodes take the low indices (internal → carry forwarding), weak / opted-out nodes fall to
/// the high indices (leaves, fan-out 0). Index `i`'s parent is `(i-1)/F`; its children are
/// `i*F+1 ..= i*F+F`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallTree {
    /// Rank order — index 0 is the root. Deterministic for a given participant set.
    order: Vec<String>,
    fanout: usize,
}

impl CallTree {
    /// Build the tree over the LIVE participants (dead ones can't be in the topology).
    pub fn build(participants: &[CallParticipantV1], fanout: usize) -> Self {
        let mut live: Vec<&CallParticipantV1> = participants.iter().filter(|p| p.is_live).collect();
        live.sort_by(|a, b| {
            b.can_forward
                .cmp(&a.can_forward) // willing forwarders first (so they land internal)
                .then_with(|| b.uplink_tier.cmp(&a.uplink_tier)) // strongest uplink first
                .then_with(|| a.member_pub.cmp(&b.member_pub)) // stable tie-break: smaller pub first
        });
        Self {
            order: live.into_iter().map(|p| p.member_pub.clone()).collect(),
            fanout: fanout.max(1),
        }
    }

    fn index_of(&self, member: &str) -> Option<usize> {
        self.order.iter().position(|m| m == member)
    }

    /// The root (top-ranked) member, if any.
    pub fn root(&self) -> Option<&str> {
        self.order.first().map(String::as_str)
    }

    /// The member `member` sends its uplink to. `None` for the root (or an unknown member).
    pub fn parent(&self, member: &str) -> Option<&str> {
        let i = self.index_of(member)?;
        if i == 0 {
            return None;
        }
        self.order.get((i - 1) / self.fanout).map(String::as_str)
    }

    /// The members `member` fans media out to (empty ⇒ `member` is a leaf).
    pub fn children(&self, member: &str) -> Vec<&str> {
        let Some(i) = self.index_of(member) else {
            return Vec::new();
        };
        (1..=self.fanout)
            .filter_map(|k| self.order.get(i * self.fanout + k).map(String::as_str))
            .collect()
    }

    /// All tree-neighbours of `member`: parent + children.
    pub fn neighbors(&self, member: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(p) = self.parent(member) {
            out.push(p.to_string());
        }
        out.extend(self.children(member).into_iter().map(str::to_string));
        out
    }

    /// The tree-neighbour of `from` on the tree path to `to`: the child whose subtree holds `to`, or
    /// else `from`'s parent. `None` if either is not in the tree or they are the same member.
    pub fn next_hop(&self, from: &str, to: &str) -> Option<&str> {
        let from_at = self.index_of(from)?;
        let mut at = self.index_of(to)?;
        if at == from_at {
            return None;
        }
        while at != 0 {
            let parent = (at - 1) / self.fanout;
            if parent == from_at {
                return Some(self.order[at].as_str());
            }
            at = parent;
        }
        self.parent(from)
    }

    /// True if `member` forwards for others (has children / is internal).
    pub fn is_forwarder(&self, member: &str) -> bool {
        !self.children(member).is_empty()
    }

    /// Rebuild the tree over the survivors after `dropped` left — same deterministic function, so
    /// every remaining participant lands on the SAME new tree (the failover contract).
    pub fn rebuild_without(
        participants: &[CallParticipantV1],
        dropped: &[String],
        fanout: usize,
    ) -> Self {
        let survivors: Vec<CallParticipantV1> = participants
            .iter()
            .filter(|p| !dropped.contains(&p.member_pub))
            .cloned()
            .collect();
        Self::build(&survivors, fanout)
    }
}

/// Whom a participant sends a media datagram to over the cascade tree:
///   - own media (`arrival == None`): ALL tree-neighbours (parent + children).
///   - forwarding a received frame (`arrival == Some(neighbour)`): all neighbours EXCEPT `arrival`
///     — spanning-tree flood, so each frame reaches everyone exactly once and never loops.
pub fn tree_send_targets(tree: &CallTree, me: &str, arrival: Option<&str>) -> Vec<String> {
    tree.neighbors(me)
        .into_iter()
        .filter(|n| Some(n.as_str()) != arrival)
        .collect()
}

// --- Group-call media routing envelope --------------------------------------
// A group-call media datagram wraps the SFrame-sealed frame with a thin routing
// header the cascade forwarders read WITHOUT any key:
//   `[MAGIC(2) ‖ hop_limit(1) ‖ relay_tag(8) ‖ call_tag(8) ‖ sender_tag(8) ‖ sealed]`.
// A forwarder matches `call_tag` to a live call, maps `relay_tag` to the tree-neighbour it arrived
// from (so it can flood to its OTHER neighbours — never back), decrements `hop_limit`, overwrites
// `relay_tag` with its own, and re-addresses the WHOLE datagram to those neighbours. The recipient
// matches `sender_tag` to a participant → that sender's media key → opens.
//
// Why `relay_tag` (not the mesh source address): the immediate tree-neighbour that relayed the frame
// is what the spanning-tree flood must exclude, but the mesh `from_node` is a transport hop — it does
// not reliably identify the tree-neighbour once the tree is multi-hop (a member's advertised node hint
// can differ from the address a datagram actually arrives on). So each hop stamps its own identity tag
// into `relay_tag`; the origin sets `relay_tag == sender_tag` (it is its own last hop). `hop_limit`
// bounds the flood so a transient tree inconsistency during join/leave/failover can't loop a frame
// forever. The magic distinguishes group-call media from 1:1 media on the wire.

pub const GROUP_MEDIA_MAGIC: [u8; 2] = [0x47, 0x4D]; // "GM"
pub const GROUP_MEDIA_TAG_LEN: usize = 8;
/// Header layout: MAGIC(2) + hop_limit(1) + media_meta(1) + relay_tag(8) + call_tag(8) + sender_tag(8).
pub const GROUP_MEDIA_HEADER_LEN: usize = 2 + 1 + 1 + GROUP_MEDIA_TAG_LEN * 3;
const MEDIA_META_OFFSET: usize = 3;
const RELAY_TAG_OFFSET: usize = 4;
const CALL_TAG_OFFSET: usize = RELAY_TAG_OFFSET + GROUP_MEDIA_TAG_LEN;
const SENDER_TAG_OFFSET: usize = CALL_TAG_OFFSET + GROUP_MEDIA_TAG_LEN;
/// Starting hop budget for a group-media datagram — cascade tree height + margin. Decremented at each
/// forwarder; a frame at 0 is dropped (loop guard). 8 covers deep trees (fanout 4 → thousands).
pub const GROUP_MEDIA_DEFAULT_HOP_LIMIT: u8 = 8;

// --- Active-speaker gating (C1) ---------------------------------------------
// `media_meta` is a cleartext byte the SENDER stamps and every forwarder reads WITHOUT a key: high bit
// = is_video, low 7 bits = a coarse audio-activity level (0..=127). It lets a cascade forwarder relay
// only the top-K most-active audio streams + the active speaker's video and DROP the rest, so the
// per-receiver downlink is bounded at ~K streams instead of N-1 — the piece that makes video scale
// (the tree alone only fixes uplink; see docs/decisions/group-call-cascade-sfu.md §5, §12 C1).
//
// Privacy: the activity byte reveals to the forwarder (a mesh peer, possibly not a group member) WHO
// is speaking + rough loudness. Transmission timing already leaks that, so the marginal leak is small;
// content stays E2EE (the sealed body is untouched). Accepted as the price of active-speaker scale —
// see the threat note in omni-threat-model.md / the cascade ADR §12.

/// High bit of `media_meta`: the frame carries video (else audio).
const MEDIA_META_VIDEO_FLAG: u8 = 0x80;
/// Max coarse audio-activity level packed into the low 7 bits of `media_meta`.
pub const MEDIA_ACTIVITY_MAX: u8 = 0x7F;
/// How many most-active AUDIO senders a forwarder relays (others dropped). Hear a few people at once.
pub const ACTIVE_AUDIO_K: usize = 4;
/// How many most-active senders' VIDEO a forwarder relays (others dropped). See mainly the speaker.
pub const ACTIVE_VIDEO_K: usize = 1;

/// Pack `is_video` + a coarse audio-activity level (clamped to 0..=127) into the cleartext `media_meta`
/// byte. `activity` is the sender's own current audio level, quantised — 0 means silent/unknown.
pub fn pack_media_meta(is_video: bool, activity: u8) -> u8 {
    let level = activity.min(MEDIA_ACTIVITY_MAX);
    if is_video {
        level | MEDIA_META_VIDEO_FLAG
    } else {
        level
    }
}

/// Unpack `media_meta` → `(is_video, activity_level)`.
pub fn unpack_media_meta(meta: u8) -> (bool, u8) {
    (meta & MEDIA_META_VIDEO_FLAG != 0, meta & MEDIA_ACTIVITY_MAX)
}

/// Stable 8-byte tag of a call id (routing pseudonym; not secret).
pub fn call_tag_from_id(call_id: &str) -> [u8; GROUP_MEDIA_TAG_LEN] {
    tag8(b"omni.group.call|", call_id.as_bytes())
}

/// Stable 8-byte tag of a member pub (routing pseudonym; not secret). Doubles as the `relay_tag` a
/// forwarding member stamps when it relays a frame.
pub fn sender_tag_from_pub(member_pub: &str) -> [u8; GROUP_MEDIA_TAG_LEN] {
    tag8(b"omni.group.sender|", member_pub.as_bytes())
}

fn tag8(domain: &[u8], data: &[u8]) -> [u8; GROUP_MEDIA_TAG_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(data);
    let digest = hasher.finalize();
    let mut tag = [0u8; GROUP_MEDIA_TAG_LEN];
    tag.copy_from_slice(&digest[..GROUP_MEDIA_TAG_LEN]);
    tag
}

pub fn wrap_group_media(
    hop_limit: u8,
    media_meta: u8,
    relay_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    call_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    sender_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    sealed: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(GROUP_MEDIA_HEADER_LEN + sealed.len());
    out.extend_from_slice(&GROUP_MEDIA_MAGIC);
    out.push(hop_limit);
    out.push(media_meta);
    out.extend_from_slice(relay_tag);
    out.extend_from_slice(call_tag);
    out.extend_from_slice(sender_tag);
    out.extend_from_slice(sealed);
    out
}

/// Parse a group-call media datagram: `(hop_limit, media_meta, relay_tag, call_tag, sender_tag,
/// sealed)`. Returns `None` for anything without the magic (i.e. ordinary 1:1 media) so the mesh
/// receive path can cheaply tell the two apart.
#[allow(clippy::type_complexity)]
pub fn parse_group_media(
    data: &[u8],
) -> Option<(
    u8,
    u8,
    [u8; GROUP_MEDIA_TAG_LEN],
    [u8; GROUP_MEDIA_TAG_LEN],
    [u8; GROUP_MEDIA_TAG_LEN],
    &[u8],
)> {
    if data.len() < GROUP_MEDIA_HEADER_LEN || data[..2] != GROUP_MEDIA_MAGIC {
        return None;
    }
    let hop_limit = data[2];
    let media_meta = data[MEDIA_META_OFFSET];
    let mut relay_tag = [0u8; GROUP_MEDIA_TAG_LEN];
    let mut call_tag = [0u8; GROUP_MEDIA_TAG_LEN];
    let mut sender_tag = [0u8; GROUP_MEDIA_TAG_LEN];
    relay_tag.copy_from_slice(&data[RELAY_TAG_OFFSET..CALL_TAG_OFFSET]);
    call_tag.copy_from_slice(&data[CALL_TAG_OFFSET..SENDER_TAG_OFFSET]);
    sender_tag.copy_from_slice(&data[SENDER_TAG_OFFSET..GROUP_MEDIA_HEADER_LEN]);
    Some((hop_limit, media_meta, relay_tag, call_tag, sender_tag, &data[GROUP_MEDIA_HEADER_LEN..]))
}

/// Prepare a received group-media datagram for FORWARDING along the cascade tree: decrement its
/// `hop_limit` and stamp `relay_tag` with the forwarder's own tag (so the next hop excludes us from
/// its flood). Returns `None` if the budget was already 0 (drop — the frame has looped or travelled
/// too far) or it isn't group media. Only the header bytes change; the sealed body is untouched
/// (E2E). The forwarder sends the returned bytes on to its tree-neighbours (minus the arrival).
pub fn prepare_group_media_forward(
    payload: &[u8],
    relay_tag: &[u8; GROUP_MEDIA_TAG_LEN],
) -> Option<Vec<u8>> {
    if payload.len() < GROUP_MEDIA_HEADER_LEN || payload[..2] != GROUP_MEDIA_MAGIC {
        return None;
    }
    let hop = payload[2];
    if hop == 0 {
        return None;
    }
    let mut out = payload.to_vec();
    out[2] = hop - 1;
    // media_meta (data[MEDIA_META_OFFSET]) is the sender's — carried through untouched on relay.
    out[RELAY_TAG_OFFSET..CALL_TAG_OFFSET].copy_from_slice(relay_tag);
    Some(out)
}

// --- Group-call feedback (RTCP back to a sender) -----------------------------
// A receiver's RTCP about one sender's stream travels back to that sender — never flooded:
// `[GF(2) ‖ hop_limit(1) ‖ call_tag(8) ‖ target_tag(8) ‖ reporter_tag(8) ‖ sealed]`, sealed with the
// REPORTER's media key; `target_tag` is the sender it is about. The sender's encoder serves every
// receiver at once, so its congestion controller must hear ONE link, like an SFU hearing only its next
// hop: the sender's [`feedback_reporter`] sends everything (delivery reports, RR, NACK, PLI); the
// others send only repair requests (NACK, PLI, FIR).
//
// Path ([`feedback_next_hop`]): full mesh — straight to the sender (every member is a direct media
// target); cascade — the media path backwards, one tree hop at a time, because a member need not have
// a link to a sender it hears through the tree (the far PC heard b only through a: its repair
// requests to b failed `no_link`). A hop whose tag is not the target relays the datagram unopened.
// See docs/bugs/group-call-joiner-saw-no-video.md, «Обратная связь».

pub const GROUP_FEEDBACK_MAGIC: [u8; 2] = [0x47, 0x46]; // "GF"
/// The same upstream datagram carrying a viewer's signed screen-control batch to the sharer
/// (docs/decisions/remote-screen-control.md). Same header, same tree path, same opaque relaying.
pub const GROUP_SCREEN_CONTROL_MAGIC: [u8; 2] = [0x47, 0x53]; // "GS"
/// Header layout: MAGIC(2) + hop_limit(1) + call_tag(8) + target_tag(8) + reporter_tag(8).
pub const GROUP_FEEDBACK_HEADER_LEN: usize = 2 + 1 + GROUP_MEDIA_TAG_LEN * 3;
const FEEDBACK_CALL_TAG_OFFSET: usize = 3;
const FEEDBACK_TARGET_TAG_OFFSET: usize = FEEDBACK_CALL_TAG_OFFSET + GROUP_MEDIA_TAG_LEN;
const FEEDBACK_REPORTER_TAG_OFFSET: usize = FEEDBACK_TARGET_TAG_OFFSET + GROUP_MEDIA_TAG_LEN;

/// What an upstream (toward-the-sender) group datagram carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupUpstreamKind {
    /// RTCP sealed with the reporter's media key.
    Feedback,
    /// A screen-control batch signed with the viewer's identity key; the target is the sharer.
    ScreenControl,
}

impl GroupUpstreamKind {
    fn magic(self) -> [u8; 2] {
        match self {
            Self::Feedback => GROUP_FEEDBACK_MAGIC,
            Self::ScreenControl => GROUP_SCREEN_CONTROL_MAGIC,
        }
    }
}

/// A parsed group feedback datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupFeedback<'a> {
    pub kind: GroupUpstreamKind,
    pub hop_limit: u8,
    pub call_tag: [u8; GROUP_MEDIA_TAG_LEN],
    /// The sender whose stream the RTCP is about — where the datagram is going.
    pub target_tag: [u8; GROUP_MEDIA_TAG_LEN],
    pub reporter_tag: [u8; GROUP_MEDIA_TAG_LEN],
    pub sealed: &'a [u8],
}

pub fn wrap_group_feedback(
    kind: GroupUpstreamKind,
    call_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    target_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    reporter_tag: &[u8; GROUP_MEDIA_TAG_LEN],
    sealed: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(GROUP_FEEDBACK_HEADER_LEN + sealed.len());
    out.extend_from_slice(&kind.magic());
    out.push(GROUP_MEDIA_DEFAULT_HOP_LIMIT);
    out.extend_from_slice(call_tag);
    out.extend_from_slice(target_tag);
    out.extend_from_slice(reporter_tag);
    out.extend_from_slice(sealed);
    out
}

/// Parse a group feedback datagram; `None` for anything else.
pub fn parse_group_feedback(data: &[u8]) -> Option<GroupFeedback<'_>> {
    if data.len() < GROUP_FEEDBACK_HEADER_LEN {
        return None;
    }
    let kind = match [data[0], data[1]] {
        GROUP_FEEDBACK_MAGIC => GroupUpstreamKind::Feedback,
        GROUP_SCREEN_CONTROL_MAGIC => GroupUpstreamKind::ScreenControl,
        _ => return None,
    };
    let tag = |at: usize| {
        let mut tag = [0u8; GROUP_MEDIA_TAG_LEN];
        tag.copy_from_slice(&data[at..at + GROUP_MEDIA_TAG_LEN]);
        tag
    };
    Some(GroupFeedback {
        kind,
        hop_limit: data[2],
        call_tag: tag(FEEDBACK_CALL_TAG_OFFSET),
        target_tag: tag(FEEDBACK_TARGET_TAG_OFFSET),
        reporter_tag: tag(FEEDBACK_REPORTER_TAG_OFFSET),
        sealed: &data[GROUP_FEEDBACK_HEADER_LEN..],
    })
}

/// Prepare a feedback datagram for relaying one more hop toward its target: decrement `hop_limit`.
/// `None` if the budget is spent (a loop during a tree change) or it isn't feedback. The sealed body
/// is untouched — a relaying hop never opens it.
pub fn prepare_group_feedback_forward(payload: &[u8]) -> Option<Vec<u8>> {
    let feedback = parse_group_feedback(payload)?;
    if feedback.hop_limit == 0 {
        return None;
    }
    let mut out = payload.to_vec();
    out[2] = feedback.hop_limit - 1;
    Some(out)
}

/// Where `me` hands feedback about `sender`'s stream: full mesh — `sender` itself; cascade — the
/// tree-neighbour toward `sender` (the way its media came, backwards). `None` when there is no way.
pub fn feedback_next_hop(tree: Option<&CallTree>, me: &str, sender: &str) -> Option<String> {
    match tree {
        Some(tree) => tree.next_hop(me, sender).map(str::to_string),
        None => (me != sender).then(|| sender.to_string()),
    }
}

/// The one receiver whose full feedback drives `sender`'s congestion controller: the sender's next
/// hop. Cascade — its tree parent (the root: its first child); full mesh — the smallest other
/// participant. Every participant computes the same answer from the same member set.
pub fn feedback_reporter(tree: Option<&CallTree>, participants: &[String], sender: &str) -> Option<String> {
    match tree {
        Some(tree) => tree
            .parent(sender)
            .or_else(|| tree.children(sender).first().copied())
            .map(str::to_string),
        None => participants.iter().filter(|p| p.as_str() != sender).min().cloned(),
    }
}

/// Keep only the repair requests of an RTCP compound packet — NACK (205/1), PLI (206/1), FIR (206/4) —
/// for a receiver that is NOT the sender's [`feedback_reporter`]. `None` when nothing is left (or the
/// packet is malformed). Its delivery reports and RR stay home: the sender's congestion controller
/// must hear one link only.
pub fn rtcp_repair_requests_only(packet: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= packet.len() {
        let length_words = u16::from_be_bytes([packet[at + 2], packet[at + 3]]) as usize;
        let end = at + 4 + length_words * 4;
        if end > packet.len() || packet[at] >> 6 != 2 {
            return None;
        }
        let (fmt, pt) = (packet[at] & 0x1F, packet[at + 1]);
        if (pt == 205 && fmt == 1) || (pt == 206 && (fmt == 1 || fmt == 4)) {
            out.extend_from_slice(&packet[at..end]);
        }
        at = end;
    }
    (!out.is_empty()).then_some(out)
}

/// Decide whether a cascade forwarder should relay a frame under active-speaker gating (C1).
///
/// `ranked` is the call's **currently-active** senders (activity > 0), most-active first — the
/// forwarder maintains it from the `media_meta` activity levels it sees flow through, and updates the
/// frame's own sender into it BEFORE calling this. `is_video` is the frame's media kind (from
/// `media_meta`), `sender` its author.
///
/// Rule: audio frames from the top [`ACTIVE_AUDIO_K`] active senders are relayed, video frames only
/// from the top [`ACTIVE_VIDEO_K`]; everything else is dropped, so the per-receiver downlink is bounded
/// at ~K streams instead of N-1.
///
/// **Fail-open on absence, not on presence:** if the call has at most K *active* senders (`ranked.len()
/// <= k`) there is nothing worth gating → forward all. This is the no-regression guard: with no real
/// activity signal (e.g. before the native level source is wired) `ranked` is empty and every frame is
/// forwarded, exactly like un-gated cascade. But once there ARE more than K active senders, a frame
/// whose sender is NOT among them (i.e. silent while K others are loud) is dropped — that is the whole
/// point. The caller updates the sender's activity first, so an actually-active sender is always
/// present in `ranked`.
pub fn active_speaker_should_forward(is_video: bool, sender: &str, ranked: &[String]) -> bool {
    let k = if is_video { ACTIVE_VIDEO_K } else { ACTIVE_AUDIO_K };
    if ranked.len() <= k {
        return true; // at most K active senders — nothing to gate, fail open
    }
    match ranked.iter().position(|s| s == sender) {
        Some(rank) => rank < k,
        None => false, // silent while > K others are loud → drop (that's the downlink saving)
    }
}

#[cfg(test)]
mod group_call_tests {
    use super::*;

    #[test]
    fn feedback_round_trips_and_is_not_media() {
        let wire = wrap_group_feedback(GroupUpstreamKind::Feedback, &[1; 8], &[3; 8], &[2; 8], b"sealed");
        let feedback = parse_group_feedback(&wire).expect("feedback parses");
        assert_eq!(
            feedback,
            GroupFeedback {
                kind: GroupUpstreamKind::Feedback,
                hop_limit: GROUP_MEDIA_DEFAULT_HOP_LIMIT,
                call_tag: [1; 8],
                target_tag: [3; 8],
                reporter_tag: [2; 8],
                sealed: b"sealed",
            }
        );
        // Negative half: feedback is not media and media is not feedback.
        assert!(parse_group_media(&wire).is_none());
        let media = wrap_group_media(8, 0, &[2; 8], &[1; 8], &[2; 8], b"x");
        assert!(parse_group_feedback(&media).is_none());
    }

    #[test]
    fn screen_control_rides_the_feedback_path_under_its_own_kind() {
        let wire = wrap_group_feedback(GroupUpstreamKind::ScreenControl, &[1; 8], &[3; 8], &[2; 8], b"signed");
        let parsed = parse_group_feedback(&wire).expect("upstream parses");
        assert_eq!((parsed.kind, parsed.target_tag, parsed.sealed), (GroupUpstreamKind::ScreenControl, [3; 8], &b"signed"[..]));
        let relayed = prepare_group_feedback_forward(&wire).expect("relayed like feedback");
        assert_eq!(parse_group_feedback(&relayed).unwrap().kind, GroupUpstreamKind::ScreenControl, "kind survives a hop");
        // Negative half: neither kind is mistaken for the other, an unknown magic is nothing.
        let feedback = wrap_group_feedback(GroupUpstreamKind::Feedback, &[1; 8], &[3; 8], &[2; 8], b"signed");
        assert_eq!(parse_group_feedback(&feedback).unwrap().kind, GroupUpstreamKind::Feedback);
        let mut unknown = wire;
        unknown[1] = b'X';
        assert_eq!(parse_group_feedback(&unknown), None);
    }

    #[test]
    fn a_relayed_feedback_spends_one_hop_and_dies_at_zero() {
        let wire = wrap_group_feedback(GroupUpstreamKind::Feedback, &[1; 8], &[3; 8], &[2; 8], b"sealed");
        let relayed = prepare_group_feedback_forward(&wire).expect("budget left");
        let feedback = parse_group_feedback(&relayed).unwrap();
        assert_eq!(feedback.hop_limit, GROUP_MEDIA_DEFAULT_HOP_LIMIT - 1);
        assert_eq!((feedback.target_tag, feedback.sealed), ([3; 8], &b"sealed"[..]), "body untouched");
        // Negative half: a spent budget is dropped (the loop guard).
        let mut spent = wire.clone();
        spent[2] = 0;
        assert_eq!(prepare_group_feedback_forward(&spent), None);
    }

    #[test]
    fn feedback_walks_the_media_path_backwards() {
        let ps: Vec<CallParticipantV1> =
            ["a", "b", "c", "d", "e", "f"].iter().map(|k| p(k, true, 2, true)).collect();
        // fanout 2: a → (b, c); b → (d, e); c → (f).
        let tree = CallTree::build(&ps, 2);
        // A leaf hears a sibling subtree only through the tree: d → b → a → c → f.
        assert_eq!(feedback_next_hop(Some(&tree), "d", "f").as_deref(), Some("b"));
        assert_eq!(feedback_next_hop(Some(&tree), "b", "f").as_deref(), Some("a"));
        assert_eq!(feedback_next_hop(Some(&tree), "a", "f").as_deref(), Some("c"));
        assert_eq!(feedback_next_hop(Some(&tree), "c", "f").as_deref(), Some("f"));
        // Down from the root, and up from a leaf to the root.
        assert_eq!(feedback_next_hop(Some(&tree), "a", "e").as_deref(), Some("b"));
        assert_eq!(feedback_next_hop(Some(&tree), "e", "a").as_deref(), Some("b"));
        // Full mesh: straight to the sender.
        assert_eq!(feedback_next_hop(None, "d", "f").as_deref(), Some("f"));
        // Negative halves: not to ourselves, not to someone outside the tree.
        assert_eq!(feedback_next_hop(Some(&tree), "d", "d"), None);
        assert_eq!(feedback_next_hop(None, "d", "d"), None);
        assert_eq!(feedback_next_hop(Some(&tree), "d", "zed"), None);
    }

    #[test]
    fn the_feedback_reporter_is_the_senders_next_hop() {
        let ps: Vec<CallParticipantV1> =
            ["a", "b", "c", "d", "e", "f"].iter().map(|k| p(k, true, 2, true)).collect();
        let tree = CallTree::build(&ps, 2);
        // a is the root (children b, c); d's parent is b.
        assert_eq!(feedback_reporter(Some(&tree), &[], "d").as_deref(), Some("b"));
        assert_eq!(feedback_reporter(Some(&tree), &[], "a").as_deref(), Some("b"), "root: first child");
        let mesh: Vec<String> = ["c", "a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(feedback_reporter(None, &mesh, "a").as_deref(), Some("b"));
        assert_eq!(feedback_reporter(None, &mesh, "b").as_deref(), Some("a"));
        // Negative half: nobody else in the call — no reporter.
        assert_eq!(feedback_reporter(None, &["a".to_string()], "a"), None);
    }

    #[test]
    fn a_non_reporter_keeps_only_repair_requests() {
        // RR (201, empty) ‖ NACK (205/1, 3 words) ‖ CCFB (205/11, 2 words) ‖ PLI (206/1, 2 words).
        let rr = [0x80, 201, 0, 1, 0, 0, 0, 9];
        let nack = [0x81, 205, 0, 3, 0, 0, 0, 9, 0, 0, 0, 7, 0, 5, 0, 0];
        let ccfb = [0x8B, 205, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7];
        let pli = [0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7];
        let compound = [&rr[..], &nack, &ccfb, &pli].concat();
        assert_eq!(rtcp_repair_requests_only(&compound), Some([&nack[..], &pli].concat()));
        // Negative halves: nothing to repair → nothing sent; a truncated packet is rejected.
        assert_eq!(rtcp_repair_requests_only(&[&rr[..], &ccfb].concat()), None);
        assert_eq!(rtcp_repair_requests_only(&compound[..compound.len() - 2]), None);
    }

    fn p(pubk: &str, live: bool, tier: u8, can_forward: bool) -> CallParticipantV1 {
        CallParticipantV1 {
            member_pub: pubk.to_string(),
            is_live: live,
            uplink_tier: tier,
            can_forward,
        }
    }

    #[test]
    fn topology_switches_at_four() {
        assert_eq!(decide_call_topology(2), CallTopology::FullMesh);
        assert_eq!(decide_call_topology(3), CallTopology::FullMesh);
        assert_eq!(decide_call_topology(4), CallTopology::Cascade);
        assert_eq!(decide_call_topology(8), CallTopology::Cascade);
    }

    #[test]
    fn media_meta_packs_video_flag_and_clamps_activity() {
        // Audio, mid level.
        let (v, a) = unpack_media_meta(pack_media_meta(false, 42));
        assert!(!v);
        assert_eq!(a, 42);
        // Video flag survives; activity in the low 7 bits.
        let (v, a) = unpack_media_meta(pack_media_meta(true, 100));
        assert!(v);
        assert_eq!(a, 100);
        // Activity above 127 clamps (never spills into the video flag bit).
        let (v, a) = unpack_media_meta(pack_media_meta(false, 255));
        assert!(!v);
        assert_eq!(a, MEDIA_ACTIVITY_MAX);
    }

    #[test]
    fn group_media_envelope_roundtrips_and_rejects_1to1() {
        let relay_tag = sender_tag_from_pub("pub-a");
        let call_tag = call_tag_from_id("call-123");
        let sender_tag = sender_tag_from_pub("pub-a");
        let meta = pack_media_meta(true, 77);
        let sealed = vec![1u8, 2, 3, 4, 5];
        let wire = wrap_group_media(
            GROUP_MEDIA_DEFAULT_HOP_LIMIT,
            meta,
            &relay_tag,
            &call_tag,
            &sender_tag,
            &sealed,
        );
        assert_eq!(&wire[..2], &GROUP_MEDIA_MAGIC);

        let (hop, mm, rt, ct, st, body) = parse_group_media(&wire).expect("parse");
        assert_eq!(hop, GROUP_MEDIA_DEFAULT_HOP_LIMIT);
        assert_eq!(mm, meta);
        assert_eq!(unpack_media_meta(mm), (true, 77));
        assert_eq!(rt, relay_tag);
        assert_eq!(ct, call_tag);
        assert_eq!(st, sender_tag);
        assert_eq!(body, sealed.as_slice());

        // Tags are stable + distinct per input.
        assert_eq!(call_tag_from_id("call-123"), call_tag);
        assert_ne!(sender_tag_from_pub("pub-b"), sender_tag);

        // Ordinary 1:1 media (no magic) is not mistaken for a group frame.
        assert!(parse_group_media(&[0x80, 0x60, 1, 2, 3]).is_none());
        assert!(parse_group_media(&[]).is_none());
    }

    #[test]
    fn forward_rewrites_relay_tag_decrements_hop_and_drops_at_zero() {
        let origin = sender_tag_from_pub("origin");
        let ct = call_tag_from_id("c");
        let st = sender_tag_from_pub("origin");
        let meta = pack_media_meta(true, 63);
        let wire = wrap_group_media(2, meta, &origin, &ct, &st, &[9, 9]);

        // Forward once as "hopper" → hop 1, relay_tag = hopper, sender_tag + media_meta + body intact.
        let hopper = sender_tag_from_pub("hopper");
        let f1 = prepare_group_media_forward(&wire, &hopper).expect("hop 2 → forwardable");
        let (h1, m1, r1, _c1, s1, b1) = parse_group_media(&f1).unwrap();
        assert_eq!(h1, 1);
        assert_eq!(m1, meta, "media_meta (the sender's) is carried through untouched");
        assert_eq!(r1, hopper, "relay_tag is rewritten to the forwarder");
        assert_eq!(s1, st, "sender_tag (original author) is untouched");
        assert_eq!(b1, &[9, 9], "sealed body is untouched (E2E)");

        // Forward again → hop 0.
        let f2 = prepare_group_media_forward(&f1, &sender_tag_from_pub("hopper2")).expect("hop 1");
        assert_eq!(parse_group_media(&f2).unwrap().0, 0);
        // At hop 0 → dropped (None), so a looped/expired frame stops.
        assert!(prepare_group_media_forward(&f2, &hopper).is_none());
        // Not group media → None.
        assert!(prepare_group_media_forward(&[0x00, 0x11, 0x22], &hopper).is_none());
    }

    #[test]
    fn active_speaker_gating_forwards_top_k_audio_and_only_speaker_video() {
        // Ranked most-active first: a, b, c, d, e.
        let ranked: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        // Audio (K=4): a..d relayed, e dropped.
        assert!(active_speaker_should_forward(false, "a", &ranked));
        assert!(active_speaker_should_forward(false, "d", &ranked));
        assert!(!active_speaker_should_forward(false, "e", &ranked), "5th-loudest audio dropped");
        // Video (K=1): only the active speaker "a"; everyone else's video dropped.
        assert!(active_speaker_should_forward(true, "a", &ranked));
        assert!(!active_speaker_should_forward(true, "b", &ranked), "non-speaker video dropped");
        assert!(!active_speaker_should_forward(true, "e", &ranked));
    }

    #[test]
    fn active_speaker_gating_fails_open_without_enough_activity() {
        // Empty ranking (no activity anywhere — e.g. native level source not wired) → forward all.
        // This is the no-regression guard: un-gated cascade behaviour until real levels flow.
        assert!(active_speaker_should_forward(false, "a", &[]));
        assert!(active_speaker_should_forward(true, "a", &[]));
        // Fewer ACTIVE senders than K → nothing to gate: forward all.
        let few: Vec<String> = vec!["a".into()];
        assert!(active_speaker_should_forward(false, "a", &few)); // audio K=4, len<=4
        assert!(active_speaker_should_forward(true, "a", &few)); // video K=1, len<=1
        // But once > K senders are active, a sender NOT among them (silent) is dropped — the gate
        // engaging is the whole point, so this must NOT fail open.
        let ranked: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert!(!active_speaker_should_forward(false, "silent", &ranked), "silent audio dropped when 5 loud");
        assert!(!active_speaker_should_forward(true, "silent", &ranked), "silent video dropped when 5 loud");
    }

    // --- Cascade tree ---

    /// Simulate spanning-tree media flood from `origin`; return how many times each member is
    /// delivered the frame. A correct (loop-free, connected) tree delivers to every non-origin member
    /// exactly once and never back to the origin. A capped step count catches a buggy loop.
    fn flood_receipts(
        tree: &CallTree,
        members: &[&str],
        origin: &str,
    ) -> std::collections::HashMap<String, usize> {
        use std::collections::VecDeque;
        let mut receipts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        let mut queue: VecDeque<(String, String)> = VecDeque::new(); // (node, arrived_from)
        for n in tree_send_targets(tree, origin, None) {
            queue.push_back((n, origin.to_string()));
        }
        let cap = members.len() * members.len() + 16;
        let mut steps = 0usize;
        while let Some((node, from)) = queue.pop_front() {
            steps += 1;
            assert!(steps <= cap, "flood did not terminate — tree has a loop");
            *receipts.entry(node.clone()).or_insert(0) += 1;
            for n in tree_send_targets(tree, &node, Some(&from)) {
                queue.push_back((n, node.clone()));
            }
        }
        receipts
    }

    #[test]
    fn cascade_tree_deterministic_regardless_of_input_order() {
        let base = vec![
            p("a", true, 2, true),
            p("b", true, 3, true), // strongest uplink → root
            p("c", true, 2, true),
            p("d", true, 1, true),
            p("e", true, 2, true),
        ];
        let mut shuffled = base.clone();
        shuffled.reverse();
        let t1 = CallTree::build(&base, 4);
        let t2 = CallTree::build(&shuffled, 4);
        assert_eq!(t1, t2, "every participant must build the identical tree");
        assert_eq!(t1.root(), Some("b"));
    }

    #[test]
    fn cascade_tree_degree_bounded_and_nonforwarders_are_leaves() {
        let mut ps: Vec<CallParticipantV1> =
            (0..6).map(|i| p(&format!("f{i}"), true, 2, true)).collect();
        // High uplink but opted out of forwarding → must be a leaf despite the strong tier.
        ps.push(p("weak1", true, 3, false));
        ps.push(p("weak2", true, 3, false));
        let tree = CallTree::build(&ps, 3);
        assert!(!tree.is_forwarder("weak1"));
        assert!(!tree.is_forwarder("weak2"));
        for m in ps.iter().map(|x| x.member_pub.as_str()) {
            assert!(tree.neighbors(m).len() <= 3 + 1, "degree of {m} exceeds fanout+1");
        }
    }

    #[test]
    fn cascade_tree_flood_delivers_to_everyone_exactly_once() {
        // 10 nodes, fanout 3 → a genuine multi-level tree.
        let ps: Vec<CallParticipantV1> =
            (0..10).map(|i| p(&format!("p{i:02}"), true, 2, true)).collect();
        let tree = CallTree::build(&ps, 3);
        let members: Vec<&str> = ps.iter().map(|x| x.member_pub.as_str()).collect();
        for origin in &members {
            let receipts = flood_receipts(&tree, &members, origin);
            for m in &members {
                if m == origin {
                    assert_eq!(
                        receipts.get(*m).copied().unwrap_or(0),
                        0,
                        "origin {origin} received its own frame back (loop)"
                    );
                } else {
                    assert_eq!(
                        receipts.get(*m).copied().unwrap_or(0),
                        1,
                        "member {m} not delivered exactly once from origin {origin}"
                    );
                }
            }
        }
    }

    #[test]
    fn cascade_tree_failover_rebuild_is_consistent() {
        let ps: Vec<CallParticipantV1> =
            (0..7).map(|i| p(&format!("n{i}"), true, 2, true)).collect();
        let full = CallTree::build(&ps, 3);
        let root = full.root().unwrap().to_string();
        let after = CallTree::rebuild_without(&ps, std::slice::from_ref(&root), 3);
        let after2 = CallTree::rebuild_without(&ps, std::slice::from_ref(&root), 3);
        assert_eq!(after, after2, "rebuild over survivors must be deterministic");
        assert_ne!(after.root(), Some(root.as_str()), "dropped root is gone");
        // Survivors still flood loop-free after failover.
        let survivors: Vec<&str> = ps
            .iter()
            .map(|x| x.member_pub.as_str())
            .filter(|m| *m != root)
            .collect();
        let receipts = flood_receipts(&after, &survivors, survivors[0]);
        for m in &survivors[1..] {
            assert_eq!(receipts.get(*m).copied().unwrap_or(0), 1);
        }
    }

    #[test]
    fn cascade_tree_generalises_single_forwarder() {
        // 4 participants, fanout ≥3 → height-1 tree = the old single-forwarder (root fans to 3 leaves).
        let ps = vec![
            p("fwd", true, 3, true),
            p("a", true, 1, true),
            p("b", true, 1, true),
            p("c", true, 1, true),
        ];
        let tree = CallTree::build(&ps, 4);
        assert_eq!(tree.root(), Some("fwd"));
        let mut kids = tree.children("fwd");
        kids.sort();
        assert_eq!(kids, vec!["a", "b", "c"]);
        // A leaf has exactly one neighbour (the root) → one uplink, like the old single forwarder.
        assert_eq!(tree.neighbors("a"), vec!["fwd".to_string()]);
        assert!(!tree.is_forwarder("a"));
        // The root's own media goes to every child; a leaf forwarding nothing back up is loop-free.
        let mut root_targets = tree_send_targets(&tree, "fwd", None);
        root_targets.sort();
        assert_eq!(root_targets, vec!["a", "b", "c"]);
        assert!(tree_send_targets(&tree, "a", Some("fwd")).is_empty());
    }
}
