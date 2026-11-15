//! Задачи workspace (docs/decisions/omni-workspace-git.md, раздел 3; workspace-files-storage.md, «Задачи»):
//! `.md` files in the workspace's files project [`TASKS_REPO`], laid out so that two people never
//! write the same file by changing a task:
//!
//! - `<id>/task.md` — the task as made, and its description (the text under the header);
//! - `<id>/changes/<at>-<who>.md` — one change of fields (title, status, assignee, labels, links): a
//!   field's value is that of the latest change naming it (time, then account), so two changes of
//!   different fields both stay and of one field the later wins; `mentioned` lines add up; `place` is
//!   the card's place in its column, set when it is dragged between two others;
//! - `<id>/comments/<at>-<who>.md` — one comment.
//!
//! Only two edits of one description at once give a conflict copy beside `task.md`, as for any file.
//! The number #N is the task's place by creation time in the merged project: two made offline as #12
//! become #12 and #13 when they meet. Every change is a version of the project, accepted at once (the
//! admin's policy when tasks are turned on), so the feed of the group tells it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use omni_storage::{GroupMemberRecord, GroupRecord, Storage, WorkspaceRepoRecord};
use omni_types::{OmniError, OmniResult};
use serde::Serialize;

use super::{files, my_account, versions};
use crate::{current_unix_ms, omni_app_trace_log};

#[path = "app_workspace_pair.rs"]
pub(crate) mod pair;
pub use pair::WorkspacePairView;

/// The files project that holds a workspace's tasks.
pub(crate) const TASKS_REPO: &str = "omni-tasks";
/// The board's columns, in order.
pub(super) const STATUSES: [&str; 4] = ["new", "doing", "review", "done"];
const TITLE_MAX_CHARS: usize = 200;
const BODY_MAX_BYTES: usize = 64 * 1024;
const COMMENT_MAX_BYTES: usize = 16 * 1024;
const LABELS_MAX: usize = 16;
const LABEL_MAX_CHARS: usize = 32;
const LINKS_MAX: usize = 32;
const LINK_MAX_CHARS: usize = 1100;
/// Between the place of a card put at a column's end and its neighbour's.
const PLACE_STEP: f64 = 1000.0;

/// The board of a workspace.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkspaceTasksView {
    /// An admin turned tasks on for the workspace.
    pub enabled: bool,
    /// We may turn them on (an admin).
    pub can_enable: bool,
    /// The folder the task files are in here.
    pub folder: Option<String>,
    pub tasks: Vec<WorkspaceTaskView>,
    /// Whom a task may be given to.
    pub members: Vec<WorkspaceTaskMemberView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTaskMemberView {
    pub account_id: String,
    pub name: String,
    pub is_me: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTaskView {
    pub id: String,
    /// #N
    pub number: u32,
    pub title: String,
    /// One of [`STATUSES`].
    pub status: String,
    pub assignee: Option<String>,
    pub assignee_name: Option<String>,
    pub labels: Vec<String>,
    /// `repo:path` of the workspace's files.
    pub links: Vec<String>,
    pub body: String,
    pub author_name: String,
    pub created_unix_ms: i64,
    pub updated_unix_ms: i64,
    /// Its place in its column, the smaller the higher: where it was dragged to, else the latest
    /// changed on top (minus the time of its last change).
    pub place: f64,
    pub comments: Vec<WorkspaceTaskCommentView>,
    /// Versions whose comment named the task («#12»).
    pub mentions: Vec<WorkspaceTaskMentionView>,
    /// Two edits of the description at once: a copy is beside `task.md`.
    pub conflict: bool,
    /// «Проработать вдвоём»: the task's latest session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<WorkspacePairView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTaskCommentView {
    pub author_name: String,
    pub at_unix_ms: i64,
    pub text: String,
    pub mine: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceTaskMentionView {
    pub repo: String,
    pub oid: String,
    pub subject: String,
    pub author_name: String,
    pub at_unix_ms: i64,
}

/// The fields one change sets; none: left as they are.
#[derive(Debug, Clone, Default)]
pub(super) struct TaskFields {
    pub title: Option<String>,
    pub status: Option<String>,
    /// Some(""): nobody.
    pub assignee: Option<String>,
    pub labels: Option<Vec<String>>,
    pub links: Option<Vec<String>>,
    /// Dragged right above this task of its (new) column; Some(""): to the column's end.
    pub before: Option<String>,
}

/// A task file: `key: value` lines between `---` and the text after.
#[derive(Debug, Default, PartialEq, Eq)]
struct Note {
    fields: Vec<(String, String)>,
    body: String,
}

impl Note {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn parse(text: &str) -> Note {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Note { fields: Vec::new(), body: text.trim().to_string() };
    }
    let mut fields = Vec::new();
    let mut rest = Vec::new();
    let mut header = true;
    for line in lines {
        if header {
            if line.trim() == "---" {
                header = false;
            } else if let Some((key, value)) = line.split_once(':') {
                fields.push((key.trim().to_string(), value.trim().to_string()));
            }
        } else {
            rest.push(line);
        }
    }
    Note { fields, body: rest.join("\n").trim().to_string() }
}

fn render(fields: &[(&str, String)], body: &str) -> String {
    let mut text = String::from("---\n");
    for (key, value) in fields {
        text.push_str(&format!("{key}: {value}\n"));
    }
    text.push_str("---\n");
    if !body.trim().is_empty() {
        text.push('\n');
        text.push_str(body.trim());
        text.push('\n');
    }
    text
}

/// A value on one line of the header.
fn one_line(text: &str) -> String {
    text.split(['\r', '\n']).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

fn list(value: Option<&str>) -> Vec<String> {
    value.map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

fn checked_title(title: &str) -> OmniResult<String> {
    let title = one_line(title);
    if title.is_empty() || title.chars().count() > TITLE_MAX_CHARS {
        return Err(OmniError::InvalidInput("a task needs a title of at most 200 characters".into()));
    }
    Ok(title)
}

fn checked_list(values: &[String], most: usize, chars: usize, what: &str) -> OmniResult<Vec<String>> {
    let values: Vec<String> = values.iter().map(|v| one_line(v).replace(',', " ")).filter(|v| !v.is_empty()).collect();
    if values.len() > most || values.iter().any(|v| v.chars().count() > chars) {
        return Err(OmniError::InvalidInput(format!("too many or too long task {what}")));
    }
    Ok(values)
}

/// The part of an account id in a file name.
fn short(account: &str) -> String {
    account.chars().filter(char::is_ascii_alphanumeric).take(8).collect()
}

/// The tasks project's record here, made with its working folder when missing: under the
/// workspace root the person chose, else in the profile's data.
pub(crate) fn prepare(storage: &Storage, group: &GroupRecord) -> OmniResult<WorkspaceRepoRecord> {
    let record = files::ensure_project(storage, &group.group_id, TASKS_REPO)?;
    if record.work_path.is_some() {
        return Ok(record);
    }
    let folder = files::free_auto_folder(storage, default_folder(storage, group)?)?;
    files::bind_folder(storage, &group.group_id, TASKS_REPO, &folder.to_string_lossy())
}

pub(super) fn default_folder(storage: &Storage, group: &GroupRecord) -> OmniResult<PathBuf> {
    if let Some(root) = storage.get_workspace_file_local("", "", "root")? {
        return Ok(super::custody::default_folder(&root, group, "Задачи"));
    }
    Ok(crate::app_workspace::own_folder(storage, omni_storage::ProfileFolder::WorkspaceTasks)?.join(&group.group_id))
}

/// One task's files read from the folder.
struct Read {
    id: String,
    task: Note,
    changes: Vec<Note>,
    comments: Vec<Note>,
    conflict: bool,
}

fn read_notes(dir: &Path) -> Vec<Note> {
    let mut named: Vec<(String, Note)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
        .filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), parse(&std::fs::read_to_string(e.path()).ok()?))))
        .collect();
    named.sort_by(|(a, x), (b, y)| (at(x), a).cmp(&(at(y), b)));
    named.into_iter().map(|(_, n)| n).collect()
}

fn at(note: &Note) -> i64 {
    note.get("at").or_else(|| note.get("created")).and_then(|v| v.parse().ok()).unwrap_or(0)
}

fn read_all(work: &Path) -> Vec<Read> {
    let mut tasks: Vec<Read> = std::fs::read_dir(work)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("task.md").is_file())
        .filter_map(|e| {
            let dir = e.path();
            let id = e.file_name().to_string_lossy().into_owned();
            let task = parse(&std::fs::read_to_string(dir.join("task.md")).ok()?);
            let conflict = std::fs::read_dir(&dir).into_iter().flatten().flatten().any(|f| versions::conflict_base(&format!("{id}/{}", f.file_name().to_string_lossy())).is_some_and(|(base, _)| base == format!("{id}/task.md")));
            Some(Read { changes: read_notes(&dir.join("changes")), comments: read_notes(&dir.join("comments")), task, conflict, id })
        })
        .collect();
    tasks.sort_by(|a, b| (at(&a.task), &a.id).cmp(&(at(&b.task), &b.id)));
    tasks
}

/// The board as the folder holds it.
pub(super) fn view(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], enabled: bool, admin: bool) -> OmniResult<WorkspaceTasksView> {
    let me = my_account(members, group);
    let names: BTreeMap<&str, &str> = members.iter().filter(|m| m.status == "member").map(|m| (m.account_id.as_str(), m.display_name.as_str())).collect();
    let name_of = |account: &str, fallback: Option<&str>| names.get(account).map(|n| n.to_string()).or_else(|| fallback.map(str::to_string)).unwrap_or_else(|| account.to_string());
    let mut view = WorkspaceTasksView {
        enabled,
        can_enable: admin,
        members: names.iter().map(|(account, name)| WorkspaceTaskMemberView { account_id: account.to_string(), name: name.to_string(), is_me: me.as_deref() == Some(*account) }).collect(),
        ..Default::default()
    };
    let Some(work) = storage.get_workspace_repo(&group.group_id, TASKS_REPO)?.and_then(|r| r.work_path) else { return Ok(view) };
    view.folder = Some(work.replace('/', std::path::MAIN_SEPARATOR_STR));
    for (i, read) in read_all(Path::new(&work)).into_iter().enumerate() {
        const FIELDS: [&str; 6] = ["title", "status", "assignee", "labels", "links", "place"];
        let mut fields: BTreeMap<String, String> = read.task.fields.iter().filter(|(k, _)| FIELDS.contains(&k.as_str())).cloned().collect();
        let mut updated = at(&read.task);
        let mut mentions = Vec::new();
        // Oldest first: a later change of a field replaces an earlier one.
        for change in &read.changes {
            updated = updated.max(at(change));
            for (key, value) in &change.fields {
                match key.as_str() {
                    key if FIELDS.contains(&key) => {
                        fields.insert(key.to_string(), value.clone());
                    }
                    // «<repo>@<version> <comment>»
                    "mentioned" => {
                        if let Some((place, subject)) = value.split_once(' ').or(Some((value.as_str(), ""))) {
                            if let Some((repo, oid)) = place.split_once('@') {
                                mentions.push(WorkspaceTaskMentionView {
                                    repo: repo.to_string(),
                                    oid: oid.to_string(),
                                    subject: subject.to_string(),
                                    author_name: name_of(change.get("author").unwrap_or(""), change.get("author_name")),
                                    at_unix_ms: at(change),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let status = fields.get("status").filter(|s| STATUSES.contains(&s.as_str())).cloned().unwrap_or_else(|| STATUSES[0].to_string());
        let assignee = fields.get("assignee").filter(|a| !a.is_empty()).cloned();
        let updated = updated.max(read.comments.iter().map(at).max().unwrap_or(0));
        view.tasks.push(WorkspaceTaskView {
            place: fields.get("place").and_then(|p| p.parse::<f64>().ok()).filter(|p| p.is_finite()).unwrap_or(-(updated as f64)),
            number: (i + 1) as u32,
            title: fields.get("title").cloned().unwrap_or_default(),
            status,
            assignee_name: assignee.as_deref().map(|a| name_of(a, None)),
            assignee,
            labels: list(fields.get("labels").map(String::as_str)),
            links: list(fields.get("links").map(String::as_str)),
            body: read.task.body.clone(),
            author_name: name_of(read.task.get("author").unwrap_or(""), read.task.get("author_name")),
            created_unix_ms: at(&read.task),
            updated_unix_ms: updated,
            comments: read
                .comments
                .iter()
                .map(|c| WorkspaceTaskCommentView {
                    author_name: name_of(c.get("author").unwrap_or(""), c.get("author_name")),
                    at_unix_ms: at(c),
                    text: c.body.clone(),
                    mine: c.get("author").is_some_and(|a| me.as_deref() == Some(a)),
                })
                .collect(),
            mentions,
            conflict: read.conflict,
            pair: pair::view(Path::new(&work), &read.id, &name_of),
            id: read.id,
        });
    }
    Ok(view)
}

/// Who we are, for the files we write.
fn author(group: &GroupRecord, members: &[GroupMemberRecord]) -> OmniResult<(String, String)> {
    let me = my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
    let name = members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| one_line(&m.display_name)).unwrap_or_default();
    Ok((me, name))
}

/// Write `files` (paths in the tasks folder) and save them as one version with `message`.
fn write_and_save(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], files_to_write: &[(String, String)], message: &str) -> OmniResult<()> {
    let record = prepare(storage, group)?;
    let work = PathBuf::from(record.work_path.clone().ok_or_else(|| OmniError::Internal("the tasks project has no folder".into()))?);
    for (path, text) in files_to_write {
        omni_protocol::validate_workspace_file_path(path)?;
        let target = work.join(path);
        let folder = target.parent().ok_or_else(|| OmniError::Internal("a task file has no folder".into()))?;
        std::fs::create_dir_all(folder).map_err(|e| OmniError::Storage(format!("{}: {e}", folder.display())))?;
        let temp = folder.join(format!(".omni-new~{}", hex::encode(&omni_crypto::generate_group_key()[..6])));
        std::fs::write(&temp, text).and_then(|_| std::fs::rename(&temp, &target)).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            OmniError::Storage(format!("{}: {e}", target.display()))
        })?;
    }
    files::save_now(db_path, storage, group, members, TASKS_REPO, message)?;
    omni_app_trace_log("workspace/tasks", &format!("group={} {message}", group.group_id));
    Ok(())
}

/// The task with `id` and its number, as the folder holds them.
pub(super) fn find(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], id: &str) -> OmniResult<WorkspaceTaskView> {
    view(storage, group, members, true, false)?.tasks.into_iter().find(|t| t.id == id).ok_or_else(|| OmniError::InvalidInput("no such task".into()))
}

/// «Новая задача».
pub(super) fn create(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], title: &str, body: &str, fields: TaskFields) -> OmniResult<String> {
    let title = checked_title(title)?;
    if body.len() > BODY_MAX_BYTES {
        return Err(OmniError::InvalidInput("the task's description is too long".into()));
    }
    let (me, name) = author(group, members)?;
    let id = hex::encode(&omni_crypto::generate_group_key()[..6]);
    let mut header = vec![("title", title.clone()), ("created", current_unix_ms().to_string()), ("author", me), ("author_name", name)];
    header.extend(checked_fields(members, fields)?);
    write_and_save(db_path, storage, group, members, &[(format!("{id}/task.md"), render(&header, body))], &format!("новая задача «{title}»"))?;
    Ok(id)
}

fn checked_fields(members: &[GroupMemberRecord], fields: TaskFields) -> OmniResult<Vec<(&'static str, String)>> {
    let mut out = Vec::new();
    if let Some(title) = fields.title {
        out.push(("title", checked_title(&title)?));
    }
    if let Some(status) = fields.status {
        if !STATUSES.contains(&status.as_str()) {
            return Err(OmniError::InvalidInput("unknown task status".into()));
        }
        out.push(("status", status));
    }
    if let Some(assignee) = fields.assignee {
        if !assignee.is_empty() && !members.iter().any(|m| m.status == "member" && m.account_id == assignee) {
            return Err(OmniError::InvalidInput("a task is given to a member of the workspace".into()));
        }
        out.push(("assignee", assignee));
    }
    if let Some(labels) = fields.labels {
        out.push(("labels", checked_list(&labels, LABELS_MAX, LABEL_MAX_CHARS, "labels")?.join(", ")));
    }
    if let Some(links) = fields.links {
        out.push(("links", checked_list(&links, LINKS_MAX, LINK_MAX_CHARS, "links")?.join(", ")));
    }
    Ok(out)
}

/// A change of fields: a file of its own, so another change at the same time never collides.
/// Moved to another column without a place: on top of it, as the latest changed.
pub(super) fn change(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], id: &str, fields: TaskFields) -> OmniResult<()> {
    let board = view(storage, group, members, true, false)?;
    let task = board.tasks.iter().find(|t| t.id == id).ok_or_else(|| OmniError::InvalidInput("no such task".into()))?;
    let before = fields.before.clone();
    let mut named = checked_fields(members, fields)?;
    let status = named.iter().find(|(key, _)| *key == "status").map(|(_, value)| value.clone()).unwrap_or_else(|| task.status.clone());
    match before {
        Some(before) => named.push(("place", place_before(&board, id, &status, &before)?.to_string())),
        None if status != task.status => named.push(("place", String::new())),
        None => {}
    }
    if named.is_empty() {
        return Ok(());
    }
    let what = named
        .iter()
        .filter(|(key, _)| *key != "place" || named.len() == 1)
        .map(|(key, value)| match *key {
            "place" => "место на доске".to_string(),
            "status" => format!("{} → «{}»", "статус", status_name(value)),
            "assignee" if value.is_empty() => "без исполнителя".to_string(),
            "assignee" => format!("исполнитель — {}", members.iter().find(|m| &m.account_id == value).map_or(value.as_str(), |m| m.display_name.as_str())),
            "title" => format!("название «{value}»"),
            "labels" => "метки".to_string(),
            _ => "ссылки".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    write_note(db_path, storage, group, members, id, "changes", named, "", &format!("#{} «{}»: {what}", task.number, task.title))
}

/// The place of a card put right above `before` in the `status` column ("": at its end), between
/// its new neighbours.
fn place_before(board: &WorkspaceTasksView, moved: &str, status: &str, before: &str) -> OmniResult<f64> {
    let mut column: Vec<&WorkspaceTaskView> = board.tasks.iter().filter(|t| t.status == status && t.id != moved).collect();
    column.sort_by(|a, b| a.place.total_cmp(&b.place));
    let at = match before {
        "" => column.len(),
        before => column.iter().position(|t| t.id == before).ok_or_else(|| OmniError::InvalidInput("no such task in that column".into()))?,
    };
    Ok(match (at.checked_sub(1).map(|i| column[i].place), column.get(at).map(|t| t.place)) {
        (Some(above), Some(below)) => (above + below) / 2.0,
        (Some(above), None) => above + PLACE_STEP,
        (None, Some(below)) => below - PLACE_STEP,
        (None, None) => 0.0,
    })
}

fn status_name(status: &str) -> &'static str {
    match status {
        "doing" => "В работе",
        "review" => "На проверке",
        "done" => "Готово",
        _ => "Новые",
    }
}

fn write_note(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], id: &str, kind: &str, fields: Vec<(&str, String)>, body: &str, message: &str) -> OmniResult<()> {
    let (me, name) = author(group, members)?;
    let now = current_unix_ms();
    let mut header = vec![("at", now.to_string()), ("author", me.clone()), ("author_name", name)];
    header.extend(fields);
    write_and_save(db_path, storage, group, members, &[(format!("{id}/{kind}/{now}-{}.md", short(&me)), render(&header, body))], message)
}

/// The description, rewritten (two at once: a copy beside it, as for any file).
pub(super) fn describe(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], id: &str, body: &str) -> OmniResult<()> {
    if body.len() > BODY_MAX_BYTES {
        return Err(OmniError::InvalidInput("the task's description is too long".into()));
    }
    let task = find(storage, group, members, id)?;
    let work = prepare(storage, group)?.work_path.ok_or_else(|| OmniError::Internal("the tasks project has no folder".into()))?;
    let path = Path::new(&work).join(id).join("task.md");
    let note = parse(&std::fs::read_to_string(&path).map_err(|e| OmniError::Storage(format!("{}: {e}", path.display())))?);
    let header: Vec<(&str, String)> = note.fields.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    write_and_save(db_path, storage, group, members, &[(format!("{id}/task.md"), render(&header, body))], &format!("#{} «{}»: описание", task.number, task.title))
}

/// A comment on a task.
pub(super) fn comment(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], id: &str, text: &str) -> OmniResult<()> {
    if text.trim().is_empty() || text.len() > COMMENT_MAX_BYTES {
        return Err(OmniError::InvalidInput("a comment is some text of at most 16 KB".into()));
    }
    let task = find(storage, group, members, id)?;
    write_note(db_path, storage, group, members, id, "comments", Vec::new(), text, &format!("комментарий к #{} «{}»", task.number, task.title))
}

/// The tasks a version's comment names: `#12`, and whether a word before it closes it
/// («закрывает #12», «готово #12», «fixes #12»).
fn named_tasks(message: &str) -> Vec<(u32, bool)> {
    const CLOSING: [&str; 11] = ["закрывает", "закрыта", "закрыто", "закрыл", "закрыла", "готово", "готова", "fixes", "fixed", "closes", "closed"];
    let mut named: Vec<(u32, bool)> = Vec::new();
    let mut previous = String::new();
    for word in message.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '(' || c == ')') {
        if let Some(number) = word.strip_prefix('#').map(|w| w.trim_end_matches(|c: char| !c.is_ascii_digit())).and_then(|w| w.parse::<u32>().ok()) {
            let closes = CLOSING.contains(&previous.trim_end_matches(':').to_lowercase().as_str());
            match named.iter_mut().find(|(n, _)| *n == number) {
                Some(entry) => entry.1 |= closes,
                None => named.push((number, closes)),
            }
        } else if !word.is_empty() {
            previous = word.to_string();
        }
    }
    named
}

/// A version of another project whose comment names tasks: each gets a mention (and «Готово» when
/// a closing word comes before it) — written once, by the node that saved the version.
pub(super) fn mentioned(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, version: &str, message: &str) -> OmniResult<()> {
    let named = named_tasks(message);
    if named.is_empty() || repo == TASKS_REPO || storage.get_workspace_repo(&group.group_id, TASKS_REPO)?.and_then(|r| r.work_path).is_none() {
        return Ok(());
    }
    let board = view(storage, group, members, true, false)?;
    for (number, closes) in named {
        let Some(task) = board.tasks.iter().find(|t| t.number == number) else { continue };
        let mut fields = vec![("mentioned", format!("{repo}@{version} {}", one_line(message)))];
        if closes && task.status != "done" {
            fields.push(("status", "done".to_string()));
            fields.push(("place", String::new()));
        }
        let what = if closes { "закрыта" } else { "упомянута" };
        write_note(db_path, storage, group, members, &task.id, "changes", fields, "", &format!("#{number} «{}» {what} в {repo}", task.title))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_file_reads_back_as_written() {
        let text = render(&[("title", "Корпус: отверстие 12".into()), ("labels", "срочно, корпус".into())], "Сделать отверстие.\n\nПо чертежу.");
        let note = parse(&text);
        assert_eq!(note.get("title"), Some("Корпус: отверстие 12"));
        assert_eq!(list(note.get("labels")), vec!["срочно", "корпус"]);
        assert_eq!(note.body, "Сделать отверстие.\n\nПо чертежу.");
        assert_eq!(parse("просто текст").body, "просто текст");
        assert_eq!(one_line("a\r\nb\n c"), "a b c");
    }

    #[test]
    fn a_comment_names_tasks_and_a_closing_word_closes_them() {
        assert_eq!(named_tasks("Отверстие по #12, см. #3"), vec![(12, false), (3, false)]);
        assert_eq!(named_tasks("Закрывает #12; fixes #4."), vec![(12, true), (4, true)]);
        assert_eq!(named_tasks("готово: #7 и #7"), vec![(7, true)]);
        assert!(named_tasks("цвет #fff и #").is_empty());
    }
}
