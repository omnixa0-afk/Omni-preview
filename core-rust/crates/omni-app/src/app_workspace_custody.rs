//! File-workspace roles, policy, reviews and local download intent. All product rules live here.
use super::*;
use super::versions::{self, Ref};
use omni_protocol::{WorkspaceFilesEventV1 as Event, WorkspaceFilesPolicyV1};
use omni_storage::WorkspaceFileEventRecord;
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub(super) struct CustodyState {
    pub policy: WorkspaceFilesPolicyV1,
    pub keepers: HashSet<String>,
    pub archive_members: HashSet<String>,
    pub declined_members: HashSet<String>,
    pub nominations: HashMap<String, (String, bool)>,
    pub reviews: HashMap<(String, String), bool>,
    pub reviewers: HashSet<String>,
    /// Released revisions: version → (designation, note).
    pub revisions: HashMap<String, (String, String)>,
    /// Who released each revision (account) and when.
    pub released: HashMap<String, (String, i64)>,
    /// Revisions another reviewer approved under their current designation (нормоконтроль).
    pub approved_revisions: HashSet<String>,
    /// Versions marked «Сохранить навсегда».
    pub kept: HashSet<String>,
    /// The admin's confirmed retention rule; none: nothing is thinned out.
    pub retention: Option<RetentionRule>,
    /// «Беру в работу»: each file's takes neither given back nor taken back by an admin, earliest
    /// first; one that ran out stays until [`CustodyState::holder`] passes it by.
    pub takes: HashMap<String, Vec<Take>>,
}

/// One account's hold of a file (the `Lock` events of its devices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Take {
    pub account: String,
    pub since: i64,
    pub until: i64,
}

/// How long a project keeps its versions' content (the `Retention` event).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RetentionRule {
    pub keep_last: u32,
    pub keep_days: u32,
    /// Masks of files kept only in the current version, revisions and «навсегда».
    pub only_current: Vec<String>,
}

/// The retention rule offered before an admin confirms one (maintainers' decision 2026-11-01).
pub const WORKSPACE_DEFAULT_KEEP_LAST: u32 = 10;
pub const WORKSPACE_DEFAULT_KEEP_DAYS: u32 = 30;

impl CustodyState {
    /// A version is in once a reviewer's acceptance of it is recorded, or while the rule is "at once".
    /// The author's role today does not count: a reviewer's own versions are accepted by the review
    /// its node publishes when it makes them ([`endorse_own`]), so a promotion does not accept an
    /// older proposal, and a later policy change or demotion does not take back what was accepted.
    pub fn allows(&self, repo: &str, oid: &str) -> bool {
        self.accepted(repo, oid) || (!self.rejected(repo, oid) && !self.policy.participants_need_approval)
    }
    pub fn accepted(&self, repo: &str, oid: &str) -> bool {
        self.reviews.get(&(repo.into(), oid.into())) == Some(&true)
    }
    pub fn rejected(&self, repo: &str, oid: &str) -> bool {
        self.reviews.get(&(repo.into(), oid.into())) == Some(&false)
    }
    /// A revision released under the нормоконтроль rule and not approved yet («На нормоконтроле»).
    pub fn revision_pending(&self, version: &str) -> bool {
        self.policy.revisions_checked
            && !self.approved_revisions.contains(version)
            && self.released.get(version).is_some_and(|(_, at)| *at >= self.policy.revisions_checked_since_unix_ms)
    }
    /// Who holds `path` at `now`: the earliest take that has not run out — every node alike.
    pub fn holder(&self, path: &str, now: i64) -> Option<&Take> {
        self.takes.get(path)?.iter().find(|t| t.until > now)
    }
    /// The files `account` has taken and not given back, with whether it holds each (a take after
    /// someone else's earlier one does not hold).
    pub fn taken_by(&self, account: &str, now: i64) -> Vec<(String, bool)> {
        let mut taken: Vec<(String, bool)> = self
            .takes
            .iter()
            .filter(|(_, takes)| takes.iter().any(|t| t.account == account && t.until > now))
            .map(|(path, _)| (path.clone(), self.holder(path, now).is_some_and(|h| h.account == account)))
            .collect();
        taken.sort();
        taken
    }
}

/// Roles of one project (repository) of the workspace: its custodians are appointed for it alone
/// (maintainers' decision 2026-10-31); admins review every project; the policy is the workspace's.
pub(super) fn state(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    repo: &str,
) -> OmniResult<CustodyState> {
    let events = storage.list_workspace_file_events(&group.group_id)?;
    let mut result = CustodyState {
        reviewers: maintainers(members),
        ..Default::default()
    };
    for record in &events {
        match serde_json::from_str::<Event>(&record.event_json) {
            Ok(Event::Policy { repo: of, policy }) if of == repo => result.policy = policy,
            Ok(Event::Retention { repo: of, keep_last, keep_days, only_current, .. }) if of == repo => {
                result.retention = Some(RetentionRule { keep_last, keep_days, only_current })
            }
            Ok(Event::Nominate {
                account_id,
                repo: of,
                nomination,
                enabled,
                ..
            }) if of == repo => {
                result.nominations.insert(account_id, (nomination, enabled));
            }
            _ => {}
        }
    }
    for record in &events {
        match serde_json::from_str::<Event>(&record.event_json) {
            Ok(Event::Consent {
                account_id,
                repo: of,
                nomination,
                accepted: true,
                ..
            }) if of == repo
                && result
                .nominations
                .get(&account_id)
                .is_some_and(|(id, enabled)| *enabled && id == &nomination)
                && members.iter().any(|m| {
                    m.status == "member"
                        && m.member_pub == record.sender_member
                        && m.account_id == account_id
                }) =>
            {
                result.keepers.insert(account_id.clone());
                result.reviewers.insert(account_id);
                result.archive_members.insert(record.sender_member.clone());
            }
            Ok(Event::Consent {
                account_id,
                repo: of,
                nomination,
                accepted: false,
                ..
            }) if of == repo
                && result
                .nominations
                .get(&account_id)
                .is_some_and(|(id, active)| *active && id == &nomination) =>
            {
                result.declined_members.insert(record.sender_member.clone());
            }
            // Previously authenticated reviews keep their force after a demotion. They cannot be
            // replayed as a new decision by a former custodian: receive() checks current authority.
            Ok(Event::Review {
                repo: of,
                oid,
                accepted,
                ..
            }) if of == repo => {
                let value = result.reviews.entry((of, oid)).or_insert(false);
                *value |= accepted; // Acceptance is final; another reviewer cannot revoke it.
            }
            // One record per version (the latest wins); receive() checked the sender's role.
            Ok(Event::Revision { repo: of, version, label, note, at_unix_ms, .. }) if of == repo => {
                result.released.insert(version.clone(), (record.sender_account.clone(), at_unix_ms));
                result.revisions.insert(version, (label.trim().to_string(), note));
            }
            Ok(Event::Keep { repo: of, version, keep: true, .. }) if of == repo => {
                result.kept.insert(version);
            }
            _ => {}
        }
    }
    // An approval counts for the designation it names, from an account other than the releaser's.
    for record in &events {
        let Ok(Event::RevisionApproval { repo: of, version, label, .. }) = serde_json::from_str::<Event>(&record.event_json) else { continue };
        let named = result.revisions.get(&version).is_some_and(|(current, _)| current.eq_ignore_ascii_case(label.trim()));
        let other = result.released.get(&version).is_some_and(|(by, _)| by != &record.sender_account);
        if of == repo && named && other {
            result.approved_revisions.insert(version);
        }
    }
    // Each account's latest lock event per file (of any of its devices), and the admins' givebacks.
    let mut latest: HashMap<(String, String), (i64, bool, i64, i64)> = HashMap::new();
    let mut taken_back: HashMap<(String, String), i64> = HashMap::new();
    for record in &events {
        let Ok(Event::Lock { repo: of, path, take, since_unix_ms, until_unix_ms, holder, at_unix_ms }) = serde_json::from_str::<Event>(&record.event_json) else { continue };
        if of != repo {
            continue;
        }
        match holder {
            Some(holder) => {
                let at = taken_back.entry((path, holder)).or_insert(at_unix_ms);
                *at = (*at).max(at_unix_ms);
            }
            None => {
                let key = (path, record.sender_account.clone());
                if latest.get(&key).is_none_or(|(at, ..)| *at < at_unix_ms) {
                    latest.insert(key, (at_unix_ms, take, since_unix_ms, until_unix_ms));
                }
            }
        }
    }
    let accounts: HashSet<&str> = members.iter().filter(|m| m.status == "member").map(|m| m.account_id.as_str()).collect();
    for ((path, account), (_, take, since, until)) in latest {
        let given_back = taken_back.get(&(path.clone(), account.clone())).is_some_and(|at| *at >= since);
        if take && !given_back && accounts.contains(account.as_str()) {
            result.takes.entry(path).or_default().push(Take { account, since, until });
        }
    }
    for takes in result.takes.values_mut() {
        takes.sort_by(|a, b| (a.since, &a.account).cmp(&(b.since, &b.account)));
    }
    Ok(result)
}

/// How many projects a workspace takes from what its members say (a project made here by its owner
/// is not counted against: it is theirs to make).
pub(crate) const WORKSPACE_PROJECTS_MAX: usize = 64;

fn event_key(event: &Event, sender: &str) -> String {
    match event {
        Event::Policy { repo, .. } => format!("policy:{repo}"),
        Event::Nominate { account_id, repo, .. } => format!("nominate:{account_id}:{repo}"),
        Event::Consent { repo, .. } => format!("consent:{sender}:{repo}"),
        Event::Review { repo, oid, .. } => format!("review:{repo}:{oid}:{sender}"),
        Event::Inventory { repo, .. } => format!("inventory:{repo}:{sender}"),
        Event::Tip { repo, .. } => format!("tip:{repo}:{sender}"),
        Event::Revision { repo, version, .. } => format!("revision:{repo}:{version}"),
        Event::RevisionApproval { repo, version, .. } => format!("revision-approval:{repo}:{version}:{sender}"),
        Event::Keep { repo, version, .. } => format!("keep:{repo}:{version}"),
        Event::Retention { repo, .. } => format!("retention:{repo}"),
        Event::Lock { repo, path, holder: None, .. } => format!("lock:{repo}:{path}:{sender}"),
        Event::Lock { repo, path, holder: Some(holder), .. } => format!("unlock:{repo}:{path}:{holder}"),
    }
}

#[derive(Serialize, Deserialize)]
struct DeferredReview {
    deferred_review: Event,
}

fn review_authority(
    current: &CustodyState,
    me: &str,
    members: &[GroupMemberRecord],
) -> Option<String> {
    if maintainers(members).contains(me) {
        None
    } else {
        current.nominations.get(me).map(|(id, _)| id.clone())
    }
}

fn review_authorized(
    current: &CustodyState,
    sender: &str,
    members: &[GroupMemberRecord],
    authority: &Option<String>,
) -> bool {
    maintainers(members).contains(sender)
        || (current.keepers.contains(sender)
            && current
                .nominations
                .get(sender)
                .is_some_and(|(id, active)| *active && authority.as_ref() == Some(id)))
}

/// Different senders' MLS application messages can arrive in either order. Keep a review awaiting
/// its specific appointment/consent, without granting it any authority in the meantime.
fn promote_deferred_reviews(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
) -> OmniResult<()> {
    for record in storage.list_workspace_file_events(&group.group_id)? {
        let Ok(deferred) = serde_json::from_str::<DeferredReview>(&record.event_json) else {
            continue;
        };
        if deferred
            .deferred_review
            .validate(current_unix_ms())
            .is_err()
            || deferred.deferred_review.at() != record.at_unix_ms
        {
            continue;
        }
        let Event::Review { repo, authority, .. } = &deferred.deferred_review else {
            continue;
        };
        let current = state(storage, group, members, repo)?;
        if !members.iter().any(|m| {
            m.status == "member"
                && m.member_pub == record.sender_member
                && m.account_id == record.sender_account
        }) || !review_authorized(&current, &record.sender_account, members, authority)
        {
            continue;
        }
        if let Event::Review {
            repo,
            oid,
            accepted: false,
            ..
        } = &deferred.deferred_review
        {
            if current.reviews.get(&(repo.clone(), oid.clone())) == Some(&true) {
                continue;
            }
        }
        let json = serde_json::to_string(&deferred.deferred_review)
            .map_err(|e| OmniError::Internal(e.to_string()))?;
        storage.put_workspace_file_event(
            &group.group_id,
            &event_key(&deferred.deferred_review, &record.sender_member),
            &WorkspaceFileEventRecord {
                event_json: json,
                ..record
            },
        )?;
    }
    Ok(())
}

pub(crate) fn receive(
    storage: &Storage,
    group_id: &str,
    sender: &MemberCard,
    body: &str,
) -> OmniResult<()> {
    let event: Event = serde_json::from_str(body)
        .map_err(|e| OmniError::InvalidInput(format!("workspace files: {e}")))?;
    event.validate(current_unix_ms())?;
    let group = storage
        .get_group(group_id)?
        .ok_or_else(|| OmniError::InvalidInput("unknown group".into()))?;
    let members = super::agents::members(storage, group_id)?;
    if group.my_status != "member"
        || !members.iter().any(|m| {
            m.status == "member"
                && m.member_pub == sender.member_pub
                && m.account_id == sender.account_id
        })
    {
        return Err(OmniError::InvalidInput(
            "workspace sender is not a member".into(),
        ));
    }
    // The sender's role as it is now, not at the epoch the event was sent in — on purpose (owner's
    // decision 2026-11-10, docs/bugs/code-audit-classes-2026-11-09.md «Событие в полёте»): a demoted
    // admin cannot act through old epochs' keys. An event sent just before the demotion counts only
    // where it came first, until an admin sets the same thing again (the later one wins everywhere).
    let authorized = match &event {
        Event::Policy { .. } | Event::Nominate { .. } | Event::Retention { .. } => is_admin(&members, &sender.member_pub),
        Event::Consent { account_id, .. } => account_id == &sender.account_id,
        Event::Review { repo, authority, .. }
        | Event::Revision { repo, authority, .. }
        | Event::RevisionApproval { repo, authority, .. }
        | Event::Keep { repo, authority, .. } => review_authorized(
            &state(storage, &group, &members, repo)?,
            &sender.account_id,
            &members,
            authority,
        ),
        Event::Inventory { repo, .. } => state(storage, &group, &members, repo)?
            .reviewers
            .contains(&sender.account_id),
        // Any member's device has a line of its own and takes files for its account; only an admin
        // gives back someone else's.
        Event::Tip { .. } | Event::Lock { holder: None, .. } => true,
        Event::Lock { holder: Some(_), .. } => is_admin(&members, &sender.member_pub),
    };
    if !authorized {
        if let Event::Review {
            repo,
            oid,
            authority: Some(_),
            ..
        } = &event
        {
            let pending = serde_json::to_string(&DeferredReview {
                deferred_review: event.clone(),
            })
            .map_err(|e| OmniError::Internal(e.to_string()))?;
            storage.put_workspace_file_event(
                group_id,
                &format!("pending_review:{repo}:{oid}:{}", sender.member_pub),
                &WorkspaceFileEventRecord {
                    sender_account: sender.account_id.clone(),
                    sender_member: sender.member_pub.clone(),
                    event_json: pending,
                    at_unix_ms: event.at(),
                },
            )?;
            return Ok(());
        }
        return Err(OmniError::InvalidInput(
            "workspace role does not permit this action".into(),
        ));
    }
    if let Event::Review {
        repo,
        oid,
        accepted: false,
        ..
    } = &event
    {
        if state(storage, &group, &members, repo)?
            .reviews
            .get(&(repo.clone(), oid.clone()))
            == Some(&true)
        {
            return Ok(());
        }
    }
    // A nomination can only target a current participant; admins already have the authority. An
    // agent is never a custodian (docs/decisions/omni-workspace-git.md section 5).
    if let Event::Nominate { account_id, .. } = &event {
        if !members
            .iter()
            .any(|m| m.status == "member" && &m.account_id == account_id)
        {
            return Err(OmniError::InvalidInput(
                "custodian must be a workspace member".into(),
            ));
        }
        if super::agents::is_agent(storage, account_id) {
            return Err(OmniError::InvalidInput("агент не может быть хранителем".into()));
        }
    }
    // A line of a project we have not got makes the project here — a folder and a record. Up to a
    // bound: a member naming projects by the thousand had each of us make them all (blind audit
    // 2026-11-11, pass 13).
    if let Event::Tip { repo, .. } = &event {
        let known = storage.list_workspace_repos(group_id)?;
        if known.len() >= WORKSPACE_PROJECTS_MAX && !known.iter().any(|r| &r.repo == repo) {
            return Err(OmniError::InvalidInput("the workspace has as many projects as it may".into()));
        }
    }
    let changed = storage.put_workspace_file_event(
        group_id,
        &event_key(&event, &sender.member_pub),
        &WorkspaceFileEventRecord {
            sender_account: sender.account_id.clone(),
            sender_member: sender.member_pub.clone(),
            event_json: body.to_string(),
            at_unix_ms: event.at(),
        },
    )?;
    if let Event::Tip { repo, .. } = &event {
        files::ensure_project(storage, group_id, repo)?;
    }
    if changed && matches!(event, Event::Nominate { .. } | Event::Consent { .. }) {
        promote_deferred_reviews(storage, &group, &members)?;
        storage.set_group_newcomers_told(group_id, None)?;
    }
    Ok(())
}

pub(super) fn publish(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    event: Event,
) -> OmniResult<()> {
    event.validate(current_unix_ms())?;
    let me = members
        .iter()
        .find(|m| m.member_pub == group.my_member_pub && m.status == "member")
        .ok_or_else(|| OmniError::InvalidInput("not an active workspace member".into()))?;
    let body = serde_json::to_string(&event).map_err(|e| OmniError::Internal(e.to_string()))?;
    let record = WorkspaceFileEventRecord {
        sender_account: me.account_id.clone(),
        sender_member: me.member_pub.clone(),
        event_json: body,
        at_unix_ms: event.at(),
    };
    // Stored and queued as one: an unsent policy must not govern only this device, and a sent one
    // must not be missing here. The feed retries delivery while the other members are away.
    super::do_and_say(storage, &group.group_id, GroupAppKindV1::WorkspaceFiles, &event, |storage| {
        storage.put_workspace_file_event(&group.group_id, &event_key(&event, &me.member_pub), &record)?;
        if matches!(event, Event::Nominate { .. } | Event::Consent { .. }) {
            promote_deferred_reviews(storage, group, members)?;
        }
        Ok(())
    })
}

/// An explicit UI/daemon command. Slow work (reading the folder, the history) runs on a detached
/// database connection, outside FFI's serializer, just like WorkspaceViewSnapshot::render and
/// WorkspaceSaveVersion::save.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkspaceFilesCommand {
    Details {
        group_id: String,
        repo: String,
        #[serde(default)]
        oid: Option<String>,
    },
    Nominate {
        group_id: String,
        repo: String,
        account_id: String,
    },
    Dismiss {
        group_id: String,
        repo: String,
        account_id: String,
    },
    Consent {
        group_id: String,
        repo: String,
        nomination: String,
        accepted: bool,
        folder: Option<String>,
    },
    Policy {
        group_id: String,
        repo: String,
        participants_need_approval: bool,
        newcomers_download_current: bool,
        #[serde(default)]
        edit_only_taken: bool,
        #[serde(default)]
        revisions_checked: bool,
    },
    /// Нормоконтроль: approve revision `oid` released by another account.
    ApproveRevision {
        group_id: String,
        repo: String,
        oid: String,
    },
    Review {
        group_id: String,
        repo: String,
        oid: String,
        accepted: bool,
    },
    Download {
        group_id: String,
        repo: String,
        path: String,
        target: String,
        #[serde(default)]
        oid: Option<String>,
    },
    CancelDownload {
        group_id: String,
        repo: String,
    },
    Bind {
        group_id: String,
        repo: String,
        folder: String,
    },
    /// «Убрать папку с этого устройства».
    Unbind {
        group_id: String,
        repo: String,
    },
    /// The project's history: the accepted versions, newest first, with revisions and «навсегда».
    History {
        group_id: String,
        repo: String,
    },
    /// «Выпустить ревизию» of an accepted version (admin, custodian).
    Release {
        group_id: String,
        repo: String,
        oid: String,
        label: String,
        #[serde(default)]
        note: String,
    },
    /// «Сохранить навсегда» or taking it back (admin, custodian).
    Keep {
        group_id: String,
        repo: String,
        oid: String,
        keep: bool,
    },
    /// «Восстановить»: a new version with the files of an accepted one (any member; goes through
    /// acceptance as any save).
    Restore {
        group_id: String,
        repo: String,
        oid: String,
    },
    /// «Вернуть к общей»: our line, waiting for review or rejected, goes back to the shared version.
    BackToShared {
        group_id: String,
        repo: String,
    },
    /// The report before thinning out: what a rule of the last `keep_last` versions and `keep_days`
    /// days, with the files under `only_current` kept in the current version only, would take away
    /// against the confirmed one (admin).
    Thinning {
        group_id: String,
        repo: String,
        keep_last: u32,
        keep_days: u32,
        #[serde(default)]
        only_current: Vec<String>,
    },
    /// Confirm the retention rule (admin): from then on each node thins out by it by itself.
    Retain {
        group_id: String,
        repo: String,
        keep_last: u32,
        keep_days: u32,
        #[serde(default)]
        only_current: Vec<String>,
    },
    /// «На этом устройстве»: the project's folders this device does not hold in its working folder.
    LeaveOut {
        group_id: String,
        repo: String,
        #[serde(default)]
        folders: Vec<String>,
    },
    Root {
        group_id: String,
        folder: String,
    },
    /// «Беру в работу» (again: «Продлить»): the files for our account for a day.
    Take {
        group_id: String,
        repo: String,
        paths: Vec<String>,
    },
    /// «Отдать».
    Give {
        group_id: String,
        repo: String,
        paths: Vec<String>,
    },
    /// «Снять замок»: an admin gives back another account's take.
    Unlock {
        group_id: String,
        repo: String,
        path: String,
        account_id: String,
    },
    /// «Отложить мою правку рядом»: our change of a file someone else took first goes beside it.
    Aside {
        group_id: String,
        repo: String,
        path: String,
    },
    /// «Выбрать итоговую»: of `path` and its conflict copies, `chosen` stays as `path`; the copies go.
    Choose {
        group_id: String,
        repo: String,
        path: String,
        chosen: String,
    },
    /// `path` of version `oid` (none: the shared one) as a file with its own name — to drag out of
    /// Omni or open in its program; fetched first when not here (`export.pending`).
    Export {
        group_id: String,
        repo: String,
        path: String,
        #[serde(default)]
        oid: Option<String>,
    },
    /// A file's history: the accepted versions that changed it, newest first.
    FileHistory {
        group_id: String,
        repo: String,
        path: String,
    },
    /// The file to show for `path` of version `oid` (none: the shared one): here at once, or fetched
    /// for the preview by the next rounds (`preview.pending`).
    Preview {
        group_id: String,
        repo: String,
        path: String,
        #[serde(default)]
        oid: Option<String>,
    },
    /// An STL model of version `oid` (none: the shared one) drawn turned by `yaw`/`pitch` degrees
    /// (`model_view`); its content is fetched first when not here (`model_view.pending`).
    ModelView {
        group_id: String,
        repo: String,
        path: String,
        #[serde(default)]
        oid: Option<String>,
        #[serde(default)]
        yaw: f32,
        #[serde(default)]
        pitch: f32,
        #[serde(default)]
        width: u32,
        #[serde(default)]
        height: u32,
    },
    /// «Включить задачи» (admin): the workspace's tasks project, whose versions go in at once.
    TasksEnable {
        group_id: String,
    },
    /// The board (`tasks`).
    Tasks {
        group_id: String,
    },
    TaskNew {
        group_id: String,
        title: String,
        #[serde(default)]
        body: String,
        #[serde(flatten)]
        fields: TaskFieldsJson,
    },
    /// A change of the named fields of `task`.
    TaskChange {
        group_id: String,
        task: String,
        #[serde(flatten)]
        fields: TaskFieldsJson,
    },
    TaskDescribe {
        group_id: String,
        task: String,
        body: String,
    },
    TaskComment {
        group_id: String,
        task: String,
        text: String,
    },
    /// «Проработать вдвоём» (docs/decisions/omni-workspace-git.md section 5): start a session with `with`.
    PairStart {
        group_id: String,
        task: String,
        with: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        rounds: Option<u32>,
    },
    PairDraft {
        group_id: String,
        task: String,
        text: String,
    },
    /// A remark on the other's draft `draft` (its file name without `.md`).
    PairRemark {
        group_id: String,
        task: String,
        draft: String,
        what: String,
        severity: String,
        #[serde(default)]
        why: String,
    },
    PairAnswer {
        group_id: String,
        task: String,
        remark: String,
        taken: bool,
        #[serde(default)]
        why: String,
    },
    PairAgree {
        group_id: String,
        task: String,
    },
}

/// A task's fields as a command names them; none: left as they are.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TaskFieldsJson {
    #[serde(default)]
    pub task_title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub links: Option<Vec<String>>,
    /// task_change: dragged right above this task ("": to the column's end).
    #[serde(default)]
    pub before: Option<String>,
}

impl From<TaskFieldsJson> for super::tasks::TaskFields {
    fn from(json: TaskFieldsJson) -> Self {
        Self { title: json.task_title, status: json.status, assignee: json.assignee, labels: json.labels, links: json.links, before: json.before }
    }
}

impl WorkspaceFilesCommand {
    /// The project the command is about; empty for the workspace-wide one (the root folder).
    fn repo(&self) -> &str {
        match self {
            Self::Details { repo, .. }
            | Self::Nominate { repo, .. }
            | Self::Dismiss { repo, .. }
            | Self::Consent { repo, .. }
            | Self::Review { repo, .. }
            | Self::Download { repo, .. }
            | Self::CancelDownload { repo, .. }
            | Self::Bind { repo, .. }
            | Self::Unbind { repo, .. }
            | Self::History { repo, .. }
            | Self::Release { repo, .. }
            | Self::Keep { repo, .. }
            | Self::Restore { repo, .. }
            | Self::ApproveRevision { repo, .. }
            | Self::BackToShared { repo, .. }
            | Self::Thinning { repo, .. }
            | Self::Retain { repo, .. }
            | Self::LeaveOut { repo, .. }
            | Self::Take { repo, .. }
            | Self::Give { repo, .. }
            | Self::Unlock { repo, .. }
            | Self::Aside { repo, .. }
            | Self::FileHistory { repo, .. }
            | Self::Preview { repo, .. }
            | Self::Choose { repo, .. }
            | Self::Export { repo, .. }
            | Self::ModelView { repo, .. }
            | Self::Policy { repo, .. } => repo,
            Self::TasksEnable { .. }
            | Self::Tasks { .. }
            | Self::TaskNew { .. }
            | Self::TaskChange { .. }
            | Self::TaskDescribe { .. }
            | Self::TaskComment { .. }
            | Self::PairStart { .. }
            | Self::PairDraft { .. }
            | Self::PairRemark { .. }
            | Self::PairAnswer { .. }
            | Self::PairAgree { .. } => super::tasks::TASKS_REPO,
            Self::Root { .. } => "",
        }
    }

    fn group_id(&self) -> &str {
        match self {
            Self::Details { group_id, .. }
            | Self::Nominate { group_id, .. }
            | Self::Dismiss { group_id, .. }
            | Self::Consent { group_id, .. }
            | Self::Policy { group_id, .. }
            | Self::Review { group_id, .. }
            | Self::Download { group_id, .. }
            | Self::CancelDownload { group_id, .. }
            | Self::Bind { group_id, .. }
            | Self::Unbind { group_id, .. }
            | Self::History { group_id, .. }
            | Self::Release { group_id, .. }
            | Self::Keep { group_id, .. }
            | Self::Restore { group_id, .. }
            | Self::ApproveRevision { group_id, .. }
            | Self::BackToShared { group_id, .. }
            | Self::Thinning { group_id, .. }
            | Self::Retain { group_id, .. }
            | Self::LeaveOut { group_id, .. }
            | Self::Take { group_id, .. }
            | Self::Give { group_id, .. }
            | Self::Unlock { group_id, .. }
            | Self::Aside { group_id, .. }
            | Self::FileHistory { group_id, .. }
            | Self::Preview { group_id, .. }
            | Self::Choose { group_id, .. }
            | Self::Export { group_id, .. }
            | Self::ModelView { group_id, .. }
            | Self::TasksEnable { group_id }
            | Self::Tasks { group_id }
            | Self::TaskNew { group_id, .. }
            | Self::TaskChange { group_id, .. }
            | Self::TaskDescribe { group_id, .. }
            | Self::TaskComment { group_id, .. }
            | Self::PairStart { group_id, .. }
            | Self::PairDraft { group_id, .. }
            | Self::PairRemark { group_id, .. }
            | Self::PairAnswer { group_id, .. }
            | Self::PairAgree { group_id, .. }
            | Self::Root { group_id, .. } => group_id,
        }
    }
}

pub struct WorkspaceFilesJob {
    db: String,
    command: WorkspaceFilesCommand,
}

impl OmniApp {
    pub fn prepare_workspace_files_command(
        &self,
        command: WorkspaceFilesCommand,
    ) -> OmniResult<WorkspaceFilesJob> {
        let _ = self.ensure_runtime_profile_active("workspace_files")?;
        Ok(WorkspaceFilesJob {
            db: self.storage.db_path().to_string_lossy().into_owned(),
            command,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceCustodianView {
    pub copy_complete: bool,
    pub copy_missing_bytes: Option<u64>,
    pub account_id: String,
    pub name: String,
    pub admin: bool,
    pub nominated: bool,
    pub nomination: Option<String>,
    pub keeper: bool,
    pub is_me: bool,
    pub storing_here: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFileView {
    pub path: String,
    pub bytes: u64,
    pub available_here: bool,
    pub state: WorkspaceFileState,
    /// In the version shown (a file only we have is not: nothing of it to download).
    pub in_version: bool,
    /// In a folder this device leaves out of its working folder.
    pub left_out: bool,
    /// «Беру в работу»: who holds the file now.
    pub lock: Option<WorkspaceLockView>,
    /// We took it too, but someone else first: our take waits behind theirs.
    pub taken_after: bool,
}

/// Who holds a file («🔒 Иван, до 31.10 10:00»).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceLockView {
    pub account_id: String,
    pub name: String,
    pub mine: bool,
    pub since_unix_ms: i64,
    pub until_unix_ms: i64,
}

/// A file of a files project against the shared version, as this node has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFileState {
    /// As in the shared version.
    Accepted,
    /// Changed (or added, removed) in the folder and not saved.
    Changed,
    /// Saved in a version of ours the shared one does not hold yet.
    Saved,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceProposalView {
    pub account_id: String,
    pub name: String,
    pub oid: String,
    pub subject: String,
    pub changed: Vec<String>,
    pub stale: bool,
    pub accepted: bool,
    pub rejected: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkspaceFilesDetails {
    pub history_path: Option<String>,
    pub size_known: bool,
    pub complete_copies: usize,
    pub policy: WorkspaceFilesPolicyV1,
    pub i_am_admin: bool,
    pub can_review: bool,
    pub members: Vec<WorkspaceCustodianView>,
    pub files: Vec<WorkspaceFileView>,
    pub proposals: Vec<WorkspaceProposalView>,
    pub work_path: Option<String>,
    pub root: Option<String>,
    pub suggested_folder: Option<String>,
    pub folder: Option<String>,
    pub history_bytes: u64,
    pub history_missing_bytes: u64,
    pub current_bytes: u64,
    pub single_copy: bool,
    pub download_status: Option<String>,
    /// Filled by the `history` command only.
    pub history: Vec<WorkspaceHistoryEntry>,
    /// The designation offered for the next revision (A, B, … after the latest letter).
    pub next_revision: String,
    /// The retention rule: the confirmed one, or the default offered while none is (`history`).
    pub keep_last: u32,
    pub keep_days: u32,
    pub only_current: Vec<String>,
    pub retention_confirmed: bool,
    /// Filled by the `thinning` command only.
    pub thinning: Option<WorkspaceThinningReport>,
    /// The folders this device leaves out of its working folder, and the project's top folders.
    pub left_out: Vec<String>,
    pub folders: Vec<WorkspaceFolderView>,
    /// Filled by the `file_history` command only.
    pub file_history: Vec<WorkspaceFileVersionView>,
    /// Filled by the `preview` command only.
    pub preview: Option<WorkspacePreviewView>,
    /// Filled by the `export` command only.
    pub export: Option<WorkspacePreviewView>,
    /// Filled by the task commands: the board after them.
    pub tasks: Option<super::tasks::WorkspaceTasksView>,
    /// Filled by `model_view`: the model drawn, or that its content is on its way.
    pub model_view: Option<WorkspaceModelView>,
    /// The shared version's files kept with conflict copies beside them.
    pub conflicts: Vec<WorkspaceConflictView>,
}

/// A file and the conflict copies kept beside it when two saves changed it («выбрать итоговую»).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceConflictView {
    pub path: String,
    pub copies: Vec<WorkspaceConflictCopyView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceConflictCopyView {
    pub path: String,
    /// «Петя, 30.10 10-42»: whose change it is and when it was made.
    pub label: String,
}

/// An accepted version that changed one file (в.N of the project).
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFileVersionView {
    pub number: u32,
    pub oid: String,
    pub subject: String,
    pub author: String,
    pub at_unix_ms: i64,
    /// The file's size in it; none: the version removed the file.
    pub bytes: Option<u64>,
    pub revision: Option<String>,
    /// Released under the нормоконтроль rule and not approved yet.
    pub revision_pending: bool,
}

/// A file of a version to show: where its content is here, or that it is on its way.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspacePreviewView {
    pub path: String,
    pub oid: String,
    pub bytes: u64,
    pub file: Option<String>,
    pub pending: bool,
}

/// An STL model drawn: the PNG (base64), or that its content is being fetched.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceModelView {
    pub png_base64: Option<String>,
    pub pending: bool,
    pub triangles: usize,
}

/// A top folder of the project's shared version.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFolderView {
    pub path: String,
    pub bytes: u64,
}

/// What a retention rule would take away against the confirmed one.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceThinningReport {
    pub keep_last: u32,
    pub keep_days: u32,
    pub only_current: Vec<String>,
    /// Versions that would lose their content, newest first (в.N).
    pub versions: Vec<WorkspaceHistoryEntry>,
    /// Versions that would keep all but their files under the masks.
    pub partial_versions: usize,
    /// Content only they have.
    pub freed_bytes: u64,
    /// Versions the rule brings back under keeping (their content comes back from whoever still has it).
    pub restored_versions: usize,
}

/// An accepted version in the project's history.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceHistoryEntry {
    /// в.N
    pub number: u32,
    pub oid: String,
    pub subject: String,
    pub author: String,
    pub at_unix_ms: i64,
    /// How many files the version changed.
    pub changed: usize,
    pub revision: Option<String>,
    pub revision_note: String,
    /// «На нормоконтроле»: released under the rule, not approved yet.
    pub revision_pending: bool,
    /// We released it: someone else approves it.
    pub revision_mine: bool,
    pub kept: bool,
    /// The shared version now.
    pub current: bool,
    /// Its content is thinned out by the retention rule (the record stays).
    pub thinned: bool,
    /// Kept without the content of its files under the «only the current» masks.
    pub partial: bool,
}

fn forbidden(message: &str) -> OmniError {
    OmniError::InvalidInput(message.into())
}

fn history_size(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<(u64, u64)> {
    let (mut total, mut missing) = (0_u64, 0_u64);
    let here = versions::Here::read(
        Path::new(&record.published_path),
        record.work_path.as_deref().map(Path::new),
    );
    for content in files::history_kept(storage, group, members, record)?.contents.iter() {
        total = total.saturating_add(content.size);
        if here.file(&content.id, Some(content.size)).is_none() {
            missing = missing.saturating_add(content.size);
        }
    }
    Ok((total, missing))
}

/// Every line announced in the project has all its versions here: its files and their sizes are known.
fn lines_here(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<bool> {
    for tip in files::tips(storage, group, members, &record.repo)? {
        if versions::first_missing(Path::new(&record.published_path), &record.repo, &tip.version)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn report_inventory(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<()> {
    let Some(oid) = versions::read_ref(Path::new(&record.published_path), Ref::Shared)?.map(|s| s.id)
    else {
        return Ok(());
    };
    let (history_bytes, missing_bytes) = history_size(storage, group, members, record)?;
    for previous in storage.list_workspace_file_events(&group.group_id)? {
        if previous.sender_member != group.my_member_pub {
            continue;
        }
        if matches!(serde_json::from_str::<Event>(&previous.event_json),Ok(Event::Inventory { repo,oid: at,history_bytes: bytes,missing_bytes: missing,.. }) if repo == record.repo && at == oid && bytes == history_bytes && missing == missing_bytes)
        {
            return Ok(());
        }
    }
    publish(
        storage,
        group,
        members,
        Event::Inventory {
            repo: record.repo.clone(),
            oid,
            history_bytes,
            missing_bytes,
            at_unix_ms: current_unix_ms(),
        },
    )
}

pub(super) fn ensure_repo(
    storage: &Storage,
    group_id: &str,
    repo: &str,
) -> OmniResult<WorkspaceRepoRecord> {
    files::ensure_project(storage, group_id, repo)
}

pub(super) fn bind(storage: &Storage, group_id: &str, repo: &str, folder: &str) -> OmniResult<()> {
    files::bind_folder(storage, group_id, repo, folder).map(|_| ())
}

impl WorkspaceFilesJob {
    pub fn run(self) -> OmniResult<WorkspaceFilesDetails> {
        let storage = Storage::open(&self.db)?;
        if let WorkspaceFilesCommand::Root { folder, .. } = &self.command {
            if !folder.is_empty() {
                if !Path::new(folder).is_absolute() {
                    return Err(forbidden("choose an absolute workspace root folder"));
                }
                Storage::confine_destructive_path_for_callers(Path::new(folder))?;
                storage.set_workspace_file_local("", "", "root", folder)?;
            }
            return Ok(WorkspaceFilesDetails {
                root: storage.get_workspace_file_local("", "", "root")?,
                ..Default::default()
            });
        }
        let group_id = self.command.group_id().to_string();
        let group = storage
            .get_group(&group_id)?
            .ok_or_else(|| forbidden("unknown workspace"))?;
        let members = super::agents::members(&storage, &group_id)?;
        let me = my_account(&members, &group)
            .ok_or_else(|| forbidden("not an active workspace member"))?;
        let current = state(&storage, &group, &members, self.command.repo())?;
        let admin = is_admin(&members, &group.my_member_pub);
        let can_review = current.reviewers.contains(&me);
        let mut selected = String::new();
        let mut selected_oid = None;
        let mut want_history = false;
        let mut thinning = None;
        let mut file_history = None;
        let mut preview = None;
        let mut export = None;
        let mut want_tasks = false;
        let mut model_view = None;
        let now = current_unix_ms().max(
            storage
                .list_workspace_file_events(&group_id)?
                .iter()
                .filter(|e| e.sender_member == group.my_member_pub)
                .map(|e| e.at_unix_ms)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        match self.command {
            WorkspaceFilesCommand::Details { repo, oid, .. } => {
                selected = repo;
                selected_oid = oid;
            }
            WorkspaceFilesCommand::Nominate { repo, account_id, .. } => {
                if !admin {
                    return Err(forbidden("only an admin appoints custodians"));
                }
                if super::agents::is_agent(&storage, &account_id) {
                    return Err(forbidden("агент не может быть хранителем"));
                }
                if !members
                    .iter()
                    .any(|m| m.account_id == account_id && m.status == "member")
                {
                    return Err(forbidden("not a workspace member"));
                }
                publish(
                    &storage,
                    &group,
                    &members,
                    Event::Nominate {
                        account_id,
                        repo: repo.clone(),
                        nomination: hex::encode(&omni_crypto::generate_group_key()[..16]),
                        enabled: true,
                        at_unix_ms: now,
                    },
                )?;
                selected = repo;
            }
            WorkspaceFilesCommand::Dismiss { repo, account_id, .. } => {
                if !admin {
                    return Err(forbidden("only an admin dismisses custodians"));
                }
                let (nomination, _) = current
                    .nominations
                    .get(&account_id)
                    .cloned()
                    .ok_or_else(|| forbidden("no custodian nomination"))?;
                publish(
                    &storage,
                    &group,
                    &members,
                    Event::Nominate {
                        account_id,
                        repo: repo.clone(),
                        nomination,
                        enabled: false,
                        at_unix_ms: now,
                    },
                )?;
                selected = repo;
            }
            WorkspaceFilesCommand::Consent {
                repo,
                nomination,
                accepted,
                folder,
                ..
            } => {
                if current.nominations.get(&me) != Some(&(nomination.clone(), true)) {
                    return Err(forbidden("this nomination is no longer active"));
                }
                if accepted {
                    if !storage.workspace_repo_is_files(&group_id, &repo)? {
                        return Err(forbidden("unknown repository"));
                    }
                    let record = ensure_repo(&storage, &group_id, &repo)?;
                    if !lines_here(&storage, &group, &members, &record)? {
                        return Err(forbidden("wait for the file tree before accepting custody: its size is not known yet"));
                    }
                    let folder = folder
                        .filter(|p| !p.trim().is_empty())
                        .ok_or_else(|| forbidden("choose a folder before accepting custody"))?;
                    if !Path::new(&folder).is_absolute() {
                        return Err(forbidden("choose an absolute custody folder"));
                    }
                    Storage::confine_destructive_path_for_callers(Path::new(&folder))?;
                    // Consent is not published if preparing the project's folder fails. Under the
                    // profile: two profiles of one database keeping the same project in the same
                    // folder shared one archive, and one leaving the group removed the other's
                    // (blind audit 2026-11-13, pass 18).
                    let profile = storage
                        .get_active_profile_id()?
                        .ok_or_else(|| OmniError::Storage("no active profile".to_string()))?;
                    let archive = Path::new(&folder)
                        .join(".omni")
                        .join(&profile.0)
                        .join(&group_id)
                        .join(&repo)
                        .join("content");
                    super::content::configure_store(
                        Path::new(&record.published_path),
                        &archive,
                    )?;
                    if record.work_path.is_none() {
                        bind(
                            &storage,
                            &group_id,
                            &repo,
                            &Path::new(&folder).join(&repo).to_string_lossy(),
                        )?;
                    }
                    storage.set_workspace_file_local(&group_id, &repo, "custody_folder", &folder)?;
                }
                publish(
                    &storage,
                    &group,
                    &members,
                    Event::Consent {
                        account_id: me.clone(),
                        repo: repo.clone(),
                        nomination,
                        accepted,
                        at_unix_ms: now,
                    },
                )?;
                selected = repo;
            }
            WorkspaceFilesCommand::Policy {
                repo,
                participants_need_approval,
                newcomers_download_current,
                edit_only_taken,
                revisions_checked,
                ..
            } => {
                if !admin {
                    return Err(forbidden("only an admin changes file policy"));
                }
                // The rule's start stays while it stays on: revisions released before it need nothing.
                let since = match (revisions_checked, current.policy.revisions_checked) {
                    (false, _) => 0,
                    (true, true) => current.policy.revisions_checked_since_unix_ms,
                    (true, false) => now,
                };
                publish(
                    &storage,
                    &group,
                    &members,
                    Event::Policy {
                        repo: repo.clone(),
                        policy: WorkspaceFilesPolicyV1 {
                            participants_need_approval,
                            newcomers_download_current,
                            edit_only_taken,
                            revisions_checked,
                            revisions_checked_since_unix_ms: since,
                            changed_at_unix_ms: now,
                        },
                    },
                )?;
                if let Some(record) = storage.get_workspace_repo(&group_id, &repo)? {
                    files::mark_folder(&storage, &group, &members, &record)?;
                }
                selected = repo;
            }
            WorkspaceFilesCommand::Review {
                repo,
                oid,
                accepted,
                ..
            } => {
                if !can_review {
                    return Err(forbidden(
                        "only an admin or an active custodian reviews file versions",
                    ));
                }
                if !accepted && current.reviews.get(&(repo.clone(), oid.clone())) == Some(&true) {
                    return Err(forbidden(
                        "an accepted version cannot be rejected retroactively",
                    ));
                }
                let event = Event::Review {
                    repo: repo.clone(),
                    oid: oid.clone(),
                    accepted,
                    authority: review_authority(&current, &me, &members),
                    at_unix_ms: now,
                };
                event.validate(now)?;
                let record = storage
                    .get_workspace_repo(&group_id, &repo)?
                    .ok_or_else(|| forbidden("repository has not arrived yet"))?;
                let path = Path::new(&record.published_path);
                let proposal = omni_protocol::WorkspaceContentV1 {
                    id: oid.clone(),
                    size: std::fs::metadata(versions::version_path(path, &oid)).map_or(0, |m| m.len()),
                };
                if versions::first_missing(path, &repo, &proposal)?.is_some() {
                    return Err(forbidden("the proposed version has not arrived yet"));
                }
                if accepted {
                    // Bring the reviewed version onto the current accepted line. Conflicts keep a
                    // second copy, as normal file saves do; a stale version never replaces it wholesale.
                    // Under the folder's work lock, and only over the `main` read here: a save
                    // finishing meanwhile is not overwritten, the acceptance fails instead.
                    let tip = files::with_files_work(&self.db, &group_id, &repo, || {
                        let before = versions::read_ref(path, Ref::Mine)?;
                        let tip = match versions::read_ref(path, Ref::Shared)? {
                            Some(shared)
                                if !versions::is_ancestor(path, &repo, &shared.id, &oid)?
                                    && !versions::is_ancestor(path, &repo, &oid, &shared.id)? =>
                            {
                                versions::replay(path, &repo, &shared, &oid, &|id| current.rejected(&repo, id))?
                            }
                            _ => proposal.clone(),
                        };
                        // Only over our line as read here: a save finishing meanwhile is not lost.
                        if !versions::update_ref(path, Ref::Mine, Some(&tip), before.as_ref())? {
                            return Err(OmniError::Transport("the folder was updated meanwhile; try again".into()));
                        }
                        Ok(tip.id)
                    })?;
                    publish(&storage, &group, &members, event)?;
                    publish(
                        &storage,
                        &group,
                        &members,
                        Event::Review {
                            repo: repo.clone(),
                            oid: tip,
                            accepted: true,
                            authority: review_authority(&current, &me, &members),
                            at_unix_ms: now,
                        },
                    )?;
                    files::announce_tip(&storage, &group, &members, &record)?;
                } else {
                    publish(&storage, &group, &members, event)?;
                }
                advance_main(&storage, &group, &members, &record)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Bind { repo, folder, .. } => {
                if folder.trim().is_empty() {
                    return Err(forbidden("choose a project folder"));
                }
                bind(&storage, &group_id, &repo, &folder)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Unbind { repo, .. } => {
                files::unbind_folder(&self.db, &storage, &group_id, &repo)?;
                selected = repo;
            }
            WorkspaceFilesCommand::LeaveOut { repo, folders, .. } => {
                let record = storage.get_workspace_repo(&group_id, &repo)?.ok_or_else(|| forbidden("unknown repository"))?;
                if !storage.workspace_repo_is_files(&group_id, &repo)? {
                    return Err(forbidden("the repository holds code: git takes it whole"));
                }
                files::set_left_out(&record, folders)?;
                selected = repo;
            }
            WorkspaceFilesCommand::History { repo, .. } => {
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::Release { repo, oid, label, note, .. } => {
                if !can_review {
                    return Err(forbidden("only an admin or a custodian releases a revision"));
                }
                let label = label.trim().to_string();
                if current.revisions.iter().any(|(version, (taken, _))| version != &oid && taken.eq_ignore_ascii_case(&label)) {
                    return Err(forbidden("this revision designation is taken"));
                }
                files::require_accepted(&storage, &group_id, &repo, &oid)?;
                let event = Event::Revision {
                    repo: repo.clone(),
                    version: oid,
                    label,
                    note: note.trim().to_string(),
                    authority: review_authority(&current, &me, &members),
                    at_unix_ms: now,
                };
                event.validate(now)?;
                publish(&storage, &group, &members, event)?;
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::ApproveRevision { repo, oid, .. } => {
                if !can_review {
                    return Err(forbidden("only an admin or a custodian approves a revision"));
                }
                let Some((label, _)) = current.revisions.get(&oid) else { return Err(forbidden("this version is not a revision")) };
                if current.released.get(&oid).is_some_and(|(by, _)| by == &me) {
                    return Err(forbidden("a revision is approved by someone other than who released it"));
                }
                let event = Event::RevisionApproval {
                    repo: repo.clone(),
                    version: oid,
                    label: label.clone(),
                    authority: review_authority(&current, &me, &members),
                    at_unix_ms: now,
                };
                event.validate(now)?;
                publish(&storage, &group, &members, event)?;
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::Keep { repo, oid, keep, .. } => {
                if !can_review {
                    return Err(forbidden("only an admin or a custodian keeps a version for good"));
                }
                files::require_accepted(&storage, &group_id, &repo, &oid)?;
                publish(
                    &storage,
                    &group,
                    &members,
                    Event::Keep { repo: repo.clone(), version: oid, keep, authority: review_authority(&current, &me, &members), at_unix_ms: now },
                )?;
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::Restore { repo, oid, .. } => {
                files::restore_version(&self.db, &storage, &group, &members, &repo, &oid)?;
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::BackToShared { repo, .. } => {
                files::back_to_shared(&self.db, &storage, &group, &members, &repo)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Thinning { repo, keep_last, keep_days, only_current, .. } => {
                if !admin {
                    return Err(forbidden("only an admin sets how long versions are kept"));
                }
                let rule = RetentionRule { keep_last, keep_days, only_current: masks(only_current) };
                retention_event(&repo, &rule, now)?;
                thinning = Some(rule);
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::Retain { repo, keep_last, keep_days, only_current, .. } => {
                if !admin {
                    return Err(forbidden("only an admin sets how long versions are kept"));
                }
                let rule = RetentionRule { keep_last, keep_days, only_current: masks(only_current) };
                publish(&storage, &group, &members, retention_event(&repo, &rule, now)?)?;
                want_history = true;
                selected = repo;
            }
            WorkspaceFilesCommand::Root { folder, .. } => {
                if !Path::new(&folder).is_absolute() {
                    return Err(forbidden("choose an absolute workspace root folder"));
                }
                storage.set_workspace_file_local("", "", "root", &folder)?;
            }
            WorkspaceFilesCommand::Download {
                repo,
                path,
                target,
                oid,
                ..
            } => {
                if !Path::new(&target).is_absolute() {
                    return Err(forbidden("choose a destination file"));
                }
                Storage::confine_destructive_path_for_callers(Path::new(&target))?;
                let record = storage
                    .get_workspace_repo(&group_id, &repo)?
                    .ok_or_else(|| forbidden("file tree has not arrived yet"))?;
                let shared = versions::read_ref(Path::new(&record.published_path), Ref::Shared)?.map(|s| s.id);
                let commit = oid
                    .or(shared.clone())
                    .ok_or_else(|| forbidden("no version yet"))?;
                validate_version_access(
                    &storage,
                    &group,
                    &members,
                    &repo,
                    &commit,
                    shared.as_deref(),
                )?;
                if !files::file_tree(&record, &commit)?
                    .iter()
                    .any(|f| f.path == path)
                {
                    return Err(forbidden("file is not in the version"));
                }
                request_download(&storage, &group_id, &repo, &commit, &path, &target)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Take { repo, paths, .. } => {
                files::take_files(&storage, &group, &members, &repo, &paths, now)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Give { repo, paths, .. } => {
                files::give_files(&storage, &group, &members, &repo, &paths, None, now)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Unlock { repo, path, account_id, .. } => {
                files::give_files(&storage, &group, &members, &repo, &[path], Some(&account_id), now)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Aside { repo, path, .. } => {
                files::set_aside(&storage, &group, &members, &repo, &path, now)?;
                selected = repo;
            }
            WorkspaceFilesCommand::Choose { repo, path, chosen, .. } => {
                files::choose_final(&self.db, &storage, &group, &members, &repo, &path, &chosen)?;
                selected = repo;
            }
            WorkspaceFilesCommand::FileHistory { repo, path, .. } => {
                let record = storage.get_workspace_repo(&group_id, &repo)?.ok_or_else(|| forbidden("file tree has not arrived yet"))?;
                file_history = Some(files::file_history(&record, &current, &path)?);
                selected = repo;
            }
            WorkspaceFilesCommand::ModelView { repo, path, oid, yaw, pitch, width, height, .. } => {
                let record = storage.get_workspace_repo(&group_id, &repo)?.ok_or_else(|| forbidden("file tree has not arrived yet"))?;
                let shared = versions::read_ref(Path::new(&record.published_path), Ref::Shared)?.map(|s| s.id);
                let version = oid.or(shared.clone()).ok_or_else(|| forbidden("no version yet"))?;
                validate_version_access(&storage, &group, &members, &repo, &version, shared.as_deref())?;
                model_view = Some(files::model_view(&storage, &record, &path, &version, (yaw, pitch), (width, height))?);
                selected = repo;
            }
            WorkspaceFilesCommand::TasksEnable { .. } => {
                if !admin {
                    return Err(forbidden("only an admin turns tasks on"));
                }
                if current.policy.changed_at_unix_ms == 0 {
                    // A change of a task goes in at once, and a newcomer gets the tasks.
                    publish(
                        &storage,
                        &group,
                        &members,
                        Event::Policy {
                            repo: super::tasks::TASKS_REPO.into(),
                            policy: WorkspaceFilesPolicyV1 {
                                participants_need_approval: false,
                                newcomers_download_current: true,
                                changed_at_unix_ms: now,
                                ..Default::default()
                            },
                        },
                    )?;
                }
                super::tasks::prepare(&storage, &group)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::Tasks { .. } => want_tasks = true,
            WorkspaceFilesCommand::TaskNew { title, body, fields, .. } => {
                require_tasks(&current)?;
                super::tasks::create(&self.db, &storage, &group, &members, &title, &body, fields.into())?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::TaskChange { task, fields, .. } => {
                require_tasks(&current)?;
                super::tasks::change(&self.db, &storage, &group, &members, &task, fields.into())?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::TaskDescribe { task, body, .. } => {
                require_tasks(&current)?;
                super::tasks::describe(&self.db, &storage, &group, &members, &task, &body)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::TaskComment { task, text, .. } => {
                require_tasks(&current)?;
                super::tasks::comment(&self.db, &storage, &group, &members, &task, &text)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::PairStart { task, with, topic, rounds, .. } => {
                require_tasks(&current)?;
                super::tasks::pair::start(&self.db, &storage, &group, &members, &task, &with, &topic, rounds)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::PairDraft { task, text, .. } => {
                require_tasks(&current)?;
                super::tasks::pair::draft(&self.db, &storage, &group, &members, &task, &text)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::PairRemark { task, draft, what, severity, why, .. } => {
                require_tasks(&current)?;
                super::tasks::pair::remark(&self.db, &storage, &group, &members, &task, &draft, &what, &severity, &why)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::PairAnswer { task, remark, taken, why, .. } => {
                require_tasks(&current)?;
                super::tasks::pair::answer(&self.db, &storage, &group, &members, &task, &remark, taken, &why)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::PairAgree { task, .. } => {
                require_tasks(&current)?;
                super::tasks::pair::agree(&self.db, &storage, &group, &members, &task)?;
                want_tasks = true;
            }
            WorkspaceFilesCommand::Export { repo, path, oid, .. } => {
                let record = storage.get_workspace_repo(&group_id, &repo)?.ok_or_else(|| forbidden("file tree has not arrived yet"))?;
                let shared = versions::read_ref(Path::new(&record.published_path), Ref::Shared)?.map(|s| s.id);
                let version = oid.or(shared.clone()).ok_or_else(|| forbidden("no version yet"))?;
                validate_version_access(&storage, &group, &members, &repo, &version, shared.as_deref())?;
                export = Some(files::export(&storage, &record, &path, &version)?);
                selected = repo;
            }
            WorkspaceFilesCommand::Preview { repo, path, oid, .. } => {
                let record = storage.get_workspace_repo(&group_id, &repo)?.ok_or_else(|| forbidden("file tree has not arrived yet"))?;
                let shared = versions::read_ref(Path::new(&record.published_path), Ref::Shared)?.map(|s| s.id);
                let version = oid.or(shared.clone()).ok_or_else(|| forbidden("no version yet"))?;
                validate_version_access(&storage, &group, &members, &repo, &version, shared.as_deref())?;
                preview = Some(files::preview(&storage, &record, &path, &version)?);
                selected = repo;
            }
            WorkspaceFilesCommand::CancelDownload { repo, .. } => {
                storage.set_workspace_file_local(&group_id, &repo, "download", "")?;
                storage.set_workspace_file_local(
                    &group_id,
                    &repo,
                    "download_status",
                    "Загрузка отменена",
                )?;
                selected = repo;
            }
        }
        let mut view = details(
            &storage,
            &group,
            &members,
            &selected,
            selected_oid.as_deref(),
        )?;
        view.file_history = file_history.unwrap_or_default();
        view.preview = preview;
        view.export = export;
        view.model_view = model_view;
        if want_tasks {
            // Read after the change: the policy that turned them on may be ours, just published.
            let enabled = state(&storage, &group, &members, super::tasks::TASKS_REPO)?.policy.changed_at_unix_ms != 0;
            view.tasks = Some(super::tasks::view(&storage, &group, &members, enabled, admin)?);
        }
        if want_history {
            let current = state(&storage, &group, &members, &selected)?;
            if let Some(record) = storage.get_workspace_repo(&group_id, &selected)? {
                let kept = files::history_kept(&storage, &group, &members, &record)?;
                view.history = files::history(&record, &current, &kept)?;
                for entry in &mut view.history {
                    entry.revision_mine = current.released.get(&entry.oid).is_some_and(|(by, _)| by == &me);
                }
                if let Some(rule) = thinning {
                    view.thinning = Some(thinning_report(&storage, &group, &members, &record, &current, &kept, &view.history, rule)?);
                }
            }
            view.next_revision = next_revision(&current);
            view.retention_confirmed = current.retention.is_some();
            let rule = current.retention.unwrap_or(RetentionRule {
                keep_last: WORKSPACE_DEFAULT_KEEP_LAST,
                keep_days: WORKSPACE_DEFAULT_KEEP_DAYS,
                only_current: Vec::new(),
            });
            (view.keep_last, view.keep_days, view.only_current) = (rule.keep_last, rule.keep_days, rule.only_current);
        }
        Ok(view)
    }
}

/// Ask for `path` of version `commit` at `target` (the tick fetches it); the target is replaced only
/// while it stays as it is now.
pub(super) fn request_download(storage: &Storage, group_id: &str, repo: &str, commit: &str, path: &str, target: &str) -> OmniResult<()> {
    if storage.get_workspace_file_local(group_id, repo, "download")?.is_some_and(|s| !s.is_empty()) {
        return Err(forbidden("another file download is pending; finish or cancel it first"));
    }
    let stamp = files::download_target_stamp(Path::new(target));
    let request = serde_json::json!({"commit":commit,"path":path,"target":target,"target_stamp":stamp});
    storage.set_workspace_file_local(group_id, repo, "download", &request.to_string())?;
    storage.set_workspace_file_local(group_id, repo, "download_status", "Ожидает загрузки")
}

/// The masks as typed: trimmed, empty ones left out, each once (in any case).
fn masks(typed: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    typed.into_iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty() && seen.insert(m.to_lowercase())).collect()
}

fn retention_event(repo: &str, rule: &RetentionRule, now: i64) -> OmniResult<Event> {
    let event = Event::Retention {
        repo: repo.to_string(),
        keep_last: rule.keep_last,
        keep_days: rule.keep_days,
        only_current: rule.only_current.clone(),
        at_unix_ms: now,
    };
    event.validate(now)?;
    Ok(event)
}

/// «Отчёт перед прореживанием»: the versions `rule` would thin out that the confirmed rule keeps, and
/// the content only they have.
#[allow(clippy::too_many_arguments)]
fn thinning_report(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
    current: &CustodyState,
    now_kept: &files::Kept,
    history: &[WorkspaceHistoryEntry],
    rule: RetentionRule,
) -> OmniResult<WorkspaceThinningReport> {
    let (shared, lines) = files::history_lines(storage, group, members, record)?;
    let proposed = files::kept_under(
        Path::new(&record.published_path),
        &record.repo,
        shared.as_deref(),
        &lines,
        Some(&rule),
        &files::protected_versions(current),
        current_unix_ms(),
    )?;
    let gone: HashSet<&omni_protocol::WorkspaceContentV1> = now_kept.dropped.iter().collect();
    Ok(WorkspaceThinningReport {
        keep_last: rule.keep_last,
        keep_days: rule.keep_days,
        versions: history.iter().filter(|e| proposed.thinned.contains(&e.oid) && !e.thinned).cloned().collect(),
        partial_versions: history.iter().filter(|e| proposed.partial.contains(&e.oid) && !e.partial && !e.thinned).count(),
        only_current: rule.only_current,
        freed_bytes: proposed.dropped.iter().filter(|c| !gone.contains(c)).map(|c| c.size).sum(),
        restored_versions: history.iter().filter(|e| e.thinned && !proposed.thinned.contains(&e.oid)).count(),
    })
}

/// The letter after the latest one used (A after none, AA after Z); other designations are skipped.
fn next_revision(current: &CustodyState) -> String {
    let last = current
        .revisions
        .values()
        .filter_map(|(label, _)| {
            let up = label.to_uppercase();
            (!up.is_empty() && up.len() <= 3 && up.chars().all(|c| c.is_ascii_uppercase()))
                .then(|| up.chars().fold(0u64, |n, c| n * 26 + (c as u64 - 'A' as u64 + 1)))
        })
        .max()
        .unwrap_or(0);
    let mut n = last + 1;
    let mut label = String::new();
    while n > 0 {
        let digit = ((n - 1) % 26) as u8;
        label.insert(0, (b'A' + digit) as char);
        n = (n - 1) / 26;
    }
    label
}

fn details(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    selected: &str,
    selected_oid: Option<&str>,
) -> OmniResult<WorkspaceFilesDetails> {
    let current = state(storage, group, members, selected)?;
    let me = my_account(members, group);
    let mut people = Vec::new();
    let mut seen = HashSet::new();
    for member in members.iter().filter(|m| m.status == "member") {
        if !seen.insert(member.account_id.clone()) {
            continue;
        }
        let nomination = current
            .nominations
            .get(&member.account_id)
            .filter(|(_, active)| *active)
            .map(|(id, _)| id.clone());
        people.push(WorkspaceCustodianView {
            copy_complete: false,
            copy_missing_bytes: None,
            account_id: member.account_id.clone(),
            name: member.display_name.clone(),
            admin: maintainers(members).contains(&member.account_id),
            nominated: nomination.is_some(),
            nomination,
            keeper: current.keepers.contains(&member.account_id),
            is_me: me.as_deref() == Some(&member.account_id),
            storing_here: me.as_deref() == Some(&member.account_id)
                && current.archive_members.contains(&group.my_member_pub),
        });
    }
    let mut view = WorkspaceFilesDetails {
        history_path: None,
        size_known: true,
        complete_copies: 0,
        policy: current.policy.clone(),
        i_am_admin: is_admin(members, &group.my_member_pub),
        can_review: me.as_ref().is_some_and(|m| current.reviewers.contains(m)),
        members: people,
        files: Vec::new(),
        proposals: Vec::new(),
        work_path: None,
        root: storage.get_workspace_file_local("", "", "root")?,
        suggested_folder: None,
        folder: storage.get_workspace_file_local(&group.group_id, selected, "custody_folder")?,
        history_bytes: 0,
        history_missing_bytes: 0,
        current_bytes: 0,
        single_copy: current.reviewers.len() <= 1,
        download_status: storage.get_workspace_file_local(
            &group.group_id,
            selected,
            "download_status",
        )?,
        history: Vec::new(),
        next_revision: String::new(),
        keep_last: 0,
        keep_days: 0,
        only_current: Vec::new(),
        retention_confirmed: false,
        thinning: None,
        left_out: Vec::new(),
        folders: Vec::new(),
        file_history: Vec::new(),
        preview: None,
        export: None,
        tasks: None,
        model_view: None,
        conflicts: Vec::new(),
    };
    view.suggested_folder = view.root.as_ref().map(|root| {
        default_folder(root, group, "")
            .to_string_lossy()
            .into_owned()
    });
    let Some(record) = storage.get_workspace_repo(&group.group_id, selected)? else {
        return Ok(view);
    };
    if !storage.workspace_repo_is_files(&group.group_id, selected)? {
        return Ok(view);
    }
    let path = Path::new(&record.published_path);
    let shared = versions::read_ref(path, Ref::Shared)?.map(|s| s.id);
    // Shown to the user with the system's separators.
    view.history_path = Some(
        super::content::store_path(path)
            .to_string_lossy()
            .replace('/', std::path::MAIN_SEPARATOR_STR),
    );
    view.work_path = record.work_path.clone();
    let (left_out, folders) = files::left_out_view(&record)?;
    view.left_out = left_out;
    view.folders = folders.into_iter().map(|(path, bytes)| WorkspaceFolderView { path, bytes }).collect();
    if let Some(tree) = selected_oid {
        validate_version_access(storage, group, members, selected, tree, shared.as_deref())?;
        view.files = files::file_tree(&record, tree)?;
    } else if let Some(tree) = shared.as_deref() {
        validate_version_access(storage, group, members, selected, tree, shared.as_deref())?;
        view.files = files::shared_file_tree(&record, tree)?;
        view.conflicts = versions::conflict_copies(&*versions::tree(path, selected, tree)?)
            .into_iter()
            .map(|(path, copies)| WorkspaceConflictView {
                path,
                copies: copies.into_iter().map(|(path, label)| WorkspaceConflictCopyView { path, label }).collect(),
            })
            .collect();
    }
    let now = current_unix_ms();
    for file in &mut view.files {
        file.lock = current.holder(&file.path, now).map(|holder| WorkspaceLockView {
            name: members.iter().find(|m| m.account_id == holder.account).map_or_else(|| holder.account.clone(), |m| m.display_name.clone()),
            mine: me.as_deref() == Some(holder.account.as_str()),
            account_id: holder.account.clone(),
            since_unix_ms: holder.since,
            until_unix_ms: holder.until,
        });
        file.taken_after = me.as_deref().is_some_and(|me| {
            file.lock.as_ref().is_some_and(|l| !l.mine) && current.takes.get(&file.path).is_some_and(|t| t.iter().any(|t| t.account == me && t.until > now))
        });
    }
    // Consent covers this project only: the estimate is its current files and history.
    view.size_known = lines_here(storage, group, members, &record)?;
    if let Some(tip) = shared.as_deref() {
        view.current_bytes = files::version_summary(&record, tip)?.1;
    }
    (view.history_bytes, view.history_missing_bytes) = history_size(storage, group, members, &record)?;
    let mut complete_devices = HashSet::new();
    for record in storage.list_workspace_file_events(&group.group_id)? {
        if let Ok(Event::Inventory {
            repo,
            oid,
            missing_bytes,
            ..
        }) = serde_json::from_str(&record.event_json)
        {
            if repo != selected
                || shared.as_deref() != Some(&oid)
                || !current.reviewers.contains(&record.sender_account)
            {
                continue;
            }
            if !members
                .iter()
                .any(|m| m.status == "member" && m.member_pub == record.sender_member)
            {
                continue;
            }
            if let Some(person) = view
                .members
                .iter_mut()
                .find(|m| m.account_id == record.sender_account)
            {
                person.copy_complete |= missing_bytes == 0;
                person.copy_missing_bytes = Some(
                    person
                        .copy_missing_bytes
                        .map_or(missing_bytes, |b| b.min(missing_bytes)),
                );
            }
            if missing_bytes == 0 {
                complete_devices.insert(record.sender_member);
            }
        }
    }
    view.complete_copies = complete_devices.len();
    view.single_copy = view.complete_copies == 1;
    let mut proposals: Vec<(String, String)> = files::tips(storage, group, members, selected)?
        .into_iter()
        .filter(|t| versions::first_missing(path, selected, &t.version).ok().flatten().is_none())
        .map(|t| (t.account, t.version.id))
        .collect();
    if let (Some(me), Some(mine)) = (&me, versions::read_ref(path, Ref::Mine)?) {
        proposals.push((me.clone(), mine.id));
    }
    proposals.sort();
    proposals.dedup();
    for (account, oid) in proposals {
        let in_shared = |a: &str, b: &str| versions::is_ancestor(path, selected, a, b).unwrap_or(false);
        if shared.as_deref().is_some_and(|s| in_shared(&oid, s)) {
            continue;
        }
        let Some(version) = versions::version(path, selected, &oid)? else { continue };
        view.proposals.push(WorkspaceProposalView {
            name: members
                .iter()
                .find(|m| m.account_id == account)
                .map(|m| m.display_name.clone())
                .unwrap_or_else(|| account.clone()),
            accepted: current.allows(selected, &oid),
            rejected: current.rejected(selected, &oid),
            account_id: account,
            subject: version.message.clone(),
            changed: files::changed_between(&record, shared.as_deref(), &oid)?,
            stale: shared.as_deref().is_some_and(|s| !in_shared(s, &oid)),
            oid,
        });
    }
    Ok(view)
}

pub(super) fn validate_version_access(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    repo: &str,
    oid: &str,
    shared: Option<&str>,
) -> OmniResult<()> {
    let event = omni_protocol::WorkspaceApprovalV1 {
        repo: repo.into(),
        oid: oid.into(),
    };
    validate_workspace_approval_v1(&event)?;
    if shared == Some(oid) {
        return Ok(());
    }
    // A version of the accepted line is the project's: any member sees its files, as they are served.
    if let (Some(shared), Some(record)) = (shared, storage.get_workspace_repo(&group.group_id, repo)?) {
        if versions::is_ancestor(Path::new(&record.published_path), repo, oid, shared).unwrap_or(false) {
            return Ok(());
        }
    }
    let current = state(storage, group, members, repo)?;
    let me = my_account(members, group).ok_or_else(|| forbidden("not a member"))?;
    let offered = files::tips(storage, group, members, repo)?.iter().any(|t| t.version.id == oid);
    if offered && current.reviewers.contains(&me) {
        return Ok(());
    }
    let record = storage
        .get_workspace_repo(&group.group_id, repo)?
        .ok_or_else(|| forbidden("unknown repository"))?;
    if versions::read_ref(Path::new(&record.published_path), Ref::Mine)?.is_some_and(|m| m.id == oid) {
        return Ok(());
    }
    Err(forbidden("this version is not available for review"))
}

/// What a member's node is served for object `oid` of a files project: a version record to any
/// member (versions are what the line is made of), content only when it is in the accepted line —
/// knowing a pending file's hash is not permission to download it — or to reviewers and the
/// account's own devices, who can also inspect proposals. A code repository has no such objects.
/// Why [`object_to_serve`] turns a request down when the object simply is not on this node: the asker
/// is told «not here» and tries another node, not left with a stream that closed.
pub(crate) const CONTENT_NOT_HERE: &str = "the content is not here";

pub(crate) fn object_to_serve(
    db: &str,
    group_id: &str,
    repo: &str,
    peer: &MeshNodeAddress,
    oid: &str,
    published: &Path,
) -> OmniResult<PathBuf> {
    let storage = Storage::open(db)?;
    if !storage.workspace_repo_is_files(group_id, repo)? {
        return Err(forbidden("not a files project"));
    }
    let version = versions::version_path(published, oid);
    if version.is_file() {
        return Ok(version);
    }
    let group = storage
        .get_group(group_id)?
        .ok_or_else(|| forbidden("unknown workspace"))?;
    let members = super::agents::members(&storage, group_id)?;
    let peer_hex = hex::encode(peer.as_bytes());
    let member = members
        .iter()
        .find(|m| m.status == "member" && m.node_hint_hex.eq_ignore_ascii_case(&peer_hex))
        .ok_or_else(|| forbidden("not a workspace member"))?;
    let record = storage
        .get_workspace_repo(group_id, repo)?
        .ok_or_else(|| forbidden("unknown repository"))?;
    let here = || {
        versions::Here::read(published, record.work_path.as_deref().map(Path::new))
            .file(oid, None)
            .ok_or_else(|| forbidden(CONTENT_NOT_HERE))
    };
    let current = state(&storage, &group, &members, repo)?;
    if current.reviewers.contains(&member.account_id)
        || my_account(&members, &group).as_deref() == Some(&member.account_id)
    {
        return here();
    }
    let shared = versions::read_ref(published, Ref::Shared)?
        .ok_or_else(|| forbidden("no accepted version"))?
        .id;
    let key = (group_id.to_string(), repo.to_string());
    let cached = with_profile(db, |p| {
        p.accepted_objects
            .get(&key)
            .filter(|(at, _)| *at == shared)
            .map(|(_, ids)| ids.contains(oid))
    })
    .flatten();
    let accepted = match cached {
        Some(accepted) => accepted,
        None => {
            let ids: HashSet<String> = files::line_contents(published, repo, &shared)?
                .into_iter()
                .map(|c| c.id)
                .collect();
            let accepted = ids.contains(oid);
            with_profile(db, |p| p.accepted_objects.insert(key, (shared, ids)));
            accepted
        }
    };
    if accepted {
        here()
    } else {
        Err(forbidden("file content has not been accepted"))
    }
}

pub(super) fn default_folder(root: &str, group: &GroupRecord, repo: &str) -> PathBuf {
    let mut name: String = group
        .name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(64)
        .collect();
    name = name.trim_matches([' ', '.']).to_string();
    if name.is_empty() {
        name = "Workspace".into();
    }
    Path::new(root)
        .join(format!(
            "{name}-{}",
            &group.group_id[..8.min(group.group_id.len())]
        ))
        .join(repo)
}

/// Tasks are on once an admin's policy for the tasks project is known.
fn require_tasks(current: &CustodyState) -> OmniResult<()> {
    if current.policy.changed_at_unix_ms == 0 {
        return Err(forbidden("tasks are not turned on in this workspace"));
    }
    Ok(())
}

pub(super) fn prepare_auto_folder(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    record: &WorkspaceRepoRecord,
) -> OmniResult<()> {
    if record.work_path.is_some() {
        return Ok(());
    }
    // The person removed the folder («Убрать папку»): it is not brought back on its own.
    if files::folder_released(storage, &group.group_id, &record.repo)? {
        return Ok(());
    }
    // Everyone takes the tasks: a folder of their own, chosen by nobody.
    if record.repo == super::tasks::TASKS_REPO {
        return super::tasks::prepare(storage, group).map(|_| ());
    }
    let current = state(storage, group, members, &record.repo)?;
    if current.archive_members.contains(&group.my_member_pub) {
        if let Some(folder) =
            storage.get_workspace_file_local(&group.group_id, &record.repo, "custody_folder")?
        {
            return bind(
                storage,
                &group.group_id,
                &record.repo,
                &Path::new(&folder).join(&record.repo).to_string_lossy(),
            );
        }
    }
    if current.policy.newcomers_download_current {
        if let Some(root) = storage.get_workspace_file_local("", "", "root")? {
            let folder = files::free_auto_folder(storage, default_folder(&root, group, &record.repo))?;
            return bind(storage, &group.group_id, &record.repo, &folder.to_string_lossy());
        }
    }
    Ok(())
}

/// Re-state only events we authored: never forge another member's consent for a newcomer.
pub(super) fn tell_newcomers(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
) -> OmniResult<()> {
    // `state` reads all the events through: once per repository, not once per event.
    let mut reviewers: HashMap<String, HashSet<String>> = HashMap::new();
    let admin = is_admin(members, &group.my_member_pub);
    for record in storage.list_workspace_file_events(&group.group_id)? {
        let Ok(event) = serde_json::from_str::<Event>(&record.event_json) else {
            continue;
        };
        // An admin's decisions that stand — the policy, custodians, retention, files given back —
        // every admin says again, whoever made them (A44, audit 2026-11-09): only their maker said
        // them before, and only while an admin, so one demoted or gone since left them with the old
        // members and never told a newcomer. A newcomer takes them from an admin now.
        let governing = matches!(
            event,
            Event::Policy { .. } | Event::Nominate { .. } | Event::Retention { .. } | Event::Lock { holder: Some(_), .. }
        );
        if admin && governing && record.sender_member != group.my_member_pub {
            say(storage, &group.group_id, GroupAppKindV1::WorkspaceFiles, &event)?;
            continue;
        }
        if record.sender_member == group.my_member_pub {
            match &event {
                Event::Policy { .. } | Event::Nominate { .. } | Event::Retention { .. }
                    if !is_admin(members, &group.my_member_pub) =>
                {
                    continue
                }
                Event::Review { repo, .. } | Event::Inventory { repo, .. } => {
                    if !reviewers.contains_key(repo) {
                        reviewers.insert(repo.clone(), state(storage, group, members, repo)?.reviewers);
                    }
                    if !reviewers[repo].contains(&record.sender_account) {
                        continue;
                    }
                }
                _ => {}
            }
            say(
                storage,
                &group.group_id,
                GroupAppKindV1::WorkspaceFiles,
                &event,
            )?;
        }
    }
    Ok(())
}

/// A version this node just made or moved the shared `main` to, accepted by this node's role now —
/// once: nothing when we are no reviewer or it is accepted already. Recorded as a review, so the
/// decision does not change with later roles or policy (`CustodyState::allows`).
pub(super) fn endorse_own(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    repo: &str,
    oid: &str,
) -> OmniResult<()> {
    let current = state(storage, group, members, repo)?;
    match my_account(members, group) {
        Some(me) if current.reviewers.contains(&me) && !current.accepted(repo, oid) => {
            endorse_shared(storage, group, members, repo, oid)
        }
        _ => Ok(()),
    }
}

/// An authorized reviewer can attest the accepted line it already holds, including versions once
/// accepted by a now-dismissed custodian. A newcomer need not reconstruct obsolete roles to get it.
pub(super) fn endorse_shared(
    storage: &Storage,
    group: &GroupRecord,
    members: &[GroupMemberRecord],
    repo: &str,
    oid: &str,
) -> OmniResult<()> {
    let Some(me) = my_account(members, group) else {
        return Ok(());
    };
    if !state(storage, group, members, repo)?.reviewers.contains(&me) {
        return Ok(());
    }
    let event = Event::Review {
        repo: repo.into(),
        oid: oid.into(),
        accepted: true,
        authority: review_authority(&state(storage, group, members, repo)?, &me, members),
        at_unix_ms: current_unix_ms(),
    };
    let key = event_key(&event, &group.my_member_pub);
    let existing = storage
        .list_workspace_file_events(&group.group_id)?
        .into_iter()
        .find(|e| {
            serde_json::from_str::<Event>(&e.event_json)
                .is_ok_and(|e| event_key(&e, &group.my_member_pub) == key)
                && e.sender_member == group.my_member_pub
        });
    if let Some(record) = existing {
        if matches!(
            serde_json::from_str::<Event>(&record.event_json),
            Ok(Event::Review { accepted: true, .. })
        ) {
            let event: Event = serde_json::from_str(&record.event_json)
                .map_err(|e| OmniError::Storage(e.to_string()))?;
            return say(
                storage,
                &group.group_id,
                GroupAppKindV1::WorkspaceFiles,
                &event,
            );
        }
    }
    publish(storage, group, members, event)
}
