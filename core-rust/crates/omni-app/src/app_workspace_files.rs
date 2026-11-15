//! Workspace projects that hold files — models, drawings, documents (docs/decisions/workspace-files-storage.md):
//! one line of versions without branches that any member moves by saving a version, without git.
//!
//! - A version is a record of what changed against its parent (`app_workspace_versions`), fetched by
//!   its hash like file content; each device announces only the tip of its line
//!   (`WorkspaceFilesEventV1::Tip`). Content is fetched by hash from members (`app_workspace_content`).
//! - «Сохранить версию»: everything changed in the folder becomes one version on top of the one the
//!   folder holds; every node takes it as the shared line when it goes forward and is accepted.
//! - Two saves from the same shared version went different ways: the shared line takes one by a
//!   rule every node works out alike (`next_files_main`); the node whose own line lost replays its
//!   versions on top (`versions::replay`). A file both changed keeps the winner's content, the
//!   loser's is kept beside it as `name (who, date).ext` — nothing is overwritten.
//! - The folder follows the shared line by itself, a whole version at a time (`versions::follow`): a
//!   file open in a program or changed here and not saved holds the whole version back.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_mesh_runtime::{MeshNodeAddress, MeshRuntime};
use omni_protocol::{WorkspaceContentV1 as Content, WorkspaceFilesEventV1 as Event};
use omni_storage::{GroupMemberRecord, GroupRecord, Storage, WorkspaceRepoRecord};
use omni_types::{OmniError, OmniResult};

use super::custody::WorkspaceFileState;
use super::versions::{self, Ref};
use super::{is_admin, my_account, next_shared_main, with_profile, FetchKey, WorkspaceMainState};
use crate::{current_unix_ms, omni_app_trace_log, OmniApp};

/// A history object no node gave: asked for again after this, then twice as late each time, up to
/// [`HISTORY_RETRY_MAX`].
const HISTORY_RETRY_FIRST: Duration = Duration::from_secs(60);
const HISTORY_RETRY_MAX: Duration = Duration::from_secs(30 * 60);
/// History objects that may fail in one pass before it ends (the sources are away).
const HISTORY_FAILURES_PER_PASS: usize = 3;
/// How long «Сохранить версию» waits for the background work on the project to finish.
const SAVE_WAITS_FOR_SYNC: Duration = Duration::from_secs(60);
/// How long one round of the work fetches versions, and history for an archive: a save waits for a
/// round to end.
const FETCH_PER_PASS: Duration = Duration::from_secs(10);
/// The key under which a project's files work runs in `ProfileWorkspaces::fetching`.
const FILES_WORK: &str = "#files";
/// The comment when the person wrote none.
const DEFAULT_MESSAGE: &str = "Версия";
/// Versions the shared line took at once that each get a line in the group's thread (newest kept).
const FEED_VERSIONS_NAMED: usize = 5;

/// Where the folder is against the shared line, as the files work last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct WorkspaceFilesSync {
    /// Files open in a program: the next version waits for them to be closed.
    pub waiting_open: Vec<String>,
    /// Files changed here and not saved that the next version also changes.
    pub waiting_changed: Vec<String>,
    /// Content of the shared version not here yet (bytes); being fetched from members.
    pub content_missing_bytes: u64,
    pub content_total_bytes: u64,
    /// The last thing that went wrong, for the person (none when all is well).
    pub problem: Option<String>,
}

/// One rank of a line of versions after the point two lines parted: the line more accounts built on
/// wins, then the longer one, then the one that began earlier, then the smaller first version id.
pub(super) type LineRank = (usize, usize, Reverse<i64>, Reverse<String>);

/// Where this profile keeps the local data of the files project `repo` in `group_id`.
pub(super) fn project_path(storage: &Storage, group_id: &str, repo: &str) -> OmniResult<PathBuf> {
    Ok(crate::app_workspace::own_folder(storage, omni_storage::ProfileFolder::Workspaces)?.join(group_id).join(format!("{repo}.files")))
}

/// The record of a files project here, made when there is none. A record whose data lives in another
/// folder starts over there: the folder stays bound and its files become the first version when saved.
pub(super) fn ensure_project(storage: &Storage, group_id: &str, repo: &str) -> OmniResult<WorkspaceRepoRecord> {
    omni_protocol::validate_workspace_repo_name(repo)?;
    storage.mark_workspace_repo_files(group_id, repo)?;
    let dir = project_path(storage, group_id, repo)?;
    let existing = storage.get_workspace_repo(group_id, repo)?;
    if let Some(record) = existing.as_ref().filter(|r| Path::new(&r.published_path) == dir) {
        versions::prepare_project(&dir)?;
        return Ok(record.clone());
    }
    versions::prepare_project(&dir)?;
    let record = WorkspaceRepoRecord {
        group_id: group_id.into(),
        repo: repo.into(),
        published_path: dir.to_string_lossy().into_owned(),
        work_path: existing.as_ref().and_then(|r| r.work_path.clone()),
        announced_refs_json: None,
        created_at_unix_ms: existing.map_or_else(current_unix_ms, |r| r.created_at_unix_ms),
    };
    storage.upsert_workspace_repo(&record)?;
    Ok(record)
}

/// Bind `folder` (made when missing) as the working folder of the project. Another folder than the
/// one bound before (or removed last, «Убрать папку») holds no version yet: whatever it has is its
/// own changes until a version is in.
/// Whether `folder` is, holds or lies in the working folder of a project of another profile of this
/// database. Two profiles given the same workspace root made the same project folders there: what
/// one changed was the other's local change, its unpublished files the other's to save (blind audit
/// 2026-11-13, pass 18, A18-06; maintainers' decision: the folders, not the root, are kept apart).
pub(super) fn folder_of_another_profile(storage: &Storage, folder: &Path) -> OmniResult<bool> {
    let plain = |path: &str| {
        let text = path.replace('/', "\\").trim_end_matches('\\').to_string();
        if cfg!(windows) { text.to_lowercase() } else { text }
    };
    let wanted = plain(&folder.to_string_lossy());
    Ok(storage.workspace_work_paths_of_other_profiles()?.iter().map(|path| plain(path)).any(|theirs| {
        theirs == wanted || theirs.starts_with(&format!("{wanted}\\")) || wanted.starts_with(&format!("{theirs}\\"))
    }))
}

/// A folder Omni chooses itself (`wanted`), or the first free one beside it — «Задачи (2)» — when
/// another profile here works in it. A profile nobody can open any more holds nothing up this way.
pub(super) fn free_auto_folder(storage: &Storage, wanted: PathBuf) -> OmniResult<PathBuf> {
    if !folder_of_another_profile(storage, &wanted)? {
        return Ok(wanted);
    }
    let name = wanted.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 2..100 {
        let next = wanted.with_file_name(format!("{name} ({n})"));
        if !folder_of_another_profile(storage, &next)? {
            return Ok(next);
        }
    }
    Err(OmniError::InvalidInput("no free project folder beside the one another profile uses".into()))
}

pub(super) fn bind_folder(storage: &Storage, group_id: &str, repo: &str, folder: &str) -> OmniResult<WorkspaceRepoRecord> {
    if !Path::new(folder).is_absolute() {
        return Err(OmniError::InvalidInput("choose an absolute project folder".into()));
    }
    if folder_of_another_profile(storage, Path::new(folder))? {
        return Err(OmniError::InvalidInput("this folder holds the working files of another profile on this computer".into()));
    }
    Storage::confine_destructive_path_for_callers(Path::new(folder))?;
    let mut record = ensure_project(storage, group_id, repo)?;
    std::fs::create_dir_all(folder).map_err(|e| OmniError::Storage(format!("cannot make {folder}: {e}")))?;
    let canonical = super::plain_path(&std::fs::canonicalize(folder).map_err(|e| OmniError::InvalidInput(format!("no folder {folder}: {e}")))?);
    let dir = PathBuf::from(&record.published_path);
    if canonical.starts_with(&dir) || dir.starts_with(&canonical) {
        return Err(OmniError::InvalidInput("choose a folder outside Omni's own data".into()));
    }
    if folder_of_another_profile(storage, &canonical)? {
        return Err(OmniError::InvalidInput("this folder holds the working files of another profile on this computer".into()));
    }
    let canonical = canonical.to_string_lossy().into_owned();
    let released = storage.get_workspace_file_local(group_id, repo, RELEASED_FOLDER)?;
    storage.set_workspace_file_local(group_id, repo, RELEASED_FOLDER, "")?;
    if record.work_path.as_deref() != Some(canonical.as_str()) && released.as_deref() != Some(canonical.as_str()) {
        let applied = versions::read_ref(&dir, Ref::Applied)?;
        versions::update_ref(&dir, Ref::Applied, None, applied.as_ref())?;
        versions::set_left_out(&dir, versions::Left::Held, &[])?;
        versions::forget_index(&dir);
    }
    record.work_path = Some(canonical);
    storage.upsert_workspace_repo(&record)?;
    Ok(record)
}

/// The folder last removed from the project («Убрать папку»): bound again, it still holds the version
/// applied to it, and what changed in it since is the person's own change.
const RELEASED_FOLDER: &str = "released_folder";

/// «Убрать папку с этого устройства»: the folder stops following the project and keeps its files as
/// they are (nothing in it is touched or deleted); the project stays here as metadata with what we
/// saved, the files on request.
pub(super) fn unbind_folder(db_path: &str, storage: &Storage, group_id: &str, repo: &str) -> OmniResult<()> {
    if !storage.workspace_repo_is_files(group_id, repo)? {
        return Err(OmniError::InvalidInput("only a files project folder can be removed this way".into()));
    }
    let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("workspace no longer exists".into()))?;
    let members = super::agents::members(storage, group_id)?;
    if super::custody::state(storage, &group, &members, repo)?.archive_members.contains(&group.my_member_pub) {
        return Err(OmniError::InvalidInput("this device keeps the project's history; give up keeping it first".into()));
    }
    // Not while a round of the folder's work is on: it might be writing into it.
    with_files_work(db_path, group_id, repo, || {
        let mut record = storage.get_workspace_repo(group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("repository was removed".into()))?;
        let Some(folder) = record.work_path.take() else { return Ok(()) };
        // The files stay as they are, writable: no one's take holds them for this folder any more.
        versions::set_marks(Path::new(&record.published_path), Path::new(&folder), &BTreeSet::new())?;
        storage.set_workspace_file_local(group_id, repo, RELEASED_FOLDER, &folder)?;
        storage.upsert_workspace_repo(&record)?;
        with_profile(db_path, |p| p.files.remove(&(group_id.to_string(), repo.to_string())));
        Ok(())
    })
}

/// A version in the project's shared line (what a revision or «навсегда» may name).
pub(super) fn require_accepted(storage: &Storage, group_id: &str, repo: &str, oid: &str) -> OmniResult<()> {
    let record = storage.get_workspace_repo(group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let dir = Path::new(&record.published_path);
    match versions::read_ref(dir, Ref::Shared)? {
        Some(shared) if versions::is_ancestor(dir, repo, oid, &shared.id)? => Ok(()),
        _ => Err(OmniError::InvalidInput("the version is not an accepted one".into())),
    }
}

/// The accepted versions, newest first, with the revisions and «навсегда» of `current`.
pub(super) fn history(record: &WorkspaceRepoRecord, current: &super::custody::CustodyState, kept: &Kept) -> OmniResult<Vec<super::custody::WorkspaceHistoryEntry>> {
    let dir = Path::new(&record.published_path);
    let Some(shared) = versions::read_ref(dir, Ref::Shared)? else { return Ok(Vec::new()) };
    let (chain, ids, _) = versions::chain(dir, &record.repo, &shared.id)?;
    let count = chain.len();
    Ok(chain
        .iter()
        .zip(ids)
        .enumerate()
        .map(|(i, (version, oid))| {
            let revision = current.revisions.get(&oid);
            super::custody::WorkspaceHistoryEntry {
                number: (count - i) as u32,
                subject: version.message.clone(),
                author: version.author_name.clone(),
                at_unix_ms: version.at_unix_ms,
                changed: version.changes.len(),
                revision: revision.map(|(label, _)| label.clone()),
                revision_note: revision.map(|(_, note)| note.clone()).unwrap_or_default(),
                revision_pending: current.revision_pending(&oid),
                revision_mine: false,
                kept: current.kept.contains(&oid),
                current: i == 0,
                thinned: kept.thinned.contains(&oid),
                partial: kept.partial.contains(&oid),
                oid,
            }
        })
        .collect())
}

/// «Восстановить»: a new version on our line whose files are those of the accepted version `oid`.
/// Nothing is overwritten: it goes through acceptance as any save, and the folder follows it.
pub(super) fn restore_version(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, oid: &str) -> OmniResult<()> {
    require_accepted(storage, &group.group_id, repo, oid)?;
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let kept = history_kept(storage, group, members, &record)?;
    if kept.thinned.contains(oid) {
        return Err(OmniError::InvalidInput("the version's content was thinned out".into()));
    }
    let gone: HashSet<Content> = kept.dropped.iter().cloned().collect();
    let dir = Path::new(&record.published_path);
    let me = my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
    let name = members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.display_name.clone()).unwrap_or_default();
    let restored = with_files_work(db_path, &group.group_id, repo, || {
        let shared = versions::read_ref(dir, Ref::Shared)?.ok_or_else(|| OmniError::InvalidInput("the project has no accepted version yet".into()))?;
        let (chain, ids, _) = versions::chain(dir, repo, &shared.id)?;
        let number = chain.len() - ids.iter().position(|id| id == oid).unwrap_or(0);
        let subject = versions::version(dir, repo, oid)?.map(|v| v.message.clone()).unwrap_or_default();
        versions::restore(dir, repo, oid, &gone, (&me, &name), &format!("Восстановлена в.{number} «{subject}»"), current_unix_ms())
    })?;
    let Some(restored) = restored else { return Err(OmniError::InvalidInput("the current version already has these files".into())) };
    omni_app_trace_log("workspace/restore", &format!("group={} repo={repo} {oid} -> {}", group.group_id, restored.id));
    super::custody::endorse_own(storage, group, members, repo, &restored.id)?;
    advance(storage, group, members, &record)?;
    announce_tip(storage, group, members, &record)
}

/// «Вернуть к общей»: our line that waits for review or was rejected goes back to the shared version —
/// its versions leave our line, and the folder follows the shared one next round (a file changed there
/// since the last save holds it back, as any update does). An accepted line of ours is not withdrawn.
pub(super) fn back_to_shared(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str) -> OmniResult<()> {
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let dir = Path::new(&record.published_path);
    let custody = super::custody::state(storage, group, members, repo)?;
    let withdrawn = with_files_work(db_path, &group.group_id, repo, || {
        let shared = versions::read_ref(dir, Ref::Shared)?.ok_or_else(|| OmniError::InvalidInput("the project has no accepted version yet".into()))?;
        let Some(mine) = own_ahead(dir, repo, &shared.id)? else { return Err(OmniError::InvalidInput("our line holds nothing the shared version does not".into())) };
        if custody.allows(repo, &mine.id) {
            return Err(OmniError::InvalidInput("our line is accepted; it joins the shared version".into()));
        }
        // Only over our line as read here: a save finishing meanwhile is not lost.
        if !versions::update_ref(dir, Ref::Mine, Some(&shared), Some(&mine))? {
            return Err(OmniError::Transport("the folder was updated meanwhile; try again".into()));
        }
        Ok(mine.id)
    })?;
    omni_app_trace_log("workspace/back", &format!("group={} repo={repo} {withdrawn} -> shared", group.group_id));
    advance(storage, group, members, &record)?;
    announce_tip(storage, group, members, &record)
}

/// «Выбрать итоговую»: of `base` and its conflict copies, `keep` stays as `base`; the copies go. A
/// new version on our line, accepted as any save; the folders follow it.
pub(super) fn choose_final(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, base: &str, keep: &str) -> OmniResult<()> {
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let dir = Path::new(&record.published_path);
    let me = my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
    let name = members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.display_name.clone()).unwrap_or_default();
    let file = base.rsplit('/').next().unwrap_or(base);
    let message = match versions::conflict_base(keep) {
        Some((_, label)) => format!("Итоговая «{file}» — копия ({label})"),
        None => format!("Итоговая «{file}» — как была, копии убраны"),
    };
    let chosen = with_files_work(db_path, &group.group_id, repo, || versions::choose_final(dir, repo, base, keep, (&me, &name), &message, current_unix_ms()))?;
    omni_app_trace_log("workspace/choose", &format!("group={} repo={repo} {base} <- {keep} -> {}", group.group_id, chosen.id));
    super::custody::endorse_own(storage, group, members, repo, &chosen.id)?;
    advance(storage, group, members, &record)?;
    announce_tip(storage, group, members, &record)
}

/// «На этом устройстве»: the folders of the project this device leaves out of its working folder
/// (docs/decisions/workspace-files-storage.md, 4a). The folder follows on the next round: a folder
/// left out goes from the disk unless something in it is changed, one taken back comes.
pub(super) fn set_left_out(record: &WorkspaceRepoRecord, folders: Vec<String>) -> OmniResult<()> {
    let mut folders: Vec<String> = folders.into_iter().map(|f| f.trim().trim_matches('/').to_string()).filter(|f| !f.is_empty()).collect();
    for folder in &folders {
        omni_protocol::validate_workspace_file_path(folder)?;
    }
    folders.sort();
    folders.dedup();
    // A folder inside another one left out says nothing more.
    let all = folders.clone();
    folders.retain(|f| !versions::is_left_out(&all, f));
    versions::set_left_out(Path::new(&record.published_path), versions::Left::Wanted, &folders)
}

/// The folders this device leaves out, and the project's folders to choose from: the shared
/// version's top folders with their size.
pub(super) fn left_out_view(record: &WorkspaceRepoRecord) -> OmniResult<(Vec<String>, Vec<(String, u64)>)> {
    let dir = Path::new(&record.published_path);
    let mut folders: BTreeMap<String, u64> = BTreeMap::new();
    if let Some(shared) = versions::read_ref(dir, Ref::Shared)? {
        for (path, content) in versions::tree(dir, &record.repo, &shared.id)?.iter() {
            if let Some((top, _)) = path.split_once('/') {
                *folders.entry(top.to_string()).or_default() += content.size;
            }
        }
    }
    Ok((versions::left_out(dir, versions::Left::Wanted)?, folders.into_iter().collect()))
}

/// A member's name for the screen and for errors.
fn name_of<'a>(members: &'a [GroupMemberRecord], account: &'a str) -> &'a str {
    members.iter().find(|m| m.account_id == account).map_or(account, |m| m.display_name.as_str())
}

/// «Беру в работу» (again: «Продлить»): our account takes `paths` for a day. Each is a file of the
/// shared version that no other account holds, and the folder holds its latest version — one never
/// takes an older file to edit (docs/decisions/omni-workspace-git.md, section 6).
pub(super) fn take_files(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, paths: &[String], now: i64) -> OmniResult<()> {
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let work = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()).ok_or_else(|| OmniError::InvalidInput("take the project into a folder first".into()))?;
    let dir = Path::new(&record.published_path);
    let me = my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
    let shared = versions::read_ref(dir, Ref::Shared)?.ok_or_else(|| OmniError::InvalidInput("the project has no accepted version yet".into()))?;
    let tree = versions::tree(dir, repo, &shared.id)?;
    let applied = versions::read_ref(dir, Ref::Applied)?;
    let folder_latest = match &applied {
        Some(applied) => versions::is_ancestor(dir, repo, &shared.id, &applied.id)?,
        None => false,
    };
    let custody = super::custody::state(storage, group, members, repo)?;
    if paths.is_empty() {
        return Err(OmniError::InvalidInput("choose the files to take".into()));
    }
    for path in paths {
        let content = tree.get(path).ok_or_else(|| OmniError::InvalidInput(format!("{path}: not in the shared version")))?;
        if let Some(holder) = custody.holder(path, now).filter(|h| h.account != me) {
            return Err(OmniError::InvalidInput(format!("{path}: taken by {} until {}", name_of(members, &holder.account), holder.until)));
        }
        if !folder_latest && !versions::holds(dir, work, path, content) {
            return Err(OmniError::InvalidInput(format!("{path}: the folder does not hold its latest version yet")));
        }
    }
    for path in paths {
        // An extension keeps when the hold began: it stays ahead of a later take by someone else.
        let since = custody.takes.get(path).and_then(|t| t.iter().find(|t| t.account == me && t.until > now)).map_or(now, |t| t.since);
        super::custody::publish(storage, group, members, Event::Lock {
            repo: repo.to_string(),
            path: path.clone(),
            take: true,
            since_unix_ms: since,
            until_unix_ms: now + omni_protocol::WORKSPACE_LOCK_DEFAULT_MS,
            holder: None,
            at_unix_ms: now,
        })?;
    }
    mark_folder(storage, group, members, &record)
}

/// «Отдать»: our account gives back its takes of `paths`; «Снять замок» (an admin): `holder`'s.
pub(super) fn give_files(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, paths: &[String], holder: Option<&str>, now: i64) -> OmniResult<()> {
    let me = my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
    if holder.is_some() && !is_admin(members, &group.my_member_pub) {
        return Err(OmniError::InvalidInput("only an admin gives back someone else's file".into()));
    }
    let account = holder.unwrap_or(&me);
    let custody = super::custody::state(storage, group, members, repo)?;
    for path in paths {
        if !custody.takes.get(path).is_some_and(|t| t.iter().any(|t| t.account == account && t.until > now)) {
            return Err(OmniError::InvalidInput(format!("{path}: not taken")));
        }
        super::custody::publish(storage, group, members, Event::Lock {
            repo: repo.to_string(),
            path: path.clone(),
            take: false,
            since_unix_ms: now,
            until_unix_ms: 0,
            holder: holder.map(str::to_string),
            at_unix_ms: now,
        })?;
    }
    if let Some(record) = storage.get_workspace_repo(&group.group_id, repo)? {
        mark_folder(storage, group, members, &record)?;
    }
    Ok(())
}

/// The folder's read-only marks: files another account holds and, under «править, только взяв в
/// работу», every file of the version our account does not hold. CAD programs honour them; without a
/// server it is an agreement, not a guarantee.
pub(super) fn mark_folder(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<()> {
    let Some(work) = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()) else { return Ok(()) };
    let dir = Path::new(&record.published_path);
    let custody = super::custody::state(storage, group, members, &record.repo)?;
    let me = my_account(members, group);
    let now = current_unix_ms();
    let mine: HashSet<String> = me.as_deref().map(|me| custody.taken_by(me, now).into_iter().filter(|(_, holds)| *holds).map(|(path, _)| path).collect()).unwrap_or_default();
    let wanted: BTreeSet<String> = versions::held_tree(dir, &record.repo)?
        .into_keys()
        .filter(|path| {
            let other = custody.holder(path, now).is_some_and(|h| me.as_deref() != Some(h.account.as_str()));
            other || (custody.policy.edit_only_taken && !mine.contains(path))
        })
        .collect();
    versions::set_marks(dir, work, &wanted)
}

/// «Отложить мою правку рядом»: our change of a file someone else took first is copied beside it
/// (`корпус (Петя, 30.10 10-42).prt`, saved by the next «Сохранить версию») and the file goes back to
/// the version the folder holds — fetched like a download, so the folder can follow the holder's.
pub(super) fn set_aside(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, path: &str, now: i64) -> OmniResult<String> {
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let work = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()).ok_or_else(|| OmniError::InvalidInput("the repository has no folder here".into()))?;
    let dir = Path::new(&record.published_path);
    let applied = versions::read_ref(dir, Ref::Applied)?.ok_or_else(|| OmniError::InvalidInput("the folder holds no version yet".into()))?;
    let content = versions::held_tree(dir, repo)?.get(path).cloned().ok_or_else(|| OmniError::InvalidInput(format!("{path}: not in the folder's version")))?;
    let file = work.join(path);
    if !file.is_file() || versions::holds(dir, work, path, &content) {
        return Err(OmniError::InvalidInput(format!("{path}: nothing changed here")));
    }
    if versions::in_use(&file) {
        return Err(OmniError::InvalidInput(format!("{path}: the file is open in a program — close it and save again")));
    }
    let name = members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.display_name.clone()).unwrap_or_default();
    // A name no file of the folder or its version has (a second set-aside the same minute too).
    let held = versions::held_tree(dir, repo)?;
    let aside = versions::free_conflict_name(path, &name, now, |aside| {
        let lower = aside.to_lowercase();
        std::fs::symlink_metadata(work.join(aside)).is_ok() || held.keys().any(|p| p.to_lowercase() == lower)
    });
    let target = work.join(&aside);
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(|e| OmniError::Storage(format!("{}: {e}", target.display())))?;
    let copied = std::fs::File::open(&file).and_then(|mut input| std::io::copy(&mut input, &mut out)).and_then(|_| out.sync_all());
    drop(out);
    if let Err(error) = copied {
        let _ = std::fs::remove_file(&target);
        return Err(OmniError::Storage(format!("{}: {error}", target.display())));
    }
    versions::set_read_only(&target, false);
    super::custody::request_download(storage, &group.group_id, repo, &applied.id, path, &file.to_string_lossy())?;
    omni_app_trace_log("workspace/aside", &format!("group={} repo={repo} {path} -> {aside}", group.group_id));
    Ok(aside)
}

/// The accepted versions that changed `path`, newest first (в.N of the project).
pub(super) fn file_history(record: &WorkspaceRepoRecord, current: &super::custody::CustodyState, path: &str) -> OmniResult<Vec<super::custody::WorkspaceFileVersionView>> {
    let dir = Path::new(&record.published_path);
    let Some(shared) = versions::read_ref(dir, Ref::Shared)? else { return Ok(Vec::new()) };
    let (chain, ids, _) = versions::chain(dir, &record.repo, &shared.id)?;
    let count = chain.len();
    let mut entries = Vec::new();
    for (i, (version, oid)) in chain.iter().zip(ids).enumerate() {
        let Some(change) = version.changes.iter().find(|c| c.path == path) else { continue };
        entries.push(super::custody::WorkspaceFileVersionView {
            number: (count - i) as u32,
            subject: version.message.clone(),
            author: version.author_name.clone(),
            at_unix_ms: version.at_unix_ms,
            bytes: change.content.as_ref().map(|c| c.size),
            revision: current.revisions.get(&oid).map(|(label, _)| label.clone()),
            revision_pending: current.revision_pending(&oid),
            oid,
        });
    }
    Ok(entries)
}

/// The most a file may hold to be fetched for a preview (pictures and documents; a model is opened in its program).
const PREVIEW_MAX_BYTES: u64 = 64 << 20;
/// How many fetched previews a project keeps (the oldest go first).
const PREVIEWS_KEPT: usize = 32;
/// The local record of the previews asked for and not here yet.
const PREVIEWS_WANTED: &str = "previews";

fn previews_dir(dir: &Path) -> PathBuf {
    dir.join("previews")
}

/// What to show for `path` of `version`: the file holding its content here (the folder, the store,
/// a preview fetched before), or — asked for now — fetched by the next rounds.
pub(super) fn preview(storage: &Storage, record: &WorkspaceRepoRecord, path: &str, version: &str) -> OmniResult<super::custody::WorkspacePreviewView> {
    content_here_or_asked(storage, record, path, version, Some(PREVIEW_MAX_BYTES))
}

/// An STL model of `version` drawn turned by (yaw, pitch) at (width, height): its content comes as
/// for a preview (here, or fetched by the next rounds), within the preview's limit.
pub(super) fn model_view(storage: &Storage, record: &WorkspaceRepoRecord, path: &str, version: &str, turn: (f32, f32), size: (u32, u32)) -> OmniResult<super::custody::WorkspaceModelView> {
    use base64::Engine as _;
    if !path.to_lowercase().ends_with(".stl") {
        return Err(OmniError::InvalidInput("only STL models are drawn here".into()));
    }
    let here = content_here_or_asked(storage, record, path, version, Some(PREVIEW_MAX_BYTES))?;
    let Some(file) = here.file else { return Ok(super::custody::WorkspaceModelView { png_base64: None, pending: here.pending, triangles: 0 }) };
    let bytes = std::fs::read(&file).map_err(|e| OmniError::Storage(format!("{file}: {e}")))?;
    let triangles = super::model_view::parse_stl(&bytes)?;
    let (width, height) = (if size.0 == 0 { 480 } else { size.0 }, if size.1 == 0 { 360 } else { size.1 });
    let png = super::model_view::render_png(&triangles, width, height, turn.0, turn.1)?;
    Ok(super::custody::WorkspaceModelView { png_base64: Some(base64::engine::general_purpose::STANDARD.encode(png)), pending: false, triangles: triangles.len() })
}

/// How many exported copies a project keeps (the oldest go first).
const EXPORTS_KEPT: usize = 16;

/// `path` of `version` as a file with its own name, to drag out of Omni or open in its program: the
/// working folder's own file while it holds that content (a program opens the real file), else a
/// read-only copy in the project's `exports` — a copy, as a link would let an edit change the store.
/// Not here yet: asked for like a preview.
pub(super) fn export(storage: &Storage, record: &WorkspaceRepoRecord, path: &str, version: &str) -> OmniResult<super::custody::WorkspacePreviewView> {
    let mut view = content_here_or_asked(storage, record, path, version, None)?;
    let Some(source) = view.file.clone().map(PathBuf::from) else { return Ok(view) };
    let dir = Path::new(&record.published_path);
    let content = versions::tree(dir, &record.repo, version)?.get(path).cloned().ok_or_else(|| OmniError::InvalidInput("file is not in the version".into()))?;
    if let Some(work) = record.work_path.as_deref().map(Path::new).filter(|work| versions::holds(dir, work, path, &content)) {
        // The system's separators: Explorer and StorageFile take no mixed ones.
        view.file = Some(work.join(path.replace('/', std::path::MAIN_SEPARATOR_STR)).to_string_lossy().into_owned());
        return Ok(view);
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let exports = dir.join("exports");
    let target = exports.join(&content.id[..16]).join(name);
    if target.metadata().map(|m| m.len()).ok() != Some(content.size) {
        let folder = target.parent().expect("an export's folder");
        std::fs::create_dir_all(folder).map_err(|e| OmniError::Storage(format!("{}: {e}", folder.display())))?;
        let temp = folder.join(format!(".omni-export-{}", hex::encode(&omni_crypto::generate_group_key()[..8])));
        let copied = std::fs::copy(&source, &temp).and_then(|_| {
            versions::set_read_only(&target, false);
            std::fs::rename(&temp, &target)
        });
        if let Err(error) = copied {
            let _ = std::fs::remove_file(&temp);
            return Err(OmniError::Storage(format!("{}: {error}", target.display())));
        }
        versions::set_read_only(&target, true);
        prune_exports(&exports);
    }
    view.file = Some(target.to_string_lossy().into_owned());
    Ok(view)
}

/// Keep the newest [`EXPORTS_KEPT`] exported copies (a program may still hold an older one open:
/// then it stays until the next time).
fn prune_exports(exports: &Path) {
    let mut kept: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(exports)
        .map(|entries| entries.flatten().filter(|e| e.path().is_dir()).map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path())).collect())
        .unwrap_or_default();
    kept.sort_by_key(|(at, _)| Reverse(*at));
    for (_, old) in kept.into_iter().skip(EXPORTS_KEPT) {
        for file in std::fs::read_dir(&old).into_iter().flatten().flatten() {
            versions::set_read_only(&file.path(), false);
        }
        let _ = std::fs::remove_dir_all(old);
    }
}

/// The file holding `path` of `version` here, or the content asked for (at most `limit` bytes).
fn content_here_or_asked(storage: &Storage, record: &WorkspaceRepoRecord, path: &str, version: &str, limit: Option<u64>) -> OmniResult<super::custody::WorkspacePreviewView> {
    let dir = Path::new(&record.published_path);
    let content = versions::tree(dir, &record.repo, version)?.get(path).cloned().ok_or_else(|| OmniError::InvalidInput("file is not in the version".into()))?;
    let mut view = super::custody::WorkspacePreviewView { path: path.to_string(), oid: version.to_string(), bytes: content.size, file: None, pending: false };
    let cached = previews_dir(dir).join(&content.id);
    if let Some(file) = versions::content_file(dir, record.work_path.as_deref().map(Path::new), &content.id, content.size)
        .or_else(|| cached.metadata().is_ok_and(|m| m.len() == content.size).then_some(cached))
    {
        view.file = Some(file.to_string_lossy().into_owned());
        return Ok(view);
    }
    if limit.is_some_and(|limit| content.size > limit) {
        return Err(OmniError::InvalidInput("the file is too big to preview; download it".into()));
    }
    let mut wanted: Vec<Content> = storage.get_workspace_file_local(&record.group_id, &record.repo, PREVIEWS_WANTED)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if !wanted.contains(&content) {
        wanted.push(content);
        let json = serde_json::to_string(&wanted).map_err(|e| OmniError::Internal(e.to_string()))?;
        storage.set_workspace_file_local(&record.group_id, &record.repo, PREVIEWS_WANTED, &json)?;
    }
    view.pending = true;
    Ok(view)
}

/// Fetch the previews asked for into the project's preview cache, keeping the newest few.
pub(crate) fn process_previews(storage: &Storage, record: &WorkspaceRepoRecord, nodes: &[MeshNodeAddress], open: &super::content::OpenStream) -> OmniResult<()> {
    let Some(wanted) = storage.get_workspace_file_local(&record.group_id, &record.repo, PREVIEWS_WANTED)?.filter(|s| !s.is_empty()) else { return Ok(()) };
    let wanted: Vec<Content> = serde_json::from_str(&wanted).unwrap_or_default();
    let dir = Path::new(&record.published_path);
    let cache = previews_dir(dir);
    for content in &wanted {
        let target = cache.join(&content.id);
        if let Err(error) = super::content::fetch_content(&target, &cache.join("tmp"), &record.group_id, &record.repo, &content.id, content.size, &[], nodes, open) {
            omni_app_trace_log("workspace/preview", &format!("group={} repo={} {}: {error}", record.group_id, record.repo, content.id));
        }
    }
    // Asked again by the screen while it waits; what came is dropped from the list.
    let left: Vec<&Content> = wanted.iter().filter(|c| !cache.join(&c.id).is_file()).collect();
    let json = if left.is_empty() { String::new() } else { serde_json::to_string(&left).map_err(|e| OmniError::Internal(e.to_string()))? };
    storage.set_workspace_file_local(&record.group_id, &record.repo, PREVIEWS_WANTED, &json)?;
    let mut kept: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&cache)
        .map(|entries| entries.flatten().filter(|e| e.path().is_file()).map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path())).collect())
        .unwrap_or_default();
    kept.sort_by_key(|(at, _)| Reverse(*at));
    for (_, old) in kept.into_iter().skip(PREVIEWS_KEPT) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

/// Whether the person removed the project's folder from this device and has not bound one since.
pub(super) fn folder_released(storage: &Storage, group_id: &str, repo: &str) -> OmniResult<bool> {
    Ok(storage.get_workspace_file_local(group_id, repo, RELEASED_FOLDER)?.is_some_and(|f| !f.is_empty()))
}

impl OmniApp {
    /// Add a project of files to the workspace with `folder` as its working folder. Its first version
    /// is saved by [`OmniApp::workspace_save_version`].
    pub fn workspace_add_files_repo(&self, group_id: &str, repo: &str, folder: &str) -> OmniResult<WorkspaceRepoRecord> {
        let _ = self.ensure_runtime_profile_active("workspace_add_files_repo")?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        if group.my_status != "member" {
            return Err(OmniError::InvalidInput("not an active member of the group".into()));
        }
        bind_folder(&self.storage, group_id, repo, folder)
    }

    /// «Сохранить версию»: everything changed in the folder of `repo` as one version with `message`,
    /// announced at once. `None` when nothing changed.
    pub fn workspace_save_version(&self, group_id: &str, repo: &str, message: &str) -> OmniResult<Option<String>> {
        let Some(id) = self.workspace_save_version_prepare(group_id, repo, message)?.save()? else { return Ok(None) };
        self.workspace_save_version_publish(group_id, repo, &id)?;
        Ok(Some(id))
    }

    /// The database half of «Сохранить версию» before the folder is read: what to save where, by whom.
    /// The FFI takes it under its serializer and runs [`WorkspaceSaveVersion::save`] without it — reading
    /// a folder of models is seconds, and every other call of the shell would wait for it.
    pub fn workspace_save_version_prepare(&self, group_id: &str, repo: &str, message: &str) -> OmniResult<WorkspaceSaveVersion> {
        let _ = self.ensure_runtime_profile_active("workspace_save_version_prepare")?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let members = super::agents::members(&self.storage, group_id)?;
        if !self.storage.workspace_repo_is_files(group_id, repo)? {
            return Err(OmniError::InvalidInput("the repository holds code: versions are saved in git".into()));
        }
        let record = self
            .storage
            .get_workspace_repo(group_id, repo)?
            .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
        let work = PathBuf::from(record.work_path.clone().ok_or_else(|| OmniError::InvalidInput("the repository has no folder here".into()))?);
        let me = my_account(&members, &group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
        let my_name = members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.display_name.clone()).unwrap_or_default();
        Ok(WorkspaceSaveVersion {
            db_path: self.storage.db_path().to_string_lossy().into_owned(),
            key: (group_id.to_string(), repo.to_string(), FILES_WORK.to_string()),
            dir: PathBuf::from(record.published_path),
            work,
            me,
            my_name,
            message: message.to_string(),
        })
    }

    /// The database half after the folder: the saved version goes out to the group.
    pub fn workspace_save_version_publish(&self, group_id: &str, repo: &str, id: &str) -> OmniResult<()> {
        omni_app_trace_log("workspace/save", &format!("group={group_id} repo={repo} version={id}"));
        self.flush_group_feed();
        Ok(())
    }
}

/// «Сохранить версию» between its database halves.
pub struct WorkspaceSaveVersion {
    db_path: String,
    key: FetchKey,
    dir: PathBuf,
    work: PathBuf,
    me: String,
    my_name: String,
    message: String,
}

impl WorkspaceSaveVersion {
    /// Everything changed in the folder as one version, once the background work on the folder is
    /// done; accepted by our role of the moment, the shared line moved and our tip announced — on a
    /// database connection of its own, as the screen's render does. `None` when nothing changed.
    pub fn save(self) -> OmniResult<Option<String>> {
        let started = Instant::now();
        let (group_id, repo) = (&self.key.0, &self.key.1);
        let message = if self.message.trim().is_empty() { DEFAULT_MESSAGE } else { self.message.trim() };
        let storage = Storage::open(&self.db_path)?;
        let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let members = super::agents::members(&storage, group_id)?;
        let custody = super::custody::state(&storage, &group, &members, repo)?;
        let saved = with_files_work(&self.db_path, group_id, repo, || {
            match versions::save(&self.dir, repo, &self.work, (&self.me, &self.my_name), message, current_unix_ms())? {
                Some(saved) => onto_shared(&self.dir, repo, saved, &|id| custody.rejected(repo, id)),
                None => Ok(None),
            }
        });
        omni_app_trace_log("workspace/save", &format!("folder {} ms", started.elapsed().as_millis()));
        let Some(saved) = saved? else { return Ok(None) };
        let record = storage
            .get_workspace_repo(group_id, repo)?
            .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
        // Accepted by the roles of the moment it is made: an admin's or custodian's save goes in.
        super::custody::endorse_own(&storage, &group, &members, repo, &saved.id)?;
        advance(&storage, &group, &members, &record)?;
        announce_tip(&storage, &group, &members, &record)?;
        // «#12» in the comment: the task hears of it (a failure there never undoes the save).
        if let Err(error) = super::tasks::mentioned(&self.db_path, &storage, &group, &members, repo, &saved.id, message) {
            omni_app_trace_log("workspace/tasks", &format!("group={group_id} repo={repo} mention: {error}"));
        }
        Ok(Some(saved.id))
    }
}

/// A save goes onto the shared line when its own line does not lead there — the folder held an older
/// version, or our line carries a version turned down: our versions after the shared one again on
/// top of it, the turned-down ones left out ([`versions::replay`]). Left as it is, such a line would
/// compete with the shared one by rank ([`next_files_main`]) and could win: the turned-down version
/// back in, the accepted ones after the fork laid again with new ids (and their revisions lost).
/// `None` when nothing is left that the shared version does not have.
fn onto_shared(dir: &Path, repo: &str, saved: Content, rejected: &dyn Fn(&str) -> bool) -> OmniResult<Option<Content>> {
    let Some(shared) = versions::read_ref(dir, Ref::Shared)? else { return Ok(Some(saved)) };
    let (_, accepted, _) = versions::chain(dir, repo, &shared.id)?;
    let accepted: HashSet<String> = accepted.into_iter().collect();
    let (_, ids, _) = versions::chain(dir, repo, &saved.id)?;
    let ours = ids.iter().take_while(|id| !accepted.contains(*id)).count();
    if ids.get(ours) == Some(&shared.id) && !ids[..ours].iter().any(|id| rejected(id)) {
        return Ok(Some(saved));
    }
    let replayed = versions::replay(dir, repo, &shared, &saved.id, rejected)?;
    // Only over the line just saved: the folder keeps the version it holds and follows the new tip.
    if !versions::update_ref(dir, Ref::Mine, Some(&replayed), Some(&saved))? {
        return Err(OmniError::Transport("the folder was updated meanwhile; save again".into()));
    }
    omni_app_trace_log("workspace/save", &format!("repo={repo} onto the shared line: {} -> {}", saved.id, replayed.id));
    Ok((replayed != shared).then_some(replayed))
}

/// «Сохранить версию» from a command that has the profile's database at hand (the tasks' changes).
pub(super) fn save_now(db_path: &str, storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str, message: &str) -> OmniResult<Option<String>> {
    let record = storage.get_workspace_repo(&group.group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
    let work = PathBuf::from(record.work_path.clone().ok_or_else(|| OmniError::InvalidInput("the repository has no folder here".into()))?);
    WorkspaceSaveVersion {
        db_path: db_path.to_string(),
        key: (group.group_id.clone(), repo.to_string(), FILES_WORK.to_string()),
        dir: PathBuf::from(record.published_path),
        work,
        me: my_account(members, group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?,
        my_name: members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.display_name.clone()).unwrap_or_default(),
        message: message.to_string(),
    }
    .save()
}

/// `work` while nothing else moves the project's refs: the folder's background round, a save and a
/// review's acceptance take turns.
pub(super) fn with_files_work<T>(db_path: &str, group_id: &str, repo: &str, work: impl FnOnce() -> OmniResult<T>) -> OmniResult<T> {
    let key: FetchKey = (group_id.to_string(), repo.to_string(), FILES_WORK.to_string());
    let deadline = Instant::now() + SAVE_WAITS_FOR_SYNC;
    while with_profile(db_path, |p| p.fetching.insert(key.clone())) != Some(true) {
        if Instant::now() >= deadline {
            return Err(OmniError::Transport("the folder is being updated; try again".into()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let outcome = work();
    with_profile(db_path, |p| p.fetching.remove(&key));
    outcome
}

/// Another device's line in a project, as its last `Tip` says.
#[derive(Debug, Clone)]
pub(super) struct Tip {
    pub account: String,
    pub member: String,
    pub version: Content,
}

/// The lines of the project's other devices that are members now.
pub(super) fn tips(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], repo: &str) -> OmniResult<Vec<Tip>> {
    let mut tips = Vec::new();
    for record in storage.list_workspace_file_events(&group.group_id)? {
        if record.sender_member == group.my_member_pub {
            continue;
        }
        let Ok(Event::Tip { repo: of, version, .. }) = serde_json::from_str::<Event>(&record.event_json) else { continue };
        let member = members.iter().any(|m| m.status == "member" && m.member_pub == record.sender_member && m.account_id == record.sender_account);
        if of == repo && member {
            tips.push(Tip { account: record.sender_account, member: record.sender_member, version });
        }
    }
    Ok(tips)
}

/// Tell the group our line's tip when it is not what we last told.
pub(super) fn announce_tip(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<()> {
    let Some(mine) = versions::read_ref(Path::new(&record.published_path), Ref::Mine)? else { return Ok(()) };
    let told = storage.list_workspace_file_events(&group.group_id)?.into_iter().any(|e| {
        e.sender_member == group.my_member_pub
            && matches!(serde_json::from_str::<Event>(&e.event_json), Ok(Event::Tip { repo, version, .. }) if repo == record.repo && version == mine)
    });
    if told {
        return Ok(());
    }
    omni_app_trace_log("workspace/announce", &format!("group={} repo={} tip={}", group.group_id, record.repo, mine.id));
    super::custody::publish(storage, group, members, Event::Tip { repo: record.repo.clone(), version: mine, at_unix_ms: current_unix_ms() })
}

/// Where the shared line of a files project goes: forward like a code one, and when lines went
/// different ways, the line of the higher [`LineRank`] — even back from the one held now, so every node
/// ends on the same line whatever order the saves reached it in. The lost lines' own nodes replay them.
pub(super) fn next_files_main(
    shared: Option<&str>,
    candidates: &[(String, String)],
    ancestor: impl Fn(&str, &str) -> OmniResult<bool>,
    rank: impl Fn(&str, &str) -> OmniResult<LineRank>,
) -> OmniResult<WorkspaceMainState> {
    let any = |_: &(String, String)| true;
    let mut state = next_shared_main(shared, candidates, any, &ancestor)?;
    // Each step takes a line that beats the one held; a few lines at most, the bound only guards a cycle.
    for _ in 0..=candidates.len() {
        let Some(at) = state.oid.clone() else { break };
        let mut best: Option<(LineRank, String)> = None;
        for (_, fork) in &state.forks {
            let fork_rank = rank(&at, fork)?;
            if best.as_ref().is_none_or(|(r, _)| fork_rank > *r) {
                best = Some((fork_rank, fork.clone()));
            }
        }
        let Some((fork_rank, fork)) = best else { break };
        if rank(&fork, &at)? >= fork_rank {
            break;
        }
        state = next_shared_main(Some(&fork), candidates, any, &ancestor)?;
    }
    Ok(state)
}

/// Move the shared line of the project where the rules allow, and say where it is: the candidates
/// are our line and the other devices' lines whose versions are all here; accepted ones settle by
/// [`next_files_main`], the rest wait for review.
pub(super) fn advance(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<WorkspaceMainState> {
    let dir = Path::new(&record.published_path);
    let repo = record.repo.as_str();
    let mut sizes: HashMap<String, Content> = HashMap::new();
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let (Some(me), Some(mine)) = (my_account(members, group), versions::read_ref(dir, Ref::Mine)?) {
        candidates.push((me, mine.id.clone()));
        sizes.insert(mine.id.clone(), mine);
    }
    for tip in tips(storage, group, members, repo)? {
        if versions::first_missing(dir, repo, &tip.version)?.is_none() {
            candidates.push((tip.account, tip.version.id.clone()));
            sizes.insert(tip.version.id.clone(), tip.version);
        }
    }
    candidates.sort();
    candidates.dedup();
    let custody = super::custody::state(storage, group, members, repo)?;
    let before = versions::read_ref(dir, Ref::Shared)?;
    let (ready, waiting): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|(_, id)| custody.allows(repo, id));
    let ancestor = |a: &str, b: &str| versions::is_ancestor(dir, repo, a, b);
    let rank = |other: &str, tip: &str| versions::line_rank(dir, repo, other, tip);
    let mut state = next_files_main(before.as_ref().map(|b| b.id.as_str()), &ready, ancestor, rank)?;
    let (rejected, waiting): (Vec<_>, Vec<_>) = waiting.into_iter().filter(|(_, id)| before.as_ref().map(|b| b.id.as_str()) != Some(id.as_str())).partition(|(_, id)| custody.rejected(repo, id));
    state.waiting = waiting;
    state.rejected = rejected;
    state.needs_approval = custody.policy.participants_need_approval;
    if let Some(tip) = state.oid.as_deref().filter(|tip| before.as_ref().map(|b| b.id.as_str()) != Some(*tip)) {
        let content = sizes.get(tip).cloned().ok_or_else(|| OmniError::Internal("the shared line moved to an unknown version".into()))?;
        if versions::update_ref(dir, Ref::Shared, Some(&content), before.as_ref())? {
            omni_app_trace_log("workspace/main", &format!("group={} repo={repo} {} -> {tip}", record.group_id, before.as_ref().map_or("none", |b| b.id.as_str())));
            // A line for each version the shared line took since `before` — two saved one after the
            // other (a task's field and a comment on it) are two lines, not just the last — oldest
            // first, not about our own saves.
            let me = my_account(members, group);
            let mut taken = Vec::new();
            let mut at = Some(tip.to_string());
            while let Some(id) = at.filter(|id| before.as_ref().map(|b| b.id.as_str()) != Some(id.as_str()) && taken.len() < FEED_VERSIONS_NAMED) {
                let Some(version) = versions::version(dir, repo, &id)? else { break };
                at = version.parent.as_ref().map(|p| p.id.clone());
                taken.push(version);
            }
            for version in taken.iter().rev().filter(|v| me.as_deref() != Some(v.author_account.as_str())) {
                let line = if repo == super::tasks::TASKS_REPO {
                    format!("{} · задачи: {}", version.author_name, version.message)
                } else {
                    format!("{} · {repo}: новая версия — «{}»", version.author_name, version.message)
                };
                super::say_in_feed(storage, &group.group_id, None, Some(repo), &line, current_unix_ms())?;
            }
            // Taken in by the rule "at once": a reviewer's node records it, so a later rule keeps it.
            super::custody::endorse_own(storage, group, members, repo, tip)?;
        }
    }
    let db_path = storage.db_path().to_string_lossy().into_owned();
    with_profile(&db_path, |p| p.main_state.insert((record.group_id.clone(), record.repo.clone()), state.clone()));
    Ok(state)
}

/// Fetch the versions of the lines in `wanted` not here yet, from the members' nodes (the device that
/// announced a line first), until `until`.
fn fetch_versions(
    record: &WorkspaceRepoRecord,
    wanted: &[(Content, Option<MeshNodeAddress>)],
    nodes: &[MeshNodeAddress],
    open: &super::content::OpenStream,
    until: Instant,
) -> OmniResult<()> {
    let dir = Path::new(&record.published_path);
    for (tip, announcer) in wanted {
        let mut order: Vec<MeshNodeAddress> = announcer.iter().copied().filter(|a| nodes.contains(a)).collect();
        order.extend(nodes.iter().filter(|n| Some(**n) != *announcer));
        while let Some(missing) = versions::first_missing(dir, &record.repo, tip)? {
            if Instant::now() >= until {
                return Ok(());
            }
            super::content::fetch_object(
                &versions::version_path(dir, &missing.id),
                &versions::versions_temp_dir(dir),
                &record.group_id,
                &record.repo,
                &missing.id,
                missing.size,
                &order,
                open,
            )?;
            versions::take_fetched_version(dir, &record.repo, &missing.id)?;
        }
    }
    Ok(())
}

/// The members' nodes linked now (not ours).
fn linked_nodes(mesh: &MeshRuntime, group: &GroupRecord, members: &[GroupMemberRecord]) -> Vec<MeshNodeAddress> {
    members
        .iter()
        .filter(|m| m.status == "member" && m.member_pub != group.my_member_pub)
        .filter_map(|m| crate::app_groups::node_from_hex(&m.node_hint_hex))
        .filter(|node| mesh.has_link(node))
        .collect()
}

/// The background work of a files project on each mesh tick: fetch the lines' versions, move the
/// shared line, settle our line against it, fetch the content the folder needs, bring the folder to
/// it. One at a time per project; «Сохранить версию» waits for it.
pub(super) fn files_tick(db_path: &str, mesh: &Arc<MeshRuntime>, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) {
    let key: FetchKey = (group.group_id.clone(), record.repo.clone(), FILES_WORK.to_string());
    if with_profile(db_path, |p| p.fetching.insert(key.clone())) != Some(true) {
        return;
    }
    let nodes = linked_nodes(mesh, group, members);
    let (db, mesh, record, thread_key) = (db_path.to_string(), Arc::clone(mesh), record.clone(), key.clone());
    let spawned = std::thread::Builder::new().name("omni-workspace-files".to_string()).spawn(move || {
        let open = |node: MeshNodeAddress| mesh.open_bulk_stream(node).map_err(|e| OmniError::Transport(format!("content stream to the member: {e}")));
        let outcome = (|| {
            let storage = Storage::open(&db)?;
            files_work(&storage, &record.group_id, &record.repo, &nodes, &open)
        })();
        with_profile(&db, |p| {
            let status = p.files.entry((record.group_id.clone(), record.repo.clone())).or_default();
            match outcome {
                Ok(sync) => *status = sync,
                Err(error) => {
                    omni_app_trace_log("workspace/files", &format!("group={} repo={} failed: {error}", record.group_id, record.repo));
                    status.problem = Some(error.to_string());
                }
            }
            p.fetching.remove(&thread_key);
        });
    });
    if spawned.is_err() {
        with_profile(db_path, |p| p.fetching.remove(&key));
    }
}

/// One round of a files project's work; `open` gives a bulk stream to a member's node.
pub(crate) fn files_work(
    storage: &Storage,
    group_id: &str,
    repo: &str,
    nodes: &[MeshNodeAddress],
    open: &super::content::OpenStream,
) -> OmniResult<WorkspaceFilesSync> {
    let until = Instant::now() + FETCH_PER_PASS;
    let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("workspace no longer exists".into()))?;
    if group.my_status != "member" {
        return Err(OmniError::InvalidInput("no longer a workspace member".into()));
    }
    let members = super::agents::members(storage, group_id)?;
    super::custody::prepare_auto_folder(storage, &group, &members, &ensure_project(storage, group_id, repo)?)?;
    let record = storage.get_workspace_repo(group_id, repo)?.ok_or_else(|| OmniError::InvalidInput("repository was removed".into()))?;
    let dir = Path::new(&record.published_path);
    let node_of = |member: &str| members.iter().find(|m| m.member_pub == member).and_then(|m| crate::app_groups::node_from_hex(&m.node_hint_hex));
    let wanted: Vec<(Content, Option<MeshNodeAddress>)> = tips(storage, &group, &members, repo)?.into_iter().map(|t| (t.version, node_of(&t.member))).collect();
    if let Err(error) = fetch_versions(&record, &wanted, nodes, open, until) {
        omni_app_trace_log("workspace/versions", &format!("group={group_id} repo={repo}: {error}"));
    }
    let custody = super::custody::state(storage, &group, &members, repo)?;
    let archive = custody.archive_members.contains(&group.my_member_pub) || (is_admin(&members, &group.my_member_pub) && record.work_path.is_some());
    advance(storage, &group, &members, &record)?;
    thin_out(storage, &group, &members, &record)?;
    let mut sync = WorkspaceFilesSync::default();
    let Some(shared) = versions::read_ref(dir, Ref::Shared)? else {
        process_download(storage, &record, nodes, open)?;
        process_previews(storage, &record, nodes, open)?;
        return Ok(sync);
    };
    let mine = versions::read_ref(dir, Ref::Mine)?;
    let settle = mine.as_ref().is_none_or(|m| custody.allows(repo, &m.id));
    let work = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir());

    // Our line lost to another one: replay it on top; the shared line takes it next round.
    if let Some(mine) = mine.as_ref().filter(|m| settle && m.id != shared.id) {
        if !versions::is_ancestor(dir, repo, &mine.id, &shared.id)? && !versions::is_ancestor(dir, repo, &shared.id, &mine.id)? {
            let replayed = versions::replay(dir, repo, &shared, &mine.id, &|id| custody.rejected(repo, id))?;
            versions::update_ref(dir, Ref::Mine, Some(&replayed), Some(mine))?;
            omni_app_trace_log("workspace/replay", &format!("group={group_id} repo={repo} {} -> {}", mine.id, replayed.id));
        }
    }
    // Our accepted line replayed on the shared one is a new version: a reviewer's node accepts it as
    // it does its own saves. A line that waits for review stays waiting.
    let mine = versions::read_ref(dir, Ref::Mine)?;
    if let (true, Some(mine)) = (settle, mine.as_ref()) {
        super::custody::endorse_own(storage, &group, &members, repo, &mine.id)?;
    }
    // The accepted line this node already holds is attested too, so a node that arrives later
    // reaches the same line.
    super::custody::endorse_own(storage, &group, &members, repo, &shared.id)?;
    announce_tip(storage, &group, &members, &record)?;

    // The folder follows the newer of the shared line and ours (just saved, not taken yet).
    let target = match mine.as_ref() {
        Some(mine) if mine.id != shared.id && (!settle || versions::is_ancestor(dir, repo, &shared.id, &mine.id)?) => mine.clone(),
        _ => shared.clone(),
    };
    if let Some(work) = work {
        // What the folder already holds needs no fetching: a folder bound with the project's files
        // in it is read first (only files whose size or time changed since the index saw them).
        versions::scan(dir, work)?;
        let here = versions::Here::read(dir, Some(work));
        let left = versions::left_out(dir, versions::Left::Wanted)?;
        for (path, content) in versions::tree(dir, repo, &target.id)?.iter().filter(|(p, _)| !versions::is_left_out(&left, p)) {
            sync.content_total_bytes += content.size;
            if here.file(&content.id, Some(content.size)).is_some() {
                continue;
            }
            // The file the folder has at this path now (the older version) gives the blocks it shares.
            let seeds = [work.join(path)];
            if let Err(error) = super::content::fetch_to_store(dir, group_id, repo, &content.id, content.size, &seeds, nodes, open) {
                omni_app_trace_log("workspace/content", &format!("group={group_id} repo={repo} {path}: {error}"));
                sync.content_missing_bytes += content.size;
            }
        }
        if sync.content_missing_bytes == 0 {
            let followed = versions::follow(dir, repo, work, &target, archive)?;
            sync.waiting_open = followed.waiting_open;
            sync.waiting_changed = followed.waiting_changed;
            sync.problem = followed.problem;
        }
    }
    if archive {
        // History never holds the current version back; it is fetched for a bounded time per pass. An
        // object no node gave is asked for again later, ever more rarely (a minute, doubling up to half
        // an hour); a few failures in one pass end it — the sources are away.
        let here = versions::Here::read(dir, work);
        let db = storage.db_path().to_string_lossy().into_owned();
        let key = |id: &str| (group_id.to_string(), repo.to_string(), id.to_string());
        let mut failed = 0;
        for content in history_kept(storage, &group, &members, &record)?.contents.iter().filter(|c| here.file(&c.id, Some(c.size)).is_none()) {
            let later = with_profile(&db, |p| p.history_missing.get(&key(&content.id)).is_some_and(|(next, _)| *next > Instant::now())).unwrap_or(false);
            if later {
                continue;
            }
            match super::content::fetch_to_store(dir, group_id, repo, &content.id, content.size, &[], nodes, open) {
                Ok(_) => {
                    with_profile(&db, |p| p.history_missing.remove(&key(&content.id)));
                }
                Err(error) => {
                    let wait = with_profile(&db, |p| {
                        let wait = p.history_missing.get(&key(&content.id)).map_or(HISTORY_RETRY_FIRST, |(_, w)| (*w * 2).min(HISTORY_RETRY_MAX));
                        p.history_missing.insert(key(&content.id), (Instant::now() + wait, wait));
                        wait
                    })
                    .unwrap_or(HISTORY_RETRY_FIRST);
                    omni_app_trace_log("workspace/history", &format!("group={group_id} repo={repo} {}: {error}; next try in {}s", content.id, wait.as_secs()));
                    failed += 1;
                    if failed >= HISTORY_FAILURES_PER_PASS {
                        break;
                    }
                }
            }
            if Instant::now() >= until {
                break;
            }
        }
        super::custody::report_inventory(storage, &group, &members, &record)?;
    }
    process_download(storage, &record, nodes, open)?;
    process_previews(storage, &record, nodes, open)?;
    mark_folder(storage, &group, &members, &record)?;
    Ok(sync)
}

/// Every content of a line: what its versions brought in.
pub(super) fn line_contents(dir: &Path, repo: &str, tip: &str) -> OmniResult<HashSet<Content>> {
    let (chain, _, _) = versions::chain(dir, repo, tip)?;
    Ok(chain.iter().flat_map(|v| v.changes.iter().filter_map(|c| c.content.clone())).collect())
}

/// What a project keeps of its history (docs/decisions/workspace-files-storage.md, stage 3b).
#[derive(Debug, Default)]
pub(super) struct Kept {
    /// The content kept: of every accepted version kept and of the lines not accepted yet.
    pub contents: Vec<Content>,
    /// Accepted versions whose content is thinned out (their records stay).
    pub thinned: HashSet<String>,
    /// Kept versions without the content of their files under the «only the current» masks.
    pub partial: HashSet<String>,
    /// Content that only thinned-out versions had: removed from every store.
    pub dropped: Vec<Content>,
}

/// A day, for the retention rule.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// What the shared line `shared` and the other lines keep under `rule` (none: all). An accepted
/// version is kept while it is among the last `keep_last`, of the last `keep_days` days, the current
/// one or `protected` (a revision, «навсегда»); a version is the whole project, so its whole tree is
/// kept — but for the files under the rule's `only_current` masks, which only the current and the
/// protected versions keep. Every node works this out alike from the shared journal; a version's
/// time is its author's.
pub(super) fn kept_under(dir: &Path, repo: &str, shared: Option<&str>, others: &[String], rule: Option<&super::custody::RetentionRule>, protected: &HashSet<String>, now: i64) -> OmniResult<Kept> {
    let (mut keep, mut all, mut thinned, mut accepted) = (HashSet::new(), HashSet::new(), HashSet::new(), HashSet::new());
    let masked = |path: &str| rule.is_some_and(|r| r.only_current.iter().any(|m| omni_protocol::workspace_mask_matches(m, path)));
    // Versions kept without their masked files, with what those files were.
    let mut short: Vec<(String, Vec<Content>)> = Vec::new();
    if let Some(shared) = shared {
        let (chain, ids, _) = versions::chain(dir, repo, shared)?;
        // From the oldest version on: what changed since the last kept one, by path (and for masked
        // files since the last one kept whole). A kept version holds the last kept one's tree with
        // these on top.
        let (mut since_kept, mut since_whole, mut masked_tree): (HashMap<&str, &Content>, HashMap<&str, &Content>, HashMap<&str, &Content>) = Default::default();
        for (newer, (version, id)) in chain.iter().zip(&ids).enumerate().rev() {
            for change in &version.changes {
                let (since, tree) = if masked(&change.path) { (&mut since_whole, Some(&mut masked_tree)) } else { (&mut since_kept, None) };
                match &change.content {
                    Some(content) => {
                        all.insert(content.clone());
                        since.insert(&change.path, content);
                        if let Some(tree) = tree {
                            tree.insert(&change.path, content);
                        }
                    }
                    None => {
                        since.remove(change.path.as_str());
                        if let Some(tree) = tree {
                            tree.remove(change.path.as_str());
                        }
                    }
                }
            }
            let whole = rule.is_none() || newer == 0 || protected.contains(id);
            let kept = whole
                || rule.is_some_and(|r| newer < r.keep_last.max(1) as usize || version.at_unix_ms >= now.saturating_sub(i64::from(r.keep_days) * DAY_MS));
            if whole {
                keep.extend(since_kept.drain().chain(since_whole.drain()).map(|(_, c)| c.clone()));
            } else if kept {
                keep.extend(since_kept.drain().map(|(_, c)| c.clone()));
                if !masked_tree.is_empty() {
                    short.push((id.clone(), masked_tree.values().map(|c| (*c).clone()).collect()));
                }
            } else {
                thinned.insert(id.clone());
            }
            accepted.insert(id.clone());
        }
    }
    // A line not accepted yet keeps its own versions' content and its whole tree.
    for line in others.iter().filter(|l| !accepted.contains(*l)) {
        let (chain, ids, _) = versions::chain(dir, repo, line)?;
        for (version, _) in chain.iter().zip(&ids).take_while(|(_, id)| !accepted.contains(*id)) {
            keep.extend(version.changes.iter().filter_map(|c| c.content.clone()));
        }
        if let Ok(tree) = versions::tree(dir, repo, line) {
            keep.extend(tree.values().cloned());
        }
    }
    let partial = short.into_iter().filter(|(_, files)| files.iter().any(|c| !keep.contains(c))).map(|(id, _)| id).collect();
    let mut dropped: Vec<Content> = all.difference(&keep).cloned().collect();
    dropped.sort();
    let mut contents: Vec<Content> = keep.into_iter().collect();
    contents.sort();
    Ok(Kept { contents, thinned, partial, dropped })
}

/// What the project keeps here, by its rule: a cache only, by the lines, rule and protected versions
/// it was worked out at (and the hour, while the rule counts days); emptied when a profile stops.
static HISTORY: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, (u64, Arc<Kept>)>>> = std::sync::LazyLock::new(Default::default);

pub(crate) fn clear_history_cache() {
    if let Ok(mut cache) = HISTORY.lock() {
        cache.clear();
    }
}

/// The lines a project's history is worked out from: the shared one, and every announced tip, ours
/// and the folder's version.
pub(super) fn history_lines(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<(Option<String>, Vec<String>)> {
    let dir = Path::new(&record.published_path);
    let mut lines: Vec<String> = tips(storage, group, members, &record.repo)?.into_iter().map(|t| t.version.id).collect();
    for name in [Ref::Mine, Ref::Applied] {
        lines.extend(versions::read_ref(dir, name)?.map(|c| c.id));
    }
    lines.sort();
    lines.dedup();
    Ok((versions::read_ref(dir, Ref::Shared)?.map(|c| c.id), lines))
}

/// The versions a project's rule never thins out besides the last ones: revisions and «навсегда».
pub(super) fn protected_versions(current: &super::custody::CustodyState) -> HashSet<String> {
    current.revisions.keys().chain(current.kept.iter()).cloned().collect()
}

/// What the project keeps here under its confirmed rule ([`kept_under`]).
pub(super) fn history_kept(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<Arc<Kept>> {
    use std::hash::{Hash, Hasher};
    let dir = Path::new(&record.published_path);
    let (shared, lines) = history_lines(storage, group, members, record)?;
    let current = super::custody::state(storage, group, members, &record.repo)?;
    let mut protected: Vec<String> = protected_versions(&current).into_iter().collect();
    protected.sort();
    let now = current_unix_ms();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&shared, &lines, &current.retention, &protected).hash(&mut hasher);
    if current.retention.as_ref().is_some_and(|r| r.keep_days > 0) {
        (now / (60 * 60 * 1000)).hash(&mut hasher);
    }
    let fingerprint = hasher.finish();
    if let Some((seen, kept)) = HISTORY.lock().ok().and_then(|c| c.get(dir).cloned()) {
        if seen == fingerprint {
            return Ok(kept);
        }
    }
    let kept = Arc::new(kept_under(dir, &record.repo, shared.as_deref(), &lines, current.retention.as_ref(), &protected.into_iter().collect(), now)?);
    if let Ok(mut cache) = HISTORY.lock() {
        cache.insert(dir.to_path_buf(), (fingerprint, kept.clone()));
    }
    Ok(kept)
}

/// Remove from the stores here the content only thinned-out versions had. Inside the project's
/// round of work: a save or an acceptance (the same work key) does not store content meanwhile.
fn thin_out(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<()> {
    let kept = history_kept(storage, group, members, record)?;
    let dir = Path::new(&record.published_path);
    let (mut removed, mut bytes) = (0_usize, 0_u64);
    for content in &kept.dropped {
        super::content::blocks::remove_list(dir, &content.id);
        if super::content::remove_object(dir, &content.id) {
            removed += 1;
            bytes = bytes.saturating_add(content.size);
        }
    }
    if removed > 0 {
        omni_app_trace_log("workspace/thin", &format!("group={} repo={} removed={removed} bytes={bytes} versions={}", group.group_id, record.repo, kept.thinned.len()));
    }
    Ok(())
}

/// The complete tree of a version, with whether each file's content is here.
pub(super) fn file_tree(record: &WorkspaceRepoRecord, version: &str) -> OmniResult<Vec<super::custody::WorkspaceFileView>> {
    let dir = Path::new(&record.published_path);
    let here = versions::Here::read(dir, record.work_path.as_deref().map(Path::new));
    let left = versions::left_out(dir, versions::Left::Wanted)?;
    Ok(versions::tree(dir, &record.repo, version)?
        .iter()
        .map(|(path, content)| super::custody::WorkspaceFileView {
            path: path.clone(),
            bytes: content.size,
            available_here: here.file(&content.id, Some(content.size)).is_some(),
            state: WorkspaceFileState::Accepted,
            in_version: true,
            left_out: versions::is_left_out(&left, path),
            lock: None,
            taken_after: false,
        })
        .collect())
}

/// The files of the shared version with what this node did to them: changed in the folder and not
/// saved, saved in a version of ours the shared one lacks; the files only we have are listed too.
pub(super) fn shared_file_tree(record: &WorkspaceRepoRecord, shared: &str) -> OmniResult<Vec<super::custody::WorkspaceFileView>> {
    let dir = Path::new(&record.published_path);
    let repo = record.repo.as_str();
    let mut files: BTreeMap<String, super::custody::WorkspaceFileView> = file_tree(record, shared)?.into_iter().map(|f| (f.path.clone(), f)).collect();
    let mut mark = |path: &str, bytes: Option<u64>, state: WorkspaceFileState| {
        let file = files.entry(path.to_string()).or_insert_with(|| super::custody::WorkspaceFileView { path: path.to_string(), bytes: 0, available_here: true, state, in_version: false, left_out: false, lock: None, taken_after: false });
        file.state = state;
        if let Some(bytes) = bytes {
            (file.bytes, file.available_here) = (bytes, true);
        }
    };
    if let Some(mine) = own_ahead(dir, repo, shared)? {
        let (from, to) = (versions::tree(dir, repo, shared)?, versions::tree(dir, repo, &mine.id)?);
        for change in versions::diff(&from, &to) {
            mark(&change.path, change.content.map(|c| c.size), WorkspaceFileState::Saved);
        }
    }
    if let Some(work) = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()) {
        let scan = versions::look(dir, work)?;
        let held = versions::held_tree(dir, repo)?;
        for path in versions::folder_changes(&scan, &held, &versions::left_out(dir, versions::Left::Held)?) {
            let bytes = std::fs::metadata(work.join(&path)).ok().map(|m| m.len());
            mark(&path, bytes, WorkspaceFileState::Changed);
        }
    }
    Ok(files.into_values().collect())
}

/// Our line when it holds versions the shared one does not.
fn own_ahead(dir: &Path, repo: &str, shared: &str) -> OmniResult<Option<Content>> {
    match versions::read_ref(dir, Ref::Mine)? {
        Some(mine) if mine.id != shared && !versions::is_ancestor(dir, repo, &mine.id, shared)? => Ok(Some(mine)),
        _ => Ok(None),
    }
}

/// (files, bytes together) of a version.
pub(super) fn version_summary(record: &WorkspaceRepoRecord, version: &str) -> OmniResult<(usize, u64)> {
    let tree = versions::tree(Path::new(&record.published_path), &record.repo, version)?;
    Ok((tree.len(), tree.values().map(|c| c.size).sum()))
}

/// The paths `tip` changes against `base` (none: the empty project).
pub(super) fn changed_between(record: &WorkspaceRepoRecord, base: Option<&str>, tip: &str) -> OmniResult<Vec<String>> {
    let dir = Path::new(&record.published_path);
    let from = versions::tree_of(dir, &record.repo, base)?;
    let to = versions::tree(dir, &record.repo, tip)?;
    Ok(versions::diff(&from, &to).into_iter().map(|c| c.path).collect())
}

pub(super) fn download_target_stamp(path: &Path) -> Option<(u64, u64)> {
    let meta = path.metadata().ok()?;
    let modified = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos().min(u64::MAX as u128) as u64;
    Some((meta.len(), modified))
}

/// A file asked for by «Скачать»: its content from here or from members, to the chosen place.
pub(crate) fn process_download(
    storage: &Storage,
    record: &WorkspaceRepoRecord,
    nodes: &[MeshNodeAddress],
    open: &super::content::OpenStream,
) -> OmniResult<()> {
    let Some(request) = storage.get_workspace_file_local(&record.group_id, &record.repo, "download")?.filter(|r| !r.is_empty()) else { return Ok(()) };
    let original_request = request.clone();
    let outcome = (|| -> OmniResult<()> {
        let request: serde_json::Value = serde_json::from_str(&request).map_err(|e| OmniError::InvalidInput(e.to_string()))?;
        let value = |name: &str| request.get(name).and_then(|s| s.as_str()).ok_or_else(|| OmniError::InvalidInput("invalid download request".into()));
        let (version, path, target) = (value("commit")?, value("path")?, value("target")?);
        let dir = Path::new(&record.published_path);
        Storage::confine_destructive_path_for_callers(Path::new(target))?;
        let expected: Option<(u64, u64)> =
            serde_json::from_value(request.get("target_stamp").cloned().unwrap_or(serde_json::Value::Null)).map_err(|e| OmniError::InvalidInput(e.to_string()))?;
        if download_target_stamp(Path::new(target)) != expected {
            return Err(OmniError::InvalidInput("destination changed after the download was requested; choose it again".into()));
        }
        let group = storage.get_group(&record.group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown workspace".into()))?;
        let shared = versions::read_ref(dir, Ref::Shared)?;
        super::custody::validate_version_access(storage, &group, &super::agents::members(storage, &group.group_id)?, &record.repo, version, shared.as_ref().map(|s| s.id.as_str()))?;
        let content = versions::tree(dir, &record.repo, version)?.get(path).cloned().ok_or_else(|| OmniError::InvalidInput("file is not in the version".into()))?;
        let target_dir = Path::new(target).parent().ok_or_else(|| OmniError::InvalidInput("choose a destination file".into()))?;
        let temp = target_dir.join(format!(".omni-download-{}", hex::encode(&omni_crypto::generate_group_key()[..8])));
        let outcome = (|| -> OmniResult<()> {
            // What is here is checked against its id on the way: a damaged copy in the store is not
            // handed to the person as the file — it goes, and the file comes from the members.
            let copied = versions::content_file(dir, record.work_path.as_deref().map(Path::new), &content.id, content.size)
                .map(|here| versions::copy_content(dir, &here, &temp, &content));
            match copied {
                Some(Ok(())) => {}
                _ => {
                    let _ = std::fs::remove_file(&temp);
                    // What the destination holds now (an older copy of the file) gives the blocks it shares.
                    super::content::fetch_content(&temp, target_dir, &record.group_id, &record.repo, &content.id, content.size, &[PathBuf::from(target)], nodes, open)?;
                }
            }
            if download_target_stamp(Path::new(target)) != expected {
                return Err(OmniError::InvalidInput("destination changed during the download; choose it again".into()));
            }
            if storage.get_workspace_file_local(&record.group_id, &record.repo, "download")?.as_deref() != Some(original_request.as_str()) {
                return Err(OmniError::InvalidInput("download was cancelled".into()));
            }
            // A file of the folder someone else holds is marked read-only (`mark_folder`); the chosen
            // destination is replaced all the same.
            versions::set_read_only(Path::new(target), false);
            std::fs::rename(&temp, target).map_err(|e| OmniError::Storage(e.to_string()))?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        outcome
    })();
    if storage.get_workspace_file_local(&record.group_id, &record.repo, "download")?.as_deref() != Some(original_request.as_str()) {
        return Ok(());
    }
    match outcome {
        Ok(()) => {
            storage.set_workspace_file_local(&record.group_id, &record.repo, "download", "")?;
            // The status outlives the dialog, so it names the file and where it went.
            let request: serde_json::Value = serde_json::from_str(&original_request).unwrap_or_default();
            let field = |name: &str| request.get(name).and_then(|s| s.as_str()).unwrap_or("").to_owned();
            storage.set_workspace_file_local(&record.group_id, &record.repo, "download_status", &format!("Скачан {} → {}", field("path"), field("target")))?;
        }
        Err(error) => storage.set_workspace_file_local(&record.group_id, &record.repo, "download_status", &format!("Ожидает загрузки: {error}"))?,
    }
    Ok(())
}

/// The screen's card of a files project.
pub(super) fn repo_card(
    members: &[GroupMemberRecord],
    me: Option<&str>,
    record: &WorkspaceRepoRecord,
    state: &WorkspaceMainState,
    custody: &super::custody::CustodyState,
    sync: WorkspaceFilesSync,
) -> OmniResult<super::WorkspaceRepoView> {
    const RECENT_VERSIONS: usize = 3;
    let dir = Path::new(&record.published_path);
    let repo = record.repo.as_str();
    let names: HashMap<&str, &str> = members.iter().map(|m| (m.account_id.as_str(), m.display_name.as_str())).collect();
    let described = |id: &str| -> OmniResult<Option<super::WorkspaceCommitView>> {
        Ok(versions::version(dir, repo, id)?.map(|v| super::WorkspaceCommitView {
            oid: id.to_string(),
            subject: v.message.clone(),
            author: v.author_name.clone(),
            at_unix_ms: v.at_unix_ms,
        }))
    };
    let tip_view = |(account, id): &(String, String)| -> OmniResult<super::WorkspaceTipView> {
        Ok(super::WorkspaceTipView {
            account_id: account.clone(),
            name: names.get(account.as_str()).copied().unwrap_or(account).to_string(),
            is_me: me == Some(account.as_str()),
            branch: "main".into(),
            oid: id.clone(),
            commit: described(id)?,
        })
    };
    let (mut version, mut file_count, mut total_bytes, mut recent, mut main) = (0, 0, 0, Vec::new(), None);
    let mut accepted: HashSet<String> = HashSet::new();
    if let Some(shared) = state.oid.as_deref() {
        let (chain, ids, _) = versions::chain(dir, repo, shared)?;
        version = chain.len() as u32;
        (file_count, total_bytes) = version_summary(record, shared)?;
        for id in ids.iter().take(RECENT_VERSIONS) {
            recent.extend(described(id)?);
        }
        main = described(shared)?;
        accepted = ids.into_iter().collect();
    }
    let conflicts: Vec<String> = match state.oid.as_deref() {
        Some(shared) => versions::conflict_copies(&*versions::tree(dir, repo, shared)?).into_keys().collect(),
        None => Vec::new(),
    };
    let mut own = None;
    if let (Some(shared), Some(me)) = (state.oid.as_deref(), me) {
        if let Some(mine) = own_ahead(dir, repo, shared)? {
            let (_, ids, _) = versions::chain(dir, repo, &mine.id)?;
            let (_, held, _) = versions::chain(dir, repo, shared)?;
            let held: HashSet<&String> = held.iter().collect();
            let line = (me.to_string(), mine.id.clone());
            own = described(&mine.id)?.map(|commit| super::WorkspaceOwnVersionView {
                commit,
                count: ids.iter().take_while(|id| !held.contains(id)).count() as u32,
                state: if state.rejected.contains(&line) {
                    super::WorkspaceOwnVersionState::Rejected
                } else if state.waiting.contains(&line) {
                    super::WorkspaceOwnVersionState::Waiting
                } else {
                    super::WorkspaceOwnVersionState::Joining
                },
            });
        }
    }
    let (mut changed, mut changed_count, mut folder_current) = (Vec::new(), 0, false);
    if let Some(work) = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()) {
        let applied = versions::read_ref(dir, Ref::Applied)?;
        folder_current = applied.as_ref().map(|a| a.id.as_str()) == state.oid.as_deref();
        let held = versions::held_tree(dir, repo)?;
        let all = versions::folder_changes(&versions::look(dir, work)?, &held, &versions::left_out(dir, versions::Left::Held)?);
        changed_count = all.len();
        changed = all.into_iter().take(20).collect();
    }
    let now = current_unix_ms();
    let (mut taken, mut held_elsewhere) = (Vec::new(), Vec::new());
    if let Some(me) = me {
        let mine = custody.taken_by(me, now);
        taken = mine.iter().filter(|(_, holds)| *holds).map(|(path, _)| path.clone()).collect();
        let mut theirs: Vec<&String> = mine.iter().filter(|(_, holds)| !*holds).map(|(path, _)| path).chain(changed.iter()).collect();
        theirs.sort();
        theirs.dedup();
        for path in theirs {
            if let Some(holder) = custody.holder(path, now).filter(|h| h.account != me) {
                held_elsewhere.push(super::WorkspaceHeldView { path: path.clone(), name: names.get(holder.account.as_str()).copied().unwrap_or(&holder.account).to_string() });
            }
        }
    }
    // Only revisions of the history: one whose version left it (laid again by a lost race) is not
    // there to check.
    let mut revisions_on_check: Vec<String> = custody
        .revisions
        .iter()
        .filter(|(version, _)| accepted.contains(*version))
        .filter(|(version, _)| custody.revision_pending(version) && custody.released.get(*version).is_none_or(|(by, _)| me != Some(by.as_str())))
        .map(|(_, (label, _))| label.clone())
        .collect();
    revisions_on_check.sort();
    Ok(super::WorkspaceRepoView {
        repo: record.repo.clone(),
        here: true,
        work_path: record.work_path.clone(),
        main,
        needs_approval: state.needs_approval,
        main_behind: 0,
        // Our own line is told apart (`own`).
        waiting: state.waiting.iter().filter(|(account, _)| me != Some(account.as_str())).map(tip_view).collect::<OmniResult<_>>()?,
        forks: state.forks.iter().map(tip_view).collect::<OmniResult<_>>()?,
        branches: Vec::new(),
        local_branches: Vec::new(),
        device_branches: Vec::new(),
        files: true,
        version,
        changed,
        changed_count,
        sync,
        file_count,
        total_bytes,
        recent,
        own,
        folder_current,
        left_out: versions::left_out(dir, versions::Left::Wanted)?,
        taken,
        held_elsewhere,
        conflicts,
        revisions_on_check,
        is_tasks: record.repo == super::tasks::TASKS_REPO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// r -> a (Ann), r -> b (Bob): two saves from one shared version.
    fn ancestor(a: &str, b: &str) -> OmniResult<bool> {
        let behind: &[&str] = match b {
            "r" => &["r"],
            "a" => &["r", "a"],
            "b" => &["r", "b"],
            "b2" => &["r", "b", "b2"],
            _ => &[],
        };
        Ok(behind.contains(&a))
    }

    #[test]
    fn diverging_saves_end_on_one_line_whatever_order_they_came_in() {
        // Ann's save came first (earlier); Bob's line has two saves by two accounts.
        let rank = |_other: &str, tip: &str| -> OmniResult<LineRank> {
            Ok(match tip {
                "a" => (1, 1, Reverse(100), Reverse("a".into())),
                "b" => (1, 1, Reverse(200), Reverse("b".into())),
                "b2" => (2, 2, Reverse(200), Reverse("b".into())),
                _ => (0, 0, Reverse(0), Reverse(String::new())),
            })
        };
        let pair = |a: &str, o: &str| (a.to_string(), o.to_string());
        // One save each: the earlier one wins on every node, also on one that took the later one first.
        let both = [pair("ann", "a"), pair("bob", "b")];
        assert_eq!(next_files_main(Some("r"), &both, ancestor, rank).unwrap().oid.as_deref(), Some("a"));
        let back = next_files_main(Some("b"), &both, ancestor, rank).unwrap();
        assert_eq!(back.oid.as_deref(), Some("a"), "the later line gives way even when held already");
        assert_eq!(back.forks, vec![pair("bob", "b")]);
        // A line more accounts built on beats a lone earlier save.
        let built = [pair("ann", "a"), pair("bob", "b2")];
        assert_eq!(next_files_main(Some("a"), &built, ancestor, rank).unwrap().oid.as_deref(), Some("b2"));
    }
}
