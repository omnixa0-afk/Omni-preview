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
    /// An emoji on a line: `body` is the `ReactionV1` JSON, the reactor the authenticated sender.
    Reaction,
    /// A line taken back by its author: `body` is the `MessageDeleteV1` JSON.
    MessageDelete,
    /// A line pinned or unpinned: `body` is the `MessagePinV1` JSON.
    MessagePin,
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
    /// What the sender published in a workspace repository (docs/decisions/omni-workspace-git.md):
    /// `body` is a `WorkspaceRefsV1` JSON — the sender's own branches, whole (not a diff), so a
    /// member who missed one announcement is set right by the next.
    WorkspaceRefs,
    /// A code project's rule for its shared `main`: `body` is a `WorkspaceRuleV1` JSON. Honoured only
    /// from a member who is an admin in the receiver's view; a later one replaces an earlier one.
    WorkspaceRule,
    /// The sender approves a commit for the shared `main`: `body` is a `WorkspaceApprovalV1` JSON.
    /// Under the "needs another member's approval" rule a maintainer's `main` moves the shared one
    /// only with an approval from an account other than the maintainer's.
    WorkspaceApprove,
    /// File-workspace policy, custodian nomination/consent, or a review of one exact version.
    WorkspaceFiles,
    /// Agents of the workspace (docs/decisions/omni-workspace-git.md section 5): `body` is a
    /// `WorkspaceAgentsEventV1` JSON — «stop all agents», an agent's question to its owner and the
    /// answer, a review of a branch.
    WorkspaceAgents,
}

/// A workspace repository's name: one plain path segment, so it can name a folder and a request.
pub const WORKSPACE_REPO_NAME_MAX: usize = 64;
/// Branches one announcement may carry.
pub const WORKSPACE_REFS_MAX: usize = 256;
/// A branch name (`refs/heads/...`).
pub const WORKSPACE_REF_NAME_MAX: usize = 200;
/// The announcement as JSON: 256 branches with their ids and a little more.
pub const WORKSPACE_REFS_MAX_BYTES: usize = 96 * 1024;

/// The sender's published branches in one workspace repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRefsV1 {
    pub repo: String,
    pub refs: Vec<WorkspaceRefV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRefV1 {
    /// `refs/heads/<branch>`.
    pub name: String,
    /// The commit id, lowercase hex (SHA-1 or SHA-256).
    pub oid: String,
}

/// Who moves the shared `main` of one code project (docs/decisions/omni-workspace-git.md section 2):
/// any maintainer, fast-forward only; with `main_needs_approval`, only to a commit another member
/// approved. Each project has its own (maintainers' decision 2026-11-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRuleV1 {
    pub repo: String,
    pub main_needs_approval: bool,
    /// When the admin set it (ms): the later rule wins.
    pub changed_at_unix_ms: i64,
}

/// "I approve this commit for `main` in `repo`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceApprovalV1 {
    pub repo: String,
    pub oid: String,
}

pub fn validate_workspace_approval_v1(approval: &WorkspaceApprovalV1) -> OmniResult<()> {
    validate_workspace_repo_name(&approval.repo)?;
    if !valid_oid(&approval.oid) {
        return Err(OmniError::InvalidInput("a commit id is lowercase hex".to_string()));
    }
    Ok(())
}

pub fn validate_workspace_repo_name(name: &str) -> OmniResult<()> {
    let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if name.is_empty() || name.len() > WORKSPACE_REPO_NAME_MAX || !plain || name.starts_with('.') {
        return Err(OmniError::InvalidInput("a repository name is letters, digits, '-', '_' and '.'".to_string()));
    }
    Ok(())
}

/// A branch name git itself would take and nothing a command line or a path could read more into:
/// under `refs/heads/`, no `..`, no `//`, no leading `-` in a segment, no `.lock` ending.
fn valid_branch_ref(name: &str) -> bool {
    let Some(branch) = name.strip_prefix("refs/heads/") else {
        return false;
    };
    name.len() <= WORKSPACE_REF_NAME_MAX
        && !branch.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.'))
        && !name.contains("..")
        && !name.contains("//")
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && branch.split('/').all(|segment| !segment.starts_with(['-', '.']))
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn validate_workspace_refs_v1(refs: &WorkspaceRefsV1) -> OmniResult<()> {
    validate_workspace_repo_name(&refs.repo)?;
    if refs.refs.len() > WORKSPACE_REFS_MAX {
        return Err(OmniError::InvalidInput("too many branches in one announcement".to_string()));
    }
    for r in &refs.refs {
        if !valid_branch_ref(&r.name) || !valid_oid(&r.oid) {
            return Err(OmniError::InvalidInput("a workspace branch is malformed".to_string()));
        }
    }
    Ok(())
}

/// What happens to the agents of a workspace. Each node checks who may say it: `Stop` an admin,
/// `Answer` the owner of the agent that asked, `Ask` an agent, `Review` anyone but the author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkspaceAgentsEventV1 {
    /// Stop all agents of the workspace (`stopped`) or let them go again; the later one wins.
    Stop { stopped: bool, at_unix_ms: i64 },
    /// An agent asks its owner: put `oid` of `branch` into `main` of `repo` (`action` = `main`), or a
    /// plain yes/no about `text` (`other`).
    Ask {
        id: String,
        owner: String,
        action: WorkspaceAgentAskV1,
        #[serde(default)]
        repo: String,
        #[serde(default)]
        branch: String,
        #[serde(default)]
        oid: String,
        text: String,
        at_unix_ms: i64,
        /// The discussion of the work it is about: shown there as a card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    Answer {
        id: String,
        allow: bool,
        at_unix_ms: i64,
        /// Why not, or what to change («Вернуть на доработку»).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },
    /// The author asks for a review of `oid`, the tip of `branch`.
    ReviewAsk {
        repo: String,
        branch: String,
        oid: String,
        at_unix_ms: i64,
        /// The discussion of the work: the ask and the verdicts on it are shown there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// A verdict on exactly `oid`: «принято» counts only with the tests passed.
    Review { repo: String, branch: String, oid: String, accepted: bool, tests_passed: bool, notes: String, at_unix_ms: i64 },
    /// A discussion of a piece of work opens: what it is about, and its task if it is a task's.
    ThreadOpen {
        thread: String,
        topic: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        at_unix_ms: i64,
    },
    /// A line of a discussion; the author is the event's sender.
    Talk {
        id: String,
        thread: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        /// The accounts the line names (`resolve_mentions`): what makes it a command to an agent.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mentions: Vec<String>,
        at_unix_ms: i64,
    },
    /// A person's emoji on a line of a discussion; empty takes it away, the latest stands.
    TalkReaction { thread: String, line: String, emoji: String, at_unix_ms: i64 },
    /// A line of a discussion taken back by its author.
    TalkDelete { thread: String, line: String },
    /// An agent got the lines of a discussion up to `upto`, its latest from others: people see it heard.
    TalkSeen { thread: String, upto: String, at_unix_ms: i64 },
    /// An agent waits for lines (omni_wait runs) until `until_unix_ms`; `until` at `at` or before: it stopped.
    Listening { until_unix_ms: i64, at_unix_ms: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAgentAskV1 {
    Main,
    Other,
}

/// What an agent asks or a reviewer writes, in bytes.
pub const WORKSPACE_AGENT_TEXT_MAX: usize = 4000;
/// A line of a discussion, in bytes (the group's event carries it whole).
pub const WORKSPACE_TALK_TEXT_MAX: usize = 12 * 1024;
/// How far ahead an agent's «listening» mark may reach: the longest wait and some slack.
pub const WORKSPACE_LISTENING_MAX_MS: i64 = 20 * 60 * 1000;
/// A discussion's topic, in characters.
pub const WORKSPACE_TALK_TOPIC_MAX: usize = 200;

/// A discussion's id: `task-<task>` or `t-<random>`, short and plain.
/// A line names at most this many members.
pub const GROUP_LINE_MENTIONS_MAX: usize = 64;

fn valid_mentions(mentions: &[String]) -> bool {
    mentions.len() <= GROUP_LINE_MENTIONS_MAX && mentions.iter().all(|account| !account.is_empty() && account.len() <= 128)
}

fn valid_thread_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn validate_workspace_agents_event_v1(event: &WorkspaceAgentsEventV1) -> OmniResult<()> {
    let bad = |what: &str| Err(OmniError::InvalidInput(format!("workspace agents event: {what}")));
    let code = |repo: &str, branch: &str, oid: &str| -> OmniResult<()> {
        validate_workspace_repo_name(repo)?;
        if !valid_branch_ref(&format!("refs/heads/{branch}")) || !valid_oid(oid) {
            return Err(OmniError::InvalidInput("workspace agents event: a branch or commit is malformed".to_string()));
        }
        Ok(())
    };
    match event {
        WorkspaceAgentsEventV1::Stop { .. } => Ok(()),
        WorkspaceAgentsEventV1::Ask { id, owner, action, repo, branch, oid, text, thread, .. } => {
            if id.is_empty() || id.len() > 64 || owner.is_empty() || owner.len() > 128 || text.len() > WORKSPACE_AGENT_TEXT_MAX || thread.as_deref().is_some_and(|t| !valid_thread_id(t)) {
                return bad("an ask is malformed");
            }
            match action {
                WorkspaceAgentAskV1::Main => code(repo, branch, oid),
                WorkspaceAgentAskV1::Other if text.trim().is_empty() => bad("an ask without a question"),
                WorkspaceAgentAskV1::Other => Ok(()),
            }
        }
        WorkspaceAgentsEventV1::Answer { id, reason, .. } if id.is_empty() || id.len() > 64 || reason.len() > WORKSPACE_AGENT_TEXT_MAX => bad("an answer is malformed"),
        WorkspaceAgentsEventV1::Answer { .. } => Ok(()),
        WorkspaceAgentsEventV1::ReviewAsk { thread: Some(thread), .. } if !valid_thread_id(thread) => bad("a review ask's discussion is malformed"),
        WorkspaceAgentsEventV1::ReviewAsk { repo, branch, oid, .. } => code(repo, branch, oid),
        WorkspaceAgentsEventV1::ThreadOpen { thread, topic, task, .. } => {
            if !valid_thread_id(thread) || topic.trim().is_empty() || topic.chars().count() > WORKSPACE_TALK_TOPIC_MAX || task.as_ref().is_some_and(|t| t.is_empty() || t.len() > 64) {
                return bad("a discussion's opening is malformed");
            }
            Ok(())
        }
        WorkspaceAgentsEventV1::Talk { mentions, .. } if !valid_mentions(mentions) => bad("a discussion's line names too many"),
        WorkspaceAgentsEventV1::Talk { id, thread, text, reply_to, .. } => {
            if id.is_empty() || id.len() > 64 || !valid_thread_id(thread) || text.trim().is_empty() || text.len() > WORKSPACE_TALK_TEXT_MAX || reply_to.as_ref().is_some_and(|r| r.is_empty() || r.len() > 64) {
                return bad("a discussion's line is malformed");
            }
            Ok(())
        }
        WorkspaceAgentsEventV1::TalkSeen { thread, upto, .. } if !valid_thread_id(thread) || upto.is_empty() || upto.len() > 64 => bad("a discussion's seen mark is malformed"),
        WorkspaceAgentsEventV1::TalkSeen { .. } => Ok(()),
        WorkspaceAgentsEventV1::TalkReaction { thread, line, emoji, .. } if !valid_thread_id(thread) || line.is_empty() || line.len() > 64 || emoji.len() > 64 => bad("a reaction in a discussion is malformed"),
        WorkspaceAgentsEventV1::TalkReaction { .. } => Ok(()),
        WorkspaceAgentsEventV1::TalkDelete { thread, line } if !valid_thread_id(thread) || line.is_empty() || line.len() > 64 => bad("a deletion in a discussion is malformed"),
        WorkspaceAgentsEventV1::TalkDelete { .. } => Ok(()),
        WorkspaceAgentsEventV1::Listening { until_unix_ms, at_unix_ms } if *until_unix_ms > at_unix_ms.saturating_add(WORKSPACE_LISTENING_MAX_MS) => bad("listening too far ahead"),
        WorkspaceAgentsEventV1::Listening { .. } => Ok(()),
        WorkspaceAgentsEventV1::Review { repo, branch, oid, notes, .. } => {
            if notes.len() > WORKSPACE_AGENT_TEXT_MAX {
                return bad("review notes are too long");
            }
            code(repo, branch, oid)
        }
    }
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
    /// An agent's card: the account of its owner (docs/decisions/omni-workspace-git.md section 5).
    /// Mutual with the owner's `agents`; on its own it already limits what the sender may do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_of: Option<String>,
    /// The owner's card: its agents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<GroupAgentV1>,
    /// When the sender last changed `agent_of` or `agents` (ms, its clock) — newer wins, apart from
    /// `version`: they change without a new name, and a card late or sent again must not bring back
    /// an agent's right its owner took away (code audit 2026-11-09, OMNI-04).
    #[serde(default)]
    pub agents_version: i64,
}

/// One of the owner's agents, as the owner's card names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAgentV1 {
    pub account_id: String,
    /// The workspace's members may give it work too, not only the owner.
    #[serde(default)]
    pub members_command: bool,
}

/// Agents one owner's card may name.
pub const GROUP_AGENTS_MAX: usize = 16;

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
    let account = |id: &str| !id.trim().is_empty() && id.len() <= 128;
    if profile.agent_of.as_deref().is_some_and(|owner| !account(owner) || owner == profile.account_id)
        || profile.agents.len() > GROUP_AGENTS_MAX
        || profile.agents.iter().any(|a| !account(&a.account_id) || a.account_id == profile.account_id)
    {
        return Err(OmniError::InvalidInput("group member profile agents are invalid".to_string()));
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
    /// The line this text answers (a file's offer says it itself). Signed with the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The accounts this text names (`resolve_mentions`). Signed with the line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
}

/// History for a newcomer: at most this many lines, at most this much JSON.
pub const GROUP_HISTORY_MAX_LINES: usize = 100;
pub const GROUP_HISTORY_MAX_BYTES: usize = 96 * 1024;

const GROUP_LINE_SIGNING_CONTEXT: &[u8] = b"omni-group-line-v1\0";

/// What a line's author signs: the group, the line, its kind, text, time, what it answers and whom it
/// names — length-prefixed, so no two different lines sign the same bytes.
pub fn group_line_signing_bytes(
    group_id: &str,
    message_id: &str,
    kind: GroupAppKindV1,
    body: &str,
    authored_at_unix_ms: i64,
    reply_to: Option<&str>,
    mentions: &[String],
) -> Vec<u8> {
    let kind = serde_json::to_string(&kind).unwrap_or_default();
    let mut bytes = GROUP_LINE_SIGNING_CONTEXT.to_vec();
    let reply_to = reply_to.unwrap_or_default();
    // An account has no line break in it, so no two lists join the same.
    let mentions = mentions.join("\n");
    for part in [group_id.as_bytes(), message_id.as_bytes(), kind.as_bytes(), body.as_bytes(), reply_to.as_bytes(), mentions.as_bytes()] {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
}

/// Shapes and sizes before anything in the packet is used.
pub fn validate_group_history_packet_v1(packet: &GroupHistoryPacketV1) -> OmniResult<()> {
    if packet.lines.len() > GROUP_HISTORY_MAX_LINES {
        return Err(OmniError::InvalidInput("group history carries too many lines".to_string()));
    }
    for line in &packet.lines {
        if line.message_id.is_empty() || line.message_id.len() > 64 || line.reply_to.as_ref().is_some_and(|id| id.len() > 64) || !valid_mentions(&line.mentions) {
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
    // No `End`: a call ends when its last participant leaves. An `End` any member could send ended it
    // for everyone (blind audit 2026-11-10, C5); after that fix it meant `Leave`, and so it went
    // (maintainers' decision 2026-11-11).
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
    if payload.message_id.is_empty()
        || payload.message_id.len() > 64
        || payload.message_id.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/' || c == '\\')
        || payload.reply_to.as_ref().is_some_and(|id| id.len() > 64) || !valid_mentions(&payload.mentions) {
        return Err(OmniError::InvalidInput("group message id is invalid".to_string()));
    }
    let max_body = match payload.kind {
        GroupAppKindV1::Attachment => GROUP_ATTACHMENT_OFFER_MAX_BYTES,
        GroupAppKindV1::History => GROUP_HISTORY_MAX_BYTES,
        GroupAppKindV1::Profile => GROUP_MEMBER_PROFILE_MAX_BYTES,
        GroupAppKindV1::WorkspaceRefs => WORKSPACE_REFS_MAX_BYTES,
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

    #[test]
    fn a_workspace_announcement_takes_plain_branches_and_commit_ids_only() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let ok = |name: &str| WorkspaceRefsV1 { repo: "api".into(), refs: vec![WorkspaceRefV1 { name: name.into(), oid: oid.into() }] };
        assert!(validate_workspace_refs_v1(&ok("refs/heads/main")).is_ok());
        assert!(validate_workspace_refs_v1(&ok("refs/heads/agent/helper/12-migration")).is_ok());
        for bad in ["main", "refs/tags/v1", "refs/heads/", "refs/heads/a/../b", "refs/heads/-x", "refs/heads/a//b", "refs/heads/x.lock", "refs/heads/a b", "refs/heads/.hidden"] {
            assert!(validate_workspace_refs_v1(&ok(bad)).is_err(), "{bad:?} must be refused");
        }
        let mut upper = ok("refs/heads/main");
        upper.refs[0].oid = oid.to_uppercase();
        assert!(validate_workspace_refs_v1(&upper).is_err(), "ids are lowercase hex");
        let mut repo = ok("refs/heads/main");
        repo.repo = "../x".into();
        assert!(validate_workspace_refs_v1(&repo).is_err());
        let many = WorkspaceRefsV1 {
            repo: "api".into(),
            refs: (0..=WORKSPACE_REFS_MAX).map(|i| WorkspaceRefV1 { name: format!("refs/heads/b{i}"), oid: oid.into() }).collect(),
        };
        assert!(validate_workspace_refs_v1(&many).is_err());
        let approval = |oid: &str| WorkspaceApprovalV1 { repo: "api".into(), oid: oid.into() };
        assert!(validate_workspace_approval_v1(&approval(oid)).is_ok());
        assert!(validate_workspace_approval_v1(&approval("HEAD")).is_err());
    }

    const GID: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn group_wire_validates_and_refuses_malformed() {
        let item = GroupFeedItemV2 { version: GROUP_WIRE_VERSION_V2, group_id: GID.into(), mls_b64: "AAAA".into() };
        validate_group_feed_item_v2(&item).unwrap();
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<GroupFeedItemV2>(&json).unwrap(), item);
        let payload = GroupAppPayloadV1 {
            mentions: Vec::new(),
            reply_to: None,
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
