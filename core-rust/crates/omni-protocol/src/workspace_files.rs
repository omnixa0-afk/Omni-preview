//! Authenticated group events for file workspaces. Paths are local and never appear here.
use omni_types::{OmniError, OmniResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFilesPolicyV1 {
    pub participants_need_approval: bool,
    pub newcomers_download_current: bool,
    /// Files are edited only once taken («Беру в работу»): everything else is read-only in the folder.
    /// Without it only files another member took are.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edit_only_taken: bool,
    /// Нормоконтроль: a revision released since `revisions_checked_since_unix_ms` counts once another
    /// account of the project's reviewers (an admin or a custodian) approved it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub revisions_checked: bool,
    /// When the rule was turned on (kept while it stays on): revisions released before need no approval.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revisions_checked_since_unix_ms: i64,
    pub changed_at_unix_ms: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl Default for WorkspaceFilesPolicyV1 {
    fn default() -> Self {
        Self {
            participants_need_approval: true,
            newcomers_download_current: false,
            edit_only_taken: false,
            revisions_checked: false,
            revisions_checked_since_unix_ms: 0,
            changed_at_unix_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkspaceFilesEventV1 {
    Inventory {
        repo: String,
        oid: String,
        history_bytes: u64,
        missing_bytes: u64,
        at_unix_ms: i64,
    },
    /// One files project's rule for members' versions and newcomers (maintainers' decision 2026-11-01:
    /// each project has its own).
    Policy {
        repo: String,
        policy: WorkspaceFilesPolicyV1,
    },
    /// A custodian of one project (repository) of the workspace.
    Nominate {
        account_id: String,
        repo: String,
        nomination: String,
        enabled: bool,
        at_unix_ms: i64,
    },
    Consent {
        account_id: String,
        repo: String,
        nomination: String,
        accepted: bool,
        at_unix_ms: i64,
    },
    Review {
        repo: String,
        oid: String,
        accepted: bool,
        /// The custodian appointment this decision relies on; admins need no appointment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<String>,
        at_unix_ms: i64,
    },
    /// The last version of the sender device's line in a project: members fetch the version (and the
    /// ones it builds on) by its id. The latest one per device counts.
    Tip {
        repo: String,
        version: WorkspaceContentV1,
        at_unix_ms: i64,
    },
    /// «Выпустить ревизию»: an admin or a custodian names an accepted version of the project (A, B, …);
    /// its content is kept for good. The latest event per version counts.
    Revision {
        repo: String,
        version: String,
        label: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        note: String,
        /// The custodian appointment this relies on; admins need none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<String>,
        at_unix_ms: i64,
    },
    /// Нормоконтроль: an admin or a custodian other than the one who released it approves revision
    /// `label` of `version`; an approval of another designation does not count.
    RevisionApproval {
        repo: String,
        version: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<String>,
        at_unix_ms: i64,
    },
    /// «Сохранить навсегда» (or taking it back) for an accepted version: its content is not thinned out.
    Keep {
        repo: String,
        version: String,
        keep: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authority: Option<String>,
        at_unix_ms: i64,
    },
    /// The admin's confirmed rule for keeping old versions' content: the last `keep_last` accepted
    /// versions and those of the last `keep_days` days; revisions, «навсегда» and the current one
    /// always. Until a project has one, nothing is thinned out. The latest one counts. Files named
    /// by `only_current` (`*.stl`: exports rebuilt from the model) keep their content only in the
    /// current version, revisions and «навсегда».
    Retention {
        repo: String,
        keep_last: u32,
        keep_days: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        only_current: Vec<String>,
        at_unix_ms: i64,
    },
    /// «Беру в работу» (`take`, again to extend it until a later `until_unix_ms`) or «Отдать» for a
    /// file of the project, by the sender's account. Each node works out the holder alike: the
    /// earliest `since_unix_ms` of the takes neither given back nor run out (then the account id).
    /// An admin gives back another account's take by naming it in `holder`.
    Lock {
        repo: String,
        path: String,
        take: bool,
        /// When this hold began: an extension keeps the first take's time.
        since_unix_ms: i64,
        until_unix_ms: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
        at_unix_ms: i64,
    },
}

/// How long «Беру в работу» holds a file by default, and the longest one take may.
pub const WORKSPACE_LOCK_DEFAULT_MS: i64 = 24 * 60 * 60 * 1000;
pub const WORKSPACE_LOCK_MAX_MS: i64 = 7 * WORKSPACE_LOCK_DEFAULT_MS;

/// The most versions or days a retention rule names.
pub const WORKSPACE_RETENTION_MAX: u32 = 100_000;
/// The most «only the current version» masks of a rule, and the longest one in characters.
pub const WORKSPACE_MASKS_MAX: usize = 32;
pub const WORKSPACE_MASK_MAX_CHARS: usize = 64;

/// Whether a file's name (the last part of its path) fits `mask`: `*` any run, `?` one character,
/// letters in any case (`*.STL` is `*.stl`). Every node matches alike.
pub fn workspace_mask_matches(mask: &str, path: &str) -> bool {
    let name: Vec<char> = path.rsplit('/').next().unwrap_or(path).chars().flat_map(char::to_lowercase).collect();
    let mask: Vec<char> = mask.chars().flat_map(char::to_lowercase).collect();
    // The last `*` and where the name stood at it: a mismatch later moves it on by one.
    let (mut m, mut n, mut star) = (0, 0, None::<(usize, usize)>);
    while n < name.len() {
        if m < mask.len() && (mask[m] == '?' || mask[m] == name[n]) {
            m += 1;
            n += 1;
        } else if m < mask.len() && mask[m] == '*' {
            star = Some((m, n));
            m += 1;
        } else if let Some((at, from)) = star {
            m = at + 1;
            n = from + 1;
            star = Some((at, from + 1));
        } else {
            return false;
        }
    }
    mask[m..].iter().all(|&c| c == '*')
}

/// A revision's designation, in characters.
pub const WORKSPACE_REVISION_LABEL_MAX: usize = 16;
/// A revision's note, in bytes.
pub const WORKSPACE_REVISION_NOTE_MAX: usize = 1024;

/// The largest version record (`WorkspaceVersionV1` as JSON) a node takes.
pub const WORKSPACE_VERSION_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// A file's path in a project, in bytes.
pub const WORKSPACE_FILE_PATH_MAX: usize = 1024;
/// A version's comment, in bytes.
pub const WORKSPACE_VERSION_MESSAGE_MAX: usize = 4096;

/// Content by its id: SHA-256 of the bytes (lowercase hex) and their length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkspaceContentV1 {
    pub id: String,
    pub size: u64,
}

/// One file a version changes: its new content, or none when the version removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileChangeV1 {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<WorkspaceContentV1>,
}

/// A version of a files project (docs/decisions/workspace-files-storage.md): what changed against
/// its parent. Stored and fetched as content — its id is the SHA-256 of its JSON bytes — so it is
/// immutable and any member's node can hand it out. The author is the creator's claim: the device
/// that announced a line ([`WorkspaceFilesEventV1::Tip`]) is the authenticated part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceVersionV1 {
    pub repo: String,
    /// The version this one changes; none for a project's first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<WorkspaceContentV1>,
    pub author_account: String,
    pub author_name: String,
    pub at_unix_ms: i64,
    pub message: String,
    /// Sorted by path, each path once.
    pub changes: Vec<WorkspaceFileChangeV1>,
}

fn validate_content(content: &WorkspaceContentV1, max: u64) -> OmniResult<()> {
    let hex = content.id.len() == 64 && content.id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !hex || content.size > max {
        return Err(OmniError::InvalidInput("invalid workspace content id".into()));
    }
    Ok(())
}

/// A project file's path as every system can write it: `/`-separated plain segments, none of them
/// `.`/`..`, none with characters Windows refuses or a name it reserves, none Omni's own swap files.
pub fn validate_workspace_file_path(path: &str) -> OmniResult<()> {
    let bad = || OmniError::InvalidInput(format!("invalid project file path: {path}"));
    if path.is_empty() || path.len() > WORKSPACE_FILE_PATH_MAX {
        return Err(bad());
    }
    for segment in path.split('/') {
        let stem = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
        let lower = segment.to_lowercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with([' ', '.'])
            // Omni's own files beside the project's, and a git repository's: never a project file, in any
            // letter case (a case-insensitive disk takes `.GIT/config` for `.git/config`). A version
            // writing `.git/config` would set what the next `git` run executes (core.fsmonitor).
            || lower.starts_with(".omni-")
            || lower == ".git"
            || reserved
            || segment.chars().any(|c| c.is_control() || "\\:*?\"<>|".contains(c))
        {
            return Err(bad());
        }
    }
    Ok(())
}

impl WorkspaceVersionV1 {
    pub fn validate(&self) -> OmniResult<()> {
        crate::validate_workspace_repo_name(&self.repo)?;
        if self.author_account.is_empty()
            || self.author_account.len() > 256
            || self.author_name.len() > 256
            || self.message.len() > WORKSPACE_VERSION_MESSAGE_MAX
            || self.at_unix_ms < 0
        {
            return Err(OmniError::InvalidInput("invalid workspace version".into()));
        }
        if let Some(parent) = &self.parent {
            validate_content(parent, WORKSPACE_VERSION_MAX_BYTES)?;
        }
        for (i, change) in self.changes.iter().enumerate() {
            validate_workspace_file_path(&change.path)?;
            if i > 0 && self.changes[i - 1].path >= change.path {
                return Err(OmniError::InvalidInput("workspace version changes are not sorted".into()));
            }
            if let Some(content) = &change.content {
                validate_content(content, i64::MAX as u64)?;
            }
        }
        Ok(())
    }
}

impl WorkspaceFilesEventV1 {
    pub fn at(&self) -> i64 {
        match self {
            Self::Policy { policy, .. } => policy.changed_at_unix_ms,
            Self::Nominate { at_unix_ms, .. }
            | Self::Consent { at_unix_ms, .. }
            | Self::Inventory { at_unix_ms, .. }
            | Self::Review { at_unix_ms, .. }
            | Self::Revision { at_unix_ms, .. }
            | Self::RevisionApproval { at_unix_ms, .. }
            | Self::Keep { at_unix_ms, .. }
            | Self::Retention { at_unix_ms, .. }
            | Self::Lock { at_unix_ms, .. }
            | Self::Tip { at_unix_ms, .. } => *at_unix_ms,
        }
    }

    pub fn validate(&self, now: i64) -> OmniResult<()> {
        if self.at() < 0 || self.at() > now.saturating_add(5 * 60 * 1000) {
            return Err(OmniError::InvalidInput(
                "workspace event time is outside the allowed window".into(),
            ));
        }
        match self {
            Self::Nominate {
                account_id,
                repo,
                nomination,
                ..
            }
            | Self::Consent {
                account_id,
                repo,
                nomination,
                ..
            } => {
                crate::validate_workspace_repo_name(repo)?;
                if account_id.is_empty()
                    || account_id.len() > 256
                    || nomination.len() != 32
                    || !nomination.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return Err(OmniError::InvalidInput(
                        "invalid custodian nomination".into(),
                    ));
                }
            }
            Self::Review { repo, oid, .. } | Self::Inventory { repo, oid, .. } => {
                crate::validate_workspace_approval_v1(&crate::WorkspaceApprovalV1 {
                    repo: repo.clone(),
                    oid: oid.clone(),
                })?
            }
            Self::Tip { repo, version, .. } => {
                crate::validate_workspace_repo_name(repo)?;
                validate_content(version, WORKSPACE_VERSION_MAX_BYTES)?;
            }
            Self::Policy { repo, .. } => crate::validate_workspace_repo_name(repo)?,
            Self::RevisionApproval { repo, version, label, .. } => {
                crate::validate_workspace_repo_name(repo)?;
                validate_content(&WorkspaceContentV1 { id: version.clone(), size: 0 }, 0)?;
                let label = label.trim();
                if label.is_empty() || label.chars().count() > WORKSPACE_REVISION_LABEL_MAX || label.chars().any(char::is_control) {
                    return Err(OmniError::InvalidInput("invalid revision".into()));
                }
            }
            Self::Revision { repo, version, label, note, .. } => {
                crate::validate_workspace_repo_name(repo)?;
                validate_content(&WorkspaceContentV1 { id: version.clone(), size: 0 }, 0)?;
                let label = label.trim();
                if label.is_empty()
                    || label.chars().count() > WORKSPACE_REVISION_LABEL_MAX
                    || label.chars().any(char::is_control)
                    || note.len() > WORKSPACE_REVISION_NOTE_MAX
                {
                    return Err(OmniError::InvalidInput("invalid revision".into()));
                }
            }
            Self::Keep { repo, version, .. } => {
                crate::validate_workspace_repo_name(repo)?;
                validate_content(&WorkspaceContentV1 { id: version.clone(), size: 0 }, 0)?;
            }
            Self::Retention { repo, keep_last, keep_days, only_current, .. } => {
                crate::validate_workspace_repo_name(repo)?;
                let bad_mask = |m: &String| {
                    m.trim() != m || m.is_empty() || m.chars().count() > WORKSPACE_MASK_MAX_CHARS || m.chars().any(|c| c.is_control() || c == '/' || c == '\\' || c == ',')
                };
                if *keep_last == 0
                    || *keep_last > WORKSPACE_RETENTION_MAX
                    || *keep_days > WORKSPACE_RETENTION_MAX
                    || only_current.len() > WORKSPACE_MASKS_MAX
                    || only_current.iter().any(bad_mask)
                {
                    return Err(OmniError::InvalidInput("invalid retention rule".into()));
                }
            }
            Self::Lock { repo, path, take, since_unix_ms, until_unix_ms, holder, at_unix_ms } => {
                crate::validate_workspace_repo_name(repo)?;
                validate_workspace_file_path(path)?;
                let held = *since_unix_ms >= 0
                    && since_unix_ms <= at_unix_ms
                    && (!*take || (until_unix_ms > at_unix_ms && *until_unix_ms <= at_unix_ms.saturating_add(WORKSPACE_LOCK_MAX_MS)));
                let named = holder.as_ref().is_none_or(|h| !*take && !h.is_empty() && h.len() <= 256);
                if !held || !named {
                    return Err(OmniError::InvalidInput("invalid file lock".into()));
                }
            }
        }
        if let Self::Inventory {
            history_bytes,
            missing_bytes,
            ..
        } = self
        {
            if missing_bytes > history_bytes || *history_bytes > i64::MAX as u64 {
                return Err(OmniError::InvalidInput(
                    "invalid workspace inventory size".into(),
                ));
            }
        }
        if let Self::Review { authority: Some(id), .. }
        | Self::Revision { authority: Some(id), .. }
        | Self::RevisionApproval { authority: Some(id), .. }
        | Self::Keep { authority: Some(id), .. } = self
        {
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(OmniError::InvalidInput(
                    "invalid custodian review authority".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(c: char) -> WorkspaceContentV1 {
        WorkspaceContentV1 { id: c.to_string().repeat(64), size: 7 }
    }

    fn version(paths: &[&str]) -> WorkspaceVersionV1 {
        WorkspaceVersionV1 {
            repo: "model".into(),
            parent: Some(content('a')),
            author_account: "ann".into(),
            author_name: "Аня".into(),
            at_unix_ms: 1,
            message: "Версия".into(),
            changes: paths.iter().map(|p| WorkspaceFileChangeV1 { path: p.to_string(), content: Some(content('b')) }).collect(),
        }
    }

    #[test]
    fn a_version_takes_plain_sorted_paths_only() {
        assert!(version(&["a.prt", "сборка/корпус.sldasm"]).validate().is_ok());
        assert!(version(&["b.prt", "a.prt"]).validate().is_err(), "unsorted");
        assert!(version(&["a.prt", "a.prt"]).validate().is_err(), "twice");
        for bad in ["", "/a", "a/", "a//b", "../a", "a/./b", r"a\b", "c:x", "con.txt", "COM1", "dir/lpt3.log", "a.", "a ", ".omni-new~t~a", ".OMNI-new~t~a", "a\u{1}b", ".git", ".git/config", ".Git/hooks/pre-commit", "sub/.GIT/config"] {
            assert!(validate_workspace_file_path(bad).is_err(), "{bad:?} must be refused");
        }
        for good in [".gitignore", ".gitattributes", "a.git/x", "comet.txt", "console/a", "dir/com10.txt", "Модель v2.prt"] {
            assert!(validate_workspace_file_path(good).is_ok(), "{good:?} must be taken");
        }
    }

    #[test]
    fn a_revision_has_a_short_plain_designation() {
        let revision = |label: &str, note: &str| WorkspaceFilesEventV1::Revision {
            repo: "model".into(),
            version: "a".repeat(64),
            label: label.into(),
            note: note.into(),
            authority: None,
            at_unix_ms: 1,
        };
        assert!(revision("A", "").validate(2).is_ok());
        assert!(revision("Изм. 2", "для заказчика").validate(2).is_ok());
        assert!(revision(" ", "").validate(2).is_err());
        assert!(revision(&"Б".repeat(17), "").validate(2).is_err());
        assert!(revision("A
B", "").validate(2).is_err());
        assert!(revision("A", &"x".repeat(WORKSPACE_REVISION_NOTE_MAX + 1)).validate(2).is_err());
    }

    #[test]
    fn a_retention_rule_keeps_at_least_the_current_version() {
        let rule = |keep_last, keep_days| WorkspaceFilesEventV1::Retention { repo: "model".into(), keep_last, keep_days, only_current: Vec::new(), at_unix_ms: 1 };
        assert!(rule(10, 30).validate(2).is_ok());
        assert!(rule(1, 0).validate(2).is_ok());
        assert!(rule(0, 30).validate(2).is_err());
        assert!(rule(10, WORKSPACE_RETENTION_MAX + 1).validate(2).is_err());
        let masks = |only: &[&str]| WorkspaceFilesEventV1::Retention {
            repo: "model".into(),
            keep_last: 10,
            keep_days: 30,
            only_current: only.iter().map(|m| m.to_string()).collect(),
            at_unix_ms: 1,
        };
        assert!(masks(&["*.stl", "*.step", "чертеж?.pdf"]).validate(2).is_ok());
        for bad in ["", " *.stl", "out/*.stl", "*.stl,*.pdf", "a\nb"] {
            assert!(masks(&[bad]).validate(2).is_err(), "{bad:?}");
        }
        assert!(masks(&["*.x"; WORKSPACE_MASKS_MAX + 1]).validate(2).is_err());
        // A rule without masks reads and writes as before them.
        let plain = serde_json::to_string(&rule(10, 30)).unwrap();
        assert!(!plain.contains("only_current"));
        assert_eq!(serde_json::from_str::<WorkspaceFilesEventV1>(&plain).unwrap(), rule(10, 30));
    }

    #[test]
    fn a_mask_names_files_by_their_name_in_any_case() {
        assert!(workspace_mask_matches("*.stl", "детали/корпус.STL"));
        assert!(workspace_mask_matches("*.stl", "a.stl"));
        assert!(!workspace_mask_matches("*.stl", "a.stl.bak"));
        assert!(!workspace_mask_matches("*.stl", "stl/a.step"));
        assert!(workspace_mask_matches("лист?.pdf", "out/Лист1.pdf"));
        assert!(!workspace_mask_matches("лист?.pdf", "лист12.pdf"));
        assert!(workspace_mask_matches("*", "anything"));
        assert!(workspace_mask_matches("*a*b*", "xxaYYbzz"));
        assert!(!workspace_mask_matches("*a*b", "xxaYYbzz"));
    }

    #[test]
    fn a_lock_holds_a_file_for_a_week_at_most_and_names_a_holder_only_to_give_it_back() {
        let lock = |take, since, until, holder: Option<&str>| WorkspaceFilesEventV1::Lock {
            repo: "model".into(),
            path: "сборка/корпус.prt".into(),
            take,
            since_unix_ms: since,
            until_unix_ms: until,
            holder: holder.map(str::to_string),
            at_unix_ms: 100,
        };
        assert!(lock(true, 100, 100 + WORKSPACE_LOCK_DEFAULT_MS, None).validate(200).is_ok());
        assert!(lock(true, 50, 100 + WORKSPACE_LOCK_MAX_MS, None).validate(200).is_ok(), "an extension keeps its start");
        assert!(lock(true, 100, 100 + WORKSPACE_LOCK_MAX_MS + 1, None).validate(200).is_err());
        assert!(lock(true, 100, 100, None).validate(200).is_err(), "runs out at once");
        assert!(lock(true, 150, 200, None).validate(200).is_err(), "begins after it was said");
        assert!(lock(false, 100, 0, None).validate(200).is_ok());
        assert!(lock(false, 100, 0, Some("ivan")).validate(200).is_ok());
        assert!(lock(true, 100, 200, Some("ivan")).validate(200).is_err(), "a holder is named only to give back");
        let mut bad = lock(true, 100, 200, None);
        if let WorkspaceFilesEventV1::Lock { path, .. } = &mut bad {
            *path = "../x".into();
        }
        assert!(bad.validate(200).is_err());
        // A policy without the rule reads and writes as before it.
        let plain = serde_json::to_string(&WorkspaceFilesPolicyV1::default()).unwrap();
        assert!(!plain.contains("edit_only_taken") && !plain.contains("revisions_checked"));
    }

    #[test]
    fn a_tip_names_a_version_by_its_hash() {
        let tip = |id: &str, size| WorkspaceFilesEventV1::Tip {
            repo: "model".into(),
            version: WorkspaceContentV1 { id: id.into(), size },
            at_unix_ms: 1,
        };
        assert!(tip(&"a".repeat(64), 10).validate(2).is_ok());
        assert!(tip(&"A".repeat(64), 10).validate(2).is_err());
        assert!(tip(&"a".repeat(40), 10).validate(2).is_err());
        assert!(tip(&"a".repeat(64), WORKSPACE_VERSION_MAX_BYTES + 1).validate(2).is_err());
    }
}
