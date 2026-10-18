// Group wire DTOs — what travels for groups. Group crypto + membership is MLS (RFC 9420,
// `omni-group-mls`); see docs/decisions/group-crypto-mls.md.
//
// - On the group's durable feed (one feed per (group, sender), delivered by subscription + gossip +
//   anti-entropy): `GroupFeedItemV2` — one MLS message (a Commit or an application message). The MLS
//   bytes are already encrypted for the group; the plaintext around them is the group id only.
// - Inside an MLS application message: `GroupAppPayloadV1` — text, call control, "leaving".
// - Over the pairwise 1:1 channel (consent — membership takes an explicit accept): invite /
//   accept (carries the invitee's MLS KeyPackage) / decline / welcome (the MLS Welcome).
//
// Platform-neutral by construction: MLS is a standard wire format, the rest is JSON + base64 —
// a Windows and an Android member read the same bytes.

use serde::{Deserialize, Serialize};

use omni_types::{OmniError, OmniResult};

/// Group ids are 16 random bytes, hex.
pub const GROUP_ID_HEX_LEN: usize = 32;
pub const GROUP_WIRE_VERSION_V2: u16 = 2;
pub const GROUP_TEXT_MAX_BYTES: usize = 16 * 1024;
/// An attachment offer: the caption, the file facts and a photo's preview (at most 16 KB of JPEG,
/// a third more as base64).
pub const GROUP_ATTACHMENT_OFFER_MAX_BYTES: usize = 48 * 1024;
pub const GROUP_NAME_MAX_BYTES: usize = 128;
/// An MLS message on the wire. A Welcome for 100 members is ~30 KB; a Commit ~9 KB.
pub const GROUP_MLS_MAX_BYTES: usize = 256 * 1024;

/// One MLS message on a group's durable feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupFeedItemV2 {
    pub version: u16,
    pub group_id: String,
    pub mls_b64: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupAppKindV1 {
    Text,
    /// Group-call signaling (`GroupCallControlV1` JSON in `body`).
    Control,
    /// The sender is leaving; an admin removes them with a Commit.
    Leave,
    /// An admin deleted the group for everyone: members stop, the chat stays read-only until they
    /// delete it. Honoured only from a member who is an admin in the receiver's view.
    Dissolve,
    /// A file or photo: `body` is the `AttachmentOfferV1` JSON (its content key included, so any
    /// member can open the content). The bytes are pulled from the author over a mesh bulk stream
    /// (docs/decisions/media-messages.md section 3), never carried by the feed.
    Attachment,
    /// Recent history for a newcomer (docs/decisions/group-history-sync.md): `body` is a
    /// `GroupHistoryPacketV1` JSON. Every line in it is signed by its own author, so the member who
    /// hands the packet over cannot put words in anyone's mouth.
    History,
    /// What the sender says about itself — name and photo — for members who do not have it as a
    /// contact (docs/decisions/group-member-profile.md): `body` is a `GroupMemberProfileV1` JSON.
    Profile,
    /// A newcomer asks the members for their `Profile` (empty `body`).
    ProfileAsk,
}

/// A member's card for the group: who it is in its own words. Taken only when `account_id` is the
/// account of the MLS-authenticated sender; a newer `version` replaces an older one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMemberProfileV1 {
    pub account_id: String,
    /// When the sender last changed its profile (ms) — newer wins.
    pub version: i64,
    pub display_name: String,
    /// A small JPEG (≤ [`GROUP_MEMBER_AVATAR_MAX_BYTES`]), base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_jpeg_b64: Option<String>,
}

/// A member's photo in its group card: small, like the group's own picture.
pub const GROUP_MEMBER_AVATAR_MAX_BYTES: usize = 16 * 1024;
/// The card as JSON: the photo as base64 and a little more.
pub const GROUP_MEMBER_PROFILE_MAX_BYTES: usize = 24 * 1024;

/// Shapes and sizes of a member card.
pub fn validate_group_member_profile_v1(profile: &GroupMemberProfileV1) -> OmniResult<()> {
    if profile.account_id.trim().is_empty() || profile.account_id.len() > 128 {
        return Err(OmniError::InvalidInput("group member profile account is invalid".to_string()));
    }
    if profile.display_name.trim().is_empty() || profile.display_name.len() > GROUP_NAME_MAX_BYTES {
        return Err(OmniError::InvalidInput("group member profile name is invalid".to_string()));
    }
    if profile
        .avatar_jpeg_b64
        .as_ref()
        .is_some_and(|b64| b64.len() > GROUP_MEMBER_AVATAR_MAX_BYTES.div_ceil(3) * 4)
    {
        return Err(OmniError::InvalidInput("group member profile photo is too large".to_string()));
    }
    Ok(())
}

/// What rides inside an MLS application message. The sender is the MLS-authenticated leaf — never a
/// field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAppPayloadV1 {
    pub message_id: String,
    pub kind: GroupAppKindV1,
    pub body: String,
    pub authored_at_unix_ms: i64,
    /// The author's Ed25519 signature over [`group_line_signing_bytes`], base64: what lets the line
    /// travel later in someone else's history packet. Text and files carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_b64: Option<String>,
}

/// History for a newcomer: at most this many lines, at most this much JSON.
pub const GROUP_HISTORY_MAX_LINES: usize = 100;
pub const GROUP_HISTORY_MAX_BYTES: usize = 96 * 1024;

const GROUP_LINE_SIGNING_CONTEXT: &[u8] = b"omni-group-line-v1\0";

/// What a line's author signs: the group, the line, its kind, text and time — length-prefixed, so
/// no two different lines sign the same bytes.
pub fn group_line_signing_bytes(
    group_id: &str,
    message_id: &str,
    kind: GroupAppKindV1,
    body: &str,
    authored_at_unix_ms: i64,
) -> Vec<u8> {
    let kind = serde_json::to_string(&kind).unwrap_or_default();
    let mut bytes = GROUP_LINE_SIGNING_CONTEXT.to_vec();
    for part in [group_id.as_bytes(), message_id.as_bytes(), kind.as_bytes(), body.as_bytes()] {
        bytes.extend_from_slice(&(part.len() as u32).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    bytes.extend_from_slice(&authored_at_unix_ms.to_le_bytes());
    bytes
}

/// The recent history a member hands a newcomer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupHistoryPacketV1 {
    pub lines: Vec<GroupHistoryLineV1>,
}

/// One line as its author sent it, with the author's signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupHistoryLineV1 {
    pub message_id: String,
    /// `text` or `attachment`.
    pub kind: GroupAppKindV1,
    /// The author's member key (base64), which the signature is checked against.
    pub sender_pub: String,
    pub sender_name: String,
    pub body: String,
    pub authored_at_unix_ms: i64,
    pub signature_b64: String,
}

/// Shapes and sizes before anything in the packet is used.
pub fn validate_group_history_packet_v1(packet: &GroupHistoryPacketV1) -> OmniResult<()> {
    if packet.lines.len() > GROUP_HISTORY_MAX_LINES {
        return Err(OmniError::InvalidInput("group history carries too many lines".to_string()));
    }
    for line in &packet.lines {
        if line.message_id.is_empty() || line.message_id.len() > 64 {
            return Err(OmniError::InvalidInput("group history line id is invalid".to_string()));
        }
        let max_body = match line.kind {
            GroupAppKindV1::Text => GROUP_TEXT_MAX_BYTES,
            GroupAppKindV1::Attachment => GROUP_ATTACHMENT_OFFER_MAX_BYTES,
            _ => return Err(OmniError::InvalidInput("group history carries only text and files".to_string())),
        };
        if line.body.len() > max_body
            || line.sender_pub.is_empty()
            || line.sender_pub.len() > 64
            || line.sender_name.len() > 256
            || line.signature_b64.is_empty()
            || line.signature_b64.len() > 128
        {
            return Err(OmniError::InvalidInput("group history line is malformed".to_string()));
        }
    }
    Ok(())
}

/// Opens a mesh bulk stream that pulls a group attachment (instead of carrying one): the four bytes
/// first, then a little-endian u32 length and this request as JSON; the holder answers on the same
/// stream with the attachment transfer frames. An old build reads the magic as an oversized frame
/// length and drops the stream.
pub const GROUP_ATTACHMENT_PULL_MAGIC: [u8; 4] = *b"GPL1";
pub const GROUP_ATTACHMENT_PULL_MAX_BYTES: usize = 1024;

/// "Send me these blocks of this group message's file, as transfer `transfer_id`."
/// The holder serves it only to a node that is a member of the group in its own view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAttachmentPullV1 {
    pub group_id: String,
    pub message_id: String,
    pub transfer_id: String,
    /// The blocks asked for, `[from_block, to_block)` (16 MiB each,
    /// docs/decisions/attachment-content-tree.md): the asker spreads a file over several holders,
    /// and a holder with part of it sends the asked blocks it has, then ends the stream.
    pub from_block: u64,
    pub to_block: u64,
    /// The member named for these blocks did not come through: send them, name no one (stage 4).
    #[serde(default)]
    pub insist: bool,
}

/// Blocks of the largest file the protocol allows (2 TB in 16 MiB blocks).
pub const GROUP_ATTACHMENT_PULL_BLOCKS_MAX: u64 = 131_072;

/// The holder's note opens its answer to a pull, before the transfer frames: the four bytes, a
/// little-endian u32 length, this as JSON (docs/decisions/attachment-content-tree.md, stage 4).
pub const GROUP_ATTACHMENT_NOTE_MAGIC: [u8; 4] = *b"GHN1";
/// A note: the map of the largest file (16 KiB as hex is 32 KiB) and a few names.
pub const GROUP_ATTACHMENT_NOTE_MAX_BYTES: usize = 40 * 1024;
/// Blocks a note may name someone else for: no more than one ask asks for.
pub const GROUP_ATTACHMENT_NOTE_ELSEWHERE_MAX: usize = 64;

/// "What I have, and whom to ask for what I did not send": a holder's word to the member pulling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAttachmentHolderNoteV1 {
    /// The blocks it has, a bit each (block `i` is bit `i % 8` of byte `i / 8`), hex; `None` — all.
    #[serde(default)]
    pub have_hex: Option<String>,
    /// Blocks of the ask it gave another member moments ago and does not send again (a seeding
    /// author): ask that member — a node of the group, as the mesh names it.
    #[serde(default)]
    pub elsewhere: Vec<GroupAttachmentElsewhereV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAttachmentElsewhereV1 {
    pub block: u64,
    pub node_hex: String,
}

pub fn validate_group_attachment_holder_note_v1(note: &GroupAttachmentHolderNoteV1) -> OmniResult<()> {
    if let Some(have) = &note.have_hex {
        let bytes_max = GROUP_ATTACHMENT_PULL_BLOCKS_MAX.div_ceil(8) as usize;
        if have.len() > 2 * bytes_max || have.len() % 2 != 0 || !have.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OmniError::InvalidInput("group attachment note map is invalid".to_string()));
        }
    }
    if note.elsewhere.len() > GROUP_ATTACHMENT_NOTE_ELSEWHERE_MAX {
        return Err(OmniError::InvalidInput("group attachment note names too many blocks".to_string()));
    }
    for named in &note.elsewhere {
        if named.block >= GROUP_ATTACHMENT_PULL_BLOCKS_MAX
            || named.node_hex.len() != 40
            || !named.node_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(OmniError::InvalidInput("group attachment note names a block invalidly".to_string()));
        }
    }
    Ok(())
}

pub fn validate_group_attachment_pull_v1(pull: &GroupAttachmentPullV1) -> OmniResult<()> {
    validate_group_id_shape(&pull.group_id)?;
    if pull.message_id.is_empty() || pull.message_id.len() > 64 {
        return Err(OmniError::InvalidInput("group attachment message id is invalid".to_string()));
    }
    if pull.transfer_id.is_empty() || pull.transfer_id.len() > 128 {
        return Err(OmniError::InvalidInput("group attachment transfer id is invalid".to_string()));
    }
    if pull.from_block >= pull.to_block || pull.to_block > GROUP_ATTACHMENT_PULL_BLOCKS_MAX {
        return Err(OmniError::InvalidInput("group attachment block range is invalid".to_string()));
    }
    Ok(())
}

/// 1:1: "join my group" — shows up as an invitation the user accepts or declines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteV2 {
    pub version: u16,
    pub group_id: String,
    pub name: String,
    pub inviter_name: String,
    /// Re-sent because the inviter's add lost a concurrent change: a member who already accepted
    /// answers with a fresh KeyPackage without asking the user again.
    #[serde(default)]
    pub renew: bool,
    pub authored_at_unix_ms: i64,
}

/// 1:1: the invitee accepts, handing over a fresh MLS KeyPackage to be added with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteAcceptV2 {
    pub version: u16,
    pub group_id: String,
    pub key_package_b64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInviteDeclineV2 {
    pub version: u16,
    pub group_id: String,
}

/// 1:1: the MLS Welcome for an accepted invitee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupWelcomeV2 {
    pub version: u16,
    pub group_id: String,
    pub welcome_b64: String,
}

// --- Group call signaling (rides GroupChatInnerV1::Control) -----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupCallControlKindV1 {
    /// Call started; `call_secret_b64` present (per-sender media keys derive from
    /// it). No forwarder is signalled — the cascade tree is a deterministic
    /// function of the live member set, so every participant derives the same
    /// topology (and rebuilds it identically on join/leave). See
    /// docs/decisions/group-call-cascade-sfu.md.
    Invite,
    Join,
    Leave,
    End,
    /// The sender's own media state changed (mic muted / camera off). Discrete,
    /// low-frequency — the receiver can't infer "muted" from a silent stream or
    /// "camera off" from a black frame, so the sender announces it. Speaking is
    /// NOT carried here: it changes ~120 ms and is derived locally from decode
    /// probes; a real-time speaker signal (for auto-focus) belongs on the
    /// ephemeral media path, not this durable feed.
    MediaState,
    /// A participant was removed from the GROUP during the call: a fresh `call_secret_b64` for the
    /// same call, sent in the new MLS epoch the removed member cannot read. Issued by one remaining
    /// participant (deterministic choice), so the removed member stops being able to decrypt — or
    /// forge — the call's media. See docs/decisions/group-crypto-mls.md («Групповой звонок»).
    Rekey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCallControlV1 {
    pub version: u16,
    pub call_id: String,
    pub control: GroupCallControlKindV1,
    pub call_secret_b64: Option<String>,
    /// For `MediaState`: whether the sender's mic is muted. `None` on other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    /// For `MediaState`: whether the sender's camera is off. `None` on other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_off: Option<bool>,
    /// For `MediaState`: whether the sender is sharing their screen (broadcasting the screen on the
    /// video track INSTEAD of the camera). `None` on other kinds. Lets a receiver label + auto-pin the
    /// sharer's tile (Zoom/Telemost UX). Audio-first default = not sharing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_sharing: Option<bool>,
    /// For `MediaState`: who may control the sender's shared screen. `None` on other kinds and when
    /// nobody may. See docs/decisions/remote-screen-control.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_control: Option<ScreenControlGrantV1>,
    pub at_unix_ms: i64,
}

/// Who may control a shared screen: everyone in the call, or the listed `member_pub`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenControlGrantV1 {
    #[serde(default)]
    pub everyone: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

impl ScreenControlGrantV1 {
    pub fn allows(&self, member_pub: &str) -> bool {
        self.everyone || self.members.iter().any(|m| m == member_pub)
    }
}

// --- Validation --------------------------------------------------------------

pub fn validate_group_id_shape(group_id: &str) -> OmniResult<()> {
    if group_id.len() != GROUP_ID_HEX_LEN
        || !group_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(OmniError::InvalidInput(
            "group_id must be 32 hex chars".to_string(),
        ));
    }
    Ok(())
}

fn validate_mls_b64(field: &str, value: &str) -> OmniResult<()> {
    if value.is_empty() || value.len() > GROUP_MLS_MAX_BYTES * 4 / 3 + 4 {
        return Err(OmniError::InvalidInput(format!("{field} size is invalid")));
    }
    Ok(())
}

pub fn validate_group_feed_item_v2(item: &GroupFeedItemV2) -> OmniResult<()> {
    validate_group_id_shape(&item.group_id)?;
    validate_mls_b64("group feed mls", &item.mls_b64)
}

pub fn validate_group_app_payload_v1(payload: &GroupAppPayloadV1) -> OmniResult<()> {
    if payload.message_id.is_empty() || payload.message_id.len() > 64 {
        return Err(OmniError::InvalidInput("group message id is invalid".to_string()));
    }
    let max_body = match payload.kind {
        GroupAppKindV1::Attachment => GROUP_ATTACHMENT_OFFER_MAX_BYTES,
        GroupAppKindV1::History => GROUP_HISTORY_MAX_BYTES,
        GroupAppKindV1::Profile => GROUP_MEMBER_PROFILE_MAX_BYTES,
        _ => GROUP_TEXT_MAX_BYTES,
    };
    if payload.signature_b64.as_ref().is_some_and(|signature| signature.len() > 128) {
        return Err(OmniError::InvalidInput("group message signature is malformed".to_string()));
    }
    if payload.body.len() > max_body {
        return Err(OmniError::InvalidInput("group message body too large".to_string()));
    }
    Ok(())
}

pub fn validate_group_invite_v2(invite: &GroupInviteV2) -> OmniResult<()> {
    validate_group_id_shape(&invite.group_id)?;
    if invite.name.trim().is_empty() || invite.name.len() > GROUP_NAME_MAX_BYTES {
        return Err(OmniError::InvalidInput("group name is invalid".to_string()));
    }
    Ok(())
}

pub fn validate_group_invite_accept_v2(accept: &GroupInviteAcceptV2) -> OmniResult<()> {
    validate_group_id_shape(&accept.group_id)?;
    validate_mls_b64("key package", &accept.key_package_b64)
}

pub fn validate_group_welcome_v2(welcome: &GroupWelcomeV2) -> OmniResult<()> {
    validate_group_id_shape(&welcome.group_id)?;
    validate_mls_b64("welcome", &welcome.welcome_b64)
}

#[cfg(test)]
mod group_wire_tests {
    use super::*;

    const GID: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn group_wire_validates_and_refuses_malformed() {
        let item = GroupFeedItemV2 { version: GROUP_WIRE_VERSION_V2, group_id: GID.into(), mls_b64: "AAAA".into() };
        validate_group_feed_item_v2(&item).unwrap();
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<GroupFeedItemV2>(&json).unwrap(), item);
        let payload = GroupAppPayloadV1 {
            signature_b64: None,
            message_id: "g-1".into(),
            kind: GroupAppKindV1::Text,
            body: "hi".into(),
            authored_at_unix_ms: 1,
        };
        validate_group_app_payload_v1(&payload).unwrap();
        let invite = GroupInviteV2 {
            version: GROUP_WIRE_VERSION_V2,
            group_id: GID.into(),
            name: "Друзья".into(),
            inviter_name: "Alice".into(),
            renew: false,
            authored_at_unix_ms: 1,
        };
        validate_group_invite_v2(&invite).unwrap();
        // An invite without `renew` still decodes (defaults to a fresh invite).
        let plain = serde_json::json!({
            "version": 2, "group_id": GID, "name": "x", "inviter_name": "a", "authored_at_unix_ms": 1
        });
        assert!(!serde_json::from_value::<GroupInviteV2>(plain).unwrap().renew);

        // Negative half: a bad id, an empty MLS blob, an oversized body, an empty name.
        assert!(validate_group_feed_item_v2(&GroupFeedItemV2 { group_id: "nothex".into(), ..item.clone() }).is_err());
        assert!(validate_group_feed_item_v2(&GroupFeedItemV2 { mls_b64: String::new(), ..item }).is_err());
        assert!(validate_group_app_payload_v1(&GroupAppPayloadV1 {
            signature_b64: None,
            body: "x".repeat(GROUP_TEXT_MAX_BYTES + 1),
            ..payload
        })
        .is_err());
        assert!(validate_group_invite_v2(&GroupInviteV2 { name: " ".into(), ..invite }).is_err());
    }
}
