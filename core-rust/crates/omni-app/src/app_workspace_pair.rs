//! «Проработать вдвоём» (docs/decisions/omni-workspace-git.md section 5): two agents work out one
//! task, and Omni — not the agents — keeps the protocol. Files of the tasks project, beside the task:
//!
//! - `<id>/work/<session>/session.md` — the two, the round limit, the topic;
//! - `drafts/<round>-<who>.md` — a draft of a round (independent in round 1, merged after);
//! - `remarks/<at>-<who>.md` — a remark on the other's draft: what, how serious, why;
//! - `answers/<at>-<who>.md` — the draft's author on a remark: taken in, or not, and why;
//! - `agree/<at>-<who>.md` — «согласен» with one exact draft;
//! - `outcome.md` — how it ended, written once.
//!
//! It ends in agreement (both agree on the latest draft and no remark is open), a deadlock (two
//! rounds with no draft changed) or the round limit. The outcome is a comment on the task, so the
//! group's thread tells it; the agents' agreement is not an approval of anything.

use std::path::Path;

use omni_storage::{GroupMemberRecord, GroupRecord, Storage};
use omni_types::{OmniError, OmniResult};
use serde::Serialize;

use super::{author, parse, prepare, render, short, write_and_save, Note, TASKS_REPO};
use crate::current_unix_ms;

const DRAFT_MAX_BYTES: usize = 64 * 1024;
const REMARK_MAX_BYTES: usize = 8 * 1024;
const SEVERITIES: [&str; 3] = ["major", "minor", "nit"];

#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePairView {
    pub session: String,
    pub topic: String,
    pub with: Vec<String>,
    pub rounds_max: u32,
    pub round: u32,
    /// working | agreed | deadlock | limit | closed (a person ended it)
    pub status: String,
    pub drafts: Vec<WorkspacePairDraftView>,
    pub remarks: Vec<WorkspacePairRemarkView>,
    /// Who agreed with the latest draft.
    pub agreed_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePairDraftView {
    pub file: String,
    pub round: u32,
    pub author_name: String,
    pub at_unix_ms: i64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePairRemarkView {
    pub id: String,
    /// The draft it is about.
    pub on: String,
    pub author_name: String,
    pub severity: String,
    pub what: String,
    pub why: String,
    /// None: open; Some(true): taken in; Some(false): not, with `answer_why`.
    pub taken: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub answer_why: String,
}

struct Session {
    id: String,
    head: Note,
    drafts: Vec<(String, Note)>,
    remarks: Vec<(String, Note)>,
    answers: Vec<Note>,
    agrees: Vec<Note>,
    outcome: Option<Note>,
}

fn notes(dir: &Path) -> Vec<(String, Note)> {
    let mut named: Vec<(String, Note)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| Some((e.path().file_stem()?.to_string_lossy().into_owned(), parse(&std::fs::read_to_string(e.path()).ok()?))))
        .collect();
    named.sort_by(|(a, x), (b, y)| (at(x), a).cmp(&(at(y), b)));
    named
}

fn at(note: &Note) -> i64 {
    note.get("at").and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn round_of(note: &Note) -> u32 {
    note.get("round").and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The task's latest session, if any.
fn latest(work: &Path, task: &str) -> Option<Session> {
    let base = work.join(task).join("work");
    let mut sessions: Vec<(i64, String)> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let head = parse(&std::fs::read_to_string(e.path().join("session.md")).ok()?);
            Some((at(&head), e.file_name().to_string_lossy().into_owned()))
        })
        .collect();
    sessions.sort();
    let (_, id) = sessions.pop()?;
    let dir = base.join(&id);
    Some(Session {
        head: parse(&std::fs::read_to_string(dir.join("session.md")).ok()?),
        drafts: notes(&dir.join("drafts")),
        remarks: notes(&dir.join("remarks")),
        answers: notes(&dir.join("answers")).into_iter().map(|(_, n)| n).collect(),
        agrees: notes(&dir.join("agree")).into_iter().map(|(_, n)| n).collect(),
        outcome: std::fs::read_to_string(dir.join("outcome.md")).ok().map(|t| parse(&t)),
        id,
    })
}

impl Session {
    fn with(&self) -> Vec<String> {
        self.head.get("with").map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
    }

    fn rounds_max(&self) -> u32 {
        self.head.get("rounds").and_then(|v| v.parse().ok()).unwrap_or(crate::app_workspace::agents::PAIR_ROUNDS)
    }

    fn round(&self) -> u32 {
        self.drafts.iter().map(|(_, n)| round_of(n)).max().unwrap_or(0)
    }

    /// The draft of `account` in `round`.
    fn draft(&self, account: &str, round: u32) -> Option<&(String, Note)> {
        self.drafts.iter().rev().find(|(_, n)| n.get("author") == Some(account) && round_of(n) == round)
    }

    /// The newest draft: the last written in the highest round.
    fn latest_draft(&self) -> Option<&(String, Note)> {
        let round = self.round();
        self.drafts.iter().rev().find(|(_, n)| round_of(n) == round)
    }

    fn answer(&self, remark: &str) -> Option<&Note> {
        self.answers.iter().rev().find(|a| a.get("remark") == Some(remark))
    }

    fn open_remarks_on(&self, draft: &str) -> usize {
        self.remarks.iter().filter(|(id, n)| n.get("on") == Some(draft) && self.answer(id).is_none()).count()
    }

    fn agreed_by(&self) -> Vec<String> {
        let Some((latest, _)) = self.latest_draft() else { return Vec::new() };
        let mut who: Vec<String> = self.agrees.iter().filter(|a| a.get("draft") == Some(latest.as_str())).filter_map(|a| a.get("author").map(str::to_string)).collect();
        who.sort();
        who.dedup();
        who
    }

    /// Two rounds in a row in which nobody changed their draft.
    fn deadlocked(&self) -> bool {
        let round = self.round();
        if round < 3 {
            return false;
        }
        let same = |r: u32| {
            self.with().iter().all(|who| match (self.draft(who, r), self.draft(who, r - 1)) {
                (Some((_, a)), Some((_, b))) => a.body.trim() == b.body.trim(),
                _ => false,
            })
        };
        same(round) && same(round - 1)
    }

    fn status(&self) -> String {
        if let Some(outcome) = &self.outcome {
            return outcome.get("status").unwrap_or("agreed").to_string();
        }
        let with = self.with();
        if with.len() == 2 && with.iter().all(|w| self.agreed_by().contains(w)) {
            return "agreed".into();
        }
        if self.deadlocked() {
            return "deadlock".into();
        }
        let final_round_done = self.round() >= self.rounds_max() && with.iter().all(|w| self.draft(w, self.rounds_max()).is_some());
        if final_round_done && self.latest_draft().is_some_and(|(file, _)| self.open_remarks_on(file) > 0) {
            return "limit".into();
        }
        "working".into()
    }
}

/// The task's latest session as the board shows it.
pub(crate) fn view(work: &Path, task: &str, name_of: &dyn Fn(&str, Option<&str>) -> String) -> Option<WorkspacePairView> {
    let s = latest(work, task)?;
    let named = |n: &Note| name_of(n.get("author").unwrap_or(""), n.get("author_name"));
    Some(WorkspacePairView {
        session: s.id.clone(),
        topic: s.head.body.clone(),
        with: s.with().iter().map(|w| name_of(w, None)).collect(),
        rounds_max: s.rounds_max(),
        round: s.round(),
        status: s.status(),
        drafts: s
            .drafts
            .iter()
            .map(|(file, n)| WorkspacePairDraftView { file: file.clone(), round: round_of(n), author_name: named(n), at_unix_ms: at(n), text: n.body.clone() })
            .collect(),
        remarks: s
            .remarks
            .iter()
            .map(|(id, n)| {
                let answer = s.answer(id);
                WorkspacePairRemarkView {
                    id: id.clone(),
                    on: n.get("on").unwrap_or("").to_string(),
                    author_name: named(n),
                    severity: n.get("severity").unwrap_or("minor").to_string(),
                    what: n.get("what").unwrap_or("").to_string(),
                    why: n.body.clone(),
                    taken: answer.map(|a| a.get("verdict") == Some("taken")),
                    answer_why: answer.map(|a| a.body.clone()).unwrap_or_default(),
                }
            })
            .collect(),
        agreed_by: s.agreed_by().iter().map(|w| name_of(w, None)).collect(),
    })
}

fn work_folder(storage: &Storage, group: &GroupRecord) -> OmniResult<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(prepare(storage, group)?.work_path.ok_or_else(|| OmniError::Internal("the tasks project has no folder".into()))?))
}

fn open_session(storage: &Storage, group: &GroupRecord, task: &str, me: &str) -> OmniResult<Session> {
    let s = latest(&work_folder(storage, group)?, task).ok_or_else(|| OmniError::InvalidInput("у задачи нет проработки — начни её".into()))?;
    if !s.with().iter().any(|w| w == me) {
        return Err(OmniError::InvalidInput("ты не участник этой проработки".into()));
    }
    if s.outcome.is_some() || s.status() != "working" {
        return Err(OmniError::InvalidInput(format!("проработка закончена: {}", s.status())));
    }
    Ok(s)
}

fn path_in(task: &str, s: &Session, kind: &str, me: &str) -> String {
    format!("{task}/work/{}/{kind}/{}-{}.md", s.id, current_unix_ms(), short(me))
}

/// Start a session of the task between us and `with` (both agents of one owner).
pub(crate) fn start(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, with: &str, topic: &str, rounds: Option<u32>) -> OmniResult<String> {
    let (me, _) = author(group, members)?;
    if with == me {
        return Err(OmniError::InvalidInput("второй участник проработки — другой участник workspace".into()));
    }
    let claims = crate::app_workspace::agents::claims(storage);
    if claims.contains_key(&me) && claims.contains_key(with) && claims.get(&me) != claims.get(with) {
        return Err(OmniError::InvalidInput("агенты разных хозяев работают вдвоём только с согласия обоих — пока не сделано".into()));
    }
    begin(db_path, storage, group, members, task, [&me, with], topic, rounds)
}

/// A person starts a session of the task between two agents of theirs; the agents are told in the
/// thread by the caller.
pub(crate) fn start_for(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, first: &str, second: &str, topic: &str, rounds: Option<u32>) -> OmniResult<String> {
    let (me, _) = author(group, members)?;
    let claims = crate::app_workspace::agents::claims(storage);
    if first == second || [first, second].iter().any(|a| claims.get(*a) != Some(&me)) {
        return Err(OmniError::InvalidInput("проработку начинают двое твоих агентов этого workspace".into()));
    }
    // Asked again after the start was made and its line to the group was not said: the same pair,
    // nothing written in it yet — it is that start, not a second one (blind audit 2026-11-11,
    // pass 13: the retry was refused as «already under way», and the pair was never called).
    if let Some(started) = latest(&work_folder(storage, group)?, task) {
        let same = started.head.get("author") == Some(me.as_str()) && started.head.get("with") == Some([first, second].join(", ").as_str());
        if same && started.outcome.is_none() && started.drafts.is_empty() && started.status() == "working" {
            return Ok(started.id);
        }
    }
    begin(db_path, storage, group, members, task, [first, second], topic, rounds)
}

fn begin(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, with: [&str; 2], topic: &str, rounds: Option<u32>) -> OmniResult<String> {
    let (me, name) = author(group, members)?;
    let found = super::find(storage, group, members, task)?;
    if with.iter().any(|w| !members.iter().any(|m| m.status == "member" && m.account_id == *w)) {
        return Err(OmniError::InvalidInput("второй участник проработки — другой участник workspace".into()));
    }
    if latest(&work_folder(storage, group)?, task).is_some_and(|s| s.outcome.is_none() && s.status() == "working") {
        return Err(OmniError::InvalidInput("у задачи уже идёт проработка".into()));
    }
    let rounds = rounds.unwrap_or(crate::app_workspace::agents::PAIR_ROUNDS).clamp(1, 10);
    let id = format!("{}-{}", current_unix_ms(), short(&me));
    let header = vec![("at", current_unix_ms().to_string()), ("author", me.clone()), ("author_name", name), ("with", with.join(", ")), ("rounds", rounds.to_string())];
    write_and_save(
        db_path,
        storage,
        group,
        members,
        &[(format!("{task}/work/{id}/session.md"), render(&header, topic))],
        &format!("#{} «{}»: проработка вдвоём, раундов {rounds}", found.number, found.title),
    )?;
    Ok(id)
}

/// Our draft: round 1 independent, then each next round after the other's.
pub(crate) fn draft(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, text: &str) -> OmniResult<u32> {
    if text.trim().is_empty() || text.len() > DRAFT_MAX_BYTES {
        return Err(OmniError::InvalidInput("черновик — текст до 64 КБ".into()));
    }
    let (me, name) = author(group, members)?;
    let s = open_session(storage, group, task, &me)?;
    let mine = s.drafts.iter().filter(|(_, n)| n.get("author") == Some(me.as_str())).map(|(_, n)| round_of(n)).max().unwrap_or(0);
    let round = mine + 1;
    if round > s.rounds_max() {
        return Err(OmniError::InvalidInput(format!("раундов было {} — это предел; согласись или оставь разногласия человеку", s.rounds_max())));
    }
    let other_behind = s.with().iter().filter(|w| **w != me).any(|w| s.draft(w, mine).is_none());
    if round > 1 && other_behind {
        return Err(OmniError::InvalidInput("второй ещё не дал черновик этого раунда".into()));
    }
    let header = vec![("at", current_unix_ms().to_string()), ("author", me.clone()), ("author_name", name), ("round", round.to_string())];
    let found = super::find(storage, group, members, task)?;
    write_and_save(
        db_path,
        storage,
        group,
        members,
        &[(format!("{task}/work/{}/drafts/{round}-{}.md", s.id, short(&me)), render(&header, text))],
        &format!("#{} «{}»: черновик раунда {round}", found.number, found.title),
    )?;
    finish_if_over(db_path, storage, group, members, task)?;
    Ok(round)
}

/// A remark on the other's draft.
pub(crate) fn remark(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, on: &str, what: &str, severity: &str, why: &str) -> OmniResult<()> {
    let (me, name) = author(group, members)?;
    let s = open_session(storage, group, task, &me)?;
    let (_, target) = s.drafts.iter().find(|(file, _)| file == on).ok_or_else(|| OmniError::InvalidInput("нет такого черновика".into()))?;
    if target.get("author") == Some(me.as_str()) {
        return Err(OmniError::InvalidInput("замечания — к черновику другого".into()));
    }
    let what = super::one_line(what);
    if what.is_empty() || what.len() > REMARK_MAX_BYTES || why.len() > REMARK_MAX_BYTES || !SEVERITIES.contains(&severity) {
        return Err(OmniError::InvalidInput("замечание: что (одной строкой), серьёзность major|minor|nit, обоснование".into()));
    }
    let header = vec![("at", current_unix_ms().to_string()), ("author", me.clone()), ("author_name", name), ("on", on.to_string()), ("severity", severity.to_string()), ("what", what.clone())];
    let found = super::find(storage, group, members, task)?;
    write_and_save(db_path, storage, group, members, &[(path_in(task, &s, "remarks", &me), render(&header, why))], &format!("#{} «{}»: замечание — {what}", found.number, found.title))
}

/// The draft's author on a remark: taken in, or not — and then why.
pub(crate) fn answer(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, remark: &str, taken: bool, why: &str) -> OmniResult<()> {
    let (me, name) = author(group, members)?;
    let s = open_session(storage, group, task, &me)?;
    let (_, note) = s.remarks.iter().find(|(id, _)| id == remark).ok_or_else(|| OmniError::InvalidInput("нет такого замечания".into()))?;
    let on = note.get("on").unwrap_or("");
    let draft_author = s.drafts.iter().find(|(file, _)| file == on).and_then(|(_, n)| n.get("author"));
    if draft_author != Some(me.as_str()) {
        return Err(OmniError::InvalidInput("на замечание отвечает автор черновика".into()));
    }
    if s.answer(remark).is_some() {
        return Err(OmniError::InvalidInput("на это замечание уже ответили".into()));
    }
    if !taken && why.trim().is_empty() {
        return Err(OmniError::InvalidInput("«не согласен» — только с обоснованием".into()));
    }
    let header = vec![("at", current_unix_ms().to_string()), ("author", me.clone()), ("author_name", name), ("remark", remark.to_string()), ("verdict", if taken { "taken" } else { "disagree" }.to_string())];
    let found = super::find(storage, group, members, task)?;
    write_and_save(
        db_path,
        storage,
        group,
        members,
        &[(path_in(task, &s, "answers", &me), render(&header, why))],
        &format!("#{} «{}»: {} замечание", found.number, found.title, if taken { "учтено" } else { "не принято" }),
    )?;
    finish_if_over(db_path, storage, group, members, task)
}

/// «Согласен» with the latest draft — only when no remark on it is open.
pub(crate) fn agree(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str) -> OmniResult<()> {
    let (me, name) = author(group, members)?;
    let s = open_session(storage, group, task, &me)?;
    let (latest, _) = s.latest_draft().ok_or_else(|| OmniError::InvalidInput("черновиков ещё нет".into()))?;
    if s.open_remarks_on(latest) > 0 {
        return Err(OmniError::InvalidInput("на последнем черновике есть открытые замечания".into()));
    }
    let header = vec![("at", current_unix_ms().to_string()), ("author", me.clone()), ("author_name", name), ("draft", latest.clone())];
    let found = super::find(storage, group, members, task)?;
    write_and_save(db_path, storage, group, members, &[(path_in(task, &s, "agree", &me), render(&header, ""))], &format!("#{} «{}»: согласие с черновиком", found.number, found.title))?;
    finish_if_over(db_path, storage, group, members, task)
}

/// The session ended: its outcome and a comment on the task, once.
fn finish_if_over(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str) -> OmniResult<()> {
    let Some(s) = latest(&work_folder(storage, group)?, task) else { return Ok(()) };
    let status = s.status();
    if s.outcome.is_some() || status == "working" {
        return Ok(());
    }
    write_outcome(db_path, storage, group, members, task, &s, &status)
}

/// A person ends a session that goes on: the owner of one of its agents, or an admin.
pub(crate) fn close(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str) -> OmniResult<()> {
    let (me, _) = author(group, members)?;
    let s = latest(&work_folder(storage, group)?, task).ok_or_else(|| OmniError::InvalidInput("у задачи нет проработки".into()))?;
    if s.outcome.is_some() || s.status() != "working" {
        return Err(OmniError::InvalidInput("проработка уже закончена".into()));
    }
    let claims = crate::app_workspace::agents::claims(storage);
    let owner = s.with().iter().any(|w| claims.get(w) == Some(&me));
    if !owner && !crate::app_workspace::is_admin(members, &group.my_member_pub) {
        return Err(OmniError::InvalidInput("завершает хозяин одного из агентов или админ".into()));
    }
    write_outcome(db_path, storage, group, members, task, &s, "closed")
}

fn write_outcome(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], task: &str, s: &Session, status: &str) -> OmniResult<()> {
    let names: Vec<String> = s.with().iter().map(|w| members.iter().find(|m| &m.account_id == w).map_or(w.clone(), |m| m.display_name.clone())).collect();
    let latest = s.latest_draft().map(|(f, n)| (f.clone(), round_of(n)));
    let (me, name) = author(group, members)?;
    let mut text = match status {
        "agreed" => format!("Проработка ({}): агенты согласовали черновик раунда {}. Нужна проверка человеком.", names.join(", "), latest.as_ref().map_or(0, |l| l.1)),
        "deadlock" => format!("Проработка ({}): тупик — два раунда без изменений. Решает человек.", names.join(", ")),
        "closed" => format!("Проработка ({}): завершил {name}.", names.join(", ")),
        _ => format!("Проработка ({}): лимит раундов ({}), согласия нет. Решает человек.", names.join(", "), s.rounds_max()),
    };
    let disagreements: Vec<String> = s
        .remarks
        .iter()
        .filter_map(|(id, n)| match s.answer(id) {
            Some(a) if a.get("verdict") == Some("disagree") => Some(format!("— {} (не принято: {})", n.get("what").unwrap_or(""), a.body.trim())),
            None => Some(format!("— {} (без ответа)", n.get("what").unwrap_or(""))),
            _ => None,
        })
        .collect();
    if !disagreements.is_empty() {
        text.push_str("\nРазногласия:\n");
        text.push_str(&disagreements.join("\n"));
    }
    if let Some((file, _)) = &latest {
        text.push_str(&format!("\nЧерновик: {TASKS_REPO}/{task}/work/{}/drafts/{file}.md", s.id));
    }
    let now = current_unix_ms();
    let outcome = vec![("at", now.to_string()), ("author", me.clone()), ("author_name", name.clone()), ("status", status.to_string())];
    let comment = vec![("at", now.to_string()), ("author", me.clone()), ("author_name", name)];
    let found = super::find(storage, group, members, task)?;
    write_and_save(
        db_path,
        storage,
        group,
        members,
        &[
            (format!("{task}/work/{}/outcome.md", s.id), render(&outcome, &text)),
            (format!("{task}/comments/{now}-{}.md", short(&me)), render(&comment, &text)),
        ],
        &format!("#{} «{}»: проработка закончена — {}", found.number, found.title, match status {
            "agreed" => "черновик согласован",
            "deadlock" => "тупик",
            "closed" => "завершена человеком",
            _ => "лимит раундов",
        }),
    )
}
