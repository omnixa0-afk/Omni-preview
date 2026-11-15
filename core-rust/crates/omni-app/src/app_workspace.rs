//! Workspaces: a group with git repositories — phase 1 of `docs/decisions/omni-workspace-git.md`.
//!
//! Each repository of a workspace has, on every member's node, a published bare repository
//! (`<profile dir>/workspaces/<group_id>/<repo>.git`): the person pushes to it as to any folder
//! (remote `omni`), members fetch from it. Its own branches (`refs/heads/*`) are announced to the
//! group (`WorkspaceRefsV1`, whole, not a diff) when they change; each member's announced branches
//! are fetched from that member's node into `refs/omni/members/<account>/heads/*`
//! (`app_git_bridge`). The mesh tick drives both.
//!
//! The shared `main` (`refs/omni/canonical/heads/main`) is not announced by anyone: every node works
//! it out from the maintainers' (group admins') announced `main` — fast-forward only, and under the
//! workspace rule "needs another member's approval" only to a commit approved by an account other
//! than the maintainer's. Diverging maintainers are a fork shown to people, never settled here.
//!
//! A project of files (models, drawings, documents — `app_workspace_files`) is not git: one line of
//! versions any member moves, lines that went different ways settled by a rule every node works out
//! alike (docs/decisions/workspace-files-storage.md).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use omni_group_mls::MemberCard;
use omni_mesh_runtime::{MeshNodeAddress, MeshRuntime};
use omni_protocol::{
    validate_workspace_approval_v1, validate_workspace_refs_v1, validate_workspace_repo_name, GroupAppKindV1, GroupAppPayloadV1,
    WorkspaceApprovalV1, WorkspaceRefV1, WorkspaceRefsV1, WorkspaceRuleV1, WORKSPACE_REFS_MAX,
};
use omni_storage::{GroupMemberRecord, GroupRecord, Storage, WorkspaceMemberRefRecord, WorkspaceMainRuleRecord, WorkspaceRepoRecord};
use omni_types::{OmniError, OmniResult};

use crate::app_git_bridge::git_command;

#[path = "app_workspace_files.rs"]
mod files;
#[path = "app_workspace_versions.rs"]
mod versions;
#[path = "app_workspace_view.rs"]
mod view;
#[path = "app_workspace_custody.rs"]
pub(crate) mod custody;
#[path = "app_workspace_content.rs"]
pub(crate) mod content;
#[path = "app_workspace_tasks.rs"]
pub(crate) mod tasks;
#[path = "app_workspace_model_view.rs"]
mod model_view;
#[path = "app_workspace_agents.rs"]
pub mod agents;
#[path = "app_workspace_agent_apps.rs"]
pub mod agent_apps;
#[path = "app_workspace_talk.rs"]
pub mod talk;
pub use agents::{AgentInbox, AgentInboxItem, AgentInboxPlace, AgentSelf, WorkspaceAgentsCommand, WorkspaceAgentAskView, WorkspaceAgentReviewView, WorkspaceAgentView, WorkspaceAgentsView};
pub use custody::{WorkspaceFilesCommand, WorkspaceFilesDetails};
pub use files::{WorkspaceFilesSync, WorkspaceSaveVersion};
#[cfg(test)]
pub(crate) use files::{files_work, process_download};
pub use view::{
    WorkspaceCommitView, WorkspaceFolder, WorkspaceDeviceBranchView, WorkspaceLocalBranchView, WorkspaceOwnVersionState, WorkspaceOwnVersionView,
    WorkspaceHeldView, WorkspacePullOutcome, WorkspaceRepoView, WorkspaceTipView, WorkspaceView, WorkspaceViewSnapshot,
};
use crate::{current_unix_ms, omni_app_trace_log, OmniApp};

/// A fetch that failed (the member's node went away mid-way, git refused) is tried again after this.
const FETCH_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The branch the workspace shares, and where each node keeps its shared value.
const MAIN: &str = "refs/heads/main";
const SHARED_MAIN: &str = "refs/omni/canonical/heads/main";

/// What a bound working repository fetches from `omni` besides its own branches: the shared `main`
/// as `omni-shared/main`, members' branches as `omni-members/<account>/heads/<branch>`.
const WORK_FETCH_SPECS: [(&str, &str); 3] = [
    ("+refs/omni/canonical/heads/*:refs/remotes/omni-shared/*", r"^\+refs/omni/canonical/"),
    ("+refs/omni/members/*:refs/remotes/omni-members/*", r"^\+refs/omni/members/"),
    ("+refs/omni/devices/*:refs/remotes/omni-devices/*", r"^\+refs/omni/devices/"),
];

/// «На мои устройства»: a branch sent to the account's own devices only (the team neither sees nor
/// gets it — `app_git_bridge::OWN_DEVICES_ONLY_REFS`), and where each other device's ones land here.
const MINE: &str = "refs/omni/mine/heads/";
const DEVICES: &str = "refs/omni/devices/";

/// How often an own device is asked for the branches it sent to our devices: nothing announces them
/// (that would tell the group), so a linked device of ours is asked; an unchanged answer is a few
/// hundred bytes of advertisement.
const OWN_DEVICE_FETCH_EVERY: Duration = Duration::from_secs(60);

type RepoKey = (String, String); // group, repo
type FetchKey = (String, String, String); // group, repo, account

#[derive(Default)]
struct ProfileWorkspaces {
    /// Accepted history content ids, cached by canonical tip for the content serving gate.
    accepted_objects: HashMap<RepoKey, (String, HashSet<String>)>,
    /// What the published repository's branch files and the group's members were when we last
    /// looked: a change of the first may be new branches to announce, of the second newcomers who
    /// have not heard our branches yet.
    seen: HashMap<RepoKey, (u64, u64)>,
    /// Fetches under way.
    fetching: HashSet<FetchKey>,
    /// When a fetch last failed.
    failed_at: HashMap<FetchKey, Instant>,
    /// What the shared `main` was last worked out from (rule, maintainers, their fetched `main`,
    /// approvals, our branches): unchanged, nothing to do.
    main_seen: HashMap<RepoKey, u64>,
    /// When an own device was last asked for its branches «на мои устройства» (group, repo, its node).
    own_asked_at: HashMap<FetchKey, Instant>,
    /// Where each files repository's folder is against the shared `main`, as its work last saw it.
    files: HashMap<RepoKey, WorkspaceFilesSync>,
    /// The shared `main` as `advance_main` last worked it out: the screen reads it when nothing changed.
    main_state: HashMap<RepoKey, WorkspaceMainState>,
    /// The screen's card of each repository and what it was read from: the same again is not read
    /// again (a git process costs ~100 ms on some machines, a card ~15 of them).
    views: HashMap<RepoKey, (u64, WorkspaceRepoView)>,
    /// History content (group, repo, object) no linked node gave us: when to ask again and the wait
    /// before that — a lost object is not asked for every few seconds, and it does not hold back the
    /// rest of the history.
    history_missing: HashMap<FetchKey, (Instant, Duration)>,
}

/// The shared `main` of a repository as this node sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, serde::Serialize)]
pub struct WorkspaceMainState {
    /// The shared `main` (none yet: no maintainer's `main` here, or it waits for an approval).
    pub oid: Option<String>,
    pub needs_approval: bool,
    /// Maintainers' `main` ahead of the shared one but not approved yet: (account, commit).
    pub waiting: Vec<(String, String)>,
    /// Maintainers' `main` that went another way: people settle it (a merge), Omni does not.
    pub forks: Vec<(String, String)>,
    /// Files: lines a reviewer turned down (account, version); they stay with their authors.
    pub rejected: Vec<(String, String)>,
}

/// Per profile (db_path). Cleared in `stop_runtime_context`; a fetch thread still running then
/// only removes its own key from whatever is there.
static WORKSPACES: LazyLock<Mutex<HashMap<String, ProfileWorkspaces>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Remove what a deleted profile kept of its workspaces on disk (F3, audit 2026-11-09 #5): each
/// project's own folder under `<profile folder>/workspaces` (versions, refs, content, a published git
/// repository) and, of an archive a custodian put elsewhere, its content (`objects`, `tmp`) — the
/// folder itself was the person's choice and stays. A working folder the person bound stays; one Omni
/// made itself (a task project's default, under `workspace-tasks`) goes. The last profile's folders
/// all go with the device's local data ([`omni_storage::ProfileFolder`]). Nothing outside the profile's
/// folders is removed but the archives' content. Best effort: what stays is traced.
pub(crate) fn remove_profile_workspace_data(db_path: &Path, projects: &[(String, Option<String>)]) {
    let Ok(root) = omni_storage::ProfileFolder::Workspaces.path(db_path) else { return };
    let tasks_root = omni_storage::ProfileFolder::WorkspaceTasks.path(db_path).ok();
    let gone = |path: &Path, what: &str| match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => crate::omni_app_trace_log("profile/delete", &format!("{what} {} stays: {error}", path.display())),
    };
    for (published, work) in projects {
        if let (Some(tasks_root), Some(work)) = (&tasks_root, work.as_deref().map(Path::new)) {
            if work.starts_with(tasks_root) && work != tasks_root {
                gone(work, "a task folder Omni made");
            }
        }
        let published = Path::new(published);
        if !published.starts_with(&root) || published == root {
            continue;
        }
        for store in content::store_roots(published).into_iter().filter(|store| !store.starts_with(published)) {
            remove_archive_content(&store, "profile/delete");
        }
        gone(published, "project data");
        // Each only when empty: the group's folder, then the profile's own.
        for folder in published.ancestors().skip(1).take_while(|folder| *folder != root) {
            if std::fs::remove_dir(folder).is_err() {
                break;
            }
        }
    }
}

/// Of an archive a custodian keeps in a folder of their choice, Omni's content (`objects`, `tmp`) and
/// the empty folders Omni made for it there (`.omni/<profile>/<group>/<repo>/content`, or
/// `.omni/<group>/<repo>/content` from before archives were kept per profile); the chosen folder and
/// anything else in it stay. Whether the content went.
fn remove_archive_content(store: &Path, trace: &str) -> bool {
    let mut gone = true;
    for part in ["objects", "tmp"] {
        match std::fs::remove_dir_all(store.join(part)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                omni_app_trace_log(trace, &format!("archive content {} stays: {error}", store.join(part).display()));
                gone = false;
            }
        }
    }
    let is_omni = |depth: usize| store.ancestors().nth(depth).and_then(Path::file_name).is_some_and(|name| name == ".omni");
    let omni_made = store.file_name().is_some_and(|name| name == "content")
        .then(|| if is_omni(4) { Some(5) } else if is_omni(3) { Some(4) } else { None })
        .flatten();
    if let (true, Some(levels)) = (gone, omni_made) {
        // Each only when empty: `remove_dir` leaves a folder with anything in it.
        for folder in store.ancestors().take(levels) {
            if std::fs::remove_dir(folder).is_err() {
                break;
            }
        }
    }
    gone
}

/// A group's folder set aside for removal starts with this. It is renamed on the tick, one quick step
/// that frees the group's own name at once, and removed after on a thread of its own: a project's
/// content can be gigabytes. One left over (Omni closed mid-way, a file held open) goes on a later tick.
const SET_ASIDE: &str = ".gone-";

/// A thread is removing set-aside folders (one at a time). Not per profile: the thread clears it
/// itself when done ([`RemovingSetAside`]).
static REMOVING_SET_ASIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct RemovingSetAside;

impl Drop for RemovingSetAside {
    fn drop(&mut self) {
        REMOVING_SET_ASIDE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Leaving a group takes its projects from this computer (docs/decisions/workspace-files-storage.md,
/// «Выход из группы»): Omni's copy under the profile folder (versions, refs, content, a published git
/// repository), the task folder Omni made, and Omni's content in an archive folder the person chose.
/// The working folders the person bound stay, with the files of the version last applied there.
///
/// Decided by state, not by the event, so every way out is the same and nothing is missed while Omni
/// was closed: a folder of a group we are not a member of — left (its rows are gone), removed,
/// dissolved, deleted while its farewell is still queued. A group whose rows an old build dropped and
/// that waits to be restored from its MLS state is not one we left: its folder stays. Each profile
/// of a database has its own folder ([`own_folder`]); only the active one's is looked at. Nothing is
/// touched when the groups cannot be read.
///
/// Returns the folders set aside, for [`remove_set_aside`]. The chat of a group we were removed from
/// or that was dissolved stays, and says the projects' data went.
pub(crate) fn set_aside_projects_of_groups_left(storage: &Storage) -> Vec<PathBuf> {
    let (Ok(groups), Ok(waiting)) = (storage.list_groups(), storage.list_group_ids_without_row()) else {
        return Vec::new();
    };
    let ours: HashSet<&str> = groups
        .iter()
        .filter(|group| group.my_status == "member")
        .map(|group| group.group_id.as_str())
        .chain(waiting.iter().map(String::as_str))
        .collect();
    let mut set_aside = Vec::new();
    let mut left = HashSet::new();
    for folder in [omni_storage::ProfileFolder::Workspaces, omni_storage::ProfileFolder::WorkspaceTasks] {
        let Ok(root) = own_folder(storage, folder) else { continue };
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(SET_ASIDE) {
                set_aside.push(entry.path());
                continue;
            }
            if ours.contains(name.as_str()) {
                continue;
            }
            let to = root.join(format!("{SET_ASIDE}{name}-{}", hex::encode(&omni_crypto::generate_group_key()[..4])));
            match std::fs::rename(entry.path(), &to) {
                Ok(()) => {
                    set_aside.push(to);
                    left.insert(name);
                }
                Err(error) => omni_app_trace_log("workspace/left", &format!("group={name} data stays for now: {error}")),
            }
        }
    }
    let db_path = storage.db_path().to_string_lossy().into_owned();
    for group in groups.iter().filter(|g| left.contains(&g.group_id) && matches!(g.my_status.as_str(), "removed" | "dissolved")) {
        if let Err(error) = storage.delete_group_workspace_rows(&group.group_id) {
            omni_app_trace_log("workspace/left", &format!("group={} rows stay: {error}", group.group_id));
        }
        let _ = crate::app_groups::insert_system_line(storage, &group.group_id, "Данные проектов группы удалены с этого компьютера", current_unix_ms());
        crate::record_group_changed_event(&db_path, &group.group_id);
    }
    set_aside
}

/// Remove the folders [`set_aside_projects_of_groups_left`] set aside. A project's archive in a folder
/// the person chose goes first; while it cannot, the set-aside folder that points to it stays, for a
/// later tick.
pub(crate) fn remove_set_aside(folders: &[PathBuf]) {
    for folder in folders {
        let mut archives_gone = true;
        let projects = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path()).filter(|path| path.is_dir());
        for project in projects {
            for store in content::store_roots(&project).into_iter().filter(|store| !store.starts_with(&project)) {
                archives_gone &= remove_archive_content(&store, "workspace/left");
            }
        }
        if !archives_gone {
            continue;
        }
        match std::fs::remove_dir_all(folder) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => omni_app_trace_log("workspace/left", &format!("{} stays for now: {error}", folder.display())),
        }
    }
}

/// On the tick: set aside the projects of groups we left, and remove them on a thread.
fn forget_projects_of_groups_left(storage: &Storage) {
    let set_aside = set_aside_projects_of_groups_left(storage);
    if set_aside.is_empty() || REMOVING_SET_ASIDE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let _done = RemovingSetAside;
        remove_set_aside(&set_aside);
    });
}

/// What leaving a group takes from this computer, for the leave dialog.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceLeaveNotice {
    /// The group's projects here: their history goes with Omni's copy; working folders stay.
    pub projects: Vec<String>,
    /// Those whose whole history no other member keeps (we are their only custodian): it goes for
    /// the group too.
    pub only_custodian_of: Vec<String>,
}

impl OmniApp {
    pub fn workspace_leave_notice(&self, group_id: &str) -> OmniResult<WorkspaceLeaveNotice> {
        let _ = self.ensure_runtime_profile_active("workspace_leave_notice")?;
        let mut notice = WorkspaceLeaveNotice::default();
        let Some(group) = self.storage.get_group(group_id)?.filter(|g| g.my_status == "member") else {
            return Ok(notice);
        };
        let members = agents::members(&self.storage, group_id)?;
        let me = my_account(&members, &group);
        for record in self.storage.list_workspace_repos(group_id)? {
            if record.repo == tasks::TASKS_REPO {
                continue;
            }
            notice.projects.push(record.repo.clone());
            if !self.storage.workspace_repo_is_files(group_id, &record.repo)? {
                continue;
            }
            let current = custody::state(&self.storage, &group, &members, &record.repo)?;
            let keeping: HashSet<&String> = current
                .keepers
                .iter()
                .filter(|account| members.iter().any(|m| &m.account_id == *account && m.status == "member"))
                .collect();
            if me.as_ref().is_some_and(|me| keeping.len() == 1 && keeping.contains(me)) {
                notice.only_custodian_of.push(record.repo);
            }
        }
        notice.projects.sort();
        notice.only_custodian_of.sort();
        Ok(notice)
    }
}

pub(crate) fn clear_workspaces_for_profile(db_path: &str) {
    if let Ok(mut workspaces) = WORKSPACES.lock() {
        workspaces.remove(db_path);
    }
    files::clear_history_cache();
    versions::clear_version_caches();
}

/// Tests: the file a node serves for object `id` of a files project (a version record or content).
#[cfg(test)]
pub(crate) fn test_object_path(published: &Path, work: Option<&Path>, id: &str) -> Option<PathBuf> {
    let version = versions::version_path(published, id);
    if version.is_file() {
        return Some(version);
    }
    versions::Here::read(published, work).file(id, None)
}

/// Tests: a files project's version as path → content id.
#[cfg(test)]
pub(crate) fn test_tree(published: &Path, repo: &str, id: &str) -> std::collections::BTreeMap<String, String> {
    versions::tree(published, repo, id).unwrap().iter().map(|(p, c)| (p.clone(), c.id.clone())).collect()
}

fn with_profile<T>(db_path: &str, f: impl FnOnce(&mut ProfileWorkspaces) -> T) -> Option<T> {
    WORKSPACES.lock().ok().map(|mut map| f(map.entry(db_path.to_string()).or_default()))
}

/// The active profile's own folder under `folder` (`<folder>/<profile id>`). The profiles of one
/// database shared `<folder>/<group>`: two of them in one group published one repository, and the
/// second announced the first one's local branches as its own (blind audit 2026-11-12, pass 17).
pub(crate) fn own_folder(storage: &Storage, folder: omni_storage::ProfileFolder) -> OmniResult<PathBuf> {
    let profile = storage
        .get_active_profile_id()?
        .ok_or_else(|| OmniError::Storage("no active profile".to_string()))?;
    Ok(folder.path(storage.db_path())?.join(profile.0))
}

/// Where this profile's published repository of `repo` in `group_id` lives.
fn published_path(storage: &Storage, group_id: &str, repo: &str) -> OmniResult<PathBuf> {
    Ok(own_folder(storage, omni_storage::ProfileFolder::Workspaces)?.join(group_id).join(format!("{repo}.git")))
}

/// A path git takes: no `\\?\` prefix (canonicalize adds it on Windows).
fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_string())
}

/// An account id as one segment of a ref name: kept when plain, hex otherwise.
fn account_segment(account_id: &str) -> String {
    let plain = !account_id.is_empty()
        && account_id.len() <= 128
        && account_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if plain {
        account_id.to_string()
    } else {
        format!("x{}", hex::encode(account_id.as_bytes()))
    }
}

fn run_git(dir: &Path, args: &[&str]) -> OmniResult<String> {
    let out = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| OmniError::Transport(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(OmniError::InvalidInput(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl OmniApp {
    /// Add `repo` to the workspace group `group_id` here: create our published bare repository and,
    /// with `work_path`, point that working repository's remote `omni` at it. Adding again only
    /// rebinds the working repository. Members' branches start arriving with the next mesh tick.
    pub fn workspace_add_repo(&self, group_id: &str, repo: &str, work_path: Option<&str>) -> OmniResult<WorkspaceRepoRecord> {
        let _ = self.ensure_runtime_profile_active("workspace_add_repo")?;
        validate_workspace_repo_name(repo)?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        if group.my_status != "member" {
            return Err(OmniError::InvalidInput("not an active member of the group".into()));
        }
        if self.storage.workspace_repo_is_files(group_id, repo)? {
            return match work_path {
                Some(folder) => files::bind_folder(&self.storage, group_id, repo, folder),
                None => files::ensure_project(&self.storage, group_id, repo),
            };
        }
        let published = published_path(&self.storage, group_id, repo)?;
        if !published.join("HEAD").is_file() {
            std::fs::create_dir_all(&published).map_err(|e| OmniError::Storage(format!("workspace folder: {e}")))?;
            run_git(&published, &["init", "--quiet", "--bare", "--initial-branch=main"])?;
        }
        let work_path = match work_path {
            Some(path) => Some(bind_work_repo(Path::new(path), &published)?),
            None => self.storage.get_workspace_repo(group_id, repo)?.and_then(|r| r.work_path),
        };
        let existing = self.storage.get_workspace_repo(group_id, repo)?;
        let record = WorkspaceRepoRecord {
            group_id: group_id.to_string(),
            repo: repo.to_string(),
            published_path: published.to_string_lossy().into_owned(),
            work_path,
            announced_refs_json: existing.as_ref().and_then(|r| r.announced_refs_json.clone()),
            created_at_unix_ms: existing.map_or_else(current_unix_ms, |r| r.created_at_unix_ms),
        };
        self.storage.upsert_workspace_repo(&record)?;
        Ok(record)
    }

    pub fn workspace_repos(&self, group_id: &str) -> OmniResult<Vec<WorkspaceRepoRecord>> {
        let _ = self.ensure_runtime_profile_active("workspace_repos")?;
        self.storage.list_workspace_repos(group_id)
    }

    /// The branches members announced in `repo`, and whether they are here yet.
    pub fn workspace_member_branches(&self, group_id: &str, repo: &str) -> OmniResult<Vec<WorkspaceMemberRefRecord>> {
        let _ = self.ensure_runtime_profile_active("workspace_member_branches")?;
        self.storage.list_workspace_member_refs(group_id, repo)
    }

    /// Announce our branches of `repo` now (the tick does it on its own within seconds; this is the
    /// shell's "Publish" and the stand's way not to wait).
    pub fn workspace_publish(&self, group_id: &str, repo: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_publish")?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let record = self
            .storage
            .get_workspace_repo(group_id, repo)?
            .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
        let db_path = self.storage.db_path().to_string_lossy().into_owned();
        let members = agents::members(&self.storage, group_id)?;
        if self.storage.workspace_repo_is_files(group_id, repo)? {
            files::announce_tip(&self.storage, &group, &members, &record)?;
        } else {
            with_profile(&db_path, |p| p.seen.remove(&(group_id.to_string(), repo.to_string())));
            announce_if_changed(&self.storage, &db_path, &group, &members, &record)?;
        }
        self.flush_group_feed();
        Ok(())
    }

    /// The shared `main` of `repo`, brought up to date first.
    pub fn workspace_main(&self, group_id: &str, repo: &str) -> OmniResult<WorkspaceMainState> {
        let _ = self.ensure_runtime_profile_active("workspace_main")?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let record = self
            .storage
            .get_workspace_repo(group_id, repo)?
            .ok_or_else(|| OmniError::InvalidInput("no such repository in the workspace".into()))?;
        advance_main(&self.storage, &group, &agents::members(&self.storage, group_id)?, &record)
    }

    /// An admin sets who moves the shared `main` of the code project `repo`: any maintainer, or only
    /// to an approved commit.
    pub fn workspace_set_main_rule(&self, group_id: &str, repo: &str, needs_approval: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_set_main_rule")?;
        validate_workspace_repo_name(repo)?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let members = agents::members(&self.storage, group_id)?;
        if group.my_status != "member" || !is_admin(&members, &group.my_member_pub) {
            return Err(OmniError::InvalidInput("only a group admin sets the workspace rule".into()));
        }
        let rule = WorkspaceRuleV1 { repo: repo.to_string(), main_needs_approval: needs_approval, changed_at_unix_ms: current_unix_ms() };
        let stored = WorkspaceMainRuleRecord { main_needs_approval: needs_approval, changed_at_unix_ms: rule.changed_at_unix_ms };
        do_and_say(&self.storage, group_id, GroupAppKindV1::WorkspaceRule, &rule, |storage| {
            storage.put_workspace_main_rule_if_newer(group_id, repo, &stored).map(|_| ())
        })?;
        self.flush_group_feed();
        Ok(())
    }

    /// Approve `oid` for the shared `main` of `repo` (it counts for any maintainer but ourselves).
    pub fn workspace_approve(&self, group_id: &str, repo: &str, oid: &str) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("workspace_approve")?;
        let approval = WorkspaceApprovalV1 { repo: repo.to_string(), oid: oid.to_ascii_lowercase() };
        validate_workspace_approval_v1(&approval)?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let members = agents::members(&self.storage, group_id)?;
        let me = my_account(&members, &group).ok_or_else(|| OmniError::InvalidInput("not an active member of the group".into()))?;
        do_and_say(&self.storage, group_id, GroupAppKindV1::WorkspaceApprove, &approval, |storage| {
            storage.add_workspace_approval(group_id, repo, &me, &approval.oid, current_unix_ms()).map(|_| ())
        })?;
        self.flush_group_feed();
        Ok(())
    }
}

fn is_admin(members: &[GroupMemberRecord], member_pub: &str) -> bool {
    members.iter().any(|m| m.member_pub == member_pub && m.status == "member" && m.role == "admin")
}

fn my_account(members: &[GroupMemberRecord], group: &GroupRecord) -> Option<String> {
    if group.my_status != "member" {
        return None;
    }
    members.iter().find(|m| m.member_pub == group.my_member_pub).map(|m| m.account_id.clone())
}

/// Maintainers are accounts with an admin device in the group: any of the account's devices moves
/// `main` (docs/decisions/omni-workspace-git.md section 7).
fn maintainers(members: &[GroupMemberRecord]) -> HashSet<String> {
    members.iter().filter(|m| m.status == "member" && m.role == "admin").map(|m| m.account_id.clone()).collect()
}

/// Send one workspace line to the group.
fn say(storage: &Storage, group_id: &str, kind: GroupAppKindV1, body: &impl serde::Serialize) -> OmniResult<()> {
    do_and_say(storage, group_id, kind, body, |_| Ok(()))
}

/// Make a change here and send its workspace line to the group, as one (`act_and_say`).
fn do_and_say<T>(
    storage: &Storage,
    group_id: &str,
    kind: GroupAppKindV1,
    body: &impl serde::Serialize,
    here: impl FnOnce(&Storage) -> OmniResult<T>,
) -> OmniResult<T> {
    let body = serde_json::to_string(body).map_err(|e| OmniError::Internal(e.to_string()))?;
    crate::app_groups::act_and_say(
        storage,
        group_id,
        &GroupAppPayloadV1 {
            message_id: crate::app_groups::random_group_message_id(),
            kind,
            body,
            authored_at_unix_ms: current_unix_ms(),
            signature_b64: None,
            reply_to: None,
            mentions: Vec::new(),
        },
        here,
    )
}

/// Git searches parent directories. A selected workspace folder must be its own repository root:
/// otherwise configuring or saving it would mutate an unrelated ancestor repository.
fn require_work_root(work: &Path) -> OmniResult<()> {
    if !work.join(".git").exists() {
        return Err(OmniError::InvalidInput("the workspace folder no longer has its own git repository".into()));
    }
    let work = plain_path(&std::fs::canonicalize(work).map_err(|e| OmniError::InvalidInput(format!("workspace folder: {e}")))?);
    let top = run_git(&work,&["rev-parse","--show-toplevel"])?;
    let top = plain_path(&std::fs::canonicalize(top).map_err(|e| OmniError::InvalidInput(format!("repository root: {e}")))?);
    #[cfg(windows)]
    let same = top.to_string_lossy().eq_ignore_ascii_case(&work.to_string_lossy());
    #[cfg(not(windows))]
    let same = top == work;
    if !same { return Err(OmniError::InvalidInput("choose the repository root, not a folder inside another repository".into())); }
    Ok(())
}

/// The working repository at `work` gets the remote `omni` = `published`; its canonical path.
fn bind_work_repo(work: &Path, published: &Path) -> OmniResult<String> {
    let work = plain_path(&std::fs::canonicalize(work).map_err(|e| OmniError::InvalidInput(format!("no folder {}: {e}", work.display())))?);
    require_work_root(&work)?;
    let url = published.to_string_lossy().replace('\\', "/");
    if run_git(&work, &["remote", "get-url", "omni"]).is_ok() {
        run_git(&work, &["remote", "set-url", "omni", &url])?;
    } else {
        run_git(&work, &["remote", "add", "omni", &url])?;
    }
    for (spec, pattern) in WORK_FETCH_SPECS {
        // Replaces our line if there is one, adds it otherwise: binding again adds nothing.
        run_git(&work, &["config", "--replace-all", "remote.omni.fetch", spec, pattern])?;
    }
    Ok(work.to_string_lossy().into_owned())
}

/// The published repository's branch files, as a number that changes when any of them does.
fn branch_files_fingerprint(published: &Path) -> u64 {
    let mut files = Vec::new();
    let mut dirs = vec![published.join("refs").join("heads")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs.push(entry.path());
            } else {
                files.push((entry.path(), meta.len(), meta.modified().ok()));
            }
        }
    }
    files.sort();
    let mut hasher = DefaultHasher::new();
    files.hash(&mut hasher);
    let packed = std::fs::metadata(published.join("packed-refs")).ok();
    packed.map(|m| (m.len(), m.modified().ok())).hash(&mut hasher);
    hasher.finish()
}

/// What a git directory's refs, `HEAD` and index are (sizes and times of their files): none when
/// `git_dir` is not a directory (a worktree's `.git` file: then nothing is known without git).
fn git_dir_fingerprint(git_dir: &Path) -> Option<u64> {
    if !git_dir.is_dir() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    tree_stats(&git_dir.join("refs"), None).hash(&mut hasher);
    for name in ["HEAD", "packed-refs", "index"] {
        std::fs::metadata(git_dir.join(name)).ok().map(|m| (m.len(), m.modified().ok())).hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Every file under `dir` (path, size, time), sorted; a directory named `skip` is left out.
fn tree_stats(dir: &Path, skip: Option<&str>) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if skip.is_none_or(|skip| entry.file_name() != skip) {
                    dirs.push(entry.path());
                }
            } else {
                files.push((entry.path(), meta.len(), meta.modified().ok()));
            }
        }
    }
    files.sort();
    files
}

/// Who is in the group and with which role: a change of either is told to the members again
/// ([`tell_newcomers`]) — a demoted admin no longer says its decisions, the admins now do.
fn members_fingerprint(members: &[GroupMemberRecord]) -> u64 {
    let mut pubs: Vec<(&str, &str)> =
        members.iter().filter(|m| m.status == "member").map(|m| (m.member_pub.as_str(), m.role.as_str())).collect();
    pubs.sort_unstable();
    let mut hasher = DefaultHasher::new();
    pubs.hash(&mut hasher);
    hasher.finish()
}

/// Our branches in the published repository, as an announcement takes them (malformed names and
/// any beyond the cap left out).
fn published_branches(published: &Path) -> OmniResult<Vec<WorkspaceRefV1>> {
    let listed = run_git(published, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads/"])?;
    let mut refs = Vec::new();
    for line in listed.lines() {
        let Some((oid, name)) = line.split_once(' ') else { continue };
        let one = WorkspaceRefV1 { name: name.to_string(), oid: oid.to_string() };
        if validate_workspace_refs_v1(&WorkspaceRefsV1 { repo: "r".into(), refs: vec![one.clone()] }).is_ok() {
            refs.push(one);
        }
    }
    refs.truncate(WORKSPACE_REFS_MAX);
    Ok(refs)
}

/// Announce our branches of the repository when they changed since the last announcement, or the
/// group's members did (a newcomer has not heard them). `true` when an announcement went out.
fn announce_if_changed(
    storage: &Storage,
    db_path: &str,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<bool> {
    let key = (record.group_id.clone(), record.repo.clone());
    let published = Path::new(&record.published_path);
    let now_seen = (branch_files_fingerprint(published), members_fingerprint(members));
    let before = with_profile(db_path, |p| p.seen.get(&key).copied()).flatten();
    if before == Some(now_seen) {
        return Ok(false);
    }
    let refs = published_branches(published)?;
    let json = serde_json::to_string(&refs).map_err(|e| OmniError::Internal(e.to_string()))?;
    let refs_changed = record.announced_refs_json.as_deref() != Some(json.as_str());
    let members_changed = before.is_none_or(|(_, members)| members != now_seen.1);
    let nothing_to_say = refs.is_empty() && record.announced_refs_json.is_none();
    let sent = (refs_changed || members_changed) && !nothing_to_say;
    if sent {
        do_and_say(storage, &group.group_id, GroupAppKindV1::WorkspaceRefs, &WorkspaceRefsV1 { repo: record.repo.clone(), refs }, |storage| {
            storage.upsert_workspace_repo(&WorkspaceRepoRecord { announced_refs_json: Some(json), ..record.clone() })
        })?;
        omni_app_trace_log("workspace/announce", &format!("group={} repo={}", group.group_id, record.repo));
    }
    with_profile(db_path, |p| p.seen.insert(key, now_seen));
    Ok(sent)
}

/// A member's `WorkspaceRefs` line: replaces what that account announced in the repository before.
pub(crate) fn receive_refs(storage: &Storage, group_id: &str, sender: &MemberCard, body: &str, now_unix_ms: i64) -> OmniResult<()> {
    let refs: WorkspaceRefsV1 =
        serde_json::from_str(body).map_err(|e| OmniError::InvalidInput(format!("workspace announcement: {e}")))?;
    validate_workspace_refs_v1(&refs)?;
    let mut refs = refs;
    // An agent's branches are `agent/*` only; others it announces are not taken (section 5).
    if agents::is_agent(storage, &sender.account_id) {
        refs.refs.retain(|r| r.name.starts_with(&format!("refs/heads/{}", agents::AGENT_BRANCH_PREFIX)));
    }
    // A files project of that name has no branches: its lines come as `WorkspaceFilesEventV1::Tip`.
    if storage.workspace_repo_is_files(group_id, &refs.repo)? {
        return Ok(());
    }
    let before: HashMap<String, String> = storage
        .list_workspace_member_refs(group_id, &refs.repo)?
        .into_iter()
        .filter(|r| r.account_id == sender.account_id)
        .map(|r| (r.ref_name, r.oid))
        .collect();
    let pairs: Vec<(String, String)> = refs.refs.into_iter().map(|r| (r.name, r.oid)).collect();
    storage.replace_workspace_member_refs(group_id, &refs.repo, &sender.account_id, &sender.member_pub, &pairs, now_unix_ms)?;
    if let Some(line) = branches_line(&sender.display_name, &refs.repo, &before, &pairs) {
        say_in_feed(storage, group_id, Some(&sender.account_id), Some(&refs.repo), &line, now_unix_ms)?;
    }
    Ok(())
}

/// After we join, the members' branches arrive all at once: that is what is there, not news — the
/// Workspaces page shows it. Lines start after this long.
const FEED_QUIET_AFTER_JOIN_MS: i64 = 2 * 60 * 1000;
/// Branches named in one line; the rest are counted.
const FEED_BRANCHES_NAMED: usize = 3;

/// An event of the workspace as a system line of the group's thread (docs/decisions/omni-workspace-ux.md:
/// «опубликовал ветку», «main обновился»). Each node writes its own, from what it saw; nothing more goes
/// over the wire. Not about our own account (the other device of ours did what we know), and not in
/// the burst right after we joined.
///
/// `about` is what the line is about — a project, or [`NEWS_AGENTS`] — for «Что нового» on the projects'
/// page, which keeps the same lines apart from the members' words, each leading to its place.
fn say_in_feed(storage: &Storage, group_id: &str, about_account: Option<&str>, about: Option<&str>, line: &str, now_unix_ms: i64) -> OmniResult<()> {
    let Some(group) = storage.get_group(group_id)? else { return Ok(()) };
    if now_unix_ms - group.created_at_unix_ms < FEED_QUIET_AFTER_JOIN_MS {
        return Ok(());
    }
    if let Some(account) = about_account {
        if my_account(&agents::members(storage, group_id)?, &group).as_deref() == Some(account) {
            return Ok(());
        }
    }
    crate::app_groups::insert_system_line(storage, group_id, line, now_unix_ms)?;
    let mut news = news(storage, group_id);
    news.push(NewsLine { at_unix_ms: now_unix_ms, text: line.to_string(), about: about.unwrap_or_default().to_string() });
    let excess = news.len().saturating_sub(NEWS_KEPT);
    news.drain(..excess);
    let json = serde_json::to_string(&news).map_err(|e| OmniError::Internal(e.to_string()))?;
    storage.upsert_profile_meta(&news_key(group_id), &json)
}

/// «Что нового» lines kept per workspace, the latest.
const NEWS_KEPT: usize = 60;
/// A «Что нового» line about the agents, not a project: it leads to the agents' place.
pub(crate) const NEWS_AGENTS: &str = "#agents";

/// A line of «Что нового»: when, what, and the project (or [`NEWS_AGENTS`]) it is about, empty when
/// about the workspace as a whole.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NewsLine {
    pub at_unix_ms: i64,
    pub text: String,
    #[serde(default)]
    pub about: String,
}

fn news_key(group_id: &str) -> String {
    format!("workspace_news:{group_id}")
}

/// The workspace's «Что нового», oldest first.
pub(crate) fn news(storage: &Storage, group_id: &str) -> Vec<NewsLine> {
    storage.get_profile_meta(&news_key(group_id)).ok().flatten().and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default()
}

/// «Аня · api: новая ветка feat/x; обновлена main» — what a member's announcement changed, or none.
fn branches_line(who: &str, repo: &str, before: &HashMap<String, String>, now: &[(String, String)]) -> Option<String> {
    let short = |name: &str| name.strip_prefix("refs/heads/").unwrap_or(name).to_string();
    let added: Vec<String> = now.iter().filter(|(n, _)| !before.contains_key(n)).map(|(n, _)| short(n)).collect();
    let moved: Vec<String> = now.iter().filter(|(n, o)| before.get(n).is_some_and(|b| b != o)).map(|(n, _)| short(n)).collect();
    let gone: Vec<String> = before.keys().filter(|n| !now.iter().any(|(m, _)| m == *n)).map(|n| short(n)).collect();
    let named = |list: &[String]| {
        let mut text = list.iter().take(FEED_BRANCHES_NAMED).cloned().collect::<Vec<_>>().join(", ");
        if list.len() > FEED_BRANCHES_NAMED {
            text.push_str(&format!(" и ещё {}", list.len() - FEED_BRANCHES_NAMED));
        }
        text
    };
    let mut parts = Vec::new();
    for (list, one, many) in [(&added, "новая ветка", "новые ветки"), (&moved, "обновлена", "обновлены"), (&gone, "убрана", "убраны")] {
        if !list.is_empty() {
            parts.push(format!("{} {}", if list.len() == 1 { one } else { many }, named(list)));
        }
    }
    (!parts.is_empty()).then(|| format!("{who} · {repo}: {}", parts.join("; ")))
}

/// An admin's `WorkspaceRule` line: kept when it is newer than the rule we have.
/// How far ahead of our clock a workspace rule may be dated.
const RULE_CLOCK_AHEAD_MS: i64 = 5 * 60 * 1000;

pub(crate) fn receive_rule(storage: &Storage, group_id: &str, sender: &MemberCard, body: &str) -> OmniResult<()> {
    if !is_admin(&agents::members(storage, group_id)?, &sender.member_pub) {
        omni_app_trace_log("workspace/rule", &format!("group={group_id} a rule from a non-admin ignored"));
        return Ok(());
    }
    let rule: WorkspaceRuleV1 = serde_json::from_str(body).map_err(|e| OmniError::InvalidInput(format!("workspace rule: {e}")))?;
    validate_workspace_repo_name(&rule.repo)?;
    // Not ahead of our clock: a newer rule wins, and one dated i64::MAX stood over every later
    // change, the admin who sent it demoted or not (blind audit 2026-11-12, pass 16).
    if rule.changed_at_unix_ms > crate::current_unix_ms() + RULE_CLOCK_AHEAD_MS {
        omni_app_trace_log("workspace/rule", &format!("group={group_id} a rule dated ahead ignored"));
        return Ok(());
    }
    let stored = WorkspaceMainRuleRecord { main_needs_approval: rule.main_needs_approval, changed_at_unix_ms: rule.changed_at_unix_ms };
    storage.put_workspace_main_rule_if_newer(group_id, &rule.repo, &stored)?;
    Ok(())
}

/// A member's `WorkspaceApprove` line, kept under its account.
pub(crate) fn receive_approval(storage: &Storage, group_id: &str, sender: &MemberCard, body: &str, now_unix_ms: i64) -> OmniResult<()> {
    let approval: WorkspaceApprovalV1 =
        serde_json::from_str(body).map_err(|e| OmniError::InvalidInput(format!("workspace approval: {e}")))?;
    validate_workspace_approval_v1(&approval)?;
    // An agent's approval does not count (section 5).
    if agents::is_agent(storage, &sender.account_id) {
        omni_app_trace_log("workspace/approve", &format!("group={group_id} an agent's approval ignored"));
        return Ok(());
    }
    storage.add_workspace_approval(group_id, &approval.repo, &sender.account_id, &approval.oid, now_unix_ms)
}

/// Serving gate: the published repository of `repo` in `group_id`, when `peer` is the node of
/// another member of that group and we are in it; and whether that member is a device of our own
/// account (it alone gets the branches «на мои устройства»).
pub(crate) fn published_repo_for_member_node(db_path: &str, group_id: &str, repo: &str, peer: &MeshNodeAddress) -> OmniResult<(PathBuf, bool)> {
    let storage = Storage::open(db_path)?;
    let group = storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
    if group.my_status != "member" {
        return Err(OmniError::InvalidInput("not in the group".into()));
    }
    let peer_hex = hex::encode(peer.as_bytes());
    let members = agents::members(&storage, group_id)?;
    let Some(member) = members
        .iter()
        .find(|m| m.status == "member" && m.member_pub != group.my_member_pub && m.node_hint_hex.eq_ignore_ascii_case(&peer_hex))
    else {
        return Err(OmniError::InvalidInput("the node is not a member's".into()));
    };
    // By the account's own list of its devices, not by the account a card names: a card's account was
    // a claim before cards carried device chains (audit 2026-11-09, F01), and a revoked device is no
    // longer ours.
    let own_device = is_own_live_device(&storage, &member.member_pub);
    let record = storage
        .get_workspace_repo(group_id, repo)?
        .ok_or_else(|| OmniError::InvalidInput("no such repository published here".into()))?;
    Ok((PathBuf::from(record.published_path), own_device))
}

/// Whether `member_pub` (a group member's key, base64) is the signing key of a device of our own
/// account that is not revoked.
pub(crate) fn is_own_live_device(storage: &Storage, member_pub: &str) -> bool {
    let Ok(Some(account)) = storage.get_local_account() else {
        return false;
    };
    let Ok(devices) = storage.list_account_devices(&account.descriptor.account_id) else {
        return false;
    };
    // Both marks of a revocation are read: the device's, and the signed revocation itself (fail
    // closed — one that cannot be read is taken as revoked), as the 1:1 path does (pass 18).
    devices.iter().any(|device| {
        device.revoked_at_unix_ms.is_none()
            && base64::Engine::encode(&base64::engine::general_purpose::STANDARD, device.chain.device.identity_signing_public_key.0) == member_pub
            && !storage.is_device_revoked(&account.descriptor.account_id, &device.chain.device.device_id).unwrap_or(true)
    })
}

/// The mesh tick: announce what changed in our published repositories, and start a fetch for every
/// member whose announced branches are not here yet.
pub(crate) fn workspace_tick(storage: &Storage, mesh: &Arc<MeshRuntime>) {
    forget_projects_of_groups_left(storage);
    let Ok(groups) = storage.list_groups() else { return };
    let db_path = storage.db_path().to_string_lossy().into_owned();
    for group in groups.into_iter().filter(|g| g.my_status == "member") {
        // A files repository announced before this node kept a record of one (an older build, a
        // failed write) has branches here and nothing that fetches them: its record is made now.
        for repo in storage.list_workspace_repo_names(&group.group_id).unwrap_or_default() {
            if storage.workspace_repo_is_files(&group.group_id, &repo).unwrap_or(false)
                && storage.get_workspace_repo(&group.group_id, &repo).ok().flatten().is_none()
            {
                if let Err(error) = custody::ensure_repo(storage, &group.group_id, &repo) {
                    omni_app_trace_log("workspace/files", &format!("group={} repo={repo} no record: {error}", group.group_id));
                }
            }
        }
        let Ok(repos) = storage.list_workspace_repos(&group.group_id) else { continue };
        if repos.is_empty() {
            continue;
        }
        let Ok(members) = agents::members(storage, &group.group_id) else { continue };
        for record in &repos {
            if storage.workspace_repo_is_files(&group.group_id, &record.repo).unwrap_or(false) {
                crate::tick_cost::mark("workspace-before-files");
                files::files_tick(&db_path, mesh, &group, &members, record);
                crate::tick_cost::mark("workspace-files");
                continue;
            }
            if let Err(error) = announce_if_changed(storage, &db_path, &group, &members, record) {
                omni_app_trace_log("workspace/announce", &format!("group={} repo={} failed: {error}", group.group_id, record.repo));
            }
            start_due_fetches(storage, &db_path, mesh, &group, &members, record);
            ask_own_devices(&db_path, mesh, &group, &members, record);
            if let Err(error) = advance_main_if_changed(storage, &db_path, &group, &members, record) {
                omni_app_trace_log("workspace/main", &format!("group={} repo={} failed: {error}", group.group_id, record.repo));
            }
            crate::tick_cost::mark("workspace-code");
        }
        if let Err(error) = tell_newcomers(storage, &group, &members, &repos) {
            omni_app_trace_log("workspace/rule", &format!("group={} failed: {error}", group.group_id));
        }
    }
}

/// Newcomers missed the rule and the approvals: when the members change, an admin says the rule
/// again (if one was ever set) and every member its approval of the shared `main` it holds.
pub(crate) fn tell_newcomers(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    repos: &[WorkspaceRepoRecord],
) -> OmniResult<()> {
    // Kept on disk: in memory, every start took all the members for newcomers and said all of our
    // own events to the group again, a frame each.
    let now_seen = members_fingerprint(members).to_string();
    if storage.group_newcomers_told(&group.group_id)?.as_deref() == Some(now_seen.as_str()) {
        return Ok(());
    }
    custody::tell_newcomers(storage, group, members)?;
    let me = my_account(members, group);
    for record in repos {
        if storage.workspace_repo_is_files(&group.group_id, &record.repo)? {
            if let Some(shared) = versions::read_ref(Path::new(&record.published_path), versions::Ref::Shared)? {
                custody::endorse_shared(storage, group, members, &record.repo, &shared.id)?;
            }
            continue;
        }
        let rule = storage.get_workspace_main_rule(&group.group_id, &record.repo)?;
        if rule.changed_at_unix_ms > 0 && is_admin(members, &group.my_member_pub) {
            let rule = WorkspaceRuleV1 { repo: record.repo.clone(), main_needs_approval: rule.main_needs_approval, changed_at_unix_ms: rule.changed_at_unix_ms };
            say(storage, &group.group_id, GroupAppKindV1::WorkspaceRule, &rule)?;
        }
        let Some(me) = me.as_deref() else { continue };
        let Some(shared) = ref_oid(Path::new(&record.published_path), SHARED_MAIN)? else { continue };
        let mine = storage.list_workspace_approvals(&group.group_id, &record.repo)?.into_iter().any(|(by, oid)| by == me && oid == shared);
        if mine {
            say(storage, &group.group_id, GroupAppKindV1::WorkspaceApprove, &WorkspaceApprovalV1 { repo: record.repo.clone(), oid: shared })?;
        }
    }
    // Marked once all of it is said: marked first, a line that failed was never said again — the
    // newcomers stayed untold (blind audit 2026-11-11, pass 13). Said twice does no harm.
    storage.set_group_newcomers_told(&group.group_id, Some(&now_seen))
}

/// Work the shared `main` out again when anything it depends on changed since the last time.
fn advance_main_if_changed(
    storage: &Storage,
    db_path: &str,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<()> {
    let key = (record.group_id.clone(), record.repo.clone());
    let mut hasher = DefaultHasher::new();
    let rule = storage.get_workspace_main_rule(&group.group_id, &record.repo)?;
    (rule.main_needs_approval, rule.changed_at_unix_ms).hash(&mut hasher);
    let mut maintainers: Vec<String> = maintainers(members).into_iter().collect();
    maintainers.sort_unstable();
    maintainers.hash(&mut hasher);
    storage.workspace_repo_is_files(&record.group_id, &record.repo)?.hash(&mut hasher);
    members_fingerprint(members).hash(&mut hasher);
    for r in storage.list_workspace_member_refs(&record.group_id, &record.repo)?.iter().filter(|r| r.ref_name == MAIN) {
        (&r.account_id, &r.oid, r.fetched).hash(&mut hasher);
    }
    storage.list_workspace_approvals(&record.group_id, &record.repo)?.hash(&mut hasher);
    for event in storage.list_workspace_file_events(&group.group_id)? {
        if !matches!(serde_json::from_str::<omni_protocol::WorkspaceFilesEventV1>(&event.event_json),Ok(omni_protocol::WorkspaceFilesEventV1::Inventory { .. })) {
            event.event_json.hash(&mut hasher);
        }
    }
    branch_files_fingerprint(Path::new(&record.published_path)).hash(&mut hasher);
    let now_seen = hasher.finish();
    if with_profile(db_path, |p| p.main_seen.get(&key) == Some(&now_seen)) == Some(true) {
        return Ok(());
    }
    advance_main(storage, group, members, record)?;
    with_profile(db_path, |p| p.main_seen.insert(key, now_seen));
    Ok(())
}

/// The approvals that count now: of accounts in the group now, not agents. One given by a member
/// that has left since moves nothing (code audit 2026-11-09, OMNI-06); the shared `main` only moves
/// forward, so what it already took stays.
fn current_approvals(members: &[GroupMemberRecord], is_agent: impl Fn(&str) -> bool, approvals: Vec<(String, String)>) -> Vec<(String, String)> {
    let in_group: HashSet<&str> = members
        .iter()
        .filter(|m| m.status == "member" && !m.account_id.is_empty())
        .map(|m| m.account_id.as_str())
        .collect();
    approvals
        .into_iter()
        .filter(|(by, _)| in_group.contains(by.as_str()) && !is_agent(by))
        .collect()
}

/// Move the shared `main` of the repository forward where the rule allows, and say where it is.
///
/// The candidates are the maintainers' `main` whose commits are here (ours from our own branches).
/// One ahead of the shared `main` (it descends from it) and allowed by the rule moves it — the one
/// all the others allowed lead to; allowed ones that went different ways move nothing. A candidate
/// that neither descends from the shared `main` nor is behind it is a fork.
fn advance_main(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<WorkspaceMainState> {
    if storage.workspace_repo_is_files(&group.group_id, &record.repo)? {
        return files::advance(storage, group, members, record);
    }
    let published = Path::new(&record.published_path);
    let rule = storage.get_workspace_main_rule(&group.group_id, &record.repo)?;
    let maintainers = maintainers(members);
    let mut candidates: Vec<(String, String)> = Vec::new();
    if let Some(me) = my_account(members, group).filter(|me| maintainers.contains(me)) {
        if let Some(oid) = ref_oid(published, MAIN)? {
            candidates.push((me, oid));
        }
    }
    for r in storage.list_workspace_member_refs(&record.group_id, &record.repo)? {
        if r.ref_name == MAIN && r.fetched && maintainers.contains(&r.account_id) {
            candidates.push((r.account_id, r.oid));
        }
    }
    candidates.sort();
    candidates.dedup();
    let approvals = current_approvals(members, |by| agents::is_agent(storage, by), storage.list_workspace_approvals(&record.group_id, &record.repo)?);
    let allowed = |(account, oid): &(String, String)| {
        !rule.main_needs_approval || approvals.iter().any(|(by, approved)| approved == oid && by != account)
    };

    let before = ref_oid(published, SHARED_MAIN)?;
    let ancestor = |a: &str, b: &str| is_ancestor(published, a, b);
    let mut state = next_shared_main(before.as_deref(), &candidates, allowed, ancestor)?;
    if let Some(tip) = state.oid.as_deref().filter(|tip| before.as_deref() != Some(*tip)) {
        let mut args = vec!["update-ref", "-m", "omni: shared main", SHARED_MAIN, tip];
        args.extend(before.as_deref());
        run_git(published, &args)?;
        omni_app_trace_log(
            "workspace/main",
            &format!("group={} repo={} {} -> {tip}", record.group_id, record.repo, before.as_deref().unwrap_or("none")),
        );
        let subject = run_git(published, &["log", "-1", "--format=%s", tip]).unwrap_or_default();
        say_in_feed(storage, &group.group_id, None, Some(&record.repo), &format!("{}: общий main обновлён — «{subject}»", record.repo), crate::current_unix_ms())?;
        // «#12» in the commits the shared main took: every node moves it, one writes it into the
        // tasks — the maintainer whose `main` it is (the first by account when several hold it).
        let mover = candidates.iter().filter(|(_, oid)| oid == tip).map(|(account, _)| account).min();
        if mover.is_some() && mover == my_account(members, group).as_ref() {
            if let Err(error) = mention_tasks(storage, group, members, record, before.as_deref(), tip) {
                omni_app_trace_log("workspace/tasks", &format!("group={} repo={}: {error}", record.group_id, record.repo));
            }
        }
    }
    state.needs_approval = rule.main_needs_approval;
    let db_path = storage.db_path().to_string_lossy().into_owned();
    with_profile(&db_path, |p| p.main_state.insert((record.group_id.clone(), record.repo.clone()), state.clone()));
    Ok(state)
}

/// Commits the shared `main` took, at most this many, are read for «#12».
const MENTIONS_COMMITS_MAX: &str = "200";

/// The tasks the commits from `before` to `tip` name («#12», «закрывает #12»).
fn mention_tasks(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord, before: Option<&str>, tip: &str) -> OmniResult<()> {
    let published = Path::new(&record.published_path);
    let range = before.map_or(tip.to_string(), |b| format!("{b}..{tip}"));
    let log = run_git(published, &["log", "--reverse", "-n", MENTIONS_COMMITS_MAX, "--format=%H%x1f%B%x1e", &range])?;
    let db_path = storage.db_path().to_string_lossy().into_owned();
    for entry in log.split('\u{1e}') {
        let Some((oid, message)) = entry.trim().split_once('\u{1f}') else { continue };
        tasks::mentioned(&db_path, storage, group, members, &record.repo, oid.trim(), message.trim())?;
    }
    Ok(())
}

/// The shared `main` of the repository, moved first if anything it depends on changed; unchanged —
/// the state worked out last time, without git.
fn main_state(storage: &Storage, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) -> OmniResult<WorkspaceMainState> {
    if storage.workspace_repo_is_files(&group.group_id, &record.repo)? {
        return files::advance(storage, group, members, record);
    }
    let db_path = storage.db_path().to_string_lossy().into_owned();
    advance_main_if_changed(storage, &db_path, group, members, record)?;
    let key = (record.group_id.clone(), record.repo.clone());
    match with_profile(&db_path, |p| p.main_state.get(&key).cloned()).flatten() {
        Some(state) => Ok(state),
        None => advance_main(storage, group, members, record),
    }
}

/// Where the shared `main` goes from `shared`, given the maintainers' `main` (account, commit), which
/// of them the rule allows and the commit graph (`ancestor(a, b)`: `a` is `b` or behind it).
fn next_shared_main(
    shared: Option<&str>,
    candidates: &[(String, String)],
    allowed: impl Fn(&(String, String)) -> bool,
    ancestor: impl Fn(&str, &str) -> OmniResult<bool>,
) -> OmniResult<WorkspaceMainState> {
    let mut shared = shared.map(str::to_string);
    loop {
        let (mut ahead, mut forks) = (Vec::new(), Vec::new());
        for candidate in candidates {
            match shared.as_deref() {
                Some(at) if at == candidate.1 => {}
                Some(at) if ancestor(at, &candidate.1)? => ahead.push(candidate.clone()),
                Some(at) if !ancestor(&candidate.1, at)? => forks.push(candidate.clone()),
                Some(_) => {}
                None => ahead.push(candidate.clone()),
            }
        }
        let (ready, waiting): (Vec<_>, Vec<_>) = ahead.into_iter().partition(|c| allowed(c));
        let mut tip = None;
        for (_, candidate) in &ready {
            let mut leads_here = true;
            for (_, other) in &ready {
                if other != candidate && !ancestor(other, candidate)? {
                    leads_here = false;
                    break;
                }
            }
            if leads_here {
                tip = Some(candidate.clone());
                break;
            }
        }
        match tip {
            Some(tip) => shared = Some(tip),
            None => {
                // Allowed ones that went different ways are forks too, until a maintainer merges them.
                forks.extend(ready);
                return Ok(WorkspaceMainState { oid: shared, needs_approval: false, waiting, forks, rejected: Vec::new() });
            }
        }
    }
}

/// The commit `name` points at in the repository, if it exists.
fn ref_oid(dir: &Path, name: &str) -> OmniResult<Option<String>> {
    let listed = run_git(dir, &["for-each-ref", "--format=%(objectname) %(refname)", name])?;
    Ok(listed.lines().find_map(|line| line.split_once(' ').filter(|(_, n)| *n == name).map(|(oid, _)| oid.to_string())))
}

/// `ancestor` is `descendant` or one of its ancestors.
fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> OmniResult<bool> {
    let status = git_command()
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| OmniError::Transport(format!("cannot run git: {e}")))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(OmniError::InvalidInput(format!("git merge-base {ancestor} {descendant} failed"))),
    }
}

fn start_due_fetches(
    storage: &Storage,
    db_path: &str,
    mesh: &Arc<MeshRuntime>,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) {
    let Ok(refs) = storage.list_workspace_member_refs(&group.group_id, &record.repo) else { return };
    let mut wanted: HashMap<String, Vec<WorkspaceMemberRefRecord>> = HashMap::new();
    for r in refs.into_iter().filter(|r| !r.fetched) {
        wanted.entry(r.account_id.clone()).or_default().push(r);
    }
    for (account_id, branches) in wanted {
        // The device that announced first, then the account's other devices: all hold the branches.
        let announcer = branches[0].member_pub.clone();
        let mut devices: Vec<&GroupMemberRecord> = members
            .iter()
            .filter(|m| m.status == "member" && m.account_id == account_id && m.member_pub != group.my_member_pub)
            .collect();
        devices.sort_by_key(|m| m.member_pub != announcer);
        let linked = |of: &mut dyn Iterator<Item = &GroupMemberRecord>| -> Vec<MeshNodeAddress> {
            let mut nodes: Vec<MeshNodeAddress> =
                of.filter_map(|m| crate::app_groups::node_from_hex(&m.node_hint_hex)).filter(|node| mesh.has_link(node)).collect();
            nodes.dedup();
            nodes
        };
        // No link to the member's devices (two agents, members who are not contacts): from another
        // member who took the branches already — only the commits its author announced are kept.
        let mut nodes = linked(&mut devices.iter().copied());
        let through_holder = nodes.is_empty();
        if through_holder {
            nodes = linked(&mut members.iter().filter(|m| m.status == "member" && m.account_id != account_id && m.member_pub != group.my_member_pub));
        }
        if nodes.is_empty() {
            continue;
        }
        let key = (group.group_id.clone(), record.repo.clone(), account_id.clone());
        let begun = with_profile(db_path, |p| {
            let resting = p.failed_at.get(&key).is_some_and(|at| at.elapsed() < FETCH_RETRY_AFTER);
            !resting && p.fetching.insert(key.clone())
        });
        if begun != Some(true) {
            continue;
        }
        let (db, mesh, record, thread_key) = (db_path.to_string(), Arc::clone(mesh), record.clone(), key.clone());
        let spawned = std::thread::Builder::new().name("omni-workspace-fetch".to_string()).spawn(move || {
            let key = thread_key;
            let outcome = fetch_and_mark(&db, &mesh, &record, &account_id, &branches, &nodes, through_holder);
            let ok = outcome.is_ok();
            match outcome {
                Ok(received) => omni_app_trace_log(
                    "workspace/fetch",
                    &format!("group={} repo={} account={account_id} received={received} through_holder={through_holder}", record.group_id, record.repo),
                ),
                Err(error) => omni_app_trace_log(
                    "workspace/fetch",
                    &format!("group={} repo={} account={account_id} failed: {error}", record.group_id, record.repo),
                ),
            }
            with_profile(&db, |p| {
                p.fetching.remove(&key);
                if ok {
                    p.failed_at.remove(&key);
                } else {
                    p.failed_at.insert(key.clone(), Instant::now());
                }
            });
        });
        if spawned.is_err() {
            with_profile(db_path, |p| p.fetching.remove(&key));
        }
    }
}

/// «На мои устройства»: every linked device of our account in the group is asked, once a minute, for
/// the branches it sent to our devices; they land in `refs/omni/devices/<its node>/heads/*` (what it
/// took back is pruned). Nothing about them goes to the group.
fn ask_own_devices(db_path: &str, mesh: &Arc<MeshRuntime>, group: &GroupRecord, members: &[GroupMemberRecord], record: &WorkspaceRepoRecord) {
    let Some(me) = my_account(members, group) else { return };
    for device in members.iter().filter(|m| m.status == "member" && m.account_id == me && m.member_pub != group.my_member_pub) {
        let Some(node) = crate::app_groups::node_from_hex(&device.node_hint_hex).filter(|node| mesh.has_link(node)) else { continue };
        let key = (group.group_id.clone(), record.repo.clone(), device.node_hint_hex.to_ascii_lowercase());
        let begun = with_profile(db_path, |p| {
            let asked_lately = p.own_asked_at.get(&key).is_some_and(|at| at.elapsed() < OWN_DEVICE_FETCH_EVERY);
            if asked_lately || p.fetching.contains(&key) {
                return false;
            }
            p.own_asked_at.insert(key.clone(), Instant::now());
            p.fetching.insert(key.clone())
        });
        if begun != Some(true) {
            continue;
        }
        let (db, mesh, record, thread_key) = (db_path.to_string(), Arc::clone(mesh), record.clone(), key.clone());
        let spawned = std::thread::Builder::new().name("omni-workspace-own".to_string()).spawn(move || {
            let refspec = format!("+{MINE}*:{DEVICES}{}/heads/*", thread_key.2);
            let open = |node: MeshNodeAddress| {
                mesh.open_bulk_stream(node).map_err(|e| OmniError::Transport(format!("git stream to our device: {e}")))
            };
            let published = PathBuf::from(&record.published_path);
            let space = format!("{DEVICES}{}/", thread_key.2);
            let before = run_git(&published, &["for-each-ref", &space]).unwrap_or_default();
            match crate::app_git_bridge::fetch_member_branches(&published, &record.group_id, &record.repo, &refspec, &[node], &open) {
                // Logged when the device's branches changed, not every minute it is asked.
                Ok(received) if run_git(&published, &["for-each-ref", &space]).unwrap_or_default() != before => omni_app_trace_log(
                    "workspace/own",
                    &format!("group={} repo={} device={} received={received}", record.group_id, record.repo, thread_key.2),
                ),
                Ok(_) => {}
                Err(error) => omni_app_trace_log(
                    "workspace/own",
                    &format!("group={} repo={} device={} failed: {error}", record.group_id, record.repo, thread_key.2),
                ),
            }
            with_profile(&db, |p| p.fetching.remove(&thread_key));
        });
        if spawned.is_err() {
            with_profile(db_path, |p| p.fetching.remove(&key));
        }
    }
}

/// Fetch one member's branches, then mark those whose commit is here now.
fn fetch_and_mark(
    db_path: &str,
    mesh: &Arc<MeshRuntime>,
    record: &WorkspaceRepoRecord,
    account_id: &str,
    branches: &[WorkspaceMemberRefRecord],
    nodes: &[MeshNodeAddress],
    through_holder: bool,
) -> OmniResult<u64> {
    let published = Path::new(&record.published_path);
    let open = |node: MeshNodeAddress| {
        mesh.open_bulk_stream(node).map_err(|e| OmniError::Transport(format!("git stream to the member: {e}")))
    };
    // From an agent only its `agent/*` branches (section 5).
    let segment = account_segment(account_id);
    let prefix = if agents::is_agent(&Storage::open(db_path)?, account_id) { agents::AGENT_BRANCH_PREFIX } else { "" };
    let storage = Storage::open(db_path)?;
    let mark = |present: &HashSet<String>| -> OmniResult<()> {
        for branch in branches.iter().filter(|b| present.contains(&b.oid)) {
            storage.mark_workspace_member_ref_fetched(&record.group_id, &record.repo, account_id, &branch.ref_name, &branch.oid)?;
        }
        Ok(())
    };
    if !through_holder {
        let refspec = format!("+refs/heads/{prefix}*:refs/omni/members/{segment}/heads/{prefix}*");
        let received = crate::app_git_bridge::fetch_member_branches(published, &record.group_id, &record.repo, &refspec, nodes, &open)?;
        mark(&commits_present(published, branches.iter().map(|b| b.oid.as_str()))?)?;
        return Ok(received);
    }
    // From a holder: its copy of the member's branches lands aside; a branch takes the commit its
    // author announced, never the holder's word for it.
    let refspec = format!("+refs/omni/members/{segment}/heads/{prefix}*:refs/omni/relay/{segment}/heads/{prefix}*");
    let mut received = 0;
    let mut last_error = None;
    for node in nodes {
        match crate::app_git_bridge::fetch_member_branches(published, &record.group_id, &record.repo, &refspec, &[*node], &open) {
            Ok(bytes) => received += bytes,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        }
        let present = keep_announced(published, &segment, branches)?;
        mark(&present)?;
        if branches.iter().all(|b| present.contains(&b.oid)) {
            break;
        }
    }
    match last_error {
        Some(error) if received == 0 => Err(error),
        _ => Ok(received),
    }
}

/// After a fetch from a holder: each announced branch whose commit came is set to that commit in the
/// member's space, whatever the holder's copy named; the holder's copy is dropped. The commits here.
fn keep_announced(published: &Path, segment: &str, branches: &[WorkspaceMemberRefRecord]) -> OmniResult<HashSet<String>> {
    let present = commits_present(published, branches.iter().map(|b| b.oid.as_str()))?;
    for branch in branches.iter().filter(|b| present.contains(&b.oid)) {
        let Some(name) = branch.ref_name.strip_prefix("refs/heads/") else { continue };
        run_git(published, &["update-ref", &format!("refs/omni/members/{segment}/heads/{name}"), &branch.oid])?;
    }
    for aside in run_git(published, &["for-each-ref", "--format=%(refname)", &format!("refs/omni/relay/{segment}/")])?.lines() {
        run_git(published, &["update-ref", "-d", aside])?;
    }
    Ok(present)
}

/// Which of `oids` are commits in the repository at `dir`.
fn commits_present<'a>(dir: &Path, oids: impl Iterator<Item = &'a str>) -> OmniResult<HashSet<String>> {
    let mut child = git_command()
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "--batch-check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| OmniError::Transport(format!("cannot run git: {e}")))?;
    {
        // At most WORKSPACE_REFS_MAX short lines each way: the pipes hold them, no deadlock.
        let mut stdin = child.stdin.take().expect("piped");
        for oid in oids {
            let _ = writeln!(stdin, "{oid}");
        }
    }
    let mut said = String::new();
    let _ = child.stdout.take().expect("piped").read_to_string(&mut said);
    let _ = child.wait();
    Ok(said
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(' ');
            match (parts.next(), parts.next()) {
                (Some(oid), Some("commit")) => Some(oid.to_string()),
                _ => None,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A little history: r -> a -> b (A's line), r -> c (B went another way), b -> m <- c (a merge).
    fn ancestor(a: &str, b: &str) -> OmniResult<bool> {
        let behind: &[&str] = match b {
            "r" => &["r"],
            "a" => &["r", "a"],
            "b" => &["r", "a", "b"],
            "c" => &["r", "c"],
            "m" => &["r", "a", "b", "c", "m"],
            _ => &[],
        };
        Ok(behind.contains(&a))
    }

    fn pair(account: &str, oid: &str) -> (String, String) {
        (account.to_string(), oid.to_string())
    }

    /// A holder's copy of a member's branches is taken at the commits the member announced: a holder
    /// that names another commit for the branch does not move it there.
    #[test]
    fn a_branch_taken_from_a_holder_keeps_the_commit_its_author_announced() {
        let root = std::env::temp_dir().join(format!("omni-keep-announced-{}", hex::encode(omni_crypto::generate_group_key())));
        let (work, ours) = (root.join("work"), root.join("ours.git"));
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &Path, args: &[&str]| run_git(dir, args).unwrap();
        git(&root, &["init", "--quiet", "--initial-branch=main", &work.to_string_lossy()]);
        git(&root, &["init", "--quiet", "--bare", &ours.to_string_lossy()]);
        let commit = |text: &str| {
            std::fs::write(work.join("f.txt"), text).unwrap();
            git(&work, &["add", "."]);
            git(&work, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", text]);
            git(&work, &["rev-parse", "HEAD"])
        };
        let (announced, other) = (commit("announced"), commit("other"));
        // What a holder's fetch leaves aside: the holder names `other` for the branch.
        git(&work, &["push", "--quiet", &ours.to_string_lossy(), &format!("{other}:refs/omni/relay/acct/heads/agent/x")]);
        let branch = |oid: &str| WorkspaceMemberRefRecord {
            account_id: "acct".into(),
            member_pub: "m".into(),
            ref_name: "refs/heads/agent/x".into(),
            oid: oid.into(),
            announced_at_unix_ms: 0,
            fetched: false,
        };
        let present = keep_announced(&ours, "acct", &[branch(&announced)]).unwrap();
        assert!(present.contains(&announced), "the announced commit came with the holder's copy");
        assert_eq!(git(&ours, &["rev-parse", "refs/omni/members/acct/heads/agent/x"]), announced);
        assert!(git(&ours, &["for-each-ref", "refs/omni/relay/"]).is_empty(), "the holder's copy is dropped");
        // A commit the holder does not have is not marked and names nothing.
        let missing = "0".repeat(40);
        assert!(!keep_announced(&ours, "acct", &[branch(&missing)]).unwrap().contains(&missing));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_shared_main_only_goes_forward_and_leaves_a_fork_to_people() {
        let any = |_: &(String, String)| true;
        // The first maintainer's main starts it; the furthest of a line wins whatever the order.
        let first = next_shared_main(None, &[pair("ann", "a"), pair("bob", "b")], any, ancestor).unwrap();
        assert_eq!(first.oid.as_deref(), Some("b"));
        assert!(first.forks.is_empty());
        // A maintainer behind it changes nothing; one that went another way is a fork, kept apart.
        let fork = next_shared_main(Some("b"), &[pair("ann", "r"), pair("bob", "c")], any, ancestor).unwrap();
        assert_eq!(fork.oid.as_deref(), Some("b"));
        assert_eq!(fork.forks, vec![pair("bob", "c")]);
        // Two allowed ways from the start move nothing: both are forks.
        let split = next_shared_main(Some("r"), &[pair("ann", "b"), pair("bob", "c")], any, ancestor).unwrap();
        assert_eq!(split.oid.as_deref(), Some("r"));
        assert_eq!(split.forks.len(), 2);
        // A merge of both settles it.
        let merged = next_shared_main(Some("b"), &[pair("ann", "m"), pair("bob", "c")], any, ancestor).unwrap();
        assert_eq!(merged.oid.as_deref(), Some("m"));
        assert!(merged.forks.is_empty());
    }

    #[test]
    fn under_the_approval_rule_the_shared_main_waits_for_another_accounts_approval() {
        let approvals = [pair("ann", "b"), pair("bob", "a")];
        let allowed = |(account, oid): &(String, String)| approvals.iter().any(|(by, o)| o == oid && by != account);
        // Ann's own approval of her b does not count; Bob's of a does.
        let state = next_shared_main(Some("r"), &[pair("ann", "b")], allowed, ancestor).unwrap();
        assert_eq!(state.oid.as_deref(), Some("r"));
        assert_eq!(state.waiting, vec![pair("ann", "b")]);
        let state = next_shared_main(Some("r"), &[pair("ann", "a"), pair("ann", "b")], allowed, ancestor).unwrap();
        assert_eq!(state.oid.as_deref(), Some("a"), "the approved step, not past it");
        assert_eq!(state.waiting, vec![pair("ann", "b")]);
    }

    #[test]
    fn an_approval_counts_only_from_an_account_in_the_group_now() {
        // Code audit 2026-11-09, OMNI-06.
        let member = |account: &str, status: &str| GroupMemberRecord {
            member_pub: format!("pub-{account}"),
            account_id: account.into(),
            display_name: account.into(),
            role: "member".into(),
            status: status.into(),
            node_hint_hex: String::new(),
        };
        let members = [member("ann", "member"), member("bob", "member"), member("cid", "left"), member("bot", "member")];
        let approvals = vec![pair("bob", "a"), pair("cid", "b"), pair("dan", "c"), pair("bot", "d")];
        let counted = current_approvals(&members, |by| by == "bot", approvals);
        assert_eq!(counted, vec![pair("bob", "a")], "one who left, one never here, and an agent do not count");
    }

    #[test]
    fn a_members_announcement_reads_as_one_line_of_what_changed() {
        let pair = |n: &str, o: &str| (format!("refs/heads/{n}"), o.to_string());
        let before: HashMap<String, String> = [pair("main", "a"), pair("old", "b")].into_iter().collect();
        let now = vec![pair("main", "c"), pair("feat/x", "d")];
        assert_eq!(
            branches_line("Аня", "api", &before, &now).as_deref(),
            Some("Аня · api: новая ветка feat/x; обновлена main; убрана old")
        );
        assert_eq!(branches_line("Аня", "api", &before, &[pair("main", "a"), pair("old", "b")]), None, "the same again is no news");
        let many: Vec<(String, String)> = (1..=5).map(|i| pair(&format!("b{i}"), "e")).collect();
        assert_eq!(
            branches_line("Аня", "api", &HashMap::new(), &many).as_deref(),
            Some("Аня · api: новые ветки b1, b2, b3 и ещё 2")
        );
    }

    #[test]
    fn an_account_id_becomes_one_plain_ref_segment() {
        assert_eq!(account_segment("acct_1-2"), "acct_1-2");
        assert_eq!(account_segment("a/b"), format!("x{}", hex::encode("a/b")));
        assert_eq!(account_segment(""), "x");
    }
}
