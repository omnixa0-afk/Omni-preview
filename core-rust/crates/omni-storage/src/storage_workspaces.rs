// Workspaces — a group with git repositories (docs/decisions/omni-workspace-git.md).
//
// What lives here: the repositories a workspace group has here (where this profile's published bare
// repository is, and the person's working repository bound to it, if any), and the branches each
// member last announced in each repository (`WorkspaceRefsV1`, whole per member, not a diff), the
// group's rule for its shared `main`, the commits members approved for it and which repositories hold
// files rather than code. The shared `main`
// itself is a ref in the published repository. The objects themselves live in git, in the published bare repository. Profile-scoped.

/// A repository of a workspace, as this profile keeps it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceRepoRecord {
    pub group_id: String,
    pub repo: String,
    /// This profile's published bare repository: what members fetch from us.
    pub published_path: String,
    /// The person's working repository whose `omni` remote is `published_path` (none on a member
    /// who has not cloned it yet).
    pub work_path: Option<String>,
    /// The JSON of the branches we announced last (`WorkspaceRefsV1.refs`), so an unchanged
    /// repository is not announced again.
    pub announced_refs_json: Option<String>,
    pub created_at_unix_ms: i64,
}

/// A branch a member announced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceMemberRefRecord {
    pub account_id: String,
    /// The device (member leaf) that announced it last.
    pub member_pub: String,
    pub ref_name: String,
    pub oid: String,
    pub announced_at_unix_ms: i64,
    /// The objects are in our published repository (`refs/omni/members/<account>/...` points at it).
    pub fetched: bool,
}

/// Approvals one account keeps per repository: the newest ones (an approval is for the next move of
/// `main`, older ones are history).
pub const WORKSPACE_APPROVALS_PER_ACCOUNT: usize = 64;

/// A code project's rule for its shared `main` (none received: any maintainer, fast-forward).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkspaceMainRuleRecord {
    pub main_needs_approval: bool,
    pub changed_at_unix_ms: i64,
}

impl Storage {
    pub fn upsert_workspace_repo(&self, repo: &WorkspaceRepoRecord) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO workspace_repos (profile_id, group_id, repo, published_path, work_path, announced_refs_json, created_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(profile_id, group_id, repo) DO UPDATE SET
                   published_path = excluded.published_path,
                   work_path = excluded.work_path,
                   announced_refs_json = excluded.announced_refs_json",
                params![
                    profile_id.0,
                    repo.group_id,
                    repo.repo,
                    repo.published_path,
                    repo.work_path,
                    repo.announced_refs_json,
                    repo.created_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_workspace_repo(&self, group_id: &str, repo: &str) -> OmniResult<Option<WorkspaceRepoRecord>> {
        Ok(self.list_workspace_repos(group_id)?.into_iter().find(|r| r.repo == repo))
    }

    /// Where every project of `profile_id` keeps its data here (`published_path`) and its working
    /// folder (`work_path`) — for removing what of it is Omni's with the profile. The local profiles
    /// of one database share its folder, so a project another profile here still keeps is left out:
    /// one with the same folder, or of a group another profile is a member of (its folder may come
    /// later). A working folder another profile's project is bound to comes back as none.
    pub fn workspace_published_paths_for_profile(&self, profile_id: &LocalProfileId) -> OmniResult<Vec<(String, Option<String>)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT r.published_path,
                        CASE WHEN EXISTS (SELECT 1 FROM workspace_repos o WHERE o.profile_id <> r.profile_id AND o.work_path = r.work_path)
                             THEN NULL ELSE r.work_path END
                 FROM workspace_repos r
                 WHERE r.profile_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM workspace_repos o WHERE o.profile_id <> r.profile_id AND o.published_path = r.published_path)
                   AND NOT EXISTS (SELECT 1 FROM groups g WHERE g.profile_id <> r.profile_id AND g.group_id = r.group_id AND g.my_status = 'member')",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement.query_map(params![profile_id.0], |row| Ok((row.get(0)?, row.get(1)?))).map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// The working folders the projects of the other profiles of this database are bound to.
    pub fn workspace_work_paths_of_other_profiles(&self) -> OmniResult<Vec<String>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT work_path FROM workspace_repos WHERE profile_id <> ?1 AND work_path IS NOT NULL")
            .map_err(map_sqlite_error)?;
        let rows = statement.query_map(params![profile_id.0], |row| row.get(0)).map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn list_workspace_repos(&self, group_id: &str) -> OmniResult<Vec<WorkspaceRepoRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, repo, published_path, work_path, announced_refs_json, created_at_unix_ms
                 FROM workspace_repos WHERE profile_id = ?1 AND group_id = ?2 ORDER BY repo",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id], |row| {
                Ok(WorkspaceRepoRecord {
                    group_id: row.get(0)?,
                    repo: row.get(1)?,
                    published_path: row.get(2)?,
                    work_path: row.get(3)?,
                    announced_refs_json: row.get(4)?,
                    created_at_unix_ms: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// A member's announcement replaces what it announced before in that repository: branches it no
    /// longer lists are gone. A branch whose id did not change keeps its `fetched` mark.
    pub fn replace_workspace_member_refs(
        &self,
        group_id: &str,
        repo: &str,
        account_id: &str,
        member_pub: &str,
        refs: &[(String, String)],
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.with_immediate_transaction("replace_workspace_member_refs", |storage| {
            let kept: Vec<WorkspaceMemberRefRecord> = storage.list_workspace_member_refs(group_id, repo)?;
            storage
                .connection
                .execute(
                    "DELETE FROM workspace_member_refs WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3 AND account_id = ?4",
                    params![profile_id.0, group_id, repo, account_id],
                )
                .map_err(map_sqlite_error)?;
            for (name, oid) in refs {
                let fetched = kept
                    .iter()
                    .any(|k| k.account_id == account_id && &k.ref_name == name && &k.oid == oid && k.fetched);
                storage
                    .connection
                    .execute(
                        "INSERT INTO workspace_member_refs
                           (profile_id, group_id, repo, account_id, member_pub, ref_name, oid, announced_at_unix_ms, fetched)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![profile_id.0, group_id, repo, account_id, member_pub, name, oid, now_unix_ms, fetched],
                    )
                    .map_err(map_sqlite_error)?;
            }
            Ok(())
        })
    }

    pub fn list_workspace_member_refs(&self, group_id: &str, repo: &str) -> OmniResult<Vec<WorkspaceMemberRefRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT account_id, member_pub, ref_name, oid, announced_at_unix_ms, fetched FROM workspace_member_refs
                 WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3 ORDER BY account_id, ref_name",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id, repo], |row| {
                Ok(WorkspaceMemberRefRecord {
                    account_id: row.get(0)?,
                    member_pub: row.get(1)?,
                    ref_name: row.get(2)?,
                    oid: row.get(3)?,
                    announced_at_unix_ms: row.get(4)?,
                    fetched: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Every repository of the workspace known here: ours, and those members announced branches in.
    pub fn list_workspace_repo_names(&self, group_id: &str) -> OmniResult<Vec<String>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT repo FROM workspace_repos WHERE profile_id = ?1 AND group_id = ?2
                 UNION SELECT repo FROM workspace_member_refs WHERE profile_id = ?1 AND group_id = ?2
                 ORDER BY 1",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement.query_map(params![profile_id.0, group_id], |row| row.get(0)).map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// The objects of this branch at this id are in our published repository.
    pub fn mark_workspace_member_ref_fetched(
        &self,
        group_id: &str,
        repo: &str,
        account_id: &str,
        ref_name: &str,
        oid: &str,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE workspace_member_refs SET fetched = 1
                 WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3 AND account_id = ?4 AND ref_name = ?5 AND oid = ?6",
                params![profile_id.0, group_id, repo, account_id, ref_name, oid],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_workspace_main_rule(&self, group_id: &str, repo: &str) -> OmniResult<WorkspaceMainRuleRecord> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT main_needs_approval, changed_at_unix_ms FROM workspace_main_rules WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3",
                params![profile_id.0, group_id, repo],
                |row| Ok(WorkspaceMainRuleRecord { main_needs_approval: row.get(0)?, changed_at_unix_ms: row.get(1)? }),
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(Option::unwrap_or_default)
    }

    /// Keeps the later rule: `false` when `rule` is not newer than what is stored.
    pub fn put_workspace_main_rule_if_newer(&self, group_id: &str, repo: &str, rule: &WorkspaceMainRuleRecord) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let changed = self
            .connection
            .execute(
                "INSERT INTO workspace_main_rules (profile_id, group_id, repo, main_needs_approval, changed_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(profile_id, group_id, repo) DO UPDATE SET
                   main_needs_approval = excluded.main_needs_approval,
                   changed_at_unix_ms = excluded.changed_at_unix_ms
                 WHERE excluded.changed_at_unix_ms > workspace_main_rules.changed_at_unix_ms",
                params![profile_id.0, group_id, repo, rule.main_needs_approval, rule.changed_at_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(changed > 0)
    }

    /// `account_id` approves `oid` for `main` in `repo`; only its newest
    /// [`WORKSPACE_APPROVALS_PER_ACCOUNT`] approvals there are kept.
    pub fn add_workspace_approval(&self, group_id: &str, repo: &str, account_id: &str, oid: &str, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.with_immediate_transaction("add_workspace_approval", |storage| {
            storage
                .connection
                .execute(
                    "INSERT INTO workspace_approvals (profile_id, group_id, repo, account_id, oid, approved_at_unix_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(profile_id, group_id, repo, account_id, oid) DO UPDATE SET approved_at_unix_ms = excluded.approved_at_unix_ms",
                    params![profile_id.0, group_id, repo, account_id, oid, now_unix_ms],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "DELETE FROM workspace_approvals WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3 AND account_id = ?4
                       AND oid NOT IN (SELECT oid FROM workspace_approvals
                                       WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3 AND account_id = ?4
                                       ORDER BY approved_at_unix_ms DESC, oid LIMIT ?5)",
                    params![profile_id.0, group_id, repo, account_id, WORKSPACE_APPROVALS_PER_ACCOUNT as i64],
                )
                .map_err(map_sqlite_error)?;
            Ok(())
        })
    }

    /// The repository holds files (models, drawings, documents), not code: set by whoever adds it so,
    /// or learnt from a member's announcement; it never goes back to code.
    pub fn mark_workspace_repo_files(&self, group_id: &str, repo: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO workspace_repo_files (profile_id, group_id, repo) VALUES (?1, ?2, ?3)",
                params![profile_id.0, group_id, repo],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn workspace_repo_is_files(&self, group_id: &str, repo: &str) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT 1 FROM workspace_repo_files WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3",
                params![profile_id.0, group_id, repo],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|found| found.is_some())
    }

    /// The approvals in `repo`: (account, commit) pairs.
    pub fn list_workspace_approvals(&self, group_id: &str, repo: &str) -> OmniResult<Vec<(String, String)>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT account_id, oid FROM workspace_approvals WHERE profile_id = ?1 AND group_id = ?2 AND repo = ?3
                 ORDER BY account_id, oid",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id, repo], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}
