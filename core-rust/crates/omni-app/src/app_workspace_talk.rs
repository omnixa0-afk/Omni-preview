//! «Обсуждения» of a workspace (docs/decisions/omni-workspace-git.md section 5): the conversation of one
//! piece of work — a task, or what an owner gave an agent — where people and agents write finished
//! lines (an agent's reasoning never comes here, only what it sends). It travels as workspace events of
//! the group ([`WorkspaceAgentsEventV1::ThreadOpen`], [`WorkspaceAgentsEventV1::Talk`]), not as lines of
//! the group's chat: the chat gets one line when a discussion opens.
//!
//! Each node keeps a discussion's lines with a local receive number: an agent reads what came after the
//! number it last saw, so a line that arrives late over the mesh is not skipped (its own clock time only
//! orders the view). A line is the sender's by the authenticated card, never by a field of the event.
//! A newcomer to the group does not get older discussions — the view says so.
//!
//! A line is a command to an agent only as everywhere else: its owner's (or a member's, when the owner
//! allowed it) and naming it, by its account (`app_group_mentions`). Anything else is data.

use std::collections::HashMap;

use omni_group_mls::MemberCard;
use omni_protocol::WorkspaceAgentsEventV1;
use omni_storage::Storage;
use omni_types::{OmniError, OmniResult};
use serde::{Deserialize, Serialize};

use super::agents::{agent_self, claims, lists, members, read, send, write, AgentInboxItem, AgentInboxPlace};
use crate::{current_unix_ms, omni_app_trace_log, OmniApp};

/// Lines kept per discussion, the latest.
const LINES_KEPT: usize = 500;
/// Discussions kept per workspace, the latest active.
const THREADS_KEPT: usize = 200;
/// An agent writes into the one discussion it got a command in during this time when it names none.
const WORK_FRESH_MS: i64 = 24 * 60 * 60 * 1000;
/// How much of a discussion's latest line the list keeps.
const LAST_TEXT_CHARS: usize = 200;
/// Agents' lines in a row in one discussion before a person must answer.
pub const AGENT_TURNS_IN_A_TALK: usize = 6;
/// What a discussion's chat line and «Что нового» lead to: `#talk:<id>`.
pub(crate) const NEWS_TALK_PREFIX: &str = "#talk:";

fn threads_key(group_id: &str) -> String {
    format!("workspace_talk_threads:{group_id}")
}

fn lines_key(group_id: &str, thread: &str) -> String {
    format!("workspace_talk:{group_id}:{thread}")
}

/// The emoji on a discussion's lines: line → account → (emoji, when).
fn reactions_key(group_id: &str, thread: &str) -> String {
    format!("workspace_talk_reactions:{group_id}:{thread}")
}

type TalkReactions = HashMap<String, HashMap<String, (String, i64)>>;

/// Each author's latest line in the workspace's discussions: an agent's card shows it.
fn last_key(group_id: &str) -> String {
    format!("workspace_talk_last:{group_id}")
}

/// An author's latest line in the discussions, and when.
pub(crate) fn last_said(storage: &Storage, group_id: &str, account: &str) -> Option<(String, i64)> {
    read::<HashMap<String, (String, i64)>>(storage, &last_key(group_id)).remove(account)
}

/// Which line of a discussion each agent got last, and when it said so.
fn seen_key(group_id: &str, thread: &str) -> String {
    format!("workspace_talk_seen:{group_id}:{thread}")
}

fn seq_key(group_id: &str) -> String {
    format!("workspace_talk_seq:{group_id}")
}

/// The agent's side: discussions it got a command in, and when.
fn worked_key(group_id: &str) -> String {
    format!("agent_talk_worked:{group_id}")
}

/// An agent's place in its reading of the discussions: kept in its profile, so a command that came while
/// its server was down is read when it starts again, and nothing is read twice.
fn agent_read_key(group_id: &str) -> String {
    format!("agent_talk_read:{group_id}")
}

#[derive(Serialize, Deserialize)]
struct PendingDelivery {
    receipt: String,
    items: Vec<AgentInboxItem>,
    seen: Vec<(String, String)>,
}

/// The discussion of a task: the same on every node, so two people opening it at once meet in one.
pub(crate) fn task_thread(task: &str) -> String {
    format!("task-{task}")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ThreadRecord {
    id: String,
    topic: String,
    #[serde(default)]
    task: Option<String>,
    opened_by: String,
    opened_by_name: String,
    at_unix_ms: i64,
    /// The receive number of its latest line here (0: none yet).
    #[serde(default)]
    last_seq: u64,
    #[serde(default)]
    last_at_unix_ms: i64,
    // What the list shows, kept as lines come: the list does not read every discussion's lines.
    #[serde(default)]
    last_author: String,
    #[serde(default)]
    last_text: String,
    #[serde(default)]
    lines: usize,
    #[serde(default)]
    agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LineRecord {
    id: String,
    author_account: String,
    author_name: String,
    text: String,
    #[serde(default)]
    reply_to: Option<String>,
    /// The accounts the line names.
    #[serde(default)]
    mentions: Vec<String>,
    at_unix_ms: i64,
    /// Classification recorded when this agent first collected the line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivered_command: Option<bool>,
    /// Local receive number: what an agent's cursor counts.
    seq: u64,
}

/// A discussion in the list.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTalkThreadView {
    pub id: String,
    pub topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub opened_by_name: String,
    pub at_unix_ms: i64,
    pub last_at_unix_ms: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_author: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_text: String,
    pub lines: usize,
    /// Agents who wrote in it or were named in a command there.
    pub agents: Vec<String>,
}

/// A line of a discussion as a person sees it.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTalkLineView {
    pub id: String,
    pub author_name: String,
    pub author_account: String,
    /// The author is an agent; whose — `owner_name`.
    pub agent: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub owner_name: String,
    pub mine: bool,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    pub at_unix_ms: i64,
    /// The agents this line is a command to (its author may command them and names them).
    pub command_to: Vec<String>,
    /// The agents whose latest line got is this one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub seen_by: Vec<String>,
    /// The emoji on it; `who` holds names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<omni_types::MessageReactionView>,
}

/// One discussion, whole.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTalkView {
    pub thread: WorkspaceTalkThreadView,
    pub lines: Vec<WorkspaceTalkLineView>,
    /// Who a line here can name: the members but us, the agents first.
    pub people: Vec<WorkspaceTalkPersonView>,
}

/// One the composer's suggestion list offers after `@`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTalkPersonView {
    pub account: String,
    pub name: String,
    /// An agent's owner's name; empty for a person.
    pub owner_name: String,
    pub agent: bool,
    /// A line from us naming this one is a command to it; any other line is data.
    pub commanded: bool,
}

fn threads(storage: &Storage, group_id: &str) -> Vec<ThreadRecord> {
    read(storage, &threads_key(group_id))
}

fn lines(storage: &Storage, group_id: &str, thread: &str) -> Vec<LineRecord> {
    read(storage, &lines_key(group_id, thread))
}

/// Take a discussion's opening: its topic and task, whoever opened it first.
pub(crate) fn apply_open(storage: &Storage, group_id: &str, sender: &MemberCard, thread: &str, topic: &str, task: Option<&str>, at_unix_ms: i64, mine: bool) -> OmniResult<()> {
    let at_unix_ms = not_ahead(at_unix_ms);
    let mut all = threads(storage, group_id);
    match all.iter_mut().find(|t| t.id == thread) {
        // Lines came before the opening (the mesh does not keep order): it fills the placeholder.
        Some(known) if known.topic.is_empty() => {
            known.topic = topic.to_string();
            known.task = task.map(str::to_string);
            known.opened_by = sender.account_id.clone();
            known.opened_by_name = sender.display_name.clone();
            known.at_unix_ms = at_unix_ms;
        }
        Some(_) => return Ok(()),
        None => all.push(ThreadRecord {
            id: thread.to_string(),
            topic: topic.to_string(),
            task: task.map(str::to_string),
            opened_by: sender.account_id.clone(),
            opened_by_name: sender.display_name.clone(),
            at_unix_ms,
            last_at_unix_ms: at_unix_ms,
            ..Default::default()
        }),
    }
    store_threads(storage, group_id, all)?;
    if !mine {
        let line = format!("{} начал(а) обсуждение «{topic}»", sender.display_name);
        super::say_in_feed(storage, group_id, Some(&sender.account_id), Some(&format!("{NEWS_TALK_PREFIX}{thread}")), &line, current_unix_ms())?;
    }
    Ok(())
}

/// A card came in a discussion (a question to an owner, a check asked, a verdict): the list shows it
/// as the discussion's latest.
pub(crate) fn note_card(storage: &Storage, group_id: &str, thread: &str, author: &str, said: &str, at_unix_ms: i64) -> OmniResult<()> {
    // Bounded as a line's time is: a review dated i64::MAX kept its discussion first for good, and
    // no later card moved it (blind audit 2026-11-12, pass 15).
    let at_unix_ms = not_ahead(at_unix_ms);
    let mut all = threads(storage, group_id);
    let Some(record) = all.iter_mut().find(|t| t.id == thread) else { return Ok(()) };
    if at_unix_ms >= record.last_at_unix_ms {
        record.last_at_unix_ms = at_unix_ms;
        record.last_author = author.to_string();
        record.last_text = said.chars().take(LAST_TEXT_CHARS).collect();
    }
    store_threads(storage, group_id, all)
}

/// Take a line of a discussion (once: its id is kept).
pub(crate) fn apply_talk(storage: &Storage, group_id: &str, sender: &MemberCard, id: &str, thread: &str, text: &str, reply_to: Option<&str>, mentions: &[String], at_unix_ms: i64) -> OmniResult<()> {
    let at_unix_ms = not_ahead(at_unix_ms);
    let mut kept = lines(storage, group_id, thread);
    if kept.iter().any(|l| l.id == id) {
        return Ok(());
    }
    let seq = next_seq(storage, group_id)?;
    // A reply names a line of the same discussion, or nothing.
    let reply_to = reply_to.filter(|r| kept.iter().any(|l| l.id == *r)).map(str::to_string);
    kept.push(LineRecord {
        id: id.to_string(),
        author_account: sender.account_id.clone(),
        author_name: sender.display_name.clone(),
        text: text.to_string(),
        reply_to,
        mentions: mentions.to_vec(),
        delivered_command: None,
        at_unix_ms,
        seq,
    });
    kept.sort_by_key(|l| (l.at_unix_ms, l.seq));
    // Past the bound, the one who wrote the most gives up their oldest line: the oldest of all went,
    // and one member's 501 lines took everyone else's with their reactions (blind audit 2026-11-12,
    // pass 15).
    let gone: Vec<String> = drop_oldest_of_heaviest(&mut kept, LINES_KEPT, |l| l.author_account.as_str()).into_iter().map(|l| l.id).collect();
    write(storage, &lines_key(group_id, thread), &kept)?;
    // A line that went takes its reactions with it: they stayed, one more for every line ever
    // written in the discussion (blind audit 2026-11-11, pass 14).
    if !gone.is_empty() {
        let mut reactions: TalkReactions = read(storage, &reactions_key(group_id, thread));
        let before = reactions.len();
        reactions.retain(|line, _| !gone.contains(line));
        if reactions.len() != before {
            write(storage, &reactions_key(group_id, thread), &reactions)?;
        }
    }
    let last = kept.last().cloned();
    let count = kept.len();
    let mut latest: HashMap<String, (String, i64)> = read(storage, &last_key(group_id));
    if latest.get(&sender.account_id).is_none_or(|(_, at)| *at <= at_unix_ms) {
        latest.insert(sender.account_id.clone(), (text.chars().take(200).collect(), at_unix_ms));
        write(storage, &last_key(group_id), &latest)?;
    }
    // Agents in it: who wrote, and who was named.
    let claims = claims(storage);
    let mut named: Vec<String> = Vec::new();
    if claims.contains_key(&sender.account_id) {
        named.push(sender.display_name.clone());
    }
    for member in members(storage, group_id)?.into_iter().filter(|m| claims.contains_key(&m.account_id)) {
        if mentions.contains(&member.account_id) {
            named.push(member.display_name);
        }
    }
    let mut all = threads(storage, group_id);
    let record = match all.iter_mut().position(|t| t.id == thread) {
        Some(at) => &mut all[at],
        None => {
            // Before its opening: a placeholder the opening fills. Counted as its sender's until
            // then ([`store_threads`]).
            all.push(ThreadRecord { id: thread.to_string(), opened_by: sender.account_id.clone(), at_unix_ms, ..Default::default() });
            all.last_mut().expect("just pushed")
        }
    };
    record.last_seq = seq;
    record.last_at_unix_ms = record.last_at_unix_ms.max(at_unix_ms);
    if let Some(last) = last {
        record.last_author = last.author_name;
        record.last_text = last.text.chars().take(LAST_TEXT_CHARS).collect();
    }
    record.lines = count;
    for name in named {
        if !record.agents.contains(&name) {
            record.agents.push(name);
        }
    }
    store_threads(storage, group_id, all)
}

/// A person's emoji on a line; the latest by its author's clock stands, an empty one takes it away.
pub(crate) fn apply_reaction(storage: &Storage, group_id: &str, sender: &MemberCard, thread: &str, line: &str, emoji: &str, at_unix_ms: i64) -> OmniResult<()> {
    // Dated as a line is: one dated i64::MAX kept every later change of it out (pass 16).
    let at_unix_ms = not_ahead(at_unix_ms);
    if !lines(storage, group_id, thread).iter().any(|l| l.id == line) {
        return Ok(());
    }
    let mut all: TalkReactions = read(storage, &reactions_key(group_id, thread));
    let on_line = all.entry(line.to_string()).or_default();
    if on_line.get(&sender.account_id).is_some_and(|(_, at)| *at >= at_unix_ms) {
        return Ok(());
    }
    on_line.insert(sender.account_id.clone(), (emoji.to_string(), at_unix_ms));
    write(storage, &reactions_key(group_id, thread), &all)
}

/// The author takes a line of a discussion back; no one else can.
pub(crate) fn apply_delete(storage: &Storage, group_id: &str, sender: &MemberCard, thread: &str, line: &str) -> OmniResult<()> {
    let mut kept = lines(storage, group_id, thread);
    let Some(at) = kept.iter().position(|l| l.id == line && l.author_account == sender.account_id) else {
        return Ok(());
    };
    kept.remove(at);
    write(storage, &lines_key(group_id, thread), &kept)?;
    let mut reactions: TalkReactions = read(storage, &reactions_key(group_id, thread));
    if reactions.remove(line).is_some() {
        write(storage, &reactions_key(group_id, thread), &reactions)?;
    }
    let mut all = threads(storage, group_id);
    if let Some(record) = all.iter_mut().find(|t| t.id == thread) {
        record.lines = kept.len();
        record.last_author = kept.last().map(|l| l.author_name.clone()).unwrap_or_default();
        record.last_text = kept.last().map(|l| l.text.chars().take(LAST_TEXT_CHARS).collect()).unwrap_or_default();
    }
    store_threads(storage, group_id, all)
}

/// An agent's mark that it got a discussion's lines up to `upto`; a later mark replaces an earlier one.
pub(crate) fn apply_seen(storage: &Storage, group_id: &str, sender: &MemberCard, thread: &str, upto: &str, at_unix_ms: i64) -> OmniResult<()> {
    let at_unix_ms = not_ahead(at_unix_ms);
    if !threads(storage, group_id).iter().any(|t| t.id == thread) {
        return Ok(());
    }
    let mut seen: HashMap<String, (String, i64)> = read(storage, &seen_key(group_id, thread));
    if seen.get(&sender.account_id).is_some_and(|(_, at)| *at > at_unix_ms) {
        return Ok(());
    }
    seen.insert(sender.account_id.clone(), (upto.to_string(), at_unix_ms));
    write(storage, &seen_key(group_id, thread), &seen)
}

fn store_threads(storage: &Storage, group_id: &str, mut all: Vec<ThreadRecord>) -> OmniResult<()> {
    // Past the bound, the one who opened the most gives up their oldest. The oldest of all went
    // before — and a member opening discussions by the hundred, dated ahead, pushed out everyone
    // else's, lines and all (blind audit 2026-11-11, pass 14).
    while all.len() > THREADS_KEPT {
        let mut opened: HashMap<&str, usize> = HashMap::new();
        for thread in &all {
            *opened.entry(thread.opened_by.as_str()).or_default() += 1;
        }
        let most = opened.into_iter().max_by_key(|(account, count)| (*count, *account)).map(|(account, _)| account.to_string()).unwrap_or_default();
        let Some(oldest) = all
            .iter()
            .enumerate()
            .filter(|(_, t)| t.opened_by == most)
            .min_by_key(|(_, t)| (t.last_at_unix_ms.max(t.at_unix_ms), t.id.clone()))
            .map(|(at, _)| at)
        else {
            break;
        };
        let gone = all.remove(oldest);
        // Gone with its rows: emptied ones stayed, three for every discussion ever opened (blind
        // audit 2026-11-11, pass 13).
        storage.remove_profile_meta(&lines_key(group_id, &gone.id))?;
        storage.remove_profile_meta(&seen_key(group_id, &gone.id))?;
        storage.remove_profile_meta(&reactions_key(group_id, &gone.id))?;
    }
    write(storage, &threads_key(group_id), &all)
}

/// Test hook: how many lines of `thread` have reactions kept.
#[cfg(test)]
pub(crate) fn reacted_lines_for_tests(storage: &Storage, group_id: &str, thread: &str) -> usize {
    read::<TalkReactions>(storage, &reactions_key(group_id, thread)).len()
}

/// How far ahead of our clock a discussion's event may be dated; one dated further counts as said
/// now. Its sender names the time, and the order of lines and discussions goes by it.
const TALK_CLOCK_AHEAD_MS: i64 = 5 * 60 * 1000;

pub(crate) fn not_ahead(at_unix_ms: i64) -> i64 {
    at_unix_ms.min(crate::current_unix_ms() + TALK_CLOCK_AHEAD_MS)
}

/// Bring `items` (oldest first) down to `kept`: each one over goes from whoever holds the most, its
/// oldest. Returns what went. A set every member fills is bounded so, not by «the oldest of all» —
/// that let one member push out everyone else.
pub(crate) fn drop_oldest_of_heaviest<T>(items: &mut Vec<T>, kept: usize, owner: impl Fn(&T) -> &str) -> Vec<T> {
    let mut gone = Vec::new();
    while items.len() > kept {
        let mut held: HashMap<&str, usize> = HashMap::new();
        for item in items.iter() {
            *held.entry(owner(item)).or_default() += 1;
        }
        let Some(most) = held.into_iter().max_by_key(|(who, count)| (*count, *who)).map(|(who, _)| who.to_string()) else {
            break;
        };
        let Some(at) = items.iter().position(|item| owner(item) == most) else {
            break;
        };
        gone.push(items.remove(at));
    }
    gone
}

fn next_seq(storage: &Storage, group_id: &str) -> OmniResult<u64> {
    let seq = read::<u64>(storage, &seq_key(group_id)) + 1;
    write(storage, &seq_key(group_id), &seq)?;
    Ok(seq)
}

fn thread_view(thread: &ThreadRecord) -> WorkspaceTalkThreadView {
    WorkspaceTalkThreadView {
        id: thread.id.clone(),
        topic: if thread.topic.is_empty() { "Обсуждение".into() } else { thread.topic.clone() },
        task: thread.task.clone(),
        opened_by_name: thread.opened_by_name.clone(),
        at_unix_ms: thread.at_unix_ms,
        last_at_unix_ms: thread.last_at_unix_ms.max(thread.at_unix_ms),
        last_author: thread.last_author.clone(),
        last_text: thread.last_text.clone(),
        lines: thread.lines,
        agents: thread.agents.clone(),
    }
}

/// Whether `author` commands the agent (`agent_owner`'s, `members_command` as the owner set it) by a
/// line naming `mentions`.
fn commands(mentions: &[String], author: &str, author_is_agent: bool, agent_account: &str, agent_owner: &str, members_command: bool) -> bool {
    mentions.iter().any(|named| named == agent_account) && !author.is_empty() && (author == agent_owner || (members_command && !author_is_agent))
}

impl OmniApp {
    /// The workspace's discussions, the latest first.
    pub fn workspace_talk_threads(&self, group_id: &str) -> OmniResult<Vec<WorkspaceTalkThreadView>> {
        let _ = self.ensure_runtime_profile_active("workspace_talk_threads")?;
        self.active_group(group_id)?;
        let mut all: Vec<WorkspaceTalkThreadView> = threads(&self.storage, group_id).iter().filter(|t| !t.topic.is_empty() || t.last_seq > 0).map(thread_view).collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.last_at_unix_ms));
        Ok(all)
    }

    /// One discussion, whole, as a person sees it.
    pub fn workspace_talk_read(&self, group_id: &str, thread: &str) -> OmniResult<WorkspaceTalkView> {
        let _ = self.ensure_runtime_profile_active("workspace_talk_read")?;
        let group = self.active_group(group_id)?;
        let members = members(&self.storage, group_id)?;
        let me = super::my_account(&members, &group).unwrap_or_default();
        let claims = claims(&self.storage);
        let lists = lists(&self.storage);
        let record = threads(&self.storage, group_id).into_iter().find(|t| t.id == thread).unwrap_or_else(|| ThreadRecord { id: thread.to_string(), ..Default::default() });
        let kept = lines(&self.storage, group_id, thread);
        let name_of = |account: &str| members.iter().find(|m| m.account_id == account).map(|m| m.display_name.clone()).unwrap_or_default();
        let agents_here: Vec<(String, String, String)> = members
            .iter()
            .filter(|m| m.status == "member")
            .filter_map(|m| claims.get(&m.account_id).map(|owner| (m.display_name.clone(), m.account_id.clone(), owner.clone())))
            .collect();
        let members_command = |agent: &str, owner: &str| lists.get(owner).and_then(|l| l.iter().find(|a| a.account_id == agent)).is_some_and(|a| a.members_command);
        let seen: HashMap<String, (String, i64)> = read(&self.storage, &seen_key(group_id, thread));
        let seen_by = |line: &str| -> Vec<String> {
            let mut names: Vec<String> = seen.iter().filter(|(_, (upto, _))| upto == line).map(|(account, _)| name_of(account)).filter(|n| !n.is_empty()).collect();
            names.sort();
            names
        };
        let reactions: TalkReactions = read(&self.storage, &reactions_key(group_id, thread));
        let reactions_on = |line: &str| -> Vec<omni_types::MessageReactionView> {
            let Some(on_line) = reactions.get(line) else { return Vec::new() };
            let mut placed: Vec<(&String, &(String, i64))> = on_line.iter().filter(|(_, (emoji, _))| !emoji.is_empty()).collect();
            placed.sort_by_key(|(_, (_, at))| *at);
            let mut views: Vec<omni_types::MessageReactionView> = Vec::new();
            for (account, (emoji, _)) in placed {
                let who = name_of(account);
                match views.iter_mut().find(|v| v.emoji == *emoji) {
                    Some(view) => {
                        view.count += 1;
                        view.mine |= *account == me;
                        view.who.push(who);
                    }
                    None => views.push(omni_types::MessageReactionView { emoji: emoji.clone(), count: 1, mine: *account == me, who: vec![who] }),
                }
            }
            views.sort_by_key(|v| std::cmp::Reverse(v.count));
            views
        };
        let lines = kept
            .iter()
            .map(|l| {
                let author_is_agent = claims.contains_key(&l.author_account);
                WorkspaceTalkLineView {
                    id: l.id.clone(),
                    author_name: l.author_name.clone(),
                    author_account: l.author_account.clone(),
                    agent: author_is_agent,
                    owner_name: claims.get(&l.author_account).map(|o| name_of(o)).unwrap_or_default(),
                    mine: l.author_account == me,
                    text: l.text.clone(),
                    reply_to: l.reply_to.clone(),
                    at_unix_ms: l.at_unix_ms,
                    command_to: agents_here
                        .iter()
                        .filter(|(_, account, owner)| commands(&l.mentions, &l.author_account, author_is_agent, account, owner, members_command(account, owner)))
                        .map(|(name, _, _)| name.clone())
                        .collect(),
                    seen_by: seen_by(&l.id),
                    reactions: reactions_on(&l.id),
                }
            })
            .collect();
        let mut people: Vec<WorkspaceTalkPersonView> = Vec::new();
        for member in members.iter().filter(|m| m.status == "member" && m.account_id != me && !m.account_id.is_empty()) {
            if people.iter().any(|p| p.account == member.account_id) {
                continue; // a second device of one account
            }
            let owner = claims.get(&member.account_id);
            people.push(WorkspaceTalkPersonView {
                account: member.account_id.clone(),
                name: member.display_name.clone(),
                owner_name: owner.map(|o| name_of(o)).unwrap_or_default(),
                agent: owner.is_some(),
                commanded: owner.is_some_and(|o| *o == me || members_command(&member.account_id, o)),
            });
        }
        people.sort_by_key(|p| (!p.agent, p.name.to_lowercase()));
        Ok(WorkspaceTalkView { thread: thread_view(&record), lines, people })
    }

    /// Open a discussion (the task's one, if it has it already): its id.
    pub fn workspace_talk_open(&self, group_id: &str, topic: &str, task: Option<&str>) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("workspace_talk_open")?;
        self.active_group(group_id)?;
        let topic = topic.trim();
        let id = match task.filter(|t| !t.is_empty()) {
            Some(task) => task_thread(task),
            None => format!("t-{}", crate::app_groups::random_group_message_id().trim_start_matches("g-")),
        };
        if threads(&self.storage, group_id).iter().any(|t| t.id == id && !t.topic.is_empty()) {
            return Ok(id);
        }
        let topic = if topic.is_empty() { "Обсуждение".to_string() } else { topic.chars().take(omni_protocol::WORKSPACE_TALK_TOPIC_MAX).collect() };
        send(self, group_id, WorkspaceAgentsEventV1::ThreadOpen { thread: id.clone(), topic, task: task.filter(|t| !t.is_empty()).map(str::to_string), at_unix_ms: current_unix_ms() })?;
        Ok(id)
    }

    /// Takes our own line of a discussion back, from everyone.
    pub fn workspace_talk_delete(&self, group_id: &str, thread: &str, line: &str) -> OmniResult<()> {
        self.active_group(group_id)?;
        send(self, group_id, WorkspaceAgentsEventV1::TalkDelete { thread: thread.to_string(), line: line.to_string() })
    }

    /// Our emoji on a line of a discussion; empty takes it away.
    pub fn workspace_talk_react(&self, group_id: &str, thread: &str, line: &str, emoji: &str) -> OmniResult<()> {
        self.active_group(group_id)?;
        send(self, group_id, WorkspaceAgentsEventV1::TalkReaction { thread: thread.to_string(), line: line.to_string(), emoji: emoji.to_string(), at_unix_ms: current_unix_ms() })
    }

    /// A person's line in a discussion; `picked`: the accounts chosen in the suggestion list.
    pub fn workspace_talk_send(&self, group_id: &str, thread: &str, text: &str, reply_to: Option<&str>, picked: &[String]) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("workspace_talk_send")?;
        self.active_group(group_id)?;
        if !threads(&self.storage, group_id).iter().any(|t| t.id == thread) {
            return Err(OmniError::InvalidInput("такого обсуждения нет".into()));
        }
        self.talk_line(group_id, thread, text, reply_to, picked)
    }

    fn talk_line(&self, group_id: &str, thread: &str, text: &str, reply_to: Option<&str>, picked: &[String]) -> OmniResult<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(OmniError::InvalidInput("пустое сообщение".into()));
        }
        // A reply names a line of this discussion; anything else it answers (a verdict an agent heard
        // in omni_wait, a line of another discussion) is not a line here and is left out.
        let reply_to = reply_to.filter(|r| lines(&self.storage, group_id, thread).iter().any(|l| l.id == *r));
        let mentions = crate::app_group_mentions::resolve_mentions(&members(&self.storage, group_id)?, text, picked)?;
        let id = crate::app_groups::random_group_message_id();
        send(
            self,
            group_id,
            WorkspaceAgentsEventV1::Talk { id: id.clone(), thread: thread.to_string(), text: text.to_string(), reply_to: reply_to.map(str::to_string), mentions, at_unix_ms: current_unix_ms() },
        )?;
        Ok(id)
    }

    // ---- The agent's side. ----

    /// The discussions the agent got a command in within the last day.
    fn agent_fresh_work(&self, group_id: &str) -> Vec<String> {
        let worked: HashMap<String, i64> = read(&self.storage, &worked_key(group_id));
        worked.into_iter().filter(|(_, at)| *at > current_unix_ms() - WORK_FRESH_MS).map(|(t, _)| t).collect()
    }

    /// Where an agent's question or check request shows as a card: the discussion it names (it has to
    /// be here), else its only current work, else none — a card is never put into a discussion
    /// guessed among several.
    pub(crate) fn agent_card_thread(&self, group_id: &str, named: Option<&str>) -> OmniResult<Option<String>> {
        match named {
            Some(thread) if threads(&self.storage, group_id).iter().any(|t| t.id == thread) => Ok(Some(thread.to_string())),
            Some(thread) => Err(OmniError::InvalidInput(format!("no discussion {thread} here (omni_wait gives thread_id)"))),
            None => {
                let fresh = self.agent_fresh_work(group_id);
                Ok(if fresh.len() == 1 { fresh.into_iter().next() } else { None })
            }
        }
    }

    /// The discussion an agent writes into when it names none: the one it got a command in lately, if
    /// there is exactly one. With several, it has to say which — a result of one piece of work must
    /// not land in another. None: no discussion (it writes into the group's thread, as before).
    pub fn agent_talk_default(&self, group_id: &str) -> OmniResult<Option<String>> {
        let _ = self.ensure_runtime_profile_active("agent_talk_default")?;
        let fresh = self.agent_fresh_work(group_id);
        match fresh.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            many => {
                let topics: Vec<String> = threads(&self.storage, group_id)
                    .into_iter()
                    .filter(|t| many.contains(&t.id))
                    .map(|t| format!("{} «{}»", t.id, t.topic))
                    .collect();
                Err(OmniError::InvalidInput(format!("у тебя несколько обсуждений — укажи thread_id: {}", topics.join(", "))))
            }
        }
    }

    /// The agent's line in a discussion: at most [`AGENT_TURNS_IN_A_TALK`] agents' lines in a row there
    /// without a person, and the per-minute limit over all it writes.
    pub fn agent_talk_send(&self, group_id: &str, thread: &str, text: &str, reply_to: Option<&str>, per_minute: usize) -> OmniResult<String> {
        let _ = self.ensure_runtime_profile_active("agent_talk_send")?;
        self.refuse_when_stopped(group_id)?;
        if agent_self(&self.storage).is_none() {
            return Err(OmniError::InvalidInput("this profile is not an agent".into()));
        }
        if !threads(&self.storage, group_id).iter().any(|t| t.id == thread) {
            return Err(OmniError::InvalidInput(format!("no discussion {thread} here (omni_wait gives thread_id)")));
        }
        let claims = claims(&self.storage);
        let kept = lines(&self.storage, group_id, thread);
        let in_a_row = kept.iter().rev().take_while(|l| claims.contains_key(&l.author_account) || self.is_my_account(&l.author_account)).count();
        if in_a_row >= AGENT_TURNS_IN_A_TALK {
            return Err(OmniError::InvalidInput(format!("агенты написали {AGENT_TURNS_IN_A_TALK} сообщений подряд в этом обсуждении без человека — дождись ответа человека")));
        }
        let minute_ago = current_unix_ms() - 60_000;
        let mine_in_talks: usize = threads(&self.storage, group_id)
            .iter()
            .map(|t| lines(&self.storage, group_id, &t.id).iter().filter(|l| self.is_my_account(&l.author_account) && l.at_unix_ms > minute_ago).count())
            .sum();
        let mine_in_chat = self.list_group_messages(group_id)?.iter().filter(|m| m.is_mine && m.created_at_unix_ms > minute_ago).count();
        if mine_in_talks + mine_in_chat >= per_minute {
            return Err(OmniError::InvalidInput(format!("не больше {per_minute} сообщений в минуту")));
        }
        self.talk_line(group_id, thread, text, reply_to, &[])
    }

    fn is_my_account(&self, account: &str) -> bool {
        self.storage.get_local_account().ok().flatten().is_some_and(|a| a.descriptor.account_id.0 == account)
    }

    /// Commit both input cursors and a replayable copy in the same profile transaction.
    /// The receipt is scoped to one group; a late acknowledgement cannot remove a newer batch.
    pub fn agent_wait_batch(&self, group: &str, started: i64) -> OmniResult<(String, Vec<AgentInboxItem>)> {
        let _ = self.ensure_runtime_profile_active("agent_wait_batch")?;
        self.refuse_when_stopped(group)?;
        self.active_group(group)?;
        self.storage.with_immediate_transaction("agent wait batch", |storage| {
            let key = format!("agent_wait_pending:{group}");
            if let Some(saved) = storage.get_profile_meta(&key)? {
                let pending: PendingDelivery = serde_json::from_str(&saved).map_err(|e| OmniError::Storage(format!("unreadable pending batch: {e}")))?;
                return Ok((pending.receipt, pending.items));
            }
            let place_key = format!("agent_wait_place:{group}");
            let saved = storage.get_profile_meta(&place_key)?;
            let place = match &saved {
                Some(raw) => serde_json::from_str(raw).map_err(|e| OmniError::Storage(format!("invalid inbox cursor: {e}")))?,
                None => AgentInboxPlace { seq: 0, arrived_since_unix_ms: started },
            };
            let mut inbox = self.agent_inbox(group, Some(place))?;
            let (talk, seen) = self.collect_talk_inbox(group)?;
            inbox.items.extend(talk);
            if saved.is_none() || inbox.place != place {
                write(storage, &place_key, &inbox.place)?;
            }
            if inbox.items.is_empty() {
                return Ok((String::new(), inbox.items));
            }
            let pending = PendingDelivery { receipt: hex::encode(&omni_crypto::generate_group_key()[..16]), items: inbox.items, seen };
            write(storage, &key, &pending)?;
            Ok((pending.receipt, pending.items))
        })
    }

    pub fn agent_ack_batch(&self, group: &str, receipt: &str) -> OmniResult<bool> {
        let _ = self.ensure_runtime_profile_active("agent_ack_batch")?;
        self.refuse_when_stopped(group)?;
        let seen = self.storage.with_immediate_transaction("acknowledge agent batch", |storage| {
            let key = format!("agent_wait_pending:{group}");
            let Some(raw) = storage.get_profile_meta(&key)? else { return Ok(None) };
            let pending: PendingDelivery = serde_json::from_str(&raw).map_err(|e| OmniError::Storage(e.to_string()))?;
            if pending.receipt != receipt {
                return Ok(None);
            }
            storage.remove_profile_meta(&key)?;
            Ok(Some(pending.seen))
        })?;
        if let Some(seen) = seen {
            self.mark_talk_seen(group, seen);
            return Ok(true);
        }
        Ok(false)
    }

    /// Lines of the discussions the agent has not read yet (by receive number, not time: a line late over
    /// the mesh is still newer than what was read), in time order; they count as read now. A command to
    /// the agent marks its discussion as the agent's work.
    pub fn agent_talk_inbox(&self, group_id: &str) -> OmniResult<Vec<AgentInboxItem>> {
        let (items, seen) = self.storage.with_immediate_transaction("collect talk inbox", |_| self.collect_talk_inbox(group_id))?;
        self.mark_talk_seen(group_id, seen);
        Ok(items)
    }

    fn collect_talk_inbox(&self, group_id: &str) -> OmniResult<(Vec<AgentInboxItem>, Vec<(String, String)>)> {
        let _ = self.ensure_runtime_profile_active("agent_talk_inbox")?;
        self.refuse_when_stopped(group_id)?;
        let since_seq: u64 = read(&self.storage, &agent_read_key(group_id));
        let me = agent_self(&self.storage).ok_or_else(|| OmniError::InvalidInput("this profile is not an agent".into()))?;
        let claims = claims(&self.storage);
        let lists = lists(&self.storage);
        let my_account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let members_command = lists.get(&me.owner).and_then(|l| l.iter().find(|a| a.account_id == my_account)).is_some_and(|a| a.members_command);
        let mut items = Vec::new();
        let mut top = since_seq;
        let mut worked: HashMap<String, i64> = read(&self.storage, &worked_key(group_id));
        let mut marked = false;
        // Per discussion, the latest line got: the mark people see under it.
        let mut got: Vec<(String, String)> = Vec::new();
        for thread in threads(&self.storage, group_id).into_iter().filter(|t| t.last_seq > since_seq) {
            let mut kept = lines(&self.storage, group_id, &thread.id);
            let mut classified = false;
            for line in kept.iter_mut().filter(|l| l.seq > since_seq && l.author_account != my_account) {
                top = top.max(line.seq);
                match got.iter_mut().find(|(t, _)| *t == thread.id) {
                    Some(last) => last.1 = line.id.clone(),
                    None => got.push((thread.id.clone(), line.id.clone())),
                }
                let from_agent = claims.contains_key(&line.author_account);
                let command = commands(&line.mentions, &line.author_account, from_agent, &my_account, &me.owner, members_command);
                line.delivered_command = Some(command);
                classified = true;
                if command {
                    worked.insert(thread.id.clone(), current_unix_ms());
                    marked = true;
                }
                items.push(AgentInboxItem {
                    group_id: group_id.to_string(),
                    message_id: line.id.clone(),
                    from_name: line.author_name.clone(),
                    from_owner: line.author_account == me.owner,
                    from_account: line.author_account.clone(),
                    from_agent,
                    command,
                    text: line.text.clone(),
                    at_unix_ms: line.at_unix_ms,
                    thread_id: Some(thread.id.clone()),
                    topic: Some(thread.topic.clone()),
                });
            }
            if classified {
                write(&self.storage, &lines_key(group_id, &thread.id), &kept)?;
            }
        }
        if marked {
            worked.retain(|_, at| *at > current_unix_ms() - WORK_FRESH_MS);
            write(&self.storage, &worked_key(group_id), &worked)?;
        }
        if top > since_seq {
            write(&self.storage, &agent_read_key(group_id), &top)?;
        }
        items.sort_by_key(|i| i.at_unix_ms);
        Ok((items, got))
    }

    fn mark_talk_seen(&self, group_id: &str, got: Vec<(String, String)>) {
        for (thread, upto) in got {
            // The lines are the agent's already; a mark that did not go out only leaves people unsure.
            if let Err(e) = send(self, group_id, WorkspaceAgentsEventV1::TalkSeen { thread, upto, at_unix_ms: current_unix_ms() }) {
                omni_app_trace_log("workspace/talk", &format!("group={group_id} seen mark not sent: {e}"));
            }
        }
    }

    /// The last `limit` lines of a discussion, the agent's own included.
    pub fn agent_talk_read(&self, group_id: &str, thread: &str, limit: usize) -> OmniResult<Vec<AgentInboxItem>> {
        let _ = self.ensure_runtime_profile_active("agent_talk_read")?;
        self.refuse_when_stopped(group_id)?;
        let me = agent_self(&self.storage).ok_or_else(|| OmniError::InvalidInput("this profile is not an agent".into()))?;
        let claims = claims(&self.storage);
        let my_account = self.storage.get_local_account()?.map(|a| a.descriptor.account_id.0).unwrap_or_default();
        let members_command = lists(&self.storage).get(&me.owner).and_then(|l| l.iter().find(|a| a.account_id == my_account)).is_some_and(|a| a.members_command);
        let topic = threads(&self.storage, group_id).into_iter().find(|t| t.id == thread).map(|t| t.topic);
        let kept = lines(&self.storage, group_id, thread);
        let skip = kept.len().saturating_sub(limit);
        Ok(kept
            .into_iter()
            .skip(skip)
            .map(|l| AgentInboxItem {
                group_id: group_id.to_string(),
                message_id: l.id,
                from_name: if self.is_my_account(&l.author_account) { "you".into() } else { l.author_name },
                from_owner: l.author_account == me.owner,
                from_agent: claims.contains_key(&l.author_account),
                command: l.delivered_command.unwrap_or_else(|| commands(&l.mentions, &l.author_account, claims.contains_key(&l.author_account), &my_account, &me.owner, members_command)),
                from_account: l.author_account,
                text: l.text,
                at_unix_ms: l.at_unix_ms,
                thread_id: Some(thread.to_string()),
                topic: topic.clone(),
            })
            .collect())
    }
}
