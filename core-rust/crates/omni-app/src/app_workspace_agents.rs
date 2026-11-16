//! Agents of a workspace (docs/decisions/omni-workspace-git.md section 5, «Как сделано»).
//!
//! An agent is a profile of its own, driven by a tool of its owner through `omni-mcp`. Who it belongs to is
//! mutual: the agent's member card names its owner, the owner's card names its agents. What an agent
//! may do is held by every node, not by the agent: it is never a maintainer or a custodian, its
//! approvals do not count, its branches are `agent/*` only. The owner answers its questions
//! («Разрешить / Отклонить»), an admin stops all agents of the workspace. Limits on how much agents
//! talk without a person are held by the agent's own node — `omni-mcp` is Omni, not the agent.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use omni_group_mls::MemberCard;
use omni_identity::{ContactRelationshipState, ContactRequestDirection};
use omni_protocol::{
    validate_workspace_agents_event_v1, GroupAgentV1, GroupAppKindV1, GroupMemberProfileV1, WorkspaceAgentAskV1,
    WorkspaceAgentsEventV1, WORKSPACE_LISTENING_MAX_MS,
};
use omni_storage::{GroupMemberRecord, Storage};
use omni_types::{OmniError, OmniResult};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{current_unix_ms, omni_app_trace_log, OmniApp};

/// Agents' messages in a row in a group's thread with no person in between, after which an agent
/// waits for a person.
pub const AGENT_TURNS_WITHOUT_HUMAN: usize = 6;
/// Messages one agent sends in a minute.
pub const AGENT_MESSAGES_PER_MINUTE: usize = 10;
/// Rounds of «Проработать вдвоём».
pub const PAIR_ROUNDS: u32 = 4;
/// Branches an agent announces start with this.
pub const AGENT_BRANCH_PREFIX: &str = "agent/";
/// What the agent's code starts with (the owner pastes it, or Omni reads it from the agent's folder).
const CODE_PREFIX: &str = "omni-agent:";
/// The file in the agent's folder that holds its code.
pub const AGENT_CODE_FILE: &str = "agent-code.txt";

/// account → its owner, as each agent's own card says.
const CLAIMS: &str = "agent_claims";
/// owner → its agents, as each owner's card says.
const LISTS: &str = "agent_lists";
/// This profile is an agent: whose and what it is called.
const SELF: &str = "agent_self";
/// This profile's agents.
const MINE: &str = "my_agents";
/// When our card's agent fields last changed (its `agents_version`).
const OWN_AGENTS_VERSION: &str = "agent_fields_version";
/// account → the `agents_version` of its card we took the agent fields from.
const AGENT_VERSIONS: &str = "agent_card_versions";
/// agent → owner, once both said so (the agent's card and its owner's): the agent stays its owner's
/// until the owner takes it off its list, whatever the agent's own card says later (code audit
/// 2026-11-09, OMNI-08). The agent's word alone still counts — against itself only.
const CONFIRMED: &str = "agent_confirmed";
const ASKS_KEPT: usize = 100;
/// What «Изменения…» shows of a «main» ask at most.
const DIFF_MAX_BYTES: usize = 256 * 1024;
/// A «main» ask's card lists this many commits and files at most.
const ASK_LIST_MAX: usize = 12;
const REVIEWS_KEPT: usize = 200;

fn log_key(group_id: &str) -> String {
    format!("workspace_agents:{group_id}")
}

/// The verdicts on its branches an agent was told of, kept in its profile: one that came while its
/// server was down is told when it starts, and none twice.
/// Each agent's «listening until», and when it said so (the later word wins).
/// How far ahead of our clock a «listening» mark's own time may be.
const LISTENING_CLOCK_SLACK_MS: i64 = 5 * 60 * 1000;

fn listening_key(group_id: &str) -> String {
    format!("workspace_agents_listening:{group_id}")
}

/// Until when the agent said it listens (0: never said).
pub(crate) fn listening_until(storage: &Storage, group_id: &str, account: &str) -> i64 {
    read::<HashMap<String, (i64, i64)>>(storage, &listening_key(group_id)).get(account).map_or(0, |(until, _)| *until)
}

/// An agent says it listens a little past its wait; it says it again once less than this is left.
const LISTEN_SLACK_MS: i64 = 3 * 60 * 1000;
const LISTEN_RENEW_MS: i64 = 60 * 1000;

fn verdicts_told_key(group_id: &str) -> String {
    format!("agent_verdicts_told:{group_id}")
}

pub(super) fn read<T: DeserializeOwned + Default>(storage: &Storage, key: &str) -> T {
    storage.get_profile_meta(key).ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default()
}

pub(super) fn write<T: Serialize>(storage: &Storage, key: &str, value: &T) -> OmniResult<()> {
    let json = serde_json::to_string(value).map_err(|e| OmniError::Internal(e.to_string()))?;
    storage.upsert_profile_meta(key, &json)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentSelf {
    pub owner: String,
    pub name: String,
}

/// One of our agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyAgent {
    pub account_id: String,
    pub name: String,
    #[serde(default)]
    pub members_command: bool,
    pub connected_at_unix_ms: i64,
    /// Workspaces it is to be asked into once its contact is there.
    #[serde(default)]
    pub invite_to: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Log {
    #[serde(default)]
    stop: Option<StopRecord>,
    #[serde(default)]
    asks: Vec<AskRecord>,
    #[serde(default)]
    reviews: Vec<ReviewRecord>,
    #[serde(default)]
    review_asks: Vec<ReviewAskRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StopRecord {
    stopped: bool,
    at_unix_ms: i64,
    by_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AskRecord {
    id: String,
    agent_account: String,
    agent_name: String,
    owner: String,
    action: WorkspaceAgentAskV1,
    repo: String,
    branch: String,
    oid: String,
    text: String,
    at_unix_ms: i64,
    #[serde(default)]
    answer: Option<bool>,
    /// The owner's why, with a «no» («Вернуть на доработку»).
    #[serde(default)]
    reason: String,
    /// The discussion of the work it is about.
    #[serde(default)]
    thread: Option<String>,
    /// When the answer came here (0 while open).
    #[serde(default)]
    answered_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewRecord {
    repo: String,
    branch: String,
    oid: String,
    reviewer_account: String,
    reviewer_name: String,
    accepted: bool,
    tests_passed: bool,
    notes: String,
    at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewAskRecord {
    repo: String,
    branch: String,
    oid: String,
    author_account: String,
    author_name: String,
    at_unix_ms: i64,
    #[serde(default)]
    thread: Option<String>,
}

/// What the agents' place of a workspace shows.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceAgentsView {
    /// Our account: what an agent's settings name as its owner.
    pub my_account: String,
    pub stopped: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stopped_by: String,
    /// We are an admin: stopping all agents is ours.
    pub can_stop: bool,
    pub agents: Vec<WorkspaceAgentView>,
    /// Questions of agents, newest first: open ones and the last answered.
    pub asks: Vec<WorkspaceAgentAskView>,
    pub reviews: Vec<WorkspaceAgentReviewView>,
    /// Branches their authors asked to have checked and nobody has given a verdict on that commit yet,
    /// newest first: a person checks them here too, not only another agent.
    pub review_asks: Vec<WorkspaceReviewAskView>,
    pub turns_without_human: usize,
    pub rounds: u32,
    /// What changed in a «main» ask, asked for by `diff`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The discussions, for the talk commands.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<super::talk::WorkspaceTalkThreadView>,
    /// One discussion, for `talk_read`, `talk_open`, `talk_send` and `give`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub talk: Option<super::talk::WorkspaceTalkView>,
    /// The discussion `talk_open` or `give` opened (or found).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened: Option<String>,
    /// The agent's application settings file `add_to_app` wrote Omni into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_to: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceAgentView {
    pub account_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub owner_name: String,
    pub mine: bool,
    /// The owner's card names it too.
    pub confirmed: bool,
    pub in_group: bool,
    /// in_group | invited | connecting
    pub state: String,
    pub members_command: bool,
    /// A link to the agent's node now: its server runs.
    pub online: bool,
    /// It waits for lines now (omni_wait runs): a line naming it reaches the model.
    pub listening: bool,
    /// Its latest line in the thread, and when.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_said: String,
    pub last_active_unix_ms: i64,
    /// Tasks given to it that are not done («#3 Проверить допуски»).
    pub tasks: Vec<String>,
    /// Its questions to its owner nobody answered yet.
    pub waiting_answer: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceAgentAskView {
    pub id: String,
    pub agent_name: String,
    /// main | other
    pub action: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub oid: String,
    pub text: String,
    pub at_unix_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<bool>,
    /// We are the owner and it is open.
    pub can_answer: bool,
    /// Who accepted exactly this commit (with the tests passed).
    pub accepted_by: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// When it was answered (0 while open).
    pub answered_unix_ms: i64,
    /// A «main» ask whose commit is here: what it brings over our `main` — the commits' subjects and
    /// the files changed (status and path), a few of each.
    pub commits: Vec<String>,
    pub files: Vec<String>,
    /// The discussion of the work it is about: a card there too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceAgentReviewView {
    pub repo: String,
    pub branch: String,
    pub oid: String,
    pub reviewer_name: String,
    pub accepted: bool,
    pub tests_passed: bool,
    pub notes: String,
    pub at_unix_ms: i64,
    /// The discussion the check was asked in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

/// A branch its author asked to have checked.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceReviewAskView {
    pub repo: String,
    pub branch: String,
    pub oid: String,
    pub author_name: String,
    pub at_unix_ms: i64,
    /// The commit is here: it can be read and accepted.
    pub here: bool,
    /// Not the author's own: we may give the verdict.
    pub can_review: bool,
    /// What it brings over our `main`, a few of each (as a «main» ask shows them).
    pub commits: Vec<String>,
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
}

/// A message of a group's thread as an agent reads it: a command only from its owner (or, when the
/// owner allowed it, a member) naming it; everything else is data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInboxItem {
    pub group_id: String,
    pub message_id: String,
    pub from_name: String,
    pub from_account: String,
    pub from_owner: bool,
    pub from_agent: bool,
    pub command: bool,
    pub text: String,
    pub at_unix_ms: i64,
    /// The discussion this line is of (a line of the group's thread has none): answer there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
}

/// Where an agent's reading of a group's thread stands: after line `seq` of the order lines came in
/// here, and — before its first look found any — not lines that came before `arrived_since_unix_ms`
/// (our clock: lines that came before the agent started are not new to it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInboxPlace {
    pub seq: i64,
    pub arrived_since_unix_ms: i64,
}

/// New lines of a group's thread and where the reading stands after them.
#[derive(Debug, Clone)]
pub struct AgentInbox {
    pub items: Vec<AgentInboxItem>,
    pub place: AgentInboxPlace,
}

/// What the shell asks of the agents' place; every command answers with the place as it is now.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkspaceAgentsCommand {
    View { group_id: String },
    Stop { group_id: String, stopped: bool },
    Answer {
        group_id: String,
        id: String,
        allow: bool,
        #[serde(default)]
        reason: String,
    },
    /// What a «main» ask (`id`) or a branch asked to be checked (`repo`, `oid`) changes, whole (the
    /// view's `diff`).
    Diff {
        group_id: String,
        #[serde(default)]
        id: String,
        #[serde(default)]
        repo: String,
        #[serde(default)]
        oid: String,
    },
    /// «Дать работу»: a command to our agent in the thread; with a task, the task is given to it.
    Give {
        group_id: String,
        account_id: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        task: Option<String>,
    },
    /// «Проработать вдвоём», begun by a person for two of their agents.
    PairStart {
        group_id: String,
        task: String,
        first: String,
        second: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        rounds: Option<u32>,
    },
    /// A person ends the task's session that goes on.
    PairClose { group_id: String, task: String },
    /// Write this Omni into the agent's own application (`claude` | `codex`) as its MCP server, with
    /// the agent's `name` and `folder` and the Omni program `exe`: the view's `added_to`.
    AddToApp { group_id: String, app: String, name: String, folder: String, exe: String },
    /// Whether `app` already starts the agent `name` with this `exe`: the view's `added_to` when it does.
    InApp { group_id: String, app: String, name: String, exe: String },
    /// The workspace's discussions (the view's `threads`).
    TalkList { group_id: String },
    /// One discussion, whole (the view's `talk`), with the list.
    TalkRead { group_id: String, thread: String },
    /// Open a discussion (a task's one is found if it is there): the view's `opened` and `talk`.
    TalkOpen {
        group_id: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        task: Option<String>,
    },
    /// A person's line in a discussion; the view's `talk` after it.
    TalkSend {
        group_id: String,
        thread: String,
        text: String,
        #[serde(default)]
        reply_to: Option<String>,
        /// The accounts chosen in the suggestion list after `@`.
        #[serde(default)]
        mentions: Option<Vec<String>>,
    },
    /// Our own line of a discussion taken back. The view's `talk` after it.
    TalkDelete {
        group_id: String,
        thread: String,
        line: String,
    },
    /// Our emoji on a line of a discussion; empty takes it away. The view's `talk` after it.
    TalkReact {
        group_id: String,
        thread: String,
        line: String,
        #[serde(default)]
        emoji: String,
    },
    Connect {
        group_id: String,
        code: String,
        name: String,
        #[serde(default)]
        members_command: bool,
    },
    Forget { group_id: String, account_id: String },
    Review {
        group_id: String,
        repo: String,
        branch: String,
        oid: String,
        accepted: bool,
        #[serde(default)]
        tests_passed: bool,
        #[serde(default)]
        notes: String,
    },
}

/// account → owner: the agents' own cards, the pairs both sides confirmed (an agent cannot step out
/// by dropping `agent_of` from its card), and our own agents.
pub(crate) fn claims(storage: &Storage) -> HashMap<String, String> {
    let mut claims: HashMap<String, String> = read(storage, CLAIMS);
    let lists = lists(storage);
    let listed = |agent: &str, owner: &str| lists.get(owner).is_some_and(|list| list.iter().any(|a| a.account_id == agent));
    let confirmed: HashMap<String, String> = read(storage, CONFIRMED);
    for (agent, owner) in confirmed {
        if listed(&agent, &owner) {
            claims.insert(agent, owner);
        }
    }
    if let Some(me) = storage.get_local_account().ok().flatten() {
        // Our own list is our word: an agent of ours is one here whatever its card says.
        for agent in my_agents(storage) {
            claims.insert(agent.account_id, me.descriptor.account_id.0.clone());
        }
        if let Some(agent) = agent_self(storage) {
            claims.insert(me.descriptor.account_id.0, agent.owner);
        }
    }
    claims
}

/// Our card's `agents_version`.
pub(crate) fn own_agents_version(storage: &Storage) -> i64 {
    read(storage, OWN_AGENTS_VERSION)
}

/// Our card's agent fields changed: a newer version, so a card of ours sent before loses to it.
fn bump_own_agents_version(storage: &Storage) -> OmniResult<()> {
    let next = (own_agents_version(storage) + 1).max(current_unix_ms());
    write(storage, OWN_AGENTS_VERSION, &next)
}

/// owner → its agents, from the owners' cards — and ours, which we never receive.
pub(super) fn lists(storage: &Storage) -> HashMap<String, Vec<GroupAgentV1>> {
    let mut lists: HashMap<String, Vec<GroupAgentV1>> = read(storage, LISTS);
    if let Some(me) = storage.get_local_account().ok().flatten() {
        let (_, mine) = own_card_fields(storage);
        lists.insert(me.descriptor.account_id.0, mine);
    }
    lists
}

pub(crate) fn is_agent(storage: &Storage, account: &str) -> bool {
    claims(storage).contains_key(account)
}

pub fn agent_self(storage: &Storage) -> Option<AgentSelf> {
    storage.get_profile_meta(SELF).ok().flatten().and_then(|json| serde_json::from_str(&json).ok())
}

fn my_agents(storage: &Storage) -> Vec<MyAgent> {
    read(storage, MINE)
}

/// The group's members as the workspace sees them: an agent is never an admin here, so it is never a
/// maintainer, never sets a rule and never reviews as one, whatever its leaf says.
pub(crate) fn members(storage: &Storage, group_id: &str) -> OmniResult<Vec<GroupMemberRecord>> {
    let mut members = storage.list_group_members(group_id)?;
    let claims = claims(storage);
    for member in members.iter_mut().filter(|m| claims.contains_key(&m.account_id)) {
        member.role = "member".into();
    }
    Ok(members)
}

/// What our card says about agents.
pub(crate) fn own_card_fields(storage: &Storage) -> (Option<String>, Vec<GroupAgentV1>) {
    let agent_of = agent_self(storage).map(|me| me.owner);
    let agents = my_agents(storage)
        .into_iter()
        .map(|a| GroupAgentV1 { account_id: a.account_id, members_command: a.members_command })
        .collect();
    (agent_of, agents)
}

/// A member's card arrived (already checked to be the sender's own): what it says about agents, if
/// it is newer than the card we took them from. Returns whether that changed.
pub(crate) fn take_card(storage: &Storage, card: &GroupMemberProfileV1) -> OmniResult<bool> {
    let mut versions: HashMap<String, i64> = read(storage, AGENT_VERSIONS);
    if versions.get(&card.account_id).is_some_and(|seen| *seen >= card.agents_version) {
        return Ok(false); // late or sent again: what we have is as new
    }
    versions.insert(card.account_id.clone(), card.agents_version);
    write(storage, AGENT_VERSIONS, &versions)?;
    let mut claims: HashMap<String, String> = read(storage, CLAIMS);
    let mut lists: HashMap<String, Vec<GroupAgentV1>> = read(storage, LISTS);
    let claim_before = claims.get(&card.account_id).cloned();
    let list_before = lists.get(&card.account_id).cloned().unwrap_or_default();
    match &card.agent_of {
        Some(owner) => claims.insert(card.account_id.clone(), owner.clone()),
        None => claims.remove(&card.account_id),
    };
    if card.agents.is_empty() {
        lists.remove(&card.account_id);
    } else {
        lists.insert(card.account_id.clone(), card.agents.clone());
    }
    let changed = claim_before != card.agent_of || list_before != card.agents;
    if changed {
        write(storage, CLAIMS, &claims)?;
        write(storage, LISTS, &lists)?;
    }
    // Both sides say so: confirmed (it outlives the agent's own word, not the owner's).
    let all_lists = self::lists(storage);
    let listed = |agent: &str, owner: &str| all_lists.get(owner).is_some_and(|list| list.iter().any(|a| a.account_id == agent));
    let mut confirmed: HashMap<String, String> = read(storage, CONFIRMED);
    let before = confirmed.clone();
    for (agent, owner) in &claims {
        if listed(agent, owner) {
            confirmed.insert(agent.clone(), owner.clone());
        }
    }
    confirmed.retain(|agent, owner| listed(agent, owner));
    if confirmed != before {
        write(storage, CONFIRMED, &confirmed)?;
    }
    Ok(changed)
}

/// member_pub → what follows a member's name: the owner's name for an agent its owner names too,
/// the bare word for one only it says so.
pub(crate) fn labels(storage: &Storage, members: &[GroupMemberRecord]) -> HashMap<String, String> {
    let claims = claims(storage);
    let lists = lists(storage);
    members
        .iter()
        .filter_map(|m| {
            let owner = claims.get(&m.account_id)?;
            let confirmed = lists.get(owner).is_some_and(|list| list.iter().any(|a| a.account_id == m.account_id));
            let owner_name = members.iter().find(|o| &o.account_id == owner).map(|o| o.display_name.clone());
            Some((
                m.member_pub.clone(),
                match owner_name.filter(|_| confirmed) {
                    Some(name) => format!(" · агент {name}"),
                    None => " · агент".to_string(),
                },
            ))
        })
        .collect()
}

pub(crate) fn stopped(storage: &Storage, group_id: &str) -> bool {
    read::<Log>(storage, &log_key(group_id)).stop.is_some_and(|s| s.stopped)
}

fn store_log(storage: &Storage, group_id: &str, mut log: Log) -> OmniResult<()> {
    // Past each bound, whoever holds the most gives up their oldest. The oldest of all went: a
    // member's 201 reviews pushed out another's «accepted» that main waited on, an agent's 101 asks
    // another owner's open question (blind audit 2026-11-12, pass 15).
    use super::talk::drop_oldest_of_heaviest;
    drop_oldest_of_heaviest(&mut log.asks, ASKS_KEPT, |a| a.agent_account.as_str());
    drop_oldest_of_heaviest(&mut log.reviews, REVIEWS_KEPT, |r| r.reviewer_account.as_str());
    drop_oldest_of_heaviest(&mut log.review_asks, REVIEWS_KEPT, |r| r.author_account.as_str());
    write(storage, &log_key(group_id), &log)
}

fn short(oid: &str) -> &str {
    &oid[..oid.len().min(8)]
}

/// Take one event — ours as we send it, a member's as it arrives — if its sender may say it.
fn apply(storage: &Storage, group_id: &str, sender: &MemberCard, event: &WorkspaceAgentsEventV1, mine: bool) -> OmniResult<()> {
    let members = members(storage, group_id)?;
    let claims = claims(storage);
    let mut log: Log = read(storage, &log_key(group_id));
    let now = current_unix_ms();
    let line = |text: String| -> OmniResult<()> {
        if mine {
            return Ok(());
        }
        super::say_in_feed(storage, group_id, Some(&sender.account_id), Some(super::NEWS_AGENTS), &text, now)
    };
    // Agents stopped by an admin say nothing that counts: the stop was checked only by the agent's
    // own client, and a changed one went on reviewing — its «accepted» opened the way to main
    // (blind audit 2026-11-11, M5).
    if !matches!(event, WorkspaceAgentsEventV1::Stop { .. })
        && claims.contains_key(&sender.account_id)
        && log.stop.as_ref().is_some_and(|stop| stop.stopped)
    {
        omni_app_trace_log("workspace/agents", &format!("group={group_id} a stopped agent's event ignored"));
        return Ok(());
    }
    match event {
        WorkspaceAgentsEventV1::Stop { stopped, at_unix_ms } => {
            if !super::is_admin(&members, &sender.member_pub) {
                omni_app_trace_log("workspace/agents", &format!("group={group_id} a stop from a non-admin ignored"));
                return Ok(());
            }
            // Not ahead of our clock: one admin's Stop dated i64::MAX stood over every later start
            // (blind audit 2026-11-12, pass 15).
            if *at_unix_ms > now + LISTENING_CLOCK_SLACK_MS {
                omni_app_trace_log("workspace/agents", &format!("group={group_id} a stop dated ahead ignored"));
                return Ok(());
            }
            if log.stop.as_ref().is_some_and(|s| s.at_unix_ms >= *at_unix_ms) {
                return Ok(());
            }
            log.stop = Some(StopRecord { stopped: *stopped, at_unix_ms: *at_unix_ms, by_name: sender.display_name.clone() });
            store_log(storage, group_id, log)?;
            line(if *stopped { format!("{} остановил всех агентов", sender.display_name) } else { format!("{} пустил агентов", sender.display_name) })
        }
        WorkspaceAgentsEventV1::Ask { id, owner, action, repo, branch, oid, text, at_unix_ms, thread } => {
            // Dated as a discussion line is: not ahead of our clock (pass 15).
            let at_unix_ms = &super::talk::not_ahead(*at_unix_ms);
            if claims.get(&sender.account_id) != Some(owner) {
                omni_app_trace_log("workspace/agents", &format!("group={group_id} an ask from a non-agent or for another owner ignored"));
                return Ok(());
            }
            if log.asks.iter().any(|a| &a.id == id) {
                return Ok(());
            }
            log.asks.push(AskRecord {
                id: id.clone(),
                agent_account: sender.account_id.clone(),
                agent_name: sender.display_name.clone(),
                owner: owner.clone(),
                action: *action,
                repo: repo.clone(),
                branch: branch.clone(),
                oid: oid.clone(),
                text: text.clone(),
                at_unix_ms: *at_unix_ms,
                answer: None,
                reason: String::new(),
                thread: thread.clone(),
                answered_unix_ms: 0,
            });
            store_log(storage, group_id, log)?;
            let said = match action {
                WorkspaceAgentAskV1::Main => format!("просит в main · {repo}: {branch} ({})", short(oid)),
                WorkspaceAgentAskV1::Other => format!("спрашивает: {text}"),
            };
            if let Some(thread) = thread {
                super::talk::note_card(storage, group_id, thread, &sender.display_name, &said, *at_unix_ms)?;
            }
            line(format!("{} {said}", sender.display_name))
        }
        WorkspaceAgentsEventV1::Answer { id, allow, reason, .. } => {
            let Some(ask) = log.asks.iter_mut().find(|a| &a.id == id) else { return Ok(()) };
            if ask.owner != sender.account_id || ask.answer.is_some() {
                return Ok(());
            }
            ask.answer = Some(*allow);
            ask.reason = reason.clone();
            ask.answered_unix_ms = current_unix_ms();
            let why = if reason.is_empty() { String::new() } else { format!(": {reason}") };
            let text = match (ask.action, *allow) {
                (WorkspaceAgentAskV1::Main, true) => format!("{} применил изменения {} в общую версию «{}»", sender.display_name, ask.agent_name, ask.repo),
                (WorkspaceAgentAskV1::Main, false) => format!("{} вернул {} на доработку изменения «{}»{why}", sender.display_name, ask.agent_name, ask.repo),
                (WorkspaceAgentAskV1::Other, true) => format!("{} ответил {} «да» на «{}»", sender.display_name, ask.agent_name, ask.text),
                (WorkspaceAgentAskV1::Other, false) => format!("{} ответил {} «нет» на «{}»{why}", sender.display_name, ask.agent_name, ask.text),
            };
            store_log(storage, group_id, log)?;
            line(text)
        }
        WorkspaceAgentsEventV1::ReviewAsk { repo, branch, oid, at_unix_ms, thread } => {
            // Dated as a discussion line is: not ahead of our clock (pass 15).
            let at_unix_ms = &super::talk::not_ahead(*at_unix_ms);
            log.review_asks.retain(|r| !(r.repo == *repo && r.branch == *branch && r.author_account == sender.account_id));
            log.review_asks.push(ReviewAskRecord {
                repo: repo.clone(),
                branch: branch.clone(),
                oid: oid.clone(),
                author_account: sender.account_id.clone(),
                author_name: sender.display_name.clone(),
                at_unix_ms: *at_unix_ms,
                thread: thread.clone(),
            });
            store_log(storage, group_id, log)?;
            let said = format!("просит проверить · {repo}: {branch} ({})", short(oid));
            if let Some(thread) = thread {
                super::talk::note_card(storage, group_id, thread, &sender.display_name, &said, *at_unix_ms)?;
            }
            line(format!("{} {said}", sender.display_name))
        }
        WorkspaceAgentsEventV1::Review { repo, branch, oid, accepted, tests_passed, notes, at_unix_ms } => {
            // Dated as a discussion line is: not ahead of our clock (pass 15).
            let at_unix_ms = &super::talk::not_ahead(*at_unix_ms);
            log.reviews.retain(|r| !(r.repo == *repo && r.oid == *oid && r.reviewer_account == sender.account_id));
            log.reviews.push(ReviewRecord {
                repo: repo.clone(),
                branch: branch.clone(),
                oid: oid.clone(),
                reviewer_account: sender.account_id.clone(),
                reviewer_name: sender.display_name.clone(),
                accepted: *accepted,
                tests_passed: *tests_passed,
                notes: notes.clone(),
                at_unix_ms: *at_unix_ms,
            });
            let asked_in = log.review_asks.iter().rev().find(|a| a.repo == *repo && a.oid == *oid).and_then(|a| a.thread.clone());
            store_log(storage, group_id, log)?;
            let verdict = match (accepted, tests_passed) {
                (true, true) => "принято",
                (true, false) => "принято, но тесты не прошли",
                (false, _) => "нужны правки",
            };
            if let Some(thread) = asked_in {
                super::talk::note_card(storage, group_id, &thread, &sender.display_name, &format!("{branch} — {verdict}"), *at_unix_ms)?;
            }
            line(format!("{} · {repo}: {branch} ({}) — {verdict}", sender.display_name, short(oid)))
        }
        WorkspaceAgentsEventV1::ThreadOpen { thread, topic, task, at_unix_ms } => {
            super::talk::apply_open(storage, group_id, sender, thread, topic, task.as_deref(), *at_unix_ms, mine)
        }
        WorkspaceAgentsEventV1::Talk { id, thread, text, reply_to, mentions, at_unix_ms } => {
            super::talk::apply_talk(storage, group_id, sender, id, thread, text, reply_to.as_deref(), mentions, *at_unix_ms)
        }
        WorkspaceAgentsEventV1::TalkDelete { thread, line } => {
            super::talk::apply_delete(storage, group_id, sender, thread, line)
        }
        WorkspaceAgentsEventV1::TalkReaction { thread, line, emoji, at_unix_ms } => {
            super::talk::apply_reaction(storage, group_id, sender, thread, line, emoji, *at_unix_ms)
        }
        WorkspaceAgentsEventV1::TalkSeen { thread, upto, at_unix_ms } => {
            if !claims.contains_key(&sender.account_id) {
                return Ok(());
            }
            super::talk::apply_seen(storage, group_id, sender, thread, upto, *at_unix_ms)
        }
        WorkspaceAgentsEventV1::Listening { until_unix_ms, at_unix_ms } => {
            if !claims.contains_key(&sender.account_id) {
                return Ok(());
            }
            // By our clock, not the sender's: the event's own time bounded «until», and a changed
            // client named a time far ahead for both — «listening» for good, and no later mark
            // counted (blind audit 2026-11-11, pass 13).
            if *at_unix_ms > now + LISTENING_CLOCK_SLACK_MS {
                return Ok(());
            }
            let mut all: HashMap<String, (i64, i64)> = read(storage, &listening_key(group_id));
            if all.get(&sender.account_id).is_some_and(|(_, at)| at > at_unix_ms) {
                return Ok(());
            }
            all.insert(sender.account_id.clone(), ((*until_unix_ms).min(now + WORKSPACE_LISTENING_MAX_MS), *at_unix_ms));
            write(storage, &listening_key(group_id), &all)
        }
    }
}

/// A member's `WorkspaceAgents` line.
pub(crate) fn receive(storage: &Storage, group_id: &str, sender: &MemberCard, body: &str) -> OmniResult<()> {
    let event: WorkspaceAgentsEventV1 =
        serde_json::from_str(body).map_err(|e| OmniError::InvalidInput(format!("workspace agents event: {e}")))?;
    validate_workspace_agents_event_v1(&event)?;
    apply(storage, group_id, sender, &event, false)
}

/// The questions of `owner`'s agents in the workspace nobody answered yet.
pub(crate) fn open_asks_for(storage: &Storage, group_id: &str, owner: &str) -> usize {
    read::<Log>(storage, &log_key(group_id)).asks.iter().filter(|a| a.owner == owner && a.answer.is_none()).count()
}

/// Accepted with the tests passed, on exactly `oid`, by someone other than `author`.
fn accepted_by(log: &Log, repo: &str, oid: &str, author: &str) -> Vec<String> {
    log.reviews
        .iter()
        .filter(|r| r.repo == repo && r.oid == oid && r.accepted && r.tests_passed && r.reviewer_account != author)
        .map(|r| r.reviewer_name.clone())
        .collect()
}

/// Test hook: who accepted `oid` of `repo` with tests passed, `author` aside.
#[cfg(test)]
pub(crate) fn accepted_by_for_tests(storage: &Storage, group_id: &str, repo: &str, oid: &str, author: &str) -> Vec<String> {
    accepted_by(&read(storage, &log_key(group_id)), repo, oid, author)
}

fn me_card(storage: &Storage, group_id: &str) -> OmniResult<MemberCard> {
    let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
    if group.my_status != "member" {
        return Err(OmniError::InvalidInput("not an active member of the group".into()));
    }
    let (_, card) = crate::app_groups::local_mls_identity(storage)?;
    Ok(MemberCard { member_pub: group.my_member_pub, ..card })
}

pub(super) fn send(app: &OmniApp, group_id: &str, event: WorkspaceAgentsEventV1) -> OmniResult<()> {
    validate_workspace_agents_event_v1(&event)?;
    let me = me_card(&app.storage, group_id)?;
    // Applied here and said, as one: an answer the group never got stayed answered here and could
    // not be given again (blind audit 2026-11-11, K6) — and putting only the log back left a
    // discussion's lines, marks and reactions applied (pass 13).
    super::do_and_say(&app.storage, group_id, GroupAppKindV1::WorkspaceAgents, &event, |storage| apply(storage, group_id, &me, &event, true))?;
    app.flush_group_feed();
    Ok(())
}

/// Test hook: an open «to main» ask of `agent_account`'s, to `owner` (us), in the log of `group_id`.
#[cfg(test)]
pub(crate) fn put_main_ask_for_tests(storage: &Storage, group_id: &str, id: &str, agent_account: &str, owner: &str) {
    let mut log: Log = read(storage, &log_key(group_id));
    log.asks.push(AskRecord {
        id: id.to_string(),
        agent_account: agent_account.to_string(),
        agent_name: "agent".to_string(),
        owner: owner.to_string(),
        action: WorkspaceAgentAskV1::Main,
        repo: "project".to_string(),
        branch: "agent/change-1".to_string(),
        oid: "a".repeat(40),
        text: String::new(),
        at_unix_ms: 1,
        answer: None,
        reason: String::new(),
        thread: None,
        answered_unix_ms: 0,
    });
    write(storage, &log_key(group_id), &log).unwrap();
}

/// Test hook: an open «to main» ask for commit `oid` of `repo`, with an independent review that
/// accepted it.
#[cfg(test)]
pub(crate) fn put_reviewed_main_ask_for_tests(storage: &Storage, group_id: &str, id: &str, agent_account: &str, owner: &str, repo: &str, oid: &str) {
    put_main_ask_for_tests(storage, group_id, id, agent_account, owner);
    let mut log: Log = read(storage, &log_key(group_id));
    let ask = log.asks.iter_mut().find(|ask| ask.id == id).unwrap();
    ask.repo = repo.to_string();
    ask.oid = oid.to_string();
    log.reviews.push(ReviewRecord {
        repo: repo.to_string(),
        branch: ask.branch.clone(),
        oid: oid.to_string(),
        reviewer_account: "acct-reviewer".to_string(),
        reviewer_name: "reviewer".to_string(),
        accepted: true,
        tests_passed: true,
        notes: String::new(),
        at_unix_ms: 1,
    });
    write(storage, &log_key(group_id), &log).unwrap();
}

/// Test hook: an agents event sent as a client that skips its own checks would send it.
#[cfg(test)]
pub(crate) fn send_for_tests(app: &OmniApp, group_id: &str, event: WorkspaceAgentsEventV1) -> OmniResult<()> {
    send(app, group_id, event)
}

/// Test hook: the answer an ask has here.
#[cfg(test)]
pub(crate) fn ask_answer_for_tests(storage: &Storage, group_id: &str, id: &str) -> Option<bool> {
    let log: Log = read(storage, &log_key(group_id));
    log.asks.iter().find(|ask| ask.id == id).and_then(|ask| ask.answer)
}

/// The agent's code: its account and a mesh invite to it, one line.
pub fn encode_code(account: &str, invite: &str) -> String {
    format!("{CODE_PREFIX}{account}:{invite}")
}

fn decode_code(code: &str) -> OmniResult<(String, String)> {
    let rest = code.trim().strip_prefix(CODE_PREFIX).ok_or_else(|| OmniError::InvalidInput("это не код агента Omni".into()))?;
    let (account, invite) = rest.split_once(':').ok_or_else(|| OmniError::InvalidInput("код агента обрезан".into()))?;
    if account.is_empty() || account.len() > 128 || invite.is_empty() {
        return Err(OmniError::InvalidInput("код агента обрезан".into()));
    }
    Ok((account.to_string(), invite.to_string()))
}

/// The commits `oid` brings over our `main`, patches and all (cut at [`DIFF_MAX_BYTES`]).
fn diff_over_main(dir: &Path, oid: &str) -> OmniResult<String> {
    let range = branch_tip(dir, "refs/heads/main").map_or(oid.to_string(), |main| format!("{main}..{oid}"));
    let mut diff = super::run_git(dir, &["log", "-p", "--reverse", "--format=commit %h  %s%n", &range])?;
    if diff.len() > DIFF_MAX_BYTES {
        let cut = (0..=DIFF_MAX_BYTES).rev().find(|i| diff.is_char_boundary(*i)).unwrap_or(0);
        diff.truncate(cut);
        diff.push_str("\n…");
    }
    Ok(diff)
}

/// What `oid` brings over our `main`: the commits' subjects and the files changed, a few of each.
fn brings(dir: &Path, oid: &str) -> (Vec<String>, Vec<String>) {
    let list = |args: &[&str]| -> Vec<String> {
        super::run_git(dir, args).unwrap_or_default().lines().filter(|l| !l.trim().is_empty()).take(ASK_LIST_MAX).map(str::to_string).collect()
    };
    match branch_tip(dir, "refs/heads/main") {
        Some(main) => (list(&["log", "--reverse", "--format=%s", &format!("{main}..{oid}")]), list(&["diff", "--name-status", &main, oid])),
        None => (list(&["log", "--reverse", "--format=%s", oid]), list(&["show", "--name-status", "--format=", oid])),
    }
}

/// A discussion's topic from what was asked: its first line, cut at a word near 60 characters.
fn topic_of(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default().trim();
    if first.chars().count() <= 60 {
        return first.to_string();
    }
    let cut: String = first.chars().take(60).collect();
    match cut.rfind(' ') {
        Some(at) if at > 20 => format!("{}…", &cut[..at]),
        _ => format!("{cut}…"),
    }
}

fn branch_tip(dir: &Path, reference: &str) -> Option<String> {
    super::run_git(dir, &["rev-parse", "--verify", "--quiet", &format!("{reference}^{{commit}}")]).ok().filter(|s| !s.is_empty())
}

impl OmniApp {
    pub fn workspace_agents_command(&self, command: WorkspaceAgentsCommand) -> OmniResult<WorkspaceAgentsView> {
        let _ = self.ensure_runtime_profile_active("workspace_agents_command")?;
        let group_id = match command {
            WorkspaceAgentsCommand::View { group_id } => group_id,
            WorkspaceAgentsCommand::Stop { group_id, stopped } => {
                self.workspace_agents_stop(&group_id, stopped)?;
                group_id
            }
            WorkspaceAgentsCommand::Answer { group_id, id, allow, reason } => {
                self.workspace_agent_answer(&group_id, &id, allow, &reason)?;
                group_id
            }
            WorkspaceAgentsCommand::Diff { group_id, id, repo, oid } => {
                let diff = if id.is_empty() { self.workspace_review_diff(&group_id, &repo, &oid)? } else { self.workspace_agent_ask_diff(&group_id, &id)? };
                return Ok(WorkspaceAgentsView { diff: Some(diff), ..self.workspace_agents(&group_id)? });
            }
            WorkspaceAgentsCommand::Give { group_id, account_id, text, task } => {
                let thread = self.workspace_agent_give(&group_id, &account_id, &text, task.as_deref())?;
                return self.talk_answer(&group_id, Some(thread));
            }
            WorkspaceAgentsCommand::AddToApp { group_id, app, name, folder, exe } => {
                let app = super::agent_apps::AgentApp::parse(&app)?;
                let name = name.trim();
                if name.is_empty() || folder.trim().is_empty() || exe.trim().is_empty() {
                    return Err(OmniError::InvalidInput("агенту нужны имя и папка".into()));
                }
                self.active_group(&group_id)?;
                let owner = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
                let args: Vec<String> = ["--run-helper", "mcp", "--dir", folder.trim(), "--name", name, "--owner", &owner].map(String::from).to_vec();
                let path = super::agent_apps::add_server(app, name, exe.trim(), &args)?;
                return Ok(WorkspaceAgentsView { added_to: Some(path.display().to_string()), ..self.workspace_agents(&group_id)? });
            }
            WorkspaceAgentsCommand::InApp { group_id, app, name, exe } => {
                let app = super::agent_apps::AgentApp::parse(&app)?;
                let added = super::agent_apps::has_server(app, name.trim(), exe.trim());
                return Ok(WorkspaceAgentsView { added_to: added.map(|p| p.display().to_string()), ..self.workspace_agents(&group_id)? });
            }
            WorkspaceAgentsCommand::TalkList { group_id } => {
                return Ok(WorkspaceAgentsView { threads: self.workspace_talk_threads(&group_id)?, ..self.workspace_agents(&group_id)? });
            }
            WorkspaceAgentsCommand::TalkRead { group_id, thread } => {
                let talk = self.workspace_talk_read(&group_id, &thread)?;
                return Ok(WorkspaceAgentsView { threads: self.workspace_talk_threads(&group_id)?, talk: Some(talk), ..self.workspace_agents(&group_id)? });
            }
            WorkspaceAgentsCommand::TalkOpen { group_id, topic, task } => {
                let thread = self.workspace_talk_open(&group_id, &topic, task.as_deref())?;
                return self.talk_answer(&group_id, Some(thread));
            }
            WorkspaceAgentsCommand::TalkSend { group_id, thread, text, reply_to, mentions } => {
                self.workspace_talk_send(&group_id, &thread, &text, reply_to.as_deref(), &mentions.unwrap_or_default())?;
                return self.talk_answer(&group_id, Some(thread));
            }
            WorkspaceAgentsCommand::TalkDelete { group_id, thread, line } => {
                self.workspace_talk_delete(&group_id, &thread, &line)?;
                return self.talk_answer(&group_id, Some(thread));
            }
            WorkspaceAgentsCommand::TalkReact { group_id, thread, line, emoji } => {
                self.workspace_talk_react(&group_id, &thread, &line, &emoji)?;
                return self.talk_answer(&group_id, Some(thread));
            }
            WorkspaceAgentsCommand::PairStart { group_id, task, first, second, topic, rounds } => {
                self.workspace_pair_start(&group_id, &task, &first, &second, &topic, rounds)?;
                group_id
            }
            WorkspaceAgentsCommand::PairClose { group_id, task } => {
                let group = self.active_group(&group_id)?;
                let members = members(&self.storage, &group_id)?;
                super::tasks::pair::close(&self.storage.db_path().to_string_lossy(), &self.storage, &group, &members, &task)?;
                group_id
            }
            WorkspaceAgentsCommand::Connect { group_id, code, name, members_command } => {
                self.workspace_agent_connect(&code, &name, members_command, Some(&group_id))?;
                group_id
            }
            WorkspaceAgentsCommand::Forget { group_id, account_id } => {
                self.workspace_agent_forget(&account_id)?;
                group_id
            }
            WorkspaceAgentsCommand::Review { group_id, repo, branch, oid, accepted, tests_passed, notes } => {
                self.workspace_review(&group_id, &repo, &branch, &oid, accepted, tests_passed, &notes)?;
                group_id
            }
        };
        self.workspace_agents(&group_id)
    }

    /// Stop all agents of the workspace, or let them go — an admin.
    pub fn workspace_agents_stop(&self, group_id: &str, stopped: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_agents_stop")?;
        let group = self.active_group(group_id)?;
        if !super::is_admin(&members(&self.storage, group_id)?, &group.my_member_pub) {
            return Err(OmniError::InvalidInput("остановить агентов может админ workspace".into()));
        }
        send(self, group_id, WorkspaceAgentsEventV1::Stop { stopped, at_unix_ms: current_unix_ms() })
    }

    /// The owner answers its agent. «Разрешить» for `main`: our own `main` of the project goes to the
    /// agent's commit (forward only) and is announced; the shared `main` follows by the project's rule.
    pub fn workspace_agent_answer(&self, group_id: &str, id: &str, allow: bool, reason: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_agent_answer")?;
        let me = me_card(&self.storage, group_id)?;
        let log: Log = read(&self.storage, &log_key(group_id));
        let ask = log.asks.iter().find(|a| a.id == id).ok_or_else(|| OmniError::InvalidInput("такого вопроса агента нет".into()))?;
        if ask.owner != me.account_id {
            return Err(OmniError::InvalidInput("отвечает хозяин агента".into()));
        }
        if ask.answer.is_some() {
            return Err(OmniError::InvalidInput("на этот вопрос уже ответили".into()));
        }
        // Where `main` was before this answer moved it, to put it back.
        let mut taken: Option<(std::path::PathBuf, Option<String>, String, String)> = None;
        if allow && ask.action == WorkspaceAgentAskV1::Main {
            // An independent review with its tests passed, checked here, at the change itself: an
            // agent's own client sent «to main» straight into the group, past the check that made
            // the ask, and the owner's yes took it in unreviewed (blind audit 2026-11-11, K4).
            if accepted_by(&log, &ask.repo, &ask.oid, &ask.agent_account).is_empty() {
                return Err(OmniError::InvalidInput(
                    "у этого коммита нет независимого ревью с пройденными тестами — в main его не взять".into(),
                ));
            }
            let record = self
                .storage
                .get_workspace_repo(group_id, &ask.repo)?
                .ok_or_else(|| OmniError::InvalidInput("этого проекта здесь нет".into()))?;
            let dir = Path::new(&record.published_path);
            let theirs = format!("refs/omni/members/{}/heads/{}", super::account_segment(&ask.agent_account), ask.branch);
            if branch_tip(dir, &theirs).as_deref() != Some(ask.oid.as_str()) {
                return Err(OmniError::InvalidInput("ветки агента с этим коммитом здесь ещё нет (или она ушла дальше)".into()));
            }
            let main = branch_tip(dir, "refs/heads/main");
            if let Some(main) = &main {
                super::run_git(dir, &["merge-base", "--is-ancestor", main, &ask.oid])
                    .map_err(|_| OmniError::InvalidInput("коммит агента не идёт от вашей main — его надо слить вручную".into()))?;
            }
            super::run_git(dir, &["update-ref", "refs/heads/main", &ask.oid, main.as_deref().unwrap_or("")])?;
            taken = Some((dir.to_path_buf(), main, ask.oid.clone(), ask.repo.clone()));
        }
        // The answer first, the announcement after it: `main` moved and announced before the answer
        // was said, an answer that failed left `main` taken under an error that said nothing was
        // done (blind audit 2026-11-11, L5). Not said, `main` goes back where it was. Said, the
        // announcement is the tick's to repeat when it fails here — the commit is taken either way.
        let answer = WorkspaceAgentsEventV1::Answer { id: id.to_string(), allow, at_unix_ms: current_unix_ms(), reason: reason.trim().to_string() };
        if let Err(error) = send(self, group_id, answer) {
            if let Some((dir, main, oid, _)) = &taken {
                let back = match main {
                    Some(main) => super::run_git(dir, &["update-ref", "refs/heads/main", main, oid]),
                    None => super::run_git(dir, &["update-ref", "-d", "refs/heads/main", oid]),
                };
                if let Err(back) = back {
                    omni_app_trace_log("workspace/agents", &format!("group={group_id} main not put back after an unsaid answer: {back}"));
                }
            }
            return Err(error);
        }
        if let Some((_, _, _, repo)) = &taken {
            if let Err(error) = self.workspace_publish(group_id, repo) {
                omni_app_trace_log("workspace/agents", &format!("group={group_id} repo={repo} main taken, announcing it failed: {error}"));
            }
        }
        Ok(())
    }

    /// What a «main» ask brings over our `main`, as a diff (cut at [`DIFF_MAX_BYTES`]).
    pub fn workspace_agent_ask_diff(&self, group_id: &str, id: &str) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("workspace_agent_ask_diff")?;
        let log: Log = read(&self.storage, &log_key(group_id));
        let ask = log.asks.iter().find(|a| a.id == id && a.action == WorkspaceAgentAskV1::Main).ok_or_else(|| OmniError::InvalidInput("такого запроса нет".into()))?;
        let record = self.storage.get_workspace_repo(group_id, &ask.repo)?.ok_or_else(|| OmniError::InvalidInput("этого проекта здесь нет".into()))?;
        let dir = Path::new(&record.published_path);
        if branch_tip(dir, &ask.oid).as_deref() != Some(ask.oid.as_str()) {
            return Err(OmniError::InvalidInput("изменений агента здесь ещё нет — они придут, когда будет связь".into()));
        }
        diff_over_main(dir, &ask.oid)
    }

    /// What a branch asked to be checked brings over our `main`, as a diff.
    pub fn workspace_review_diff(&self, group_id: &str, repo: &str, oid: &str) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("workspace_review_diff")?;
        let record = self.storage.get_workspace_repo(group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("этого проекта здесь нет".into()))?;
        let dir = Path::new(&record.published_path);
        let oid = oid.to_ascii_lowercase();
        if branch_tip(dir, &oid).as_deref() != Some(oid.as_str()) {
            return Err(OmniError::InvalidInput("изменений ветки здесь ещё нет — они придут, когда будет связь".into()));
        }
        diff_over_main(dir, &oid)
    }

    /// «Дать работу» our agent: a command naming it in the thread; with a task, the task is given to it.
    /// The view after a talk command: the list, the discussion and which one it is.
    fn talk_answer(&self, group_id: &str, thread: Option<String>) -> OmniResult<WorkspaceAgentsView> {
        let talk = match thread.as_deref() {
            Some(thread) => Some(self.workspace_talk_read(group_id, thread)?),
            None => None,
        };
        Ok(WorkspaceAgentsView { threads: self.workspace_talk_threads(group_id)?, talk, opened: thread, ..self.workspace_agents(group_id)? })
    }

    /// «Дать работу»: the command goes into the work's discussion — the task's one, or a new one about
    /// what was asked — where the agent answers. The discussion's id.
    pub fn workspace_agent_give(&self, group_id: &str, agent: &str, text: &str, task: Option<&str>) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("workspace_agent_give")?;
        let group = self.active_group(group_id)?;
        let members = members(&self.storage, group_id)?;
        let me = super::my_account(&members, &group).unwrap_or_default();
        if claims(&self.storage).get(agent) != Some(&me) {
            return Err(OmniError::InvalidInput("работу даёт хозяин агента".into()));
        }
        let name = members.iter().find(|m| m.status == "member" && m.account_id == agent).map(|m| m.display_name.clone()).ok_or_else(|| OmniError::InvalidInput("агента ещё нет в workspace".into()))?;
        let text = text.trim();
        let mut line = format!("@{name}");
        let topic;
        if let Some(task) = task.filter(|t| !t.is_empty()) {
            let db_path = self.storage.db_path().to_string_lossy().into_owned();
            let fields = super::tasks::TaskFields { assignee: Some(agent.to_string()), ..Default::default() };
            super::tasks::change(&db_path, &self.storage, &group, &members, task, fields)?;
            let found = super::tasks::find(&self.storage, &group, &members, task)?;
            line.push_str(&format!(" возьми задачу #{} «{}»", found.number, found.title));
            if !text.is_empty() {
                line.push(':');
            }
            topic = format!("#{} {}", found.number, found.title);
        } else if text.is_empty() {
            return Err(OmniError::InvalidInput("напиши, что сделать, или выбери задачу".into()));
        } else {
            topic = topic_of(text);
        }
        if !text.is_empty() {
            line.push(' ');
            line.push_str(text);
        }
        let thread = self.workspace_talk_open(group_id, &topic, task.filter(|t| !t.is_empty()))?;
        self.workspace_talk_send(group_id, &thread, &line, None, &[agent.to_string()])?;
        Ok(thread)
    }

    /// «Проработать вдвоём» begun by us for two of our agents, who are told in the thread.
    pub fn workspace_pair_start(&self, group_id: &str, task: &str, first: &str, second: &str, topic: &str, rounds: Option<u32>) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_pair_start")?;
        let group = self.active_group(group_id)?;
        let members = members(&self.storage, group_id)?;
        let db_path = self.storage.db_path().to_string_lossy().into_owned();
        super::tasks::pair::start_for(&db_path, &self.storage, &group, &members, task, first, second, topic, rounds)?;
        let found = super::tasks::find(&self.storage, &group, &members, task)?;
        let name = |account: &str| members.iter().find(|m| m.account_id == account).map(|m| m.display_name.clone()).unwrap_or_default();
        let about = if topic.trim().is_empty() { String::new() } else { format!(": {}", topic.trim()) };
        self.send_group_line(group_id, &format!("@{} @{} проработайте вдвоём задачу #{} «{}»{about}", name(first), name(second), found.number, found.title), None, &[first.to_string(), second.to_string()])
    }

    /// A review of exactly `oid`, the tip of `branch` (a person's or an agent's).
    pub fn workspace_review(&self, group_id: &str, repo: &str, branch: &str, oid: &str, accepted: bool, tests_passed: bool, notes: &str) -> OmniResult<()> {
        self.refuse_when_stopped(group_id)?;
        // «Принято» is said of a commit the reviewer has: one it never got cannot have been checked.
        let oid = oid.to_ascii_lowercase();
        if accepted {
            let here = self
                .storage
                .get_workspace_repo(group_id, repo)?
                .and_then(|record| branch_tip(Path::new(&record.published_path), &oid))
                .is_some_and(|found| found == oid);
            if !here {
                return Err(OmniError::InvalidInput(format!("коммита {} здесь ещё нет — сначала заберите ветку, потом проверяйте", &oid[..oid.len().min(8)])));
            }
        }
        send(
            self,
            group_id,
            WorkspaceAgentsEventV1::Review {
                repo: repo.to_string(),
                branch: branch.to_string(),
                oid,
                accepted,
                tests_passed,
                notes: notes.trim().to_string(),
                at_unix_ms: current_unix_ms(),
            },
        )
    }

    /// Connect an agent from its code: pair with it, name it in our card, and ask it into
    /// `group_id` once its contact is there.
    pub fn workspace_agent_connect(&self, code: &str, name: &str, members_command: bool, group_id: Option<&str>) -> OmniResult<String> {
        let (account, invite) = decode_code(code)?;
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(OmniError::InvalidInput("имя агента — от 1 до 64 символов".into()));
        }
        let mine = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        if account == mine {
            return Err(OmniError::InvalidInput("это код вашего же аккаунта".into()));
        }
        self.register_mesh_pairing_invite(&invite)?;
        let mut agents = my_agents(&self.storage);
        match agents.iter_mut().find(|a| a.account_id == account) {
            Some(agent) => {
                agent.name = name.to_string();
                agent.members_command = members_command;
                if let Some(group) = group_id.filter(|g| !agent.invite_to.iter().any(|x| x == g)) {
                    agent.invite_to.push(group.to_string());
                }
            }
            None => agents.push(MyAgent {
                account_id: account.clone(),
                name: name.to_string(),
                members_command,
                connected_at_unix_ms: current_unix_ms(),
                invite_to: group_id.map(|g| vec![g.to_string()]).unwrap_or_default(),
            }),
        }
        write(&self.storage, MINE, &agents)?;
        bump_own_agents_version(&self.storage)?;
        crate::app_group_member_profiles::broadcast_own_profile(&self.storage);
        self.flush_group_feed();
        Ok(account)
    }

    /// Our agent is ours no more (it stays in the groups until an admin removes it, as an agent
    /// without an owner).
    pub fn workspace_agent_forget(&self, account: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_agent_forget")?;
        let mut agents = my_agents(&self.storage);
        agents.retain(|a| a.account_id != account);
        write(&self.storage, MINE, &agents)?;
        bump_own_agents_version(&self.storage)?;
        crate::app_group_member_profiles::broadcast_own_profile(&self.storage);
        self.flush_group_feed();
        Ok(())
    }

    /// The agents' place of a workspace. Our agent waiting to be asked into it gets its invitation
    /// here, once its contact is there.
    pub fn workspace_agents(&self, group_id: &str) -> OmniResult<WorkspaceAgentsView> {
        let _ = self.ensure_runtime_profile_active("workspace_agents")?;
        let group = self.active_group(group_id)?;
        let members = members(&self.storage, group_id)?;
        let me = super::my_account(&members, &group).unwrap_or_default();
        let claims = claims(&self.storage);
        let lists = lists(&self.storage);
        let log: Log = read(&self.storage, &log_key(group_id));
        let in_group: HashSet<&str> = members.iter().filter(|m| m.status == "member").map(|m| m.account_id.as_str()).collect();
        let name_of = |account: &str| members.iter().find(|m| m.account_id == account).map(|m| m.display_name.clone()).unwrap_or_default();

        let mut agents_mine = my_agents(&self.storage);
        let contacts = self.list_contacts()?;
        let mut invited = false;
        for agent in agents_mine.iter_mut().filter(|a| a.invite_to.iter().any(|g| g == group_id)) {
            if in_group.contains(agent.account_id.as_str()) {
                agent.invite_to.retain(|g| g != group_id);
                invited = true;
                continue;
            }
            let ready = contacts.iter().find(|c| {
                c.remote_account_id.as_ref().is_some_and(|a| a.0 == agent.account_id) && c.relationship_state == ContactRelationshipState::Approved
            });
            if let Some(dialog) = ready.and_then(|c| c.primary_dialog_key.clone()) {
                self.invite_contacts_to_group(group_id, &[dialog])?;
                agent.invite_to.retain(|g| g != group_id);
                invited = true;
            }
        }
        if invited {
            write(&self.storage, MINE, &agents_mine)?;
        }

        // What each agent is doing: a link to it now, its latest line, the tasks it has, what it waits for.
        let mesh = self.mesh_runtime();
        let thread = self.list_group_messages(group_id).unwrap_or_default();
        let board = super::tasks::view(&self.storage, &group, &members, true, false).map(|b| b.tasks).unwrap_or_default();
        let now = current_unix_ms();
        let mut agents: Vec<WorkspaceAgentView> = members
            .iter()
            .filter(|m| m.status == "member")
            .filter_map(|m| {
                let owner = claims.get(&m.account_id)?;
                let confirmed = lists.get(owner).is_some_and(|l| l.iter().any(|a| a.account_id == m.account_id));
                let online = m.member_pub != group.my_member_pub
                    && crate::app_groups::node_from_hex(&m.node_hint_hex).is_some_and(|node| mesh.as_ref().is_some_and(|mesh| mesh.has_link(&node)));
                // Its latest line: in the group's thread or in a discussion, the newer.
                let in_chat = thread.iter().rev().find(|t| t.sender_pub.as_deref() == Some(m.member_pub.as_str()) && t.kind == "text").map(|t| (t.body.clone(), t.created_at_unix_ms));
                let in_talk = super::talk::last_said(&self.storage, group_id, &m.account_id);
                let last = match (in_chat, in_talk) {
                    (Some(chat), Some(talk)) => Some(if talk.1 > chat.1 { talk } else { chat }),
                    (chat, talk) => chat.or(talk),
                };
                Some(WorkspaceAgentView {
                    account_id: m.account_id.clone(),
                    name: m.display_name.clone(),
                    owner_name: if confirmed { name_of(owner) } else { String::new() },
                    mine: *owner == me && confirmed,
                    confirmed,
                    in_group: true,
                    state: "in_group".into(),
                    members_command: lists.get(owner).and_then(|l| l.iter().find(|a| a.account_id == m.account_id)).is_some_and(|a| a.members_command),
                    online,
                    listening: online && listening_until(&self.storage, group_id, &m.account_id) > now,
                    last_said: last.as_ref().map(|t| t.0.chars().take(160).collect()).unwrap_or_default(),
                    last_active_unix_ms: last.as_ref().map_or(0, |t| t.1),
                    tasks: board
                        .iter()
                        .filter(|t| t.assignee.as_deref() == Some(m.account_id.as_str()) && t.status != "done")
                        .map(|t| format!("#{} {}", t.number, t.title))
                        .collect(),
                    waiting_answer: log.asks.iter().filter(|a| a.agent_account == m.account_id && a.answer.is_none()).count(),
                })
            })
            .collect();
        for agent in agents_mine.iter().filter(|a| !in_group.contains(a.account_id.as_str())) {
            let contact = contacts.iter().find(|c| c.remote_account_id.as_ref().is_some_and(|x| x.0 == agent.account_id));
            let approved = contact.is_some_and(|c| c.relationship_state == ContactRelationshipState::Approved);
            agents.push(WorkspaceAgentView {
                account_id: agent.account_id.clone(),
                name: agent.name.clone(),
                owner_name: String::new(),
                mine: true,
                confirmed: true,
                in_group: false,
                state: if approved { "invited".into() } else { "connecting".into() },
                members_command: agent.members_command,
                online: false,
                listening: false,
                last_said: String::new(),
                last_active_unix_ms: 0,
                tasks: Vec::new(),
                waiting_answer: 0,
            });
        }

        let mut asks: Vec<WorkspaceAgentAskView> = log
            .asks
            .iter()
            .rev()
            .filter(|a| a.answer.is_none() || a.at_unix_ms > current_unix_ms() - 24 * 60 * 60 * 1000)
            .map(|a| WorkspaceAgentAskView {
                id: a.id.clone(),
                agent_name: a.agent_name.clone(),
                action: match a.action {
                    WorkspaceAgentAskV1::Main => "main".into(),
                    WorkspaceAgentAskV1::Other => "other".into(),
                },
                repo: a.repo.clone(),
                branch: a.branch.clone(),
                oid: a.oid.clone(),
                text: a.text.clone(),
                at_unix_ms: a.at_unix_ms,
                answer: a.answer,
                can_answer: a.owner == me && a.answer.is_none(),
                accepted_by: accepted_by(&log, &a.repo, &a.oid, &a.agent_account),
                reason: a.reason.clone(),
                answered_unix_ms: a.answered_unix_ms,
                commits: Vec::new(),
                files: Vec::new(),
                thread: a.thread.clone(),
            })
            .collect();
        // An open «main» ask of ours: what it brings, from our copy of the project.
        for ask in asks.iter_mut().filter(|a| a.can_answer && a.action == "main") {
            let Some(record) = self.storage.get_workspace_repo(group_id, &ask.repo)? else { continue };
            let dir = Path::new(&record.published_path);
            if branch_tip(dir, &ask.oid).as_deref() != Some(ask.oid.as_str()) {
                continue;
            }
            (ask.commits, ask.files) = brings(dir, &ask.oid);
        }
        asks.truncate(20);

        // Branches asked to be checked: open while nobody but the author gave a verdict on that very
        // commit, and while our `main` does not hold it already.
        let mut review_asks = Vec::new();
        for ask in log.review_asks.iter().rev() {
            if log.reviews.iter().any(|r| r.repo == ask.repo && r.oid == ask.oid && r.reviewer_account != ask.author_account) {
                continue;
            }
            let dir = self.storage.get_workspace_repo(group_id, &ask.repo)?.map(|r| std::path::PathBuf::from(r.published_path));
            let here = dir.as_deref().is_some_and(|d| branch_tip(d, &ask.oid).as_deref() == Some(ask.oid.as_str()));
            if here && dir.as_deref().is_some_and(|d| super::run_git(d, &["merge-base", "--is-ancestor", &ask.oid, "refs/heads/main"]).is_ok()) {
                continue;
            }
            let (commits, files) = match dir.as_deref().filter(|_| here) {
                Some(d) => brings(d, &ask.oid),
                None => (Vec::new(), Vec::new()),
            };
            review_asks.push(WorkspaceReviewAskView {
                repo: ask.repo.clone(),
                branch: ask.branch.clone(),
                oid: ask.oid.clone(),
                author_name: ask.author_name.clone(),
                at_unix_ms: ask.at_unix_ms,
                here,
                can_review: ask.author_account != me,
                commits,
                files,
                thread: ask.thread.clone(),
            });
            if review_asks.len() == 20 {
                break;
            }
        }
        let reviews = log
            .reviews
            .iter()
            .rev()
            .take(20)
            .map(|r| WorkspaceAgentReviewView {
                repo: r.repo.clone(),
                branch: r.branch.clone(),
                oid: r.oid.clone(),
                reviewer_name: r.reviewer_name.clone(),
                accepted: r.accepted,
                tests_passed: r.tests_passed,
                notes: r.notes.clone(),
                at_unix_ms: r.at_unix_ms,
                thread: log.review_asks.iter().rev().find(|a| a.repo == r.repo && a.oid == r.oid).and_then(|a| a.thread.clone()),
            })
            .collect();
        Ok(WorkspaceAgentsView {
            my_account: me.clone(),
            stopped: log.stop.as_ref().is_some_and(|s| s.stopped),
            stopped_by: log.stop.as_ref().filter(|s| s.stopped).map(|s| s.by_name.clone()).unwrap_or_default(),
            can_stop: super::is_admin(&members, &group.my_member_pub),
            agents,
            asks,
            reviews,
            review_asks,
            turns_without_human: AGENT_TURNS_WITHOUT_HUMAN,
            rounds: PAIR_ROUNDS,
            diff: None,
            threads: Vec::new(),
            talk: None,
            opened: None,
            added_to: None,
        })
    }

    /// An agent of a workspace whose agents are stopped does nothing in it.
    pub fn refuse_when_stopped(&self, group_id: &str) -> OmniResult<()> {
        if agent_self(&self.storage).is_some() && stopped(&self.storage, group_id) {
            return Err(OmniError::InvalidInput("агенты этого workspace остановлены админом".into()));
        }
        Ok(())
    }

    // ---- The agent's side: what omni-mcp calls. ----

    /// Make this profile an agent of `owner` called `name`.
    pub fn agent_init(&self, owner: &str, name: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("agent_init")?;
        if owner.trim().is_empty() || name.trim().is_empty() {
            return Err(OmniError::InvalidInput("an agent needs its owner and a name".into()));
        }
        write(&self.storage, SELF, &AgentSelf { owner: owner.trim().to_string(), name: name.trim().to_string() })?;
        bump_own_agents_version(&self.storage)?;
        crate::app_group_member_profiles::broadcast_own_profile(&self.storage);
        Ok(())
    }

    /// Whether `repo` of the workspace is a files project (no branches, versions instead).
    pub fn workspace_repo_is_files(&self, group_id: &str, repo: &str) -> bool {
        self.storage.workspace_repo_is_files(group_id, repo).unwrap_or(false)
    }

    pub fn agent_self(&self) -> Option<AgentSelf> {
        agent_self(&self.storage)
    }

    /// The agent's code for its owner: a fresh mesh invite with the account.
    pub fn agent_code(&self) -> OmniResult<String> {
        let account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).ok_or_else(|| OmniError::InvalidInput("no account".into()))?;
        Ok(encode_code(&account, &self.generate_mesh_pairing_invite()?))
    }

    /// What an agent does on its own: takes its owner as a contact and its owner's invitations, and
    /// nobody else's.
    pub fn agent_housekeeping(&self) -> OmniResult<()> {
        let Some(me) = agent_self(&self.storage) else { return Ok(()) };
        let contacts = self.list_contacts()?;
        let owner = contacts.iter().find(|c| c.remote_account_id.as_ref().is_some_and(|a| a.0 == me.owner));
        if let Some(owner) = owner.filter(|c| {
            c.relationship_state == ContactRelationshipState::Pending && c.pending_request_direction == Some(ContactRequestDirection::Incoming)
        }) {
            self.approve_contact(&owner.contact_id)?;
        }
        let owner_dialog = owner.and_then(|c| c.primary_dialog_key.clone());
        for invite in self.list_group_invites()? {
            if invite.status == "pending" && owner_dialog.as_deref() == Some(invite.inviter_dialog_key.as_str()) {
                self.accept_group_invite(&invite.group_id)?;
            }
        }
        Ok(())
    }

    /// A line of the agent into the group's thread, within the limits.
    pub fn agent_send(&self, group_id: &str, text: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("agent_send")?;
        self.refuse_when_stopped(group_id)?;
        let claims = claims(&self.storage);
        let group = self.active_group(group_id)?;
        let members = members(&self.storage, group_id)?;
        let account_of: HashMap<&str, &str> = members.iter().map(|m| (m.member_pub.as_str(), m.account_id.as_str())).collect();
        let me = super::my_account(&members, &group).unwrap_or_default();
        let messages = self.list_group_messages(group_id)?;
        let agents_in_a_row = messages
            .iter()
            .rev()
            .filter(|m| m.kind == "text" && (m.sender_pub.is_some() || m.is_mine))
            .take_while(|m| m.is_mine || m.sender_pub.as_deref().and_then(|p| account_of.get(p)).is_some_and(|a| claims.contains_key(*a)))
            .count();
        if agents_in_a_row >= AGENT_TURNS_WITHOUT_HUMAN {
            return Err(OmniError::InvalidInput(format!(
                "агенты написали {AGENT_TURNS_WITHOUT_HUMAN} сообщений подряд без человека — дождись ответа человека"
            )));
        }
        let minute_ago = current_unix_ms() - 60_000;
        if messages.iter().filter(|m| m.is_mine && m.created_at_unix_ms > minute_ago).count() >= AGENT_MESSAGES_PER_MINUTE {
            return Err(OmniError::InvalidInput(format!("не больше {AGENT_MESSAGES_PER_MINUTE} сообщений в минуту")));
        }
        let _ = me;
        self.send_group_text(group_id, text)
    }

    /// The agent asks its owner. For `main`: the tip of our `branch` (an `agent/` one), accepted by
    /// someone else with the tests passed.
    /// `thread`: the discussion of the work it is about (the card shows there).
    pub fn agent_ask(&self, group_id: &str, main: Option<(&str, &str)>, text: &str, thread: Option<&str>) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("agent_ask")?;
        self.refuse_when_stopped(group_id)?;
        let me = agent_self(&self.storage).ok_or_else(|| OmniError::InvalidInput("this profile is not an agent".into()))?;
        let thread = self.agent_card_thread(group_id, thread)?;
        let id = crate::app_groups::random_group_message_id();
        let event = match main {
            Some((repo, branch)) => {
                if !branch.starts_with(AGENT_BRANCH_PREFIX) {
                    return Err(OmniError::InvalidInput(format!("ветки агента — только {AGENT_BRANCH_PREFIX}*")));
                }
                let record = self
                    .storage
                    .get_workspace_repo(group_id, repo)?
                    .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
                let oid = branch_tip(Path::new(&record.published_path), &format!("refs/heads/{branch}"))
                    .ok_or_else(|| OmniError::InvalidInput("эта ветка не опубликована".into()))?;
                let account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
                let log: Log = read(&self.storage, &log_key(group_id));
                if accepted_by(&log, repo, &oid, &account).is_empty() {
                    return Err(OmniError::InvalidInput(format!(
                        "на коммите {} ветки нет «принято» с прошедшими тестами от другого участника",
                        short(&oid)
                    )));
                }
                WorkspaceAgentsEventV1::Ask {
                    id: id.clone(),
                    owner: me.owner,
                    action: WorkspaceAgentAskV1::Main,
                    repo: repo.to_string(),
                    branch: branch.to_string(),
                    oid,
                    text: text.trim().to_string(),
                    at_unix_ms: current_unix_ms(),
                    thread,
                }
            }
            None => WorkspaceAgentsEventV1::Ask {
                id: id.clone(),
                owner: me.owner,
                action: WorkspaceAgentAskV1::Other,
                repo: String::new(),
                branch: String::new(),
                oid: String::new(),
                text: text.trim().to_string(),
                at_unix_ms: current_unix_ms(),
                thread,
            },
        };
        send(self, group_id, event)?;
        Ok(id)
    }

    /// The answer to our question, once there is one.
    pub fn agent_answer(&self, group_id: &str, id: &str) -> Option<bool> {
        read::<Log>(&self.storage, &log_key(group_id)).asks.into_iter().find(|a| a.id == id).and_then(|a| a.answer)
    }

    /// The owner's why, given with the answer (what to change, for a «main» sent back).
    pub fn agent_answer_reason(&self, group_id: &str, id: &str) -> String {
        read::<Log>(&self.storage, &log_key(group_id)).asks.into_iter().find(|a| a.id == id).map(|a| a.reason).unwrap_or_default()
    }

    /// Ask for a review of the tip of our `branch`, in the discussion `thread` of the work.
    pub fn agent_review_ask(&self, group_id: &str, repo: &str, branch: &str, thread: Option<&str>) -> OmniResult<String> {
        self.refuse_when_stopped(group_id)?;
        let thread = self.agent_card_thread(group_id, thread)?;
        let record = self
            .storage
            .get_workspace_repo(group_id, repo)?
            .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
        let published = Path::new(&record.published_path);
        let oid = branch_tip(published, &format!("refs/heads/{branch}")).ok_or_else(|| OmniError::InvalidInput("эта ветка не опубликована".into()))?;
        // A branch with nothing over the shared main has nothing to review: its commit did not happen
        // (a refused commit is easy to miss) or it was published before it.
        if super::run_git(published, &["merge-base", "--is-ancestor", &oid, super::SHARED_MAIN]).is_ok() {
            return Err(OmniError::InvalidInput(
                "в ветке нет коммитов сверх общей main — проверь, что коммит создан, и опубликуй ветку снова".into(),
            ));
        }
        send(
            self,
            group_id,
            WorkspaceAgentsEventV1::ReviewAsk { repo: repo.to_string(), branch: branch.to_string(), oid: oid.clone(), at_unix_ms: current_unix_ms(), thread },
        )?;
        Ok(oid)
    }

    /// Where an agent may put a project: `folder` inside the agent's own folder (the one its profile
    /// lives in), given from it or in full; made when missing. Any folder would do before — an agent
    /// bound a folder of its person's and saved it as a version the whole group sees (owner's
    /// decision 2026-11-11, blind audit M6). A folder elsewhere is the person's to attach, in Omni.
    pub fn agent_own_folder(&self, folder: &str) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("agent_own_folder")?;
        let outside = || OmniError::InvalidInput("an agent keeps projects inside its own folder — name a folder in it".into());
        let root = self.storage.db_path().parent().ok_or_else(outside)?;
        let root = super::plain_path(&std::fs::canonicalize(root).map_err(|e| OmniError::Storage(format!("the agent's folder: {e}")))?);
        let asked = Path::new(folder.trim());
        if asked.as_os_str().is_empty() || asked.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
            return Err(outside());
        }
        let path = if asked.is_absolute() { super::plain_path(asked) } else { root.join(asked) };
        if !path.starts_with(&root) || path == root {
            return Err(outside());
        }
        std::fs::create_dir_all(&path).map_err(|e| OmniError::Storage(format!("cannot make {}: {e}", path.display())))?;
        // Where it really is: a link inside the agent's folder may lead out of it.
        let real = super::plain_path(&std::fs::canonicalize(&path).map_err(|e| OmniError::Storage(format!("{}: {e}", path.display())))?);
        if !real.starts_with(&root) {
            return Err(outside());
        }
        Ok(real.to_string_lossy().into_owned())
    }

    /// A project folder the agent took: its commits carry the agent's name. A fresh machine has no git
    /// identity at all, and a commit without one is refused — the agent then publishes a branch that
    /// holds nothing new. An identity set by the person (global or in the folder) is kept.
    pub fn agent_folder_identity(&self, folder: &Path) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("agent_folder_identity")?;
        let Some(me) = agent_self(&self.storage) else { return Ok(()) };
        if super::run_git(folder, &["config", "user.name"]).is_ok_and(|n| !n.trim().is_empty()) {
            return Ok(());
        }
        let account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        super::run_git(folder, &["config", "user.name", &me.name])?;
        super::run_git(folder, &["config", "user.email", &format!("{account}@agents.omni")])?;
        Ok(())
    }

    /// The agent starts waiting `wait_ms` for lines: people see it listens. The mark reaches a little past
    /// the wait and is said again only when it runs short, so a listening agent costs the group one event
    /// every couple of minutes.
    pub fn agent_listening(&self, group_id: &str, wait_ms: i64) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("agent_listening")?;
        let account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let now = current_unix_ms();
        if listening_until(&self.storage, group_id, &account) >= now + wait_ms + LISTEN_RENEW_MS {
            return Ok(());
        }
        let until = now + wait_ms.clamp(0, WORKSPACE_LISTENING_MAX_MS - LISTEN_SLACK_MS) + LISTEN_SLACK_MS;
        send(self, group_id, WorkspaceAgentsEventV1::Listening { until_unix_ms: until, at_unix_ms: now })
    }

    /// The agent's server goes away: it no longer listens.
    pub fn agent_stops_listening(&self, group_id: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("agent_stops_listening")?;
        let account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let now = current_unix_ms();
        if listening_until(&self.storage, group_id, &account) <= now {
            return Ok(());
        }
        send(self, group_id, WorkspaceAgentsEventV1::Listening { until_unix_ms: now, at_unix_ms: now })
    }

    /// The group's messages that came after `after` (all of them with `None`), as the agent reads
    /// them; with a place, also the verdicts on its branches it was not told of yet (they count as told
    /// now). The new place comes back with them.
    ///
    /// The place is the order lines came in here, never an author's clock: a line dated far ahead
    /// once moved the agent's place past every later line (code audit 2026-11-09, OMNI-03), and two
    /// lines of one millisecond lost the later one (OMNI-07).
    pub fn agent_inbox(&self, group_id: &str, after: Option<AgentInboxPlace>) -> OmniResult<AgentInbox> {
        let _ = self.ensure_runtime_profile_active("agent_inbox")?;
        // Stopped by the admin: no new work comes in either (A11, audit 2026-11-09 #6) — the agent
        // hears it is stopped instead of taking tasks it may not act on.
        self.refuse_when_stopped(group_id)?;
        let me = agent_self(&self.storage).ok_or_else(|| OmniError::InvalidInput("this profile is not an agent".into()))?;
        let mut place = after.unwrap_or_default();
        let came: HashSet<String> = match after {
            None => HashSet::new(),
            Some(after) => {
                let arrivals = self.storage.list_group_message_arrivals(group_id, after.seq)?;
                if let Some((top, ..)) = arrivals.last() {
                    // Placed by number from now on: the first look's time bound is spent.
                    place = AgentInboxPlace { seq: *top, arrived_since_unix_ms: 0 };
                }
                arrivals
                    .into_iter()
                    .filter(|(_, _, arrived)| *arrived >= after.arrived_since_unix_ms)
                    .map(|(_, id, _)| id)
                    .collect()
            }
        };
        let members = members(&self.storage, group_id)?;
        let claims = claims(&self.storage);
        let lists = lists(&self.storage);
        let my_account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let members_command = lists.get(&me.owner).and_then(|l| l.iter().find(|a| a.account_id == my_account)).is_some_and(|a| a.members_command);
        let mut items: Vec<AgentInboxItem> = self
            .list_group_messages(group_id)?
            .into_iter()
            .filter(|m| !m.is_mine && m.kind == "text" && (after.is_none() || came.contains(&m.message_id)))
            .map(|m| {
                let account = m
                    .sender_pub
                    .as_deref()
                    .and_then(|p| members.iter().find(|x| x.member_pub == p))
                    .map(|x| x.account_id.clone())
                    .unwrap_or_default();
                let from_owner = account == me.owner;
                let from_agent = claims.contains_key(&account);
                let named = self.storage.message_mentions(&m.message_id).unwrap_or_default().contains(&my_account);
                AgentInboxItem {
                    group_id: group_id.to_string(),
                    command: named && !account.is_empty() && (from_owner || (members_command && !from_agent)),
                    from_name: m.sender_name,
                    from_account: account,
                    from_owner,
                    from_agent,
                    text: m.body,
                    message_id: m.message_id,
                    at_unix_ms: m.created_at_unix_ms,
                    thread_id: None,
                    topic: None,
                }
            })
            .collect();
        if after.is_none() {
            items.sort_by_key(|i| i.at_unix_ms);
            return Ok(AgentInbox { items, place });
        }
        // Verdicts on our branches come as events, not lines of the thread: they are read here too —
        // data like any line (what to fix is the agent's to weigh), never a command.
        let log: Log = read(&self.storage, &log_key(group_id));
        let told: HashSet<String> = read(&self.storage, &verdicts_told_key(group_id));
        let verdict_key = |r: &ReviewRecord| format!("{}:{}:{}", r.oid, r.reviewer_account, r.at_unix_ms);
        let mut fresh_told = Vec::new();
        for review in log.reviews.iter().filter(|r| r.reviewer_account != my_account && !told.contains(&verdict_key(r))) {
            let Some(ours) = log.review_asks.iter().rev().find(|a| a.author_account == my_account && a.repo == review.repo && a.oid == review.oid) else {
                continue;
            };
            let verdict = match (review.accepted, review.tests_passed) {
                (true, true) => "accepted".to_string(),
                (true, false) => "accepted, but the tests failed".to_string(),
                (false, _) => "changes needed".to_string(),
            };
            let notes = if review.notes.is_empty() { String::new() } else { format!(": {}", review.notes) };
            items.push(AgentInboxItem {
                group_id: group_id.to_string(),
                message_id: format!("review:{}:{}", review.oid, review.reviewer_account),
                from_name: review.reviewer_name.clone(),
                from_account: review.reviewer_account.clone(),
                from_owner: review.reviewer_account == me.owner,
                from_agent: claims.contains_key(&review.reviewer_account),
                command: false,
                text: format!("review of {}: {} ({}) — {verdict}{notes}", review.repo, review.branch, short(&review.oid)),
                at_unix_ms: review.at_unix_ms,
                thread_id: ours.thread.clone(),
                topic: None,
            });
            fresh_told.push(verdict_key(review));
        }
        if !fresh_told.is_empty() {
            // Only verdicts the log still holds are remembered.
            let kept: HashSet<String> = log.reviews.iter().map(verdict_key).filter(|k| told.contains(k) || fresh_told.contains(k)).collect();
            write(&self.storage, &verdicts_told_key(group_id), &kept)?;
        }
        items.sort_by_key(|i| i.at_unix_ms);
        Ok(AgentInbox { items, place })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agents_code_carries_its_account_and_invite() {
        let code = encode_code("acct-1", "omni-mesh:abc:def");
        assert_eq!(decode_code(&code).unwrap(), ("acct-1".to_string(), "omni-mesh:abc:def".to_string()));
        assert!(decode_code("omni-mesh:abc").is_err());
        assert!(decode_code("omni-agent:acct-1").is_err());
    }
}
