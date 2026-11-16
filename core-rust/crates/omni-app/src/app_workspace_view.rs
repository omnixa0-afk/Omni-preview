//! What the shell's workspace screen shows and the actions it takes (docs/decisions/omni-workspace-ux.md,
//! phase 1: the Code tab, «main обновился — Забрать», «Опубликовать ветку», «Забрать сюда»). Read from
//! the published repository and the person's working repository; every rule stays in `app_workspace`.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use omni_protocol::{validate_workspace_refs_v1, validate_workspace_repo_name, WorkspaceRefV1, WorkspaceRefsV1};
use omni_storage::{GroupMemberRecord, GroupRecord, Storage, WorkspaceMemberRefRecord, WorkspaceRepoRecord};
use omni_types::{OmniError, OmniResult};

use super::{
    git_dir_fingerprint, is_admin, is_ancestor, main_state, maintainers, my_account, ref_oid, run_git, with_profile, WorkspaceFilesSync,
    WorkspaceMainState, DEVICES, MAIN, MINE,
};
use crate::OmniApp;

/// One commit as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceCommitView {
    pub oid: String,
    pub subject: String,
    pub author: String,
    pub at_unix_ms: i64,
}

/// A member's branch tip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceTipView {
    pub account_id: String,
    /// The member's name in the group.
    pub name: String,
    pub is_me: bool,
    /// Without `refs/heads/`.
    pub branch: String,
    pub oid: String,
    /// None: the commit is not here yet (being fetched, or the member's devices are away).
    pub commit: Option<WorkspaceCommitView>,
}

/// A branch of the person's working repository.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceLocalBranchView {
    pub branch: String,
    pub oid: String,
    pub subject: String,
    /// The group has it at this very commit (a `main` at the shared one counts).
    pub published: bool,
    /// Sent «на мои устройства» at this very commit.
    pub on_my_devices: bool,
}

/// A branch another device of ours sent «на мои устройства», against the same branch here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceDeviceBranchView {
    /// The device's name («ПК», «Ноутбук»), its node when we do not know it.
    pub device: String,
    /// Its node (hex): what «Забрать» names.
    pub node: String,
    pub branch: String,
    pub oid: String,
    pub commit: Option<WorkspaceCommitView>,
    /// new (no such branch here) | newer (ours is behind it) | same | older (ours is ahead) |
    /// diverged | unknown (ours has commits the device's copy cannot be compared with yet).
    pub state: String,
}

/// One repository of the workspace.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceRepoView {
    pub repo: String,
    /// False: members have it, this device has not taken it yet («Забрать сюда»).
    pub here: bool,
    pub work_path: Option<String>,
    /// The shared `main`.
    pub main: Option<WorkspaceCommitView>,
    /// Code: the project's `main` moves only to a commit another member approved. Files: members'
    /// versions wait for an admin or a custodian.
    pub needs_approval: bool,
    /// Commits the working repository's `main` is behind the shared one (its `main` not born yet
    /// counts the whole shared `main`); 0 when even, ahead or gone another way.
    pub main_behind: u32,
    /// Maintainers' `main` ahead of the shared one, waiting for an approval.
    pub waiting: Vec<WorkspaceTipView>,
    /// Maintainers' `main` that went another way.
    pub forks: Vec<WorkspaceTipView>,
    /// Other members' branches.
    pub branches: Vec<WorkspaceTipView>,
    pub local_branches: Vec<WorkspaceLocalBranchView>,
    /// What our other devices sent «на мои устройства».
    pub device_branches: Vec<WorkspaceDeviceBranchView>,
    /// The repository holds files (models, drawings, documents): versions, not branches.
    pub files: bool,
    /// Files: the shared `main` is version N (its commits counted).
    pub version: u32,
    /// Files: what changed in the folder and is not saved (the first 20), and how many.
    pub changed: Vec<String>,
    pub changed_count: usize,
    /// Files: where the folder is against the shared `main`.
    pub sync: WorkspaceFilesSync,
    /// Files: how many files the shared version has and how big they are together.
    pub file_count: usize,
    pub total_bytes: u64,
    /// The latest versions (files) or commits of the shared `main` (code), newest first (a few).
    pub recent: Vec<WorkspaceCommitView>,
    /// Files: our saved versions the shared line does not hold yet (none: everything we saved is in).
    pub own: Option<WorkspaceOwnVersionView>,
    /// Files: the folder holds the shared version (false while it catches up, or without a folder).
    pub folder_current: bool,
    /// Files: the project's folders this device does not take into its working folder.
    pub left_out: Vec<String>,
    /// Files: what our account holds («Беру в работу»).
    pub taken: Vec<String>,
    /// Files: what we changed or took that someone else holds (they took it first).
    pub held_elsewhere: Vec<WorkspaceHeldView>,
    /// Files: the shared version's files kept with conflict copies beside them («выбрать итоговую»).
    pub conflicts: Vec<String>,
    /// Files: revisions «На нормоконтроле» someone else released (ours waits for another reviewer).
    pub revisions_on_check: Vec<String>,
    /// The workspace's tasks project: shown as «Задачи», not as a project card.
    pub is_tasks: bool,
}

/// A file someone else holds that we changed or took too.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceHeldView {
    pub path: String,
    pub name: String,
}

/// Where our saves stand that are not in the shared version of a files project: the folder holds
/// them, members get them once they are accepted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceOwnVersionView {
    /// The latest of them.
    pub commit: WorkspaceCommitView,
    /// How many of our versions the shared line lacks.
    pub count: u32,
    pub state: WorkspaceOwnVersionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOwnVersionState {
    /// An admin or a custodian has to accept it.
    Waiting,
    /// A reviewer turned it down: it stays here only.
    Rejected,
    /// Accepted by the rule; the shared line takes it on the next round.
    Joining,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceView {
    /// Projects this account keeps (custodians are appointed per project).
    pub custodian_of: Vec<String>,
    /// Projects whose custody this account is asked for and has not answered.
    pub custody_asked: Vec<String>,
    pub i_am_admin: bool,
    pub i_am_maintainer: bool,
    /// Another device of our account is in the group: «На мои устройства» has somewhere to go.
    pub i_have_other_devices: bool,
    /// Our agents' questions to us nobody answered yet: the count on the agents' tab.
    pub agent_asks: usize,
    /// «Что нового»: what others did here lately, newest first.
    pub news: Vec<crate::app_workspace::NewsLine>,
    pub repos: Vec<WorkspaceRepoView>,
}

/// What «Забрать» did to the working repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePullOutcome {
    /// `main` moved forward to the shared one (or was checked out in an empty repository).
    Updated,
    UpToDate,
    /// Changes in the working tree: fetched only, the person brings `main` forward in the IDE.
    Dirty,
    /// Another branch is checked out: fetched only.
    NotOnMain,
    /// The local `main` went another way: fetched only, merging is the person's.
    Diverged,
    /// No shared `main` yet.
    NoMain,
}

/// The workspace screen read in two parts: what the database says ([`OmniApp::workspace_view_snapshot`],
/// under the FFI serializer) and what git says ([`WorkspaceViewSnapshot::render`], without it). Read
/// whole under the serializer, three repositories took 3 s where one git process costs ~100 ms, and
/// every other call of the shell (the group list, the settings) waited behind it: switching sections
/// froze for seconds.
pub struct WorkspaceViewSnapshot {
    db_path: String,
    group_id: String,
    group: GroupRecord,
    members: Vec<GroupMemberRecord>,
    me: Option<String>,
    /// Our devices' names by node.
    devices: HashMap<String, String>,
    /// The screen without its repositories.
    view: WorkspaceView,
    repos: Vec<RepoSnapshot>,
}

enum RepoSnapshot {
    Here(RepoRead),
    /// Not taken here: everything about it is in the database.
    Offered(WorkspaceRepoView),
}

/// What a repository's card needs from the database, read under the serializer.
struct RepoRead {
    record: WorkspaceRepoRecord,
    refs: Vec<WorkspaceMemberRefRecord>,
    files: bool,
}

/// What a repository's card is drawn from: the database's part and where the shared `main` is.
struct RepoInputs {
    record: WorkspaceRepoRecord,
    state: WorkspaceMainState,
    refs: Vec<WorkspaceMemberRefRecord>,
    files: bool,
}

impl OmniApp {
    /// The workspace screen of `group_id`: every repository with its shared `main`, what waits, forks,
    /// members' branches and the working repository's branches.
    pub fn workspace_view(&self, group_id: &str) -> OmniResult<WorkspaceView> {
        self.workspace_view_snapshot(group_id)?.render()
    }

    /// The database part of the screen; the shared `main` is moved in [`WorkspaceViewSnapshot::render`].
    pub fn workspace_view_snapshot(&self, group_id: &str) -> OmniResult<WorkspaceViewSnapshot> {
        let _ = self.ensure_runtime_profile_active("workspace_view_snapshot")?;
        let group = self.storage.get_group(group_id)?.ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
        let members = crate::app_workspace::agents::members(&self.storage, group_id)?;
        let me = my_account(&members, &group);
        let mut repos = Vec::new();
        for repo in self.storage.list_workspace_repo_names(group_id)? {
            repos.push(match self.storage.get_workspace_repo(group_id, &repo)? {
                Some(record) => RepoSnapshot::Here(RepoRead {
                    refs: self.storage.list_workspace_member_refs(group_id, &repo)?,
                    files: self.storage.workspace_repo_is_files(group_id, &repo)?,
                    record,
                }),
                None => RepoSnapshot::Offered(offered_repo_view(&self.storage, &members, me.as_deref(), group_id, &repo)?),
            });
        }
        let (mut custodian_of, mut custody_asked) = (Vec::new(), Vec::new());
        for repo in self.storage.list_workspace_repo_names(group_id)? {
            if !self.storage.workspace_repo_is_files(group_id, &repo)? { continue; }
            let custody = super::custody::state(&self.storage,&group,&members,&repo)?;
            if me.as_ref().is_some_and(|me| custody.keepers.contains(me)) {
                custodian_of.push(repo.clone());
            }
            let asked = me.as_ref().and_then(|me| custody.nominations.get(me)).is_some_and(|(_,active)| *active)
                && !custody.archive_members.contains(&group.my_member_pub) && !custody.declined_members.contains(&group.my_member_pub);
            if asked { custody_asked.push(repo); }
        }
        let view = WorkspaceView {
            custodian_of,
            custody_asked,
            i_am_admin: group.my_status == "member" && is_admin(&members, &group.my_member_pub),
            i_am_maintainer: me.as_ref().is_some_and(|me| maintainers(&members).contains(me)),
            i_have_other_devices: me.as_ref().is_some_and(|me| {
                members.iter().any(|m| m.status == "member" && &m.account_id == me && m.member_pub != group.my_member_pub)
            }),
            agent_asks: me.as_deref().map_or(0, |me| crate::app_workspace::agents::open_asks_for(&self.storage, group_id, me)),
            news: crate::app_workspace::news(&self.storage, group_id).into_iter().rev().take(NEWS_SHOWN).collect(),
            repos: Vec::new(),
        };
        Ok(WorkspaceViewSnapshot {
            db_path: self.storage.db_path().to_string_lossy().into_owned(),
            group_id: group_id.to_string(),
            devices: device_names(&self.storage),
            group,
            members,
            me,
            view,
            repos,
        })
    }

    /// «Забрать сюда»: a repository members have, into `folder` (made, and made a git repository, when
    /// it is not one). Its code arrives with the next mesh ticks; «Забрать» then checks `main` out.
    pub fn workspace_take_repo(&self, group_id: &str, repo: &str, folder: &str) -> OmniResult<WorkspaceRepoRecord> {
        let _ = self.ensure_runtime_profile_active("workspace_take_repo")?;
        validate_workspace_repo_name(repo)?;
        if self.storage.workspace_repo_is_files(group_id, repo)? {
            return self.workspace_add_repo(group_id, repo, Some(folder));
        }
        let path = Path::new(folder);
        std::fs::create_dir_all(path).map_err(|e| OmniError::Storage(format!("cannot make {folder}: {e}")))?;
        if !path.join(".git").exists() {
            run_git(path, &["init", "--quiet", "--initial-branch=main"])?;
        }
        self.workspace_add_repo(group_id, repo, Some(folder))
    }

    /// The working folder of `repo`: the buttons that are git in it alone («Забрать», «Опубликовать
    /// ветку», «На мои устройства») run on it without the FFI serializer — a push of a big repository
    /// is seconds, and every other call of the shell would wait for it.
    pub fn workspace_folder(&self, group_id: &str, repo: &str) -> OmniResult<WorkspaceFolder> {
        // Not for a profile that signed out: «Забрать» and «Опубликовать ветку» went on changing its
        // working folder through a handle that outlived the sign-out (blind audit 2026-11-11, M7).
        let _ = self.ensure_runtime_profile_active("workspace_folder")?;
        self.storage
            .get_workspace_repo(group_id, repo)?
            .and_then(|r| r.work_path)
            .map(|path| WorkspaceFolder(PathBuf::from(path)))
            .ok_or_else(|| OmniError::InvalidInput("the repository has no working folder here".into()))
    }

    pub fn workspace_pull(&self, group_id: &str, repo: &str) -> OmniResult<WorkspacePullOutcome> {
        self.workspace_folder(group_id, repo)?.pull()
    }

    /// «Опубликовать ветку»: push `branch` of the working repository to `omni` and tell the group now.
    pub fn workspace_publish_branch(&self, group_id: &str, repo: &str, branch: &str) -> OmniResult<()> {
        self.workspace_folder(group_id, repo)?.push_branch(repo, branch)?;
        self.workspace_publish(group_id, repo)
    }

    pub fn workspace_send_to_my_devices(&self, group_id: &str, repo: &str, branch: &str) -> OmniResult<()> {
        self.workspace_folder(group_id, repo)?.send_to_my_devices(repo, branch)
    }

    pub fn workspace_take_from_device(&self, group_id: &str, repo: &str, node: &str, branch: &str) -> OmniResult<WorkspacePullOutcome> {
        self.workspace_folder(group_id, repo)?.take_from_device(repo, node, branch)
    }
}

/// A repository's working folder, for git in it alone (see [`OmniApp::workspace_folder`]).
pub struct WorkspaceFolder(PathBuf);

impl WorkspaceFolder {
    /// «Забрать»: fetch the working repository from `omni`, then bring its `main` forward to the shared
    /// one when that is safe (on `main`, nothing uncommitted, nothing of its own) — never a merge.
    pub fn pull(&self) -> OmniResult<WorkspacePullOutcome> {
        let work = self.0.as_path();
        super::require_work_root(work)?;
        run_git(work, &["fetch", "--quiet", "omni"])?;
        let Some(shared) = ref_oid(work, "refs/remotes/omni-shared/main")? else {
            return Ok(WorkspacePullOutcome::NoMain);
        };
        if run_git(work, &["symbolic-ref", "-q", "HEAD"]).ok().as_deref() != Some(MAIN) {
            return Ok(WorkspacePullOutcome::NotOnMain);
        }
        let Some(local) = ref_oid(work, MAIN)? else {
            // An empty repository taken with «Забрать сюда»: its `main` starts at the shared one.
            run_git(work, &["checkout", "--quiet", "-B", "main", "refs/remotes/omni-shared/main"])?;
            return Ok(WorkspacePullOutcome::Updated);
        };
        if local == shared || is_ancestor(work, &shared, &local)? {
            return Ok(WorkspacePullOutcome::UpToDate);
        }
        if !is_ancestor(work, &local, &shared)? {
            return Ok(WorkspacePullOutcome::Diverged);
        }
        if !run_git(work, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
            return Ok(WorkspacePullOutcome::Dirty);
        }
        run_git(work, &["merge", "--quiet", "--ff-only", "refs/remotes/omni-shared/main"])?;
        Ok(WorkspacePullOutcome::Updated)
    }

    /// «Опубликовать ветку», its git half: push `branch` to `omni` ([`OmniApp::workspace_publish`] tells the group).
    pub fn push_branch(&self, repo: &str, branch: &str) -> OmniResult<()> {
        validate_branch(repo, branch)?;
        super::require_work_root(&self.0)?;
        let name = format!("refs/heads/{branch}");
        run_git(&self.0, &["push", "--quiet", "omni", &format!("{name}:{name}")])?;
        Ok(())
    }

    /// «На мои устройства»: `branch` of the working repository goes to the account's other devices only;
    /// the group neither sees nor gets it until «Опубликовать ветку».
    pub fn send_to_my_devices(&self, repo: &str, branch: &str) -> OmniResult<()> {
        validate_branch(repo, branch)?;
        super::require_work_root(&self.0)?;
        run_git(&self.0, &["push", "--quiet", "--force", "omni", &format!("refs/heads/{branch}:{MINE}{branch}")])?;
        Ok(())
    }

    /// «Забрать» a branch another device of ours sent: made here when there is none, moved forward
    /// when ours is behind it (checked out — only on a clean tree); one that went another way is left
    /// to the person (`Diverged`).
    pub fn take_from_device(&self, repo: &str, node: &str, branch: &str) -> OmniResult<WorkspacePullOutcome> {
        validate_branch(repo, branch)?;
        super::require_work_root(&self.0)?;
        if node.len() != 40 || !node.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OmniError::InvalidInput("a device's node is 40 hex digits".into()));
        }
        let work = self.0.as_path();
        let theirs = format!("refs/remotes/omni-devices/{node}/heads/{branch}");
        run_git(work, &["fetch", "--quiet", "omni", &format!("+{DEVICES}{node}/heads/{branch}:{theirs}")])?;
        let theirs_oid = ref_oid(work, &theirs)?.ok_or_else(|| OmniError::InvalidInput("the device no longer has the branch".into()))?;
        let local_ref = format!("refs/heads/{branch}");
        let Some(local) = ref_oid(work, &local_ref)? else {
            run_git(work, &["branch", "--quiet", "--no-track", branch, &theirs])?;
            return Ok(WorkspacePullOutcome::Updated);
        };
        if local == theirs_oid || is_ancestor(work, &theirs_oid, &local)? {
            return Ok(WorkspacePullOutcome::UpToDate);
        }
        if !is_ancestor(work, &local, &theirs_oid)? {
            return Ok(WorkspacePullOutcome::Diverged);
        }
        if run_git(work, &["symbolic-ref", "-q", "HEAD"]).ok().as_deref() == Some(local_ref.as_str()) {
            if !run_git(work, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
                return Ok(WorkspacePullOutcome::Dirty);
            }
            run_git(work, &["merge", "--quiet", "--ff-only", &theirs])?;
        } else {
            run_git(work, &["update-ref", &local_ref, &theirs_oid, &local])?;
        }
        Ok(WorkspacePullOutcome::Updated)
    }
}

/// A branch name an announcement would take.
fn validate_branch(repo: &str, branch: &str) -> OmniResult<()> {
    validate_workspace_refs_v1(&WorkspaceRefsV1 {
        repo: repo.to_string(),
        refs: vec![WorkspaceRefV1 { name: format!("refs/heads/{branch}"), oid: "0".repeat(40) }],
    })
}

/// Our devices' names by their mesh node (hex).
fn device_names(storage: &Storage) -> HashMap<String, String> {
    let Ok(Some(account)) = storage.get_local_account() else { return HashMap::new() };
    storage
        .list_account_devices(&account.descriptor.account_id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            let node = omni_mesh_runtime::node_address_from_ed25519_pubkey(record.chain.device.identity_signing_public_key.0).ok()?;
            Some((hex::encode(node.as_bytes()), record.device_label))
        })
        .collect()
}

fn names(members: &[GroupMemberRecord]) -> HashMap<&str, &str> {
    members.iter().map(|m| (m.account_id.as_str(), m.display_name.as_str())).collect()
}

fn tip(names: &HashMap<&str, &str>, me: Option<&str>, account: &str, branch: &str, oid: &str, commits: &HashMap<String, WorkspaceCommitView>) -> WorkspaceTipView {
    WorkspaceTipView {
        account_id: account.to_string(),
        name: names.get(account).copied().unwrap_or(account).to_string(),
        is_me: me == Some(account),
        branch: branch.strip_prefix("refs/heads/").unwrap_or(branch).to_string(),
        oid: oid.to_string(),
        commit: commits.get(oid).cloned(),
    }
}

impl WorkspaceViewSnapshot {
    /// The git part of the screen, without the FFI serializer. The shared `main` is moved here when
    /// anything it depends on changed — ancestry, line ranks, the ref update: a few git processes, under
    /// the serializer they held every other call of the shell (a saved version froze the window for
    /// seconds, 2026-10-30) — on a database connection of its own, like the workspace's fetch threads.
    pub fn render(self) -> OmniResult<WorkspaceView> {
        let mut view = self.view;
        let here = self.repos.iter().any(|repo| matches!(repo, RepoSnapshot::Here(_)));
        let storage = if here { Some(Storage::open(&self.db_path)?) } else { None };
        for repo in self.repos {
            view.repos.push(match (repo, storage.as_ref()) {
                (RepoSnapshot::Here(read), Some(storage)) if read.files => {
                    let state = main_state(storage, &self.group, &self.members, &read.record)?;
                    let key = (self.group_id.clone(), read.record.repo.clone());
                    let sync = with_profile(&self.db_path, |p| p.files.get(&key).cloned()).flatten().unwrap_or_default();
                    let custody = super::custody::state(storage, &self.group, &self.members, &read.record.repo)?;
                    super::files::repo_card(&self.members, self.me.as_deref(), &read.record, &state, &custody, sync)?
                }
                (RepoSnapshot::Here(read), Some(storage)) => {
                    let inputs = RepoInputs {
                        state: main_state(storage, &self.group, &self.members, &read.record)?,
                        record: read.record,
                        refs: read.refs,
                        files: read.files,
                    };
                    cached_repo_view(&self.db_path, &self.group_id, &self.members, self.me.as_deref(), &self.devices, &inputs)?
                }
                (RepoSnapshot::Offered(card), _) => card,
                (RepoSnapshot::Here(_), None) => continue,
            });
        }
        Ok(view)
    }
}

/// A repository's card, read again only when something it is read from changed: the database's part,
/// the published repository's refs, the working repository's refs and index and, for files, the
/// folder's files. Where the folder is against the shared `main` is the files work's, always fresh.
fn cached_repo_view(
    db_path: &str,
    group_id: &str,
    members: &[GroupMemberRecord],
    me: Option<&str>,
    devices: &HashMap<String, String>,
    inputs: &RepoInputs,
) -> OmniResult<WorkspaceRepoView> {
    let key = (group_id.to_string(), inputs.record.repo.clone());
    let print = card_fingerprint(members, me, devices, inputs);
    let cached =
        print.and_then(|print| with_profile(db_path, |p| p.views.get(&key).filter(|(seen, _)| *seen == print).map(|(_, card)| card.clone())).flatten());
    let card = match cached {
        Some(card) => card,
        None => {
            let card = repo_view(members, me, devices, inputs)?;
            if let Some(print) = print {
                with_profile(db_path, |p| p.views.insert(key.clone(), (print, card.clone())));
            }
            card
        }
    };
    Ok(card)
}

/// What a card is read from, without git; none when the working repository's refs cannot be seen as
/// files (then the card is read every time).
fn card_fingerprint(members: &[GroupMemberRecord], me: Option<&str>, devices: &HashMap<String, String>, inputs: &RepoInputs) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    for m in members {
        (&m.member_pub, &m.account_id, &m.display_name, &m.status).hash(&mut hasher);
    }
    me.hash(&mut hasher);
    let mut devices: Vec<_> = devices.iter().collect();
    devices.sort();
    devices.hash(&mut hasher);
    let record = &inputs.record;
    (&record.published_path, &record.work_path, &inputs.state, inputs.files).hash(&mut hasher);
    for r in &inputs.refs {
        (&r.account_id, &r.ref_name, &r.oid, r.fetched).hash(&mut hasher);
    }
    git_dir_fingerprint(Path::new(&record.published_path))?.hash(&mut hasher);
    if let Some(work) = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()) {
        git_dir_fingerprint(&work.join(".git"))?.hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn repo_view(members: &[GroupMemberRecord], me: Option<&str>, devices: &HashMap<String, String>, inputs: &RepoInputs) -> OmniResult<WorkspaceRepoView> {
    let RepoInputs { record, state, refs, .. } = inputs;
    let published = Path::new(&record.published_path);
    // Our other devices' branches «на мои устройства»: refs/omni/devices/<node>/heads/<branch>.
    let from_devices: Vec<(String, String, String)> = run_git(published, &["for-each-ref", "--format=%(refname) %(objectname)", DEVICES])?
        .lines()
        .filter_map(|line| {
            let (name, oid) = line.split_once(' ')?;
            let (node, branch) = name.strip_prefix(DEVICES)?.split_once("/heads/")?;
            Some((node.to_string(), branch.to_string(), oid.to_string()))
        })
        .collect();
    let mut present: Vec<&str> = refs.iter().filter(|r| r.fetched).map(|r| r.oid.as_str()).collect();
    present.extend(state.oid.as_deref());
    present.extend(from_devices.iter().map(|(_, _, oid)| oid.as_str()));
    let commits = describe(published, &present)?;
    let names = names(members);
    let main_tip = |(account, oid): &(String, String)| tip(&names, me, account, MAIN, oid, &commits);

    let mut local_branches = Vec::new();
    let mut main_behind = 0;
    if let Some(work) = record.work_path.as_deref().map(Path::new).filter(|w| w.is_dir()) {
        let ours = run_git(published, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads/", MINE])?;
        let ours: HashMap<&str, &str> = ours.lines().filter_map(|l| l.split_once(' ')).collect();
        // A working folder that is no git repository any more (moved, half deleted) reads as none: the
        // rest of the workspace screen still shows.
        let listed = run_git(work, &["for-each-ref", "--format=%(refname)%1f%(objectname)%1f%(subject)", "refs/heads/"]).unwrap_or_default();
        for line in listed.lines() {
            let mut parts = line.splitn(3, '\u{1f}');
            let (Some(name), Some(oid)) = (parts.next(), parts.next()) else { continue };
            local_branches.push(WorkspaceLocalBranchView {
                branch: name.strip_prefix("refs/heads/").unwrap_or(name).to_string(),
                oid: oid.to_string(),
                subject: parts.next().unwrap_or_default().to_string(),
                // A `main` at the shared one has nothing to tell the group.
                published: ours.get(name) == Some(&oid) || (name == MAIN && state.oid.as_deref() == Some(oid)),
                on_my_devices: name.strip_prefix("refs/heads/").is_some_and(|b| ours.get(format!("{MINE}{b}").as_str()) == Some(&oid)),
            });
        }
        if let Some(shared) = state.oid.as_deref() {
            let local = local_branches.iter().find(|b| b.branch == "main").map(|b| b.oid.as_str());
            main_behind = behind(published, local, shared)?;
        }
    }

    let device_branches = from_devices
        .iter()
        .map(|(node, branch, oid)| {
            let local = local_branches.iter().find(|b| &b.branch == branch).map(|b| b.oid.as_str());
            WorkspaceDeviceBranchView {
                device: devices.get(node).cloned().unwrap_or_else(|| node[..8.min(node.len())].to_string()),
                node: node.clone(),
                branch: branch.clone(),
                oid: oid.clone(),
                commit: commits.get(oid).cloned(),
                state: compare(published, local, oid).to_string(),
            }
        })
        .collect();

    Ok(WorkspaceRepoView {
        repo: record.repo.clone(),
        here: true,
        work_path: record.work_path.clone(),
        main: state.oid.as_ref().and_then(|oid| commits.get(oid).cloned()),
        needs_approval: state.needs_approval,
        main_behind,
        waiting: state.waiting.iter().map(main_tip).collect(),
        forks: state.forks.iter().map(main_tip).collect(),
        branches: refs.iter().map(|r| tip(&names, me, &r.account_id, &r.ref_name, &r.oid, &commits)).collect(),
        local_branches,
        device_branches,
        files: false,
        version: 0,
        changed: Vec::new(),
        changed_count: 0,
        sync: WorkspaceFilesSync::default(),
        file_count: 0,
        total_bytes: 0,
        recent: match state.oid.as_deref() {
            Some(tip) => main_log(published, tip)?,
            None => Vec::new(),
        },
        own: None,
        folder_current: false,
        left_out: Vec::new(),
        taken: Vec::new(),
        held_elsewhere: Vec::new(),
        conflicts: Vec::new(),
        revisions_on_check: Vec::new(),
        is_tasks: record.repo == super::tasks::TASKS_REPO,
    })
}

/// «Что нового» lines the projects' page shows.
const NEWS_SHOWN: usize = 30;

/// Commits of the shared `main` a code project's page lists beside its state.
const MAIN_LOG_SHOWN: usize = 12;

/// The shared `main`'s latest commits, newest first.
fn main_log(dir: &Path, tip: &str) -> OmniResult<Vec<WorkspaceCommitView>> {
    let count = format!("-{MAIN_LOG_SHOWN}");
    let listed = run_git(dir, &["log", &count, "--format=%H%x1f%an%x1f%ct%x1f%s", tip])?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\u{1f}');
            let (oid, author, at, subject) = (parts.next()?, parts.next()?, parts.next()?, parts.next().unwrap_or_default());
            Some(WorkspaceCommitView { oid: oid.to_string(), subject: subject.to_string(), author: author.to_string(), at_unix_ms: at.parse::<i64>().unwrap_or(0) * 1000 })
        })
        .collect())
}

fn offered_repo_view(storage: &Storage, members: &[GroupMemberRecord], me: Option<&str>, group_id: &str, repo: &str) -> OmniResult<WorkspaceRepoView> {
    let names = names(members);
    let none = HashMap::new();
    Ok(WorkspaceRepoView {
        repo: repo.to_string(),
        here: false,
        work_path: None,
        main: None,
        needs_approval: false,
        main_behind: 0,
        waiting: Vec::new(),
        forks: Vec::new(),
        branches: storage
            .list_workspace_member_refs(group_id, repo)?
            .iter()
            .map(|r| tip(&names, me, &r.account_id, &r.ref_name, &r.oid, &none))
            .collect(),
        local_branches: Vec::new(),
        device_branches: Vec::new(),
        files: storage.workspace_repo_is_files(group_id, repo)?,
        version: 0,
        changed: Vec::new(),
        changed_count: 0,
        sync: WorkspaceFilesSync::default(),
        file_count: 0,
        total_bytes: 0,
        recent: Vec::new(),
        own: None,
        folder_current: false,
        left_out: Vec::new(),
        taken: Vec::new(),
        held_elsewhere: Vec::new(),
        conflicts: Vec::new(),
        revisions_on_check: Vec::new(),
        is_tasks: repo == super::tasks::TASKS_REPO,
    })
}

/// Our branch (`local`, none when there is no such branch) against a device's copy `theirs`, both
/// looked up in the published repository, which has the device's commits.
fn compare(published: &Path, local: Option<&str>, theirs: &str) -> &'static str {
    let Some(local) = local else { return "new" };
    if local == theirs {
        return "same";
    }
    if run_git(published, &["cat-file", "-e", &format!("{local}^{{commit}}")]).is_err() {
        return "unknown";
    }
    match (is_ancestor(published, local, theirs), is_ancestor(published, theirs, local)) {
        (Ok(true), _) => "newer",
        (_, Ok(true)) => "older",
        (Ok(false), Ok(false)) => "diverged",
        _ => "unknown",
    }
}

/// How many commits `local` (the working `main`, none when not born) is behind `shared`, counted in the
/// published repository, which has every commit of the shared `main`. 0 when `local` is not there
/// (commits not published) or not behind.
fn behind(published: &Path, local: Option<&str>, shared: &str) -> OmniResult<u32> {
    let range = match local {
        None => shared.to_string(),
        Some(local) if local == shared => return Ok(0),
        Some(local) => {
            if run_git(published, &["cat-file", "-e", &format!("{local}^{{commit}}")]).is_err() || !is_ancestor(published, local, shared)? {
                return Ok(0);
            }
            format!("{local}..{shared}")
        }
    };
    Ok(run_git(published, &["rev-list", "--count", &range])?.parse().unwrap_or(0))
}

/// Subject, author and time of commits that are in `dir`.
fn describe(dir: &Path, oids: &[&str]) -> OmniResult<HashMap<String, WorkspaceCommitView>> {
    let mut unique: Vec<&str> = oids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    if unique.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = vec!["log", "--no-walk=unsorted", "--format=%H%x1f%an%x1f%ct%x1f%s"];
    args.extend(unique.iter().copied());
    let listed = run_git(dir, &args)?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\u{1f}');
            let (oid, author, at, subject) = (parts.next()?, parts.next()?, parts.next()?, parts.next().unwrap_or_default());
            Some((
                oid.to_string(),
                WorkspaceCommitView {
                    oid: oid.to_string(),
                    subject: subject.to_string(),
                    author: author.to_string(),
                    at_unix_ms: at.parse::<i64>().unwrap_or(0) * 1000,
                },
            ))
        })
        .collect())
}
