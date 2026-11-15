//! Versions of a files project without git (docs/decisions/workspace-files-storage.md, stage 1).
//!
//! A project's local data lives in its own folder (`<profile dir>/workspaces/<group>/<repo>.files`):
//! - `versions/<id>` — every version record (`WorkspaceVersionV1` JSON) seen, by the SHA-256 of its
//!   bytes. A version lists what changed against its parent; the whole tree is the chain's sum.
//! - `refs/mine`, `refs/shared`, `refs/applied` — our line's tip, the shared line every node works
//!   out alike, and the version the working folder holds. Each moves only from the value read.
//! - the content store (`app_workspace_content_store`: here or on the disk a custodian chose) — file
//!   contents by their SHA-256, and `index.json`: what each file of the working folder hashed to
//!   at its size and time, so only changed files are read again.
//!
//! The working folder is the store of the version it holds: its files are served as they are (by
//! the index, checked against the disk), received content is copied in and, on a member who keeps
//! no archive, dropped from the store afterwards.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use cap_fs_ext::DirExt as _;
use cap_std::fs::Dir;
use omni_protocol::{validate_workspace_file_path, WorkspaceContentV1 as Content, WorkspaceFileChangeV1, WorkspaceVersionV1, WORKSPACE_VERSION_MAX_BYTES};
use omni_types::{OmniError, OmniResult};
use sha2::Digest as _;

/// A version's whole tree: path → content.
pub(super) type Tree = BTreeMap<String, Content>;

/// Versions and trees by id. Both are fixed by the id (a version names its parent by hash), so a
/// cache entry is the same for every profile; kept small, emptied when a profile stops.
static VERSIONS: LazyLock<Mutex<HashMap<String, Arc<WorkspaceVersionV1>>>> = LazyLock::new(Default::default);
static TREES: LazyLock<Mutex<HashMap<String, Arc<Tree>>>> = LazyLock::new(Default::default);
const VERSIONS_CACHED: usize = 4096;
const TREES_CACHED: usize = 32;
/// The refs of all projects move one at a time: a move is a read and a write.
static REFS: Mutex<()> = Mutex::new(());

pub(crate) fn clear_version_caches() {
    if let Ok(mut cache) = VERSIONS.lock() {
        cache.clear();
    }
    if let Ok(mut cache) = TREES.lock() {
        cache.clear();
    }
}

fn storage_error(what: impl std::fmt::Display) -> OmniError {
    OmniError::Storage(what.to_string())
}

/// A file name no one else uses, for a write that is renamed into place.
fn temp_name(base: &Path) -> PathBuf {
    base.with_extension(format!("{}.omni-tmp", hex::encode(&omni_crypto::generate_group_key()[..8])))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> OmniResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| storage_error(format!("{}: {e}", parent.display())))?;
    }
    let temp = temp_name(path);
    // The bytes are on the disk before the name moves to them: a ref that survives a crash names
    // what was written, not an empty file (F03, audit 2026-11-09).
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(storage_error(format!("{}: {e}", temp.display())));
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        storage_error(format!("{}: {e}", path.display()))
    })
}

pub(super) fn is_content_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

// ---- versions ----

pub(super) fn version_path(dir: &Path, id: &str) -> PathBuf {
    dir.join("versions").join(id)
}

pub(super) fn versions_temp_dir(dir: &Path) -> PathBuf {
    dir.join("tmp")
}

/// The version `id` if its record is here (checked against its id and the project when read).
pub(super) fn version(dir: &Path, repo: &str, id: &str) -> OmniResult<Option<Arc<WorkspaceVersionV1>>> {
    if !is_content_id(id) {
        return Err(OmniError::InvalidInput("not a version id".into()));
    }
    // The cache knows what a version says, not whether this project holds it: it is shared by every
    // project and profile, and answering "here" from it said a version another project of the same
    // process held was in this store — it was never fetched, and once the cache was emptied the
    // project's tree could not be read («is not here yet»; a flaky test with two profiles in one
    // process, 2026-11-09).
    if let Some(found) = VERSIONS.lock().ok().and_then(|c| c.get(id).cloned()) {
        if version_path(dir, id).is_file() {
            return Ok((found.repo == repo).then_some(found));
        }
        return Ok(None);
    }
    let bytes = match std::fs::read(version_path(dir, id)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // A file that is here but could not be read is not a version that is missing (and fetched
        // again, or removed as not ours): the caller fails and tries later.
        Err(error) => return Err(storage_error(format!("version {id}: {error}"))),
    };
    if hex::encode(sha2::Sha256::digest(&bytes)) != id {
        let _ = std::fs::remove_file(version_path(dir, id));
        return Ok(None);
    }
    let parsed: WorkspaceVersionV1 = serde_json::from_slice(&bytes).map_err(|e| OmniError::InvalidInput(format!("version {id}: {e}")))?;
    parsed.validate()?;
    let parsed = Arc::new(parsed);
    if let Ok(mut cache) = VERSIONS.lock() {
        if cache.len() >= VERSIONS_CACHED {
            cache.clear();
        }
        cache.insert(id.to_string(), parsed.clone());
    }
    Ok((parsed.repo == repo).then_some(parsed))
}

/// Keep a version record here; its id.
pub(super) fn write_version(dir: &Path, record: &WorkspaceVersionV1) -> OmniResult<Content> {
    record.validate()?;
    let bytes = serde_json::to_vec(record).map_err(|e| OmniError::Internal(e.to_string()))?;
    if bytes.len() as u64 > WORKSPACE_VERSION_MAX_BYTES {
        return Err(OmniError::InvalidInput("the version lists too many files".into()));
    }
    let id = hex::encode(sha2::Sha256::digest(&bytes));
    let path = version_path(dir, &id);
    if !path.is_file() {
        write_atomic(&path, &bytes)?;
    }
    Ok(Content { id, size: bytes.len() as u64 })
}

/// A version that came from a member (already checked against its hash): kept when it is one of
/// this project's.
pub(super) fn take_fetched_version(dir: &Path, repo: &str, id: &str) -> OmniResult<()> {
    match version(dir, repo, id)? {
        Some(_) => Ok(()),
        None => {
            let _ = std::fs::remove_file(version_path(dir, id));
            Err(OmniError::InvalidInput(format!("version {id} is not one of this project's")))
        }
    }
}

/// The versions from `tip` back to the first, newest first; the first missing one ends it.
pub(super) fn chain(dir: &Path, repo: &str, tip: &str) -> OmniResult<(Vec<Arc<WorkspaceVersionV1>>, Vec<String>, Option<Content>)> {
    let (mut versions, mut ids) = (Vec::new(), Vec::new());
    let mut at = Some(Content { id: tip.to_string(), size: 0 });
    let mut seen = HashSet::new();
    while let Some(content) = at {
        if !seen.insert(content.id.clone()) {
            return Err(OmniError::InvalidInput("the versions form a loop".into()));
        }
        let Some(found) = version(dir, repo, &content.id)? else { return Ok((versions, ids, Some(content))) };
        at = found.parent.clone();
        ids.push(content.id);
        versions.push(found);
    }
    Ok((versions, ids, None))
}

/// The first version of `tip`'s chain not here yet (to fetch), none when all are.
pub(super) fn first_missing(dir: &Path, repo: &str, tip: &Content) -> OmniResult<Option<Content>> {
    let (_, ids, missing) = chain(dir, repo, &tip.id)?;
    Ok(match missing {
        // The tip itself: its size is the announcement's.
        Some(_) if ids.is_empty() => Some(tip.clone()),
        other => other,
    })
}

/// The whole tree of version `id`.
pub(super) fn tree(dir: &Path, repo: &str, id: &str) -> OmniResult<Arc<Tree>> {
    if let Some(found) = TREES.lock().ok().and_then(|c| c.get(id).cloned()) {
        return Ok(found);
    }
    let mut pending = Vec::new();
    let mut base: Option<Arc<Tree>> = None;
    let mut at = Some(id.to_string());
    while let Some(current) = at {
        if let Some(found) = TREES.lock().ok().and_then(|c| c.get(&current).cloned()) {
            base = Some(found);
            break;
        }
        let found = version(dir, repo, &current)?.ok_or_else(|| OmniError::InvalidInput(format!("version {current} is not here yet")))?;
        at = found.parent.as_ref().map(|p| p.id.clone());
        pending.push(found);
        if pending.len() > 1_000_000 {
            return Err(OmniError::InvalidInput("the versions form a loop".into()));
        }
    }
    let mut tree: Tree = base.map(|b| (*b).clone()).unwrap_or_default();
    for one in pending.iter().rev() {
        apply_changes(&mut tree, &one.changes);
    }
    let tree = Arc::new(tree);
    if let Ok(mut cache) = TREES.lock() {
        if cache.len() >= TREES_CACHED {
            cache.clear();
        }
        cache.insert(id.to_string(), tree.clone());
    }
    Ok(tree)
}

fn apply_changes(tree: &mut Tree, changes: &[WorkspaceFileChangeV1]) {
    for change in changes {
        match &change.content {
            Some(content) => tree.insert(change.path.clone(), content.clone()),
            None => tree.remove(&change.path),
        };
    }
}

/// The tree of an optional version (none: the empty project).
pub(super) fn tree_of(dir: &Path, repo: &str, id: Option<&str>) -> OmniResult<Arc<Tree>> {
    match id {
        Some(id) => tree(dir, repo, id),
        None => Ok(Arc::new(Tree::new())),
    }
}

/// What turns `from` into `to`, sorted by path.
pub(super) fn diff(from: &Tree, to: &Tree) -> Vec<WorkspaceFileChangeV1> {
    let mut changes: Vec<WorkspaceFileChangeV1> = to
        .iter()
        .filter(|(path, content)| from.get(*path) != Some(*content))
        .map(|(path, content)| WorkspaceFileChangeV1 { path: path.clone(), content: Some(content.clone()) })
        .collect();
    changes.extend(from.keys().filter(|path| !to.contains_key(*path)).map(|path| WorkspaceFileChangeV1 { path: path.clone(), content: None }));
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// `ancestor` is `descendant` or a version it builds on.
pub(super) fn is_ancestor(dir: &Path, repo: &str, ancestor: &str, descendant: &str) -> OmniResult<bool> {
    let (_, ids, _) = chain(dir, repo, descendant)?;
    Ok(ids.iter().any(|id| id == ancestor))
}

/// The rank of `tip`'s line from where it parted from `other` (`super::files::LineRank`): accounts
/// that built on it, its length, the earlier start, the smaller first id.
pub(super) fn line_rank(dir: &Path, repo: &str, other: &str, tip: &str) -> OmniResult<super::files::LineRank> {
    let (_, other_ids, _) = chain(dir, repo, other)?;
    let other_ids: HashSet<String> = other_ids.into_iter().collect();
    let (versions, ids, _) = chain(dir, repo, tip)?;
    let mut authors = HashSet::new();
    let mut first: Option<(i64, String)> = None;
    let mut count = 0;
    for (one, id) in versions.iter().zip(&ids) {
        if other_ids.contains(id) {
            break;
        }
        authors.insert(one.author_account.clone());
        count += 1;
        first = Some((one.at_unix_ms / 1000, id.clone()));
    }
    let (at, id) = first.unwrap_or_default();
    Ok((authors.len(), count, std::cmp::Reverse(at), std::cmp::Reverse(id)))
}

/// Our versions after `onto` (those of `mine` it does not build on) again on top of `onto`, oldest
/// first. A file both lines changed keeps `onto`'s content and ours goes beside it as
/// `name (who, date).ext`. The versions `left_out` says (turned down) do not come along, nor does
/// one that changes nothing on top of `onto` (a repeat of what it already has). The new tip.
pub(super) fn replay(dir: &Path, repo: &str, onto: &Content, mine: &str, left_out: &dyn Fn(&str) -> bool) -> OmniResult<Content> {
    let (_, onto_ids, _) = chain(dir, repo, &onto.id)?;
    let onto_ids: HashSet<String> = onto_ids.into_iter().collect();
    let (versions, ids, _) = chain(dir, repo, mine)?;
    let ours: Vec<(Arc<WorkspaceVersionV1>, String)> =
        versions.into_iter().zip(ids).take_while(|(_, id)| !onto_ids.contains(id)).collect();
    let mut tip = onto.clone();
    for (one, _) in ours.iter().rev().filter(|(_, id)| !left_out(id)) {
        let base = tree_of(dir, repo, one.parent.as_ref().map(|p| p.id.as_str()))?;
        let now = tree(dir, repo, &tip.id)?;
        let mut changes: BTreeMap<String, Option<Content>> = BTreeMap::new();
        let mut conflicts = Vec::new();
        for change in &one.changes {
            let theirs = now.get(&change.path);
            if theirs == base.get(&change.path) {
                changes.insert(change.path.clone(), change.content.clone());
            } else if theirs != change.content.as_ref() {
                if let Some(content) = &change.content {
                    conflicts.push((change.path.clone(), content.clone()));
                }
            }
        }
        // The copies go after every plain change, each under a name no file of the result has: a
        // copy named like a file the version adds (or one the line it goes onto has) replaced that
        // file (F4, audit 2026-11-09 #5).
        for (path, content) in conflicts {
            let name = free_conflict_name(&path, &one.author_name, one.at_unix_ms, |name| {
                let lower = name.to_lowercase();
                let in_changes = changes.iter().any(|(p, c)| c.is_some() && p.to_lowercase() == lower);
                let in_tree = now.keys().any(|p| p.to_lowercase() == lower && !matches!(changes.get(p), Some(None)));
                in_changes || in_tree
            });
            changes.insert(name, Some(content));
        }
        if changes.is_empty() {
            continue;
        }
        let record = WorkspaceVersionV1 {
            repo: repo.to_string(),
            parent: Some(tip.clone()),
            author_account: one.author_account.clone(),
            author_name: one.author_name.clone(),
            at_unix_ms: one.at_unix_ms,
            message: one.message.clone(),
            changes: changes.into_iter().map(|(path, content)| WorkspaceFileChangeV1 { path, content }).collect(),
        };
        tip = write_version(dir, &record)?;
    }
    Ok(tip)
}

/// The name of a conflict copy of `path` that `taken` says no file has (letter case aside):
/// `name (who, date).ext`, then `name (who 2, date).ext`, … — the shape [`conflict_base`] reads.
pub(super) fn free_conflict_name(path: &str, who: &str, at_unix_ms: i64, taken: impl Fn(&str) -> bool) -> String {
    let mut n = 1_u32;
    loop {
        let label = if n == 1 { conflict_label(who, at_unix_ms) } else { conflict_label(&format!("{} {n}", who.trim()), at_unix_ms) };
        let name = conflict_copy_name(path, &label);
        if !taken(&name) {
            return name;
        }
        n += 1;
    }
}

/// `dir/корпус.sldasm` → `dir/корпус (Петя, 30.10 10-42).sldasm`.
pub(super) fn conflict_copy_name(path: &str, label: &str) -> String {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let named = match file.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({label}).{extension}"),
        _ => format!("{file} ({label})"),
    };
    if dir.is_empty() {
        named
    } else {
        format!("{dir}/{named}")
    }
}

/// The file a conflict copy was kept beside ([`conflict_copy_name`]) and its «who, date» label:
/// `dir/корпус (Петя, 30.10 10-42).sldasm` → (`dir/корпус.sldasm`, `Петя, 30.10 10-42`).
pub(super) fn conflict_base(path: &str) -> Option<(String, String)> {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let (named, extension) = match file.rsplit_once('.') {
        Some((stem, extension)) if stem.ends_with(')') => (stem, Some(extension)),
        _ => (file, None),
    };
    let open = named.strip_suffix(')')?.rfind(" (")?;
    let (stem, label) = (&named[..open], &named[open + 2..named.len() - 1]);
    // «…, дд.мм чч-мм»: the date is what tells a copy from a file named with brackets.
    let (_, when) = label.rsplit_once(", ")?;
    let shape = when.len() == 11 && when.char_indices().all(|(i, c)| match i {
        2 => c == '.',
        5 => c == ' ',
        8 => c == '-',
        _ => c.is_ascii_digit(),
    });
    if stem.is_empty() || !shape {
        return None;
    }
    let base = match extension {
        Some(extension) => format!("{stem}.{extension}"),
        None => stem.to_string(),
    };
    Some((if dir.is_empty() { base } else { format!("{dir}/{base}") }, label.to_string()))
}

/// The conflict copies of a tree by the file they were kept beside (one that is still there).
pub(super) fn conflict_copies(tree: &Tree) -> BTreeMap<String, Vec<(String, String)>> {
    let mut copies: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for path in tree.keys() {
        if let Some((base, label)) = conflict_base(path).filter(|(base, _)| tree.contains_key(base)) {
            copies.entry(base).or_default().push((path.clone(), label));
        }
    }
    copies
}

/// «Выбрать итоговую»: a version on top of the newer of the shared line and ours where `base` holds
/// `keep`'s content (`keep` is `base` itself or one of its conflict copies) and its copies are gone.
/// Nothing is overwritten in the folder by this: it follows the version, like any other.
pub(super) fn choose_final(dir: &Path, repo: &str, base: &str, keep: &str, author: (&str, &str), message: &str, at_unix_ms: i64) -> OmniResult<Content> {
    let shared = read_ref(dir, Ref::Shared)?.ok_or_else(|| OmniError::InvalidInput("the project has no accepted version yet".into()))?;
    let mine = read_ref(dir, Ref::Mine)?;
    let parent = match &mine {
        Some(mine) if is_ancestor(dir, repo, &shared.id, &mine.id)? => mine.clone(),
        _ => shared,
    };
    let from = tree(dir, repo, &parent.id)?;
    let copies = conflict_copies(&from).remove(base).ok_or_else(|| OmniError::InvalidInput(format!("{base}: no conflict copies")))?;
    if keep != base && !copies.iter().any(|(copy, _)| copy == keep) {
        return Err(OmniError::InvalidInput(format!("{keep}: not a copy of {base}")));
    }
    let mut to = Arc::unwrap_or_clone(from.clone());
    if let Some(chosen) = from.get(keep).filter(|_| keep != base) {
        to.insert(base.to_string(), chosen.clone());
    }
    for (copy, _) in &copies {
        to.remove(copy);
    }
    let saved = write_version(dir, &WorkspaceVersionV1 {
        repo: repo.to_string(),
        parent: Some(parent),
        author_account: author.0.to_string(),
        author_name: if author.1.trim().is_empty() { "Omni".into() } else { author.1.trim().to_string() },
        at_unix_ms,
        message: message.to_string(),
        changes: diff(&from, &to),
    })?;
    if !update_ref(dir, Ref::Mine, Some(&saved), mine.as_ref())? {
        return Err(OmniError::Transport("the folder was updated meanwhile; try again".into()));
    }
    Ok(saved)
}

/// «Петя, 30.10 10-42»: who and when (this computer's local time).
pub(super) fn conflict_label(who: &str, at_unix_ms: i64) -> String {
    let (_, month, day, hour, minute) = local_time(at_unix_ms);
    let who = if who.trim().is_empty() { "Omni" } else { who.trim() };
    // A name may hold characters a file name cannot.
    let who: String = who.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    format!("{who}, {day:02}.{month:02} {hour:02}-{minute:02}")
}

/// (year, month, day, hour, minute) of a moment in this computer's time zone (UTC where it is not known).
fn local_time(at_unix_ms: i64) -> (i64, u32, u32, u32, u32) {
    let utc = utc_time(at_unix_ms);
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTime([u16; 8]); // year, month, weekday, day, hour, minute, second, ms
        #[link(name = "kernel32")]
        extern "system" {
            #[link_name = "SystemTimeToTzSpecificLocalTime"]
            fn system_time_to_local(zone: *const core::ffi::c_void, utc: *const SystemTime, local: *mut SystemTime) -> i32;
        }
        let (year, month, day, hour, minute) = utc;
        if (1601..=30_827).contains(&year) {
            let given = SystemTime([year as u16, month as u16, 0, day as u16, hour as u16, minute as u16, 0, 0]);
            let mut local = SystemTime::default();
            // SAFETY: both point at live SYSTEMTIME-shaped values; a null zone is the current one.
            if unsafe { system_time_to_local(std::ptr::null(), &given, &mut local) } != 0 {
                let l = local.0;
                return (i64::from(l[0]), u32::from(l[1]), u32::from(l[3]), u32::from(l[4]), u32::from(l[5]));
            }
        }
    }
    utc
}

/// (year, month, day, hour, minute) of a moment in UTC.
fn utc_time(at_unix_ms: i64) -> (i64, u32, u32, u32, u32) {
    let seconds = at_unix_ms.div_euclid(1000);
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, (rest / 3600) as u32, (rest % 3600 / 60) as u32)
}

// ---- refs ----

/// Our line's tip (what we announce), the shared line, the version the folder holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ref {
    Mine,
    Shared,
    Applied,
}

impl Ref {
    fn file(self) -> &'static str {
        match self {
            Self::Mine => "mine",
            Self::Shared => "shared",
            Self::Applied => "applied",
        }
    }
}

pub(super) fn read_ref(dir: &Path, name: Ref) -> OmniResult<Option<Content>> {
    let path = dir.join("refs").join(name.file());
    // Only a ref that is not there is no ref: one that could not be read is not "no history" (F09,
    // audit 2026-11-09) — taken as none, it made the project's line start over.
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage_error(format!("{}: {error}", path.display()))),
    };
    let mut parts = text.split_whitespace();
    match (parts.next(), parts.next().and_then(|s| s.parse::<u64>().ok())) {
        (Some(id), Some(size)) if is_content_id(id) => Ok(Some(Content { id: id.to_string(), size })),
        _ => Err(OmniError::Storage(format!("{} is damaged", dir.join("refs").join(name.file()).display()))),
    }
}

/// Move `name` to `new` only when it still is `expected`; false when it was moved meanwhile.
pub(super) fn update_ref(dir: &Path, name: Ref, new: Option<&Content>, expected: Option<&Content>) -> OmniResult<bool> {
    let _guard = REFS.lock().map_err(|_| OmniError::Internal("workspace refs lock was poisoned".into()))?;
    if read_ref(dir, name)?.as_ref() != expected {
        return Ok(false);
    }
    let path = dir.join("refs").join(name.file());
    match new {
        Some(content) => write_atomic(&path, format!("{} {}\n", content.id, content.size).as_bytes())?,
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(true)
}

// ---- folders not held here (stage 4a) ----

/// Which folders of the project this device leaves out: the person's choice, and what the folder
/// was last brought to (it follows the choice as it follows a version).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Left {
    Wanted,
    Held,
}

impl Left {
    fn file(self) -> &'static str {
        match self {
            Self::Wanted => "left-out",
            Self::Held => "left-out-held",
        }
    }
}

/// The folders (project paths, `a/b`) left out; none: the whole project.
///
/// A list that is there but cannot be read is an error, not "nothing left out" (F09, audit
/// 2026-11-09): the held one taken as empty made the files of a folder left out look deleted, and a
/// save published their deletion.
pub(super) fn left_out(dir: &Path, which: Left) -> OmniResult<Vec<String>> {
    let path = dir.join("refs").join(which.file());
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| storage_error(format!("{} is damaged: {e}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(storage_error(format!("{}: {error}", path.display()))),
    }
}

pub(super) fn set_left_out(dir: &Path, which: Left, folders: &[String]) -> OmniResult<()> {
    let path = dir.join("refs").join(which.file());
    if folders.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    write_atomic(&path, &serde_json::to_vec(folders).map_err(|e| OmniError::Internal(e.to_string()))?)
}

/// Whether `path` is inside one of the folders `left`.
pub(super) fn is_left_out(left: &[String], path: &str) -> bool {
    left.iter().any(|folder| path.strip_prefix(folder.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

/// `tree` without the files of the folders `left`.
pub(super) fn without(tree: &Tree, left: &[String]) -> Tree {
    if left.is_empty() {
        return tree.clone();
    }
    tree.iter().filter(|(path, _)| !is_left_out(left, path)).map(|(p, c)| (p.clone(), c.clone())).collect()
}

/// What the working folder holds of the version applied to it: the version without the folders it
/// leaves out.
pub(super) fn held_tree(dir: &Path, repo: &str) -> OmniResult<Tree> {
    let applied = read_ref(dir, Ref::Applied)?;
    Ok(without(&*tree_of(dir, repo, applied.as_ref().map(|a| a.id.as_str()))?, &left_out(dir, Left::Held)?))
}

/// Make a project's local folder.
pub(super) fn prepare_project(dir: &Path) -> OmniResult<()> {
    for sub in ["versions", "refs"] {
        std::fs::create_dir_all(dir.join(sub)).map_err(|e| storage_error(format!("{}: {e}", dir.display())))?;
    }
    Ok(())
}

// ---- content ----

pub(super) fn store_object(dir: &Path, id: &str) -> PathBuf {
    super::content::object_path(dir, id)
}

fn present(path: &Path, size: u64) -> bool {
    path.metadata().is_ok_and(|m| m.is_file() && m.len() == size)
}

/// Where content can be read here: the store (current or an earlier one), or a file of the working
/// folder that the index says holds it and that has not changed since.
pub(super) struct Here {
    dir: PathBuf,
    folder: HashMap<String, Vec<(PathBuf, IndexEntry)>>,
}

impl Here {
    pub(super) fn read(dir: &Path, work: Option<&Path>) -> Self {
        let mut folder: HashMap<String, Vec<(PathBuf, IndexEntry)>> = HashMap::new();
        if let Some(work) = work {
            for (path, entry) in read_index(dir).files {
                folder.entry(entry.id.clone()).or_default().push((work.join(path), entry));
            }
        }
        Self { dir: dir.to_path_buf(), folder }
    }

    /// The file holding content `id` (of `size` bytes when given).
    pub(super) fn file(&self, id: &str, size: Option<u64>) -> Option<PathBuf> {
        let stored = super::content::existing_object_path(&self.dir, id);
        if stored.metadata().is_ok_and(|m| m.is_file() && size.is_none_or(|size| m.len() == size)) {
            return Some(stored);
        }
        self.folder.get(id)?.iter().find_map(|(file, entry)| {
            (size.is_none_or(|size| entry.size == size) && stamp(file) == Some((entry.size, entry.modified))).then(|| file.clone())
        })
    }
}

/// Where content `id` of `size` bytes can be read here ([`Here`]).
pub(super) fn content_file(dir: &Path, work: Option<&Path>, id: &str, size: u64) -> Option<PathBuf> {
    Here::read(dir, work).file(id, Some(size))
}

/// Copy `file` into the store while hashing the copy: its content as it was read.
pub(super) fn store_file(dir: &Path, file: &Path) -> OmniResult<Content> {
    let temp_dir = super::content::store_path(dir).join("tmp");
    std::fs::create_dir_all(&temp_dir).map_err(|e| storage_error(format!("{}: {e}", temp_dir.display())))?;
    let temp = temp_dir.join(format!("{}.omni-part", hex::encode(&omni_crypto::generate_group_key()[..8])));
    let copied = (|| -> OmniResult<Content> {
        let mut input = std::fs::File::open(file).map_err(|e| storage_error(format!("{}: {e}", file.display())))?;
        let mut output = std::fs::File::create(&temp).map_err(|e| storage_error(format!("{}: {e}", temp.display())))?;
        let (mut hash, mut size) = (sha2::Sha256::new(), 0_u64);
        let mut buffer = vec![0_u8; 256 * 1024];
        loop {
            let n = input.read(&mut buffer).map_err(|e| storage_error(format!("{}: {e}", file.display())))?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n]).map_err(|e| storage_error(format!("{}: {e}", temp.display())))?;
            size += n as u64;
        }
        output.sync_all().map_err(|e| storage_error(e.to_string()))?;
        Ok(Content { id: hex::encode(hash.finalize()), size })
    })();
    let content = match copied {
        Ok(content) => content,
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    };
    let target = store_object(dir, &content.id);
    if present(&target, content.size) {
        let _ = std::fs::remove_file(&temp);
    } else {
        std::fs::create_dir_all(target.parent().expect("object folder")).map_err(|e| storage_error(e.to_string()))?;
        std::fs::rename(&temp, &target).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            storage_error(format!("{}: {e}", target.display()))
        })?;
    }
    Ok(content)
}

fn hash_file(file: &Path) -> std::io::Result<Content> {
    hash_reader(std::fs::File::open(file)?)
}

fn hash_reader(mut input: impl std::io::Read) -> std::io::Result<Content> {
    let (mut hash, mut size) = (sha2::Sha256::new(), 0_u64);
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        size += n as u64;
    }
    Ok(Content { id: hex::encode(hash.finalize()), size })
}

// ---- the working folder ----

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    id: String,
    size: u64,
    modified: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Index {
    files: BTreeMap<String, IndexEntry>,
}

fn read_index(dir: &Path) -> Index {
    std::fs::read(dir.join("index.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_index(dir: &Path, index: &Index) {
    if let Ok(bytes) = serde_json::to_vec(index) {
        // A cache: a failed write only means hashing again.
        let _ = write_atomic(&dir.join("index.json"), &bytes);
    }
}

/// Forget what the folder's files hashed to (another folder is bound).
pub(super) fn forget_index(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("index.json"));
}

/// (size, modification time in ns) of a file.
pub(super) fn stamp(path: &Path) -> Option<(u64, u64)> {
    file_stamp(path).ok().flatten()
}

/// [`stamp`] telling a file that is not there (or not a file: `None`) from one that could not be read.
fn file_stamp(path: &Path) -> std::io::Result<Option<(u64, u64)>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let modified = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other)?.as_nanos().min(u64::MAX as u128) as u64;
    Ok(Some((meta.len(), modified)))
}

/// What the working folder holds.
#[derive(Debug, Default)]
pub(super) struct Scan {
    pub files: Tree,
    /// Files whose path no other system could write (`validate_workspace_file_path`).
    pub invalid: Vec<String>,
    /// Files that could not be read (open in a program that locks them).
    pub unreadable: Vec<String>,
    /// Files changed since the index saw them, left unread by a quick look ([`look`]).
    pub unhashed: Vec<String>,
}

/// Omni's own names in a folder: a version's file being written or replaced, a git repository.
fn omni_own(name: &str) -> bool {
    let name = name.to_lowercase();
    name.starts_with(".omni-") || name == ".git"
}

/// Two paths of `tree` a case-insensitive disk (Windows, macOS by default) takes for one file — or a
/// file and a folder of another path there. Applying such a tree would write one over the other.
fn case_clash(tree: &Tree) -> Option<(String, String)> {
    let mut files: HashMap<String, &String> = HashMap::new();
    for path in tree.keys() {
        if let Some(other) = files.insert(path.to_lowercase(), path) {
            return Some((other.clone(), path.clone()));
        }
    }
    for path in tree.keys() {
        let lower = path.to_lowercase();
        let mut end = 0;
        while let Some(slash) = lower[end..].find('/') {
            end += slash;
            if let Some(file) = files.get(&lower[..end]) {
                return Some(((*file).clone(), path.clone()));
            }
            end += 1;
        }
    }
    None
}

/// Every file of the working folder with its content, hashed only where its size or time changed
/// since the index saw it. Links are not followed.
///
/// An application of a version the process did not live to complete is finished or undone first
/// ([`recover`]): the folder is read as one version, never half of two.
pub(super) fn scan(dir: &Path, work: &Path) -> OmniResult<Scan> {
    recover(dir, work)?;
    walk(dir, work, true)
}

/// The folder as the index knows it, nothing read: what changed size or time since it was hashed is
/// listed as `unhashed` — for the screen, which must not read a folder of models to draw a card.
pub(super) fn look(dir: &Path, work: &Path) -> OmniResult<Scan> {
    walk(dir, work, false)
}

fn walk(dir: &Path, work: &Path, hash: bool) -> OmniResult<Scan> {
    if !work.is_dir() {
        return Err(OmniError::InvalidInput(format!("the project folder {} is not there", work.display())));
    }
    let mut index = read_index(dir);
    let mut changed = false;
    let mut result = Scan::default();
    let mut seen = HashSet::new();
    let mut dirs = vec![(work.to_path_buf(), String::new())];
    while let Some((folder, prefix)) = dirs.pop() {
        // What could not be read is never taken as gone (F04, audit 2026-11-09): a file missing from
        // the scan is a deletion to a save. A folder that cannot be listed fails the scan; a file
        // whose kind or time cannot be read is unreadable, which holds a save and a version back.
        let entries = std::fs::read_dir(&folder).map_err(|e| storage_error(format!("{}: {e}", folder.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| storage_error(format!("{}: {e}", folder.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if omni_own(&name) {
                continue;
            }
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let Ok(kind) = entry.file_type() else {
                result.unreadable.push(path);
                continue;
            };
            if kind.is_dir() {
                dirs.push((entry.path(), path));
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            if validate_workspace_file_path(&path).is_err() {
                result.invalid.push(path);
                continue;
            }
            let (size, modified) = match file_stamp(&entry.path()) {
                Ok(Some(stamp)) => stamp,
                // Removed since the folder was listed: gone indeed.
                Ok(None) => continue,
                Err(_) => {
                    seen.insert(path.clone());
                    result.unreadable.push(path);
                    continue;
                }
            };
            seen.insert(path.clone());
            let known = index.files.get(&path).filter(|e| e.size == size && e.modified == modified);
            let content = match known {
                Some(entry) => Content { id: entry.id.clone(), size },
                None if !hash => {
                    result.unhashed.push(path);
                    continue;
                }
                None => match hash_file(&entry.path()) {
                    Ok(content) => {
                        // Changed while it was read: hashed again next time.
                        if stamp(&entry.path()) == Some((size, modified)) {
                            index.files.insert(path.clone(), IndexEntry { id: content.id.clone(), size, modified });
                            changed = true;
                        }
                        content
                    }
                    Err(_) => {
                        result.unreadable.push(path);
                        continue;
                    }
                },
            };
            result.files.insert(path, content);
        }
    }
    let before = index.files.len();
    if hash {
        index.files.retain(|path, _| seen.contains(path));
    }
    if changed || index.files.len() != before {
        write_index(dir, &index);
    }
    result.invalid.sort();
    result.unreadable.sort();
    Ok(result)
}

/// The paths the folder changed against the tree it holds ([`held_tree`]), sorted; files in the
/// folders it leaves out (`left`) are not the project's here.
pub(super) fn folder_changes(scan: &Scan, held: &Tree, left: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = diff(held, &without(&scan.files, left)).into_iter().map(|c| c.path).collect();
    paths.extend(scan.unreadable.iter().chain(&scan.unhashed).filter(|p| !is_left_out(left, p)).cloned());
    paths.sort();
    paths.dedup();
    paths
}

/// A file a program holds open (Windows says "sharing violation" to an exclusive open).
pub(super) fn in_use(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        match std::fs::OpenOptions::new().read(true).share_mode(0).open(path) {
            Ok(_) => false,
            Err(error) => matches!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// Whether a folder between `work` and its file `path` is a link (std reports a Windows junction
/// as one too).
fn through_link(work: &Path, path: &str) -> bool {
    let mut at = work.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    parts[..parts.len().saturating_sub(1)].iter().any(|part| {
        at.push(part);
        std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink())
    })
}

/// Remove a file of the folder even when it is marked read-only (a file someone else holds, [`set_marks`]).
fn remove_anyway(path: &Path) {
    if std::fs::remove_file(path).is_err() {
        set_read_only(path, false);
        let _ = std::fs::remove_file(path);
    }
}

pub(super) fn set_read_only(path: &Path, read_only: bool) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        if permissions.readonly() != read_only {
            #[allow(clippy::permissions_set_readonly_false)] // Windows: the file's read-only attribute
            permissions.set_readonly(read_only);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

/// The files of the folder Omni marked read-only (`readonly.json`): only these are ever made writable
/// again, so a file the person made read-only stays so.
const MARKS: &str = "readonly.json";

/// Mark exactly `wanted` of the folder's files read-only: the ones marked before and not wanted now
/// become writable. Setting the attribute keeps the file's time, so the index stays good.
pub(super) fn set_marks(dir: &Path, work: &Path, wanted: &BTreeSet<String>) -> OmniResult<()> {
    let before: BTreeSet<String> = std::fs::read(dir.join(MARKS)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    for path in before.difference(wanted) {
        set_read_only(&work.join(path), false);
    }
    let mut marked = BTreeSet::new();
    for path in wanted {
        let file = work.join(path);
        if file.is_file() && !through_link(work, path) {
            set_read_only(&file, true);
            marked.insert(path.clone());
        }
    }
    if marked != before {
        write_atomic(&dir.join(MARKS), &serde_json::to_vec(&marked).map_err(|e| storage_error(e.to_string()))?)?;
    }
    Ok(())
}

/// Whether the folder's file at `path` holds `content`, as the index knows it (unchanged since hashed).
pub(super) fn holds(dir: &Path, work: &Path, path: &str, content: &Content) -> bool {
    read_index(dir).files.get(path).is_some_and(|e| e.id == content.id && e.size == content.size && stamp(&work.join(path)) == Some((e.size, e.modified)))
}

/// What holds the next version back, for the screen.
#[derive(Debug, Default)]
pub(super) struct FollowOutcome {
    pub waiting_open: Vec<String>,
    pub waiting_changed: Vec<String>,
    pub problem: Option<String>,
}

/// Bring the working folder from the version it holds to `target`, the whole version or nothing —
/// without the folders the person leaves out ([`Left::Wanted`]): a folder left out goes from the
/// disk (unless changed there), one taken back comes. Every content of `target` it needs must be
/// readable here (`content_file`). `keep_store`: the store keeps what was copied into the folder (an
/// archive does); otherwise it is dropped from the store.
pub(super) fn follow(dir: &Path, repo: &str, work: &Path, target: &Content, keep_store: bool) -> OmniResult<FollowOutcome> {
    let mut outcome = FollowOutcome::default();
    let applied = read_ref(dir, Ref::Applied)?;
    let (wanted, was) = (left_out(dir, Left::Wanted)?, left_out(dir, Left::Held)?);
    if applied.as_ref() == Some(target) && wanted == was {
        return Ok(outcome);
    }
    let held_tree = without(&*tree_of(dir, repo, applied.as_ref().map(|a| a.id.as_str()))?, &was);
    let target_tree = without(&*tree(dir, repo, &target.id)?, &wanted);
    // A version stored before its paths were checked as they are now (`.git/…`), or one whose paths
    // clash on this disk by letter case: nothing of it is written.
    if let Some(path) = target_tree.keys().find(|path| validate_workspace_file_path(path).is_err()) {
        outcome.problem = Some(format!("{path}: такой путь в проекте не допускается — версия не применяется"));
        return Ok(outcome);
    }
    if let Some((one, other)) = case_clash(&target_tree) {
        outcome.problem = Some(format!("{one} и {other}: на диске без различия регистра это один файл — версия не применяется"));
        return Ok(outcome);
    }
    let here = scan(dir, work)?;
    // What the scan saw of each file (size and time): a file set aside in the swap is checked
    // against it.
    let index = read_index(dir);
    // A file the version changes that the folder changed too (and not to the version's content)
    // holds it back; one that already is the version's needs no writing.
    let mut writes: Vec<(String, Option<Content>)> = Vec::new();
    let mut held = Vec::new();
    for change in diff(&held_tree, &target_tree) {
        let now = here.files.get(&change.path);
        // Something is at the path that the scan did not count as this path's file: the same file
        // under other letter case (`README.md` for `readme.md`), a folder, a link, a file of a name
        // the scan leaves out. Not the version's to replace — it waits like a changed file.
        if now.is_none() && !held_tree.contains_key(&change.path) {
            if let Ok(meta) = std::fs::symlink_metadata(work.join(&change.path)) {
                // Except a folder the version turns into a file and empties itself: every file in it
                // is the held version's and goes with this version (F06, audit 2026-11-09).
                let inside = format!("{}/", change.path);
                let emptied = meta.is_dir()
                    && change.content.is_some()
                    && here.files.iter().filter(|(p, _)| p.starts_with(&inside)).all(|(p, c)| held_tree.get(p) == Some(c) && !target_tree.contains_key(p))
                    && !here.unreadable.iter().chain(&here.invalid).any(|p| p.starts_with(&inside));
                if !emptied {
                    held.push(change.path.clone());
                    continue;
                }
            }
        }
        if now == change.content.as_ref() {
            // Already the version's: the folder serves it, the store need not hold it too.
            if let (false, Some(content)) = (keep_store, &change.content) {
                let _ = std::fs::remove_file(store_object(dir, &content.id));
            }
            continue;
        }
        if now != held_tree.get(&change.path) || here.unreadable.contains(&change.path) {
            held.push(change.path.clone());
            continue;
        }
        writes.push((change.path, change.content));
    }
    let open: Vec<String> = writes.iter().filter(|(path, _)| in_use(&work.join(path))).map(|(p, _)| p.clone()).collect();
    if !held.is_empty() || !open.is_empty() {
        outcome.waiting_changed = held;
        outcome.waiting_open = open;
        return Ok(outcome);
    }
    // A folder of the project that is a link (symlink, junction) would take the version's files
    // somewhere outside it: nothing is written, the folder says why.
    if let Some((path, _)) = writes.iter().find(|(path, _)| through_link(work, path)) {
        outcome.problem = Some(format!("{path}: одна из папок на пути — ссылка за пределы проекта; уберите её"));
        return Ok(outcome);
    }
    // One application's own folder (`.omni-apply~<tag>`, Omni's name — never read as the project's):
    // the version's files are written there first, then the swap moves the replaced ones there and
    // the new ones into place. A journal written before the first move lets a process that dies in
    // between finish or undo it on its next scan ([`recover`]): the folder holds one version, never a
    // mix (F03, audit 2026-11-09).
    let tag = hex::encode(&omni_crypto::generate_group_key()[..16]);
    let name = format!("{APPLYING}{tag}");
    let root = open_work(work).map_err(storage_error)?;
    let stage = work.join(&name);
    // Keep the digest outside the user-editable working folder. A journal without a matching receipt stays for manual recovery.
    let receipt = dir.join(&name);
    std::fs::create_dir(&stage).map_err(storage_error)?;
    let journal = Journal {
        target: target.clone(),
        from: applied.clone(),
        held_before: was.clone(),
        held_after: wanted.clone(),
        paths: writes.clone(),
    };
    let here_now = Here::read(dir, Some(work));
    let staged = (|| -> OmniResult<()> {
        for (path, content) in &writes {
            let Some(content) = content else { continue };
            let source = here_now
                .file(&content.id, Some(content.size))
                .ok_or_else(|| OmniError::Transport(format!("the content of {path} is not here yet")))?;
            copy_content(dir, &source, &stage.join("new").join(path), content)?;
        }
        let bytes = serde_json::to_vec(&journal).map_err(|e| OmniError::Internal(e.to_string()))?;
        write_atomic(&stage.join(JOURNAL), &bytes)?;
        write_atomic(&receipt, &sha2::Sha256::digest(&bytes))
    })();
    if let Err(error) = staged {
        finish_stage(&root, Path::new(&name), &receipt)?;
        return Err(error);
    }
    #[cfg(test)]
    if let Some(meanwhile) = tests::BEFORE_SWAP.with(|hook| hook.borrow_mut().take()) {
        meanwhile();
    }
    let mut steps = 0;
    let stop = swap(work, &stage, &writes, &held_tree, &index, &mut steps);
    let stop = match stop {
        None => commit(dir, target, applied.as_ref(), &wanted, &mut steps),
        stop => stop,
    };
    match stop {
        None => {}
        Some(Stop::Died) => return Ok(outcome),
        Some(stop) => {
            // Not a whole version: the folder goes back to the one it held (the next tick tries again).
            undo(&root, Path::new(&name), &journal)?;
            if left_out(dir, Left::Held)? != was {
                set_left_out(dir, Left::Held, &was)?;
            }
            finish_stage(&root, Path::new(&name), &receipt)?;
            match stop {
                Stop::Open(path) => outcome.waiting_open = vec![path],
                Stop::Changed(path) => outcome.waiting_changed = vec![path],
                Stop::Moved => outcome.problem = Some("версия папки сменилась во время обновления".into()),
                Stop::Failed(error) => return Err(error),
                Stop::Died => {}
            }
            return Ok(outcome);
        }
    }
    // Committed: the replaced files go with the application's folder.
    finish_stage(&root, Path::new(&name), &receipt)?;
    // The index learns the written files; the store keeps them only where it is an archive.
    let mut index = read_index(dir);
    for (path, content) in &writes {
        match content {
            Some(content) => {
                if let Some((size, modified)) = stamp(&work.join(path)) {
                    index.files.insert(path.clone(), IndexEntry { id: content.id.clone(), size, modified });
                }
                if !keep_store {
                    let _ = std::fs::remove_file(store_object(dir, &content.id));
                }
            }
            None => {
                index.files.remove(path);
            }
        }
    }
    write_index(dir, &index);
    Ok(outcome)
}

/// The folder of an application of a version in progress, at the root of the working folder.
const APPLYING: &str = ".omni-apply~";
/// What an application does, in its folder: written (and on the disk) before the first file moves.
const JOURNAL: &str = "journal.json";

/// One application of a version ([`follow`]): it is done when the folder's version is `target` and
/// the folders it leaves out are `held_after`; until then it is undone.
#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    target: Content,
    from: Option<Content>,
    held_before: Vec<String>,
    held_after: Vec<String>,
    /// The paths it writes, with the version's content there (none: it removes the file).
    paths: Vec<(String, Option<Content>)>,
}

/// Why a swap stopped.
enum Stop {
    /// A file is open in a program.
    Open(String),
    /// The person changed the file (or put one at the path) after the scan.
    Changed(String),
    /// The folder's version moved meanwhile.
    Moved,
    Failed(OmniError),
    /// Tests: the process "dies" here and leaves the disk as it is.
    Died,
}

/// Tests: the process dies at the `steps`-th move of an application (set aside, placed, a ref moved).
#[cfg(test)]
fn dies_here(steps: &mut usize) -> bool {
    *steps += 1;
    tests::DIE_AT.with(|at| at.get() == Some(*steps))
}

#[cfg(not(test))]
fn dies_here(_: &mut usize) -> bool {
    false
}

/// Copy content `content` from where [`Here`] found it (`source`) to `to` (a new file), checked
/// against its id: a file changed while it was read, or a damaged copy in the store, does not go out
/// as the version's. A damaged copy in the store is removed, so the content is fetched again from the
/// members instead of failing the same way on every try (audit 2026-11-09: the store did not check
/// what it read).
pub(super) fn copy_content(dir: &Path, source: &Path, to: &Path, content: &Content) -> OmniResult<()> {
    let copied = copy_checked(source, to, content);
    if copied.is_err() && source == super::content::existing_object_path(dir, &content.id) && hash_file(source).is_ok_and(|found| &found != content) {
        let _ = std::fs::remove_file(to);
        match std::fs::remove_file(source) {
            Ok(()) => crate::omni_app_trace_log("workspace/content", &format!("{} did not match its id: removed, fetched again", source.display())),
            Err(error) => crate::omni_app_trace_log("workspace/content", &format!("{} did not match its id and stays: {error}", source.display())),
        }
    }
    copied
}

/// Copy `source` to `to` (a new file), checking what was copied is `content`.
fn copy_checked(source: &Path, to: &Path, content: &Content) -> OmniResult<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| storage_error(format!("{}: {e}", parent.display())))?;
    }
    let mut input = std::fs::File::open(source).map_err(|e| storage_error(format!("{}: {e}", source.display())))?;
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(to).map_err(|e| storage_error(format!("{}: {e}", to.display())))?;
    let (mut hash, mut size) = (sha2::Sha256::new(), 0_u64);
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let n = input.read(&mut buffer).map_err(|e| storage_error(format!("{}: {e}", source.display())))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        out.write_all(&buffer[..n]).map_err(|e| storage_error(format!("{}: {e}", to.display())))?;
        size += n as u64;
    }
    out.sync_all().map_err(|e| storage_error(format!("{}: {e}", to.display())))?;
    if size != content.size || hex::encode(hash.finalize()) != content.id {
        return Err(OmniError::Transport(format!("{} changed while it was copied; tried again later", source.display())));
    }
    Ok(())
}

/// Move `from` to `to` only when nothing is at `to`: std's rename replaces a file there on Windows, and
/// a file someone saved at the path meanwhile must not go. A hard link fails when the name is taken;
/// a disk without links (FAT, exFAT) is checked just before the rename.
fn move_new(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            remove_anyway(from);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(error),
        Err(_) if std::fs::symlink_metadata(to).is_ok() => Err(std::io::ErrorKind::AlreadyExists.into()),
        Err(_) => std::fs::rename(from, to),
    }
}

/// Whether a file set aside is what the scan read at `path`, the held version's content: by size and
/// time as the index has them, or by its bytes.
fn as_scanned(aside: &Path, path: &str, held: &Tree, index: &Index) -> bool {
    let Some(content) = held.get(path) else { return false };
    if index.files.get(path).is_some_and(|e| e.id == content.id && e.size == content.size && stamp(aside) == Some((e.size, e.modified))) {
        return true;
    }
    hash_file(aside).is_ok_and(|found| &found == content)
}

/// Remove the folders between `work` and its file `path` that are empty now.
fn prune(work: &Path, path: &str) {
    let mut folder = work.join(path);
    while folder.pop() && folder.starts_with(work) && folder != work && std::fs::remove_dir(&folder).is_ok() {}
}

/// The swap of an application whose files are staged: every replaced or removed file into the
/// application's folder first — and checked there, so a file the person saved after the scan holds
/// the version back instead of going with the old version (F02, audit 2026-11-09) — then the empty
/// folders go (a folder the version turns into a file among them, F06), then the new files come.
fn swap(work: &Path, stage: &Path, writes: &[(String, Option<Content>)], held: &Tree, index: &Index, steps: &mut usize) -> Option<Stop> {
    for (path, _) in writes {
        let file = work.join(path);
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(meta) => meta,
            // Removed by the person since the scan: their change.
            Err(_) if held.contains_key(path) => return Some(Stop::Changed(path.clone())),
            Err(_) => continue,
        };
        if meta.is_dir() {
            // A folder the version turns into a file: emptied by the removals of its files.
            continue;
        }
        let aside = stage.join("old").join(path);
        let moved = aside.parent().is_some_and(|p| std::fs::create_dir_all(p).is_ok()) && std::fs::rename(&file, &aside).is_ok();
        if !moved {
            return Some(Stop::Open(path.clone()));
        }
        if dies_here(steps) {
            return Some(Stop::Died);
        }
        if !as_scanned(&aside, path, held, index) {
            return Some(Stop::Changed(path.clone()));
        }
    }
    for (path, _) in writes.iter().filter(|(_, content)| content.is_none()) {
        prune(work, path);
    }
    for (path, content) in writes {
        if content.is_none() {
            continue;
        }
        let file = work.join(path);
        if file.parent().is_some_and(|p| std::fs::create_dir_all(p).is_err()) {
            return Some(Stop::Changed(path.clone()));
        }
        match move_new(&stage.join("new").join(path), &file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Some(Stop::Changed(path.clone())),
            Err(_) => return Some(Stop::Open(path.clone())),
        }
        if dies_here(steps) {
            return Some(Stop::Died);
        }
    }
    None
}

/// The commit of an application: the folders it leaves out, then the folder's version — the moment
/// both match the journal it is done ([`recover`]).
fn commit(dir: &Path, target: &Content, applied: Option<&Content>, wanted: &[String], steps: &mut usize) -> Option<Stop> {
    if let Err(error) = set_left_out(dir, Left::Held, wanted) {
        return Some(Stop::Failed(error));
    }
    if dies_here(steps) {
        return Some(Stop::Died);
    }
    match update_ref(dir, Ref::Applied, Some(target), applied) {
        Ok(true) => {}
        Ok(false) => return Some(Stop::Moved),
        Err(error) => return Some(Stop::Failed(error)),
    }
    dies_here(steps).then_some(Stop::Died)
}

/// Put the folder back as it was before an application that was not committed: the version's files
/// placed go, the files set aside come back. A file the person put or changed at such a path meanwhile
/// stays, and the one set aside comes back beside it as a conflict copy — nothing of theirs is removed.
fn undo(work: &Dir, stage: &Path, journal: &Journal) -> OmniResult<()> {
    check_rollback(work, stage, journal)?;
    let stage = work.open_dir_nofollow(stage).map_err(storage_error)?;
    for (path, content) in journal.paths.iter().rev() {
        let Some(content) = content else { continue };
        let Some(at) = recovery_stamp(work, Path::new(path))? else { continue };
        let staged = recovery_stamp(&stage, &Path::new("new").join(path))?;
        if staged.is_none_or(|s| s == at) && work.open(path).and_then(hash_reader).is_ok_and(|found| &found == content) {
            work.remove_file(path).map_err(storage_error)?;
            let mut parent = Path::new(path).parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                if work.remove_dir(p).is_err() {
                    break;
                }
                parent = p.parent();
            }
        }
    }
    for (path, _) in &journal.paths {
        let aside = Path::new("old").join(path);
        if recovery_stamp(&stage, &aside)?.is_none() {
            continue;
        }
        let file = Path::new(path);
        if let Some(parent) = file.parent().filter(|p| !p.as_os_str().is_empty()) {
            work.create_dir_all(parent).map_err(storage_error)?;
        }
        let restore = |dest: &Path| -> std::io::Result<()> {
            match stage.hard_link(&aside, work, dest) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
                Err(_) => {
                    // Volumes without hard links still need exclusive destination creation.
                    let mut input = stage.open(&aside)?;
                    let mut output = work.open_with(dest, cap_std::fs::OpenOptions::new().write(true).create_new(true))?;
                    std::io::copy(&mut input, &mut output)?;
                    output.sync_all()
                }
            }
        };
        match restore(file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let copy = conflict_copy_name(path, &conflict_label("Omni", now_unix_ms()));
                restore(Path::new(&copy)).map_err(storage_error)?;
            }
            Err(e) => return Err(storage_error(e)),
        }
        stage.remove_file(aside).map_err(storage_error)?;
    }
    Ok(())
}

fn recovery_stamp(dir: &Dir, path: &Path) -> OmniResult<Option<(u64, cap_std::time::SystemTime)>> {
    match dir.symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some((meta.len(), meta.modified().map_err(storage_error)?))),
        Ok(_) => Ok(None),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(None),
        Err(e) => Err(storage_error(e)),
    }
}

fn open_work(work: &Path) -> std::io::Result<Dir> {
    match (work.parent(), work.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            Dir::open_ambient_dir(parent, cap_std::ambient_authority())?.open_dir_nofollow(name)
        }
        _ => Dir::open_ambient_dir(work, cap_std::ambient_authority()),
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn finish_stage(root: &Dir, stage: &Path, receipt: &Path) -> OmniResult<()> {
    root.remove_dir_all(stage).map_err(storage_error)?;
    match std::fs::remove_file(receipt) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(storage_error(e)),
    }
}

fn check_recovery_entry(root: &Dir, path: &Path) -> OmniResult<()> {
    let meta = match root.symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(()),
        Err(e) => return Err(storage_error(e)),
    };
    #[cfg(windows)]
    let link = cap_std::fs::MetadataExt::file_attributes(&meta) & 0x400 != 0;
    #[cfg(not(windows))]
    let link = meta.file_type().is_symlink();
    if link || !(meta.is_file() || meta.is_dir()) {
        return Err(storage_error(format!("unsafe recovery entry: {}", path.display())));
    }
    Ok(())
}

fn check_rollback(work: &Dir, stage: &Path, journal: &Journal) -> OmniResult<()> {
    for base in [PathBuf::from("."), stage.join("old"), stage.join("new")] {
        check_recovery_entry(work, &base)?;
        for (path, _) in &journal.paths {
            validate_workspace_file_path(path)?;
            let mut current = base.clone();
            for component in path.split('/') {
                current.push(component);
                check_recovery_entry(work, &current)?;
            }
        }
    }
    Ok(())
}

/// Finish or undo an application of a version the process did not live to complete
/// (`.omni-apply~…` left in the folder). Done — the folder's version and the folders left out are the
/// journal's — the replaced files go; otherwise the folder is put back. Cleanup requires both the
/// private receipt and its matching journal. A prefix alone does not establish ownership.
fn recover(dir: &Path, work: &Path) -> OmniResult<()> {
    let root = match open_work(work) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(storage_error(e)),
    };
    let entries = root.entries().map_err(storage_error)?;
    for entry in entries {
        let entry = entry.map_err(|e| storage_error(format!("{}: {e}", work.display())))?;
        if !entry.file_name().to_string_lossy().starts_with(APPLYING) {
            continue;
        }
        let stage = PathBuf::from(entry.file_name());
        let receipt = dir.join(entry.file_name());
        let expected = match std::fs::read(&receipt) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Old interrupted swaps can hold the user's only copy. Preserve the directory
                // and report those that look like a prior transaction requiring manual recovery.
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let tag = &name[APPLYING.len()..];
                if !tag.is_empty() && tag.bytes().all(|b| b.is_ascii_hexdigit())
                    && (root.try_exists(stage.join(JOURNAL)).map_err(storage_error)? || root.try_exists(stage.join("old")).map_err(storage_error)?) {
                    return Err(storage_error(format!("cannot verify {}: recovery receipt missing", stage.display())));
                }
                continue;
            }
            Err(e) => return Err(storage_error(e)),
        };
        check_recovery_entry(&root, &stage)?;
        check_recovery_entry(&root, &stage.join(JOURNAL))?;
        let bytes = root.read(stage.join(JOURNAL)).map_err(|e| storage_error(format!("{}: {e}", stage.display())))?;
        if expected[..] != sha2::Sha256::digest(&bytes)[..] {
            return Err(storage_error(format!("journal changed: {}", stage.display())));
        }
        let journal: Journal = serde_json::from_slice(&bytes).map_err(storage_error)?;
        let held = left_out(dir, Left::Held)?;
        let done = read_ref(dir, Ref::Applied)?.as_ref() == Some(&journal.target) && held == journal.held_after;
        if done {
            check_rollback(&root, &stage, &journal)?;
        } else {
            undo(&root, &stage, &journal)?;
            if held != journal.held_before {
                set_left_out(dir, Left::Held, &journal.held_before)?;
            }
        }
        finish_stage(&root, &stage, &receipt)?;
    }
    Ok(())
}

/// «Сохранить версию»: everything the folder changed against the version it holds, as a version on
/// top of it, the changed contents kept in the store. Our line and the folder's version move to it.
/// `None` when nothing changed.
/// The folders this device leaves out are not read: their files stay as the version has them.
pub(super) fn save(dir: &Path, repo: &str, work: &Path, author: (&str, &str), message: &str, at_unix_ms: i64) -> OmniResult<Option<Content>> {
    let applied = read_ref(dir, Ref::Applied)?;
    let left = left_out(dir, Left::Held)?;
    let held = held_tree(dir, repo)?;
    let here = scan(dir, work)?;
    if let Some(path) = here.invalid.iter().find(|p| !is_left_out(&left, p)) {
        return Err(OmniError::InvalidInput(format!("{path}: the file name cannot be written on every system — rename it")));
    }
    if let Some(path) = here.unreadable.iter().find(|p| !is_left_out(&left, p)) {
        return Err(OmniError::InvalidInput(format!("{path}: the file is open in a program — close it and save again")));
    }
    let mut changes = diff(&held, &without(&here.files, &left));
    if changes.is_empty() {
        return Ok(None);
    }
    // The content is what was copied: a file changed meanwhile goes in as it was read.
    for change in changes.iter_mut() {
        if change.content.is_some() {
            change.content = Some(store_file(dir, &work.join(&change.path))?);
        }
    }
    let record = WorkspaceVersionV1 {
        repo: repo.to_string(),
        parent: applied.clone(),
        author_account: author.0.to_string(),
        author_name: if author.1.trim().is_empty() { "Omni".into() } else { author.1.trim().to_string() },
        at_unix_ms,
        message: message.to_string(),
        changes,
    };
    let saved = write_version(dir, &record)?;
    let mine = read_ref(dir, Ref::Mine)?;
    // Our line is ours alone; a replay may have rewritten it.
    update_ref(dir, Ref::Mine, Some(&saved), mine.as_ref())?;
    if !update_ref(dir, Ref::Applied, Some(&saved), applied.as_ref())? {
        return Err(OmniError::Transport("the folder was updated meanwhile; save again".into()));
    }
    Ok(Some(saved))
}

/// «Восстановить»: a version on top of our line (when it is ahead of the shared one) or the shared
/// one, with exactly the files of `old`. `None` when they are the same already.
/// Files of `old` whose content is `gone` (thinned out by a mask) stay as they are on our line.
pub(super) fn restore(dir: &Path, repo: &str, old: &str, gone: &HashSet<Content>, author: (&str, &str), message: &str, at_unix_ms: i64) -> OmniResult<Option<Content>> {
    let shared = read_ref(dir, Ref::Shared)?.ok_or_else(|| OmniError::InvalidInput("the project has no accepted version yet".into()))?;
    let mine = read_ref(dir, Ref::Mine)?;
    let base = match &mine {
        Some(mine) if is_ancestor(dir, repo, &shared.id, &mine.id)? => mine.clone(),
        _ => shared,
    };
    let (from, to) = (tree(dir, repo, &base.id)?, tree(dir, repo, old)?);
    let mut to = Arc::unwrap_or_clone(to);
    let lost: Vec<String> = to.iter().filter(|(_, c)| gone.contains(*c)).map(|(p, _)| p.clone()).collect();
    for path in lost {
        match from.get(&path) {
            Some(now) => to.insert(path, now.clone()),
            None => to.remove(&path),
        };
    }
    let changes = diff(&from, &to);
    if changes.is_empty() {
        return Ok(None);
    }
    let saved = write_version(dir, &WorkspaceVersionV1 {
        repo: repo.to_string(),
        parent: Some(base),
        author_account: author.0.to_string(),
        author_name: if author.1.trim().is_empty() { "Omni".into() } else { author.1.trim().to_string() },
        at_unix_ms,
        message: message.to_string(),
        changes,
    })?;
    if !update_ref(dir, Ref::Mine, Some(&saved), mine.as_ref())? {
        return Err(OmniError::Transport("the folder was updated meanwhile; try again".into()));
    }
    Ok(Some(saved))
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// What happens between the staging of an application and its swap (a program saves a file).
        pub(super) static BEFORE_SWAP: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
        /// The move of an application at which the process "dies", leaving the disk as it is.
        pub(super) static DIE_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    /// A version on top of `parent` whose new contents are put in the store (as if fetched).
    fn stored_version(dir: &Path, parent: &Content, at: i64, files: &[(&str, Option<&str>)]) -> Content {
        let source = dir.join("source");
        let changes = files
            .iter()
            .map(|(path, text)| WorkspaceFileChangeV1 {
                path: path.to_string(),
                content: text.map(|text| {
                    std::fs::write(&source, text).unwrap();
                    store_file(dir, &source).unwrap()
                }),
            })
            .collect();
        version_on(dir, Some(parent), "bob", at, changes)
    }

    /// Audit 2026-11-09: a copy in the store that does not match its id (a failing disk) is not put in
    /// the folder as the version's, and it goes from the store — else every try copied it again and
    /// failed the same way. With the content back, the version is applied.
    #[test]
    fn a_damaged_copy_in_the_store_is_not_applied_and_goes_from_the_store() {
        let (dir, work) = (temp_dir("damaged-data"), temp_dir("damaged-work"));
        prepare_project(&dir).unwrap();
        std::fs::write(work.join("x.txt"), "x1").unwrap();
        let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
        let v2 = stored_version(&dir, &v1, 2_000, &[("x.txt", Some("x2"))]);
        let content = tree(&dir, "model", &v2.id).unwrap()["x.txt"].clone();
        let object = super::super::content::existing_object_path(&dir, &content.id);
        std::fs::write(&object, "xX").unwrap();

        assert!(follow(&dir, "model", &work, &v2, true).is_err());
        assert_eq!(std::fs::read_to_string(work.join("x.txt")).unwrap(), "x1", "the damaged bytes are not put in");
        assert_eq!(read_ref(&dir, Ref::Applied).unwrap().as_ref(), Some(&v1));
        assert!(!object.exists(), "the damaged copy goes, so it is fetched again");
        assert_eq!(applications_in(&work), 0);

        std::fs::write(dir.join("source"), "x2").unwrap();
        store_file(&dir, &dir.join("source")).unwrap();
        follow(&dir, "model", &work, &v2, true).unwrap();
        assert_eq!(std::fs::read_to_string(work.join("x.txt")).unwrap(), "x2");
    }

    #[cfg(windows)]
    #[test]
    fn recovery_holds_the_workspace_directory_until_it_finishes() {
        let parent = temp_dir("held-workspace");
        let work = parent.join("work");
        let moved = parent.join("moved");
        std::fs::create_dir(&work).unwrap();
        let root = open_work(&work).unwrap();
        assert!(std::fs::rename(&work, &moved).is_err());
        drop(root);
        std::fs::rename(&work, moved).unwrap();
    }

    #[test]
    fn rollback_checks_all_paths_before_restoring_any_file() {
        let work = temp_dir("invalid-rollback");
        let stage = work.join(".omni-apply~unit");
        std::fs::create_dir_all(stage.join("old")).unwrap();
        std::fs::write(stage.join("old/a.txt"), "saved").unwrap();
        let journal = Journal {
            target: content("target"),
            from: None,
            held_before: vec![],
            held_after: vec![],
            paths: vec![("a.txt".into(), None), (String::new(), None)],
        };
        assert!(undo(&open_work(&work).unwrap(), Path::new(stage.file_name().unwrap()), &journal).is_err());
        assert!(stage.join("old/a.txt").is_file());
        assert!(!work.join("a.txt").exists());
    }

    #[test]
    fn scanning_preserves_unowned_directories_with_the_staging_prefix() {
        let dir = temp_dir("unowned-data");
        let work = temp_dir("unowned-work");
        let names = [".omni-apply~notes", ".omni-apply~0123456789abcdef0123456789abcdef"];
        for name in names {
            let folder = work.join(name).join("drafts");
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("notes.txt"), "my work").unwrap();
        }
        scan(&dir, &work).unwrap();
        for name in names {
            assert_eq!(std::fs::read_to_string(work.join(name).join("drafts/notes.txt")).unwrap(), "my work");
        }
    }

    #[test]
    fn recovery_keeps_staged_files_when_the_journal_changed() {
        let (dir, work) = (temp_dir("receipt"), temp_dir("receipt-work"));
        let name = format!("{APPLYING}unit");
        let stage = work.join(&name);
        std::fs::create_dir_all(stage.join("old")).unwrap();
        let journal = Journal {
            target: content("target"),
            from: None,
            held_before: vec![],
            held_after: vec![],
            paths: vec![("x.txt".into(), None)],
        };
        let mut bytes = serde_json::to_vec(&journal).unwrap();
        std::fs::write(dir.join(name), sha2::Sha256::digest(&bytes)).unwrap();
        bytes.push(b' ');
        std::fs::write(stage.join(JOURNAL), bytes).unwrap();
        std::fs::write(stage.join("old/x.txt"), "saved").unwrap();
        assert!(recover(&dir, &work).unwrap_err().to_string().contains("journal changed"));
        assert!(stage.join("old/x.txt").is_file());
        assert!(!work.join("x.txt").exists());
    }

    fn applications_in(work: &Path) -> usize {
        std::fs::read_dir(work).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(APPLYING)).count()
    }

    /// F02 (audit 2026-11-09): a file a program saved between the scan and the swap is the person's
    /// change — the version waits, the saved bytes stay under their name, nothing of the version is in.
    #[test]
    fn a_file_saved_after_the_scan_is_not_replaced_by_a_version() {
        let (dir, work) = (temp_dir("saved-late-data"), temp_dir("saved-late-work"));
        prepare_project(&dir).unwrap();
        std::fs::write(work.join("a.txt"), "a1").unwrap();
        std::fs::write(work.join("b.txt"), "b1").unwrap();
        let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
        let v2 = stored_version(&dir, &v1, 2_000, &[("a.txt", Some("a2")), ("b.txt", Some("b2")), ("c.txt", Some("c2"))]);
        for (late, text) in [("b.txt", "мои правки"), ("c.txt", "мой новый файл")] {
            let file = work.join(late);
            BEFORE_SWAP.with(|hook| *hook.borrow_mut() = Some(Box::new(move || std::fs::write(file, text).unwrap())));
            let waiting = follow(&dir, "model", &work, &v2, true).unwrap();
            assert_eq!(waiting.waiting_changed, vec![late.to_string()], "{waiting:?}");
            assert_eq!(std::fs::read_to_string(work.join(late)).unwrap(), text, "the person's bytes stay");
            assert_eq!(std::fs::read_to_string(work.join("a.txt")).unwrap(), "a1", "nothing of the version is in");
            assert_eq!(read_ref(&dir, Ref::Applied).unwrap().as_ref(), Some(&v1));
            assert_eq!(applications_in(&work), 0);
            std::fs::write(work.join("b.txt"), "b1").unwrap();
            let _ = std::fs::remove_file(work.join("c.txt"));
        }
        let done = follow(&dir, "model", &work, &v2, true).unwrap();
        assert!(done.waiting_changed.is_empty() && done.waiting_open.is_empty() && done.problem.is_none(), "{done:?}");
        assert_eq!(std::fs::read_to_string(work.join("b.txt")).unwrap(), "b2");
        for d in [dir, work] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// F06 (audit 2026-11-09): a file that becomes a folder of the same name, and back.
    #[test]
    fn a_version_turns_a_file_into_a_folder_and_back() {
        let (dir, work) = (temp_dir("kind-data"), temp_dir("kind-work"));
        prepare_project(&dir).unwrap();
        std::fs::write(work.join("a"), "файл").unwrap();
        let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
        let v2 = stored_version(&dir, &v1, 2_000, &[("a", None), ("a/b.txt", Some("в папке"))]);
        let v3 = stored_version(&dir, &v2, 3_000, &[("a/b.txt", None), ("a", Some("снова файл"))]);
        for (version, path, text) in [(&v2, "a/b.txt", "в папке"), (&v3, "a", "снова файл")] {
            let done = follow(&dir, "model", &work, version, true).unwrap();
            assert!(done.waiting_changed.is_empty() && done.waiting_open.is_empty() && done.problem.is_none(), "{path}: {done:?}");
            assert_eq!(std::fs::read_to_string(work.join(path)).unwrap(), text);
            assert_eq!(read_ref(&dir, Ref::Applied).unwrap().as_ref(), Some(version));
        }
        // A folder holding a file of the person's is not removed for the version's file.
        let v4 = stored_version(&dir, &v3, 4_000, &[("a", None), ("a/b.txt", Some("в папке"))]);
        let v5 = stored_version(&dir, &v4, 5_000, &[("a/b.txt", None), ("a", Some("файл"))]);
        follow(&dir, "model", &work, &v4, true).unwrap();
        std::fs::write(work.join("a/моё.txt"), "моё").unwrap();
        let waiting = follow(&dir, "model", &work, &v5, true).unwrap();
        assert_eq!(waiting.waiting_changed, vec!["a".to_string()], "{waiting:?}");
        assert_eq!(std::fs::read_to_string(work.join("a/моё.txt")).unwrap(), "моё");
        assert_eq!(std::fs::read_to_string(work.join("a/b.txt")).unwrap(), "в папке");
        for d in [dir, work] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// F03 (audit 2026-11-09): a process that dies at any move of an application leaves, after the
    /// next scan, the old version or the new one whole — the folder and its ref agree.
    #[test]
    fn an_application_cut_off_anywhere_leaves_one_whole_version() {
        let mut outcomes = BTreeSet::new();
        for die_at in 1.. {
            let (dir, work) = (temp_dir("cut-data"), temp_dir("cut-work"));
            prepare_project(&dir).unwrap();
            std::fs::create_dir_all(work.join("keep")).unwrap();
            for (path, text) in [("x.txt", "x1"), ("keep/y.txt", "y1"), ("gone.txt", "g")] {
                std::fs::write(work.join(path), text).unwrap();
            }
            let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
            let v2 = stored_version(&dir, &v1, 2_000, &[("x.txt", Some("x2")), ("gone.txt", None), ("new/z.txt", Some("z"))]);
            DIE_AT.with(|at| at.set(Some(die_at)));
            let followed = follow(&dir, "model", &work, &v2, true);
            DIE_AT.with(|at| at.set(None));
            followed.unwrap();
            let finished = applications_in(&work) == 0;
            let after = scan(&dir, &work).unwrap();
            assert_eq!(applications_in(&work), 0, "recovered at {die_at}");
            let applied = read_ref(&dir, Ref::Applied).unwrap().expect("a version");
            assert!(applied == v1 || applied == v2, "{die_at}");
            assert_eq!(after.files, *tree(&dir, "model", &applied.id).unwrap(), "the folder is the version its ref names, cut at {die_at}");
            outcomes.insert(applied == v2);
            for d in [dir, work] {
                let _ = std::fs::remove_dir_all(d);
            }
            if finished {
                break;
            }
            assert!(die_at < 32, "the application never finished");
        }
        assert_eq!(outcomes.len(), 2, "cut both before and after the commit");
    }

    /// F1 (audit 2026-11-09 #5): the process dies after a file of the version was placed; the person
    /// saves that file before Omni comes back. Undoing the application keeps their bytes under the
    /// name and puts the old version's file beside it.
    #[test]
    fn an_edit_saved_while_omni_was_down_survives_the_undo() {
        let mut checked = false;
        for die_at in 1..32 {
            let (dir, work) = (temp_dir("undo-edit-data"), temp_dir("undo-edit-work"));
            prepare_project(&dir).unwrap();
            std::fs::write(work.join("notes.txt"), "old").unwrap();
            let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
            let v2 = stored_version(&dir, &v1, 2_000, &[("notes.txt", Some("remote"))]);
            DIE_AT.with(|at| at.set(Some(die_at)));
            let followed = follow(&dir, "model", &work, &v2, true);
            DIE_AT.with(|at| at.set(None));
            followed.unwrap();
            let placed_not_committed = applications_in(&work) == 1
                && std::fs::read_to_string(work.join("notes.txt")).is_ok_and(|t| t == "remote")
                && read_ref(&dir, Ref::Applied).unwrap().as_ref() == Some(&v1);
            if placed_not_committed {
                std::fs::write(work.join("notes.txt"), "my important edit").unwrap();
                scan(&dir, &work).unwrap();
                assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "my important edit");
                let kept_old = std::fs::read_dir(&work).unwrap().flatten().any(|e| std::fs::read_to_string(e.path()).is_ok_and(|t| t == "old"));
                assert!(kept_old, "the old version's file comes back beside it");
                assert_eq!(applications_in(&work), 0);
                checked = true;
            }
            for d in [dir, work] {
                let _ = std::fs::remove_dir_all(d);
            }
            if checked {
                break;
            }
        }
        assert!(checked, "no cut left the version's file placed and not committed");
    }

    /// F4 (audit 2026-11-09 #5): a conflict copy never takes the name of a file the replayed version
    /// adds, or of one the line it goes onto has.
    #[test]
    fn a_conflict_copy_takes_a_name_no_file_has() {
        let dir = temp_dir("copy-name");
        let at = 3_000;
        let copy = conflict_copy_name("notes.txt", &conflict_label("Alice", at));
        let root = version_on(&dir, None, "ann", 1_000, vec![change("notes.txt", Some("BASE"))]);
        let remote = version_on(&dir, Some(&root), "bob", 2_000, vec![change("notes.txt", Some("REMOTE")), change(&copy.replace("notes", "Notes"), Some("THEIRS"))]);
        let mine = version_on(&dir, Some(&root), "Alice", at, vec![change("notes.txt", Some("ALICE")), change(&copy, Some("KEEP ME"))]);
        let tip = replay(&dir, "model", &remote, &mine.id, &|_| false).unwrap();
        let t = tree(&dir, "model", &tip.id).unwrap();
        assert_eq!(t.get("notes.txt"), Some(&content("REMOTE")));
        assert_eq!(t.get(&copy), Some(&content("KEEP ME")), "{t:?}");
        let alice: Vec<&String> = t.iter().filter(|(_, c)| **c == content("ALICE")).map(|(p, _)| p).collect();
        assert_eq!(alice.len(), 1, "{t:?}");
        assert_eq!(conflict_base(alice[0]).map(|(base, _)| base), Some("notes.txt".to_string()), "still read as a copy of notes.txt");
        assert!(t.values().any(|c| *c == content("THEIRS")), "the other line's file stays");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// F09 (audit 2026-11-09): only a ref that is not there is no ref; one that cannot be read, or a
    /// damaged list of folders left out, is an error — not a project with no history.
    #[test]
    fn a_ref_that_cannot_be_read_is_not_a_missing_one() {
        let dir = temp_dir("refs-unreadable");
        prepare_project(&dir).unwrap();
        assert_eq!(read_ref(&dir, Ref::Applied).unwrap(), None);
        assert_eq!(left_out(&dir, Left::Held).unwrap(), Vec::<String>::new());
        std::fs::create_dir_all(dir.join("refs").join("applied")).unwrap();
        assert!(read_ref(&dir, Ref::Applied).is_err(), "a ref that cannot be read");
        std::fs::write(dir.join("refs").join("left-out-held"), "[\"cad").unwrap();
        assert!(left_out(&dir, Left::Held).is_err(), "a damaged list");
        std::fs::create_dir_all(dir.join("refs").join("left-out")).unwrap();
        assert!(left_out(&dir, Left::Wanted).is_err(), "a list that cannot be read");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_conflict_copy_names_the_file_it_was_kept_beside() {
        let label = "Петя, 30.10 10-42";
        for path in ["корпус.sldasm", "сборка/корпус.v2.prt", "README", "dir/(x) y.pdf"] {
            assert_eq!(conflict_base(&conflict_copy_name(path, label)), Some((path.to_string(), label.to_string())), "{path}");
        }
        for plain in ["корпус.prt", "корпус (копия).prt", "a (b, c).txt", "x (Петя, 30.10 10:42).prt", " (Петя, 30.10 10-42).prt"] {
            assert_eq!(conflict_base(plain), None, "{plain}");
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omni-versions-{name}-{}", hex::encode(&omni_crypto::generate_group_key()[..6])));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn content(text: &str) -> Content {
        Content { id: hex::encode(sha2::Sha256::digest(text.as_bytes())), size: text.len() as u64 }
    }

    fn change(path: &str, text: Option<&str>) -> WorkspaceFileChangeV1 {
        WorkspaceFileChangeV1 { path: path.into(), content: text.map(content) }
    }

    fn version_on(dir: &Path, parent: Option<&Content>, who: &str, at: i64, mut changes: Vec<WorkspaceFileChangeV1>) -> Content {
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        write_version(dir, &WorkspaceVersionV1 {
            repo: "model".into(),
            parent: parent.cloned(),
            author_account: who.into(),
            author_name: who.into(),
            at_unix_ms: at,
            message: format!("{who} {at}"),
            changes,
        })
        .unwrap()
    }

    #[test]
    fn a_version_tree_is_the_sum_of_its_chain() {
        let dir = temp_dir("tree");
        let v1 = version_on(&dir, None, "ann", 1_000, vec![change("a.prt", Some("a1")), change("b.prt", Some("b1"))]);
        let v2 = version_on(&dir, Some(&v1), "bob", 2_000, vec![change("a.prt", Some("a2")), change("b.prt", None)]);
        let t = tree(&dir, "model", &v2.id).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t.get("a.prt"), Some(&content("a2")));
        assert!(is_ancestor(&dir, "model", &v1.id, &v2.id).unwrap() && !is_ancestor(&dir, "model", &v2.id, &v1.id).unwrap());
        assert!(version(&dir, "other", &v1.id).unwrap().is_none(), "a version of another project is not taken");
        // A record whose bytes do not match its id is not a version.
        std::fs::write(version_path(&dir, &"c".repeat(64)), b"{}").unwrap();
        assert!(version(&dir, "model", &"c".repeat(64)).unwrap().is_none());
        assert_eq!(first_missing(&dir, "model", &Content { id: "d".repeat(64), size: 5 }).unwrap(), Some(Content { id: "d".repeat(64), size: 5 }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_replayed_line_keeps_both_contents_of_a_file_both_changed() {
        let dir = temp_dir("replay");
        let root = version_on(&dir, None, "ann", 1_000, vec![change("notes.txt", Some("v1")), change("part.prt", Some("p1"))]);
        let theirs = version_on(&dir, Some(&root), "ann", 2_000, vec![change("notes.txt", Some("Ann")), change("bracket.stp", Some("solid"))]);
        let mine = version_on(&dir, Some(&root), "bob", 3_000, vec![change("notes.txt", Some("Bob")), change("drawing.pdf", Some("%PDF")), change("part.prt", None)]);
        let tip = replay(&dir, "model", &theirs, &mine.id, &|_| false).unwrap();
        let t = tree(&dir, "model", &tip.id).unwrap();
        assert_eq!(t.get("notes.txt"), Some(&content("Ann")), "the line it went onto keeps its content in place");
        let beside: Vec<&String> = t.keys().filter(|p| p.starts_with("notes (bob, ")).collect();
        assert_eq!(beside.len(), 1, "{t:?}");
        assert_eq!(t.get(beside[0]), Some(&content("Bob")));
        assert_eq!((t.get("drawing.pdf"), t.get("bracket.stp")), (Some(&content("%PDF")), Some(&content("solid"))));
        assert!(!t.contains_key("part.prt"), "a removal only one line made goes through");
        let replayed = version(&dir, "model", &tip.id).unwrap().unwrap();
        assert_eq!((replayed.author_account.as_str(), replayed.parent.as_ref()), ("bob", Some(&theirs)));
        assert!(is_ancestor(&dir, "model", &theirs.id, &tip.id).unwrap());
        // The line rank: more accounts, then longer, then the earlier start.
        assert_eq!(line_rank(&dir, "model", &theirs.id, &mine.id).unwrap().1, 1);
        assert!(line_rank(&dir, "model", &mine.id, &theirs.id).unwrap() > line_rank(&dir, "model", &theirs.id, &mine.id).unwrap(), "the earlier save wins a tie");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record cache is shared by every project of the process: a version another project holds
    /// is not "here" for one whose store lacks it — it must be fetched, not taken from the cache.
    #[test]
    fn a_version_another_project_holds_is_not_here_until_this_one_has_it() {
        let (one, other) = (temp_dir("cache-one"), temp_dir("cache-other"));
        let id = version_on(&one, None, "ann", 1_000, vec![change("a.txt", Some("a"))]);
        assert!(version(&one, "model", &id.id).unwrap().is_some(), "in the cache now");
        assert!(version(&other, "model", &id.id).unwrap().is_none(), "not in the other project's store");
        assert_eq!(first_missing(&other, "model", &id).unwrap(), Some(id.clone()), "so it is fetched there");
        for d in [one, other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// A version may not write over what the scan did not count as the path's file — a local
    /// `README.md` under an incoming `readme.md`, a `.git` stored before paths were checked — nor
    /// apply a tree two of whose paths are one file on a case-insensitive disk.
    #[test]
    fn a_version_does_not_write_over_a_file_it_did_not_see_or_into_git() {
        let (dir, work) = (temp_dir("clash-data"), temp_dir("clash-work"));
        prepare_project(&dir).unwrap();
        std::fs::write(work.join("a.txt"), "a").unwrap();
        let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
        // Not saved yet: the person's own README.md.
        std::fs::write(work.join("README.md"), "мой").unwrap();
        let other = temp_dir("clash-other");
        std::fs::write(other.join("p"), "чужой").unwrap();
        let part = store_file(&dir, &other.join("p")).unwrap();
        let on_top = |at: i64, paths: &[&str]| {
            let mut changes: Vec<WorkspaceFileChangeV1> =
                paths.iter().map(|p| WorkspaceFileChangeV1 { path: p.to_string(), content: Some(part.clone()) }).collect();
            changes.sort_by(|a, b| a.path.cmp(&b.path));
            write_version(&dir, &WorkspaceVersionV1 {
                repo: "model".into(),
                parent: Some(v1.clone()),
                author_account: "bob".into(),
                author_name: "Боб".into(),
                at_unix_ms: at,
                message: "v2".into(),
                changes,
            })
        };
        let lower = on_top(2_000, &["readme.md"]).unwrap();
        let waiting = follow(&dir, "model", &work, &lower, false).unwrap();
        if work.join("readme.md").exists() {
            // A case-insensitive disk: the same file.
            assert_eq!(waiting.waiting_changed, vec!["readme.md".to_string()]);
        }
        assert_eq!(std::fs::read_to_string(work.join("README.md")).unwrap(), "мой", "the person's file stays");

        let clash = on_top(3_000, &["Docs/x.txt", "docs/x.txt"]).unwrap();
        let refused = follow(&dir, "model", &work, &clash, false).unwrap();
        assert!(refused.problem.as_deref().is_some_and(|p| p.contains("регистр")), "{refused:?}");
        assert!(!work.join("docs").exists() && !work.join("Docs").exists());

        // A version with `.git/config` is refused at the door now; one stored before is not applied.
        assert!(on_top(4_000, &[".git/config"]).is_err());
        let file_and_folder = on_top(5_000, &["b", "B/c"]).unwrap();
        assert!(follow(&dir, "model", &work, &file_and_folder, false).unwrap().problem.is_some());
        for d in [dir, work, other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_saved_folder_follows_another_version_and_holds_back_for_a_changed_file() {
        let (dir, work) = (temp_dir("folder-data"), temp_dir("folder-work"));
        prepare_project(&dir).unwrap();
        std::fs::create_dir_all(work.join("сборка")).unwrap();
        std::fs::write(work.join("сборка/корпус.prt"), "v1").unwrap();
        std::fs::write(work.join("заметки.txt"), "n1").unwrap();
        std::fs::write(work.join(".omni-new~x~a"), "swap").unwrap();
        let v1 = save(&dir, "model", &work, ("ann", "Аня"), "Первая", 1_000).unwrap().expect("saved");
        assert_eq!(save(&dir, "model", &work, ("ann", "Аня"), "", 1_001).unwrap(), None, "nothing changed");
        assert_eq!(read_ref(&dir, Ref::Mine).unwrap().as_ref(), Some(&v1));
        let t1 = tree(&dir, "model", &v1.id).unwrap();
        assert_eq!(t1.keys().cloned().collect::<Vec<_>>(), vec!["заметки.txt".to_string(), "сборка/корпус.prt".to_string()], "Omni's swap files are not the project's");
        // Another version on top of it, its content in the store (as if fetched).
        let other = temp_dir("folder-other");
        std::fs::write(other.join("p"), "v2").unwrap();
        let new_part = store_file(&dir, &other.join("p")).unwrap();
        let v2 = write_version(&dir, &WorkspaceVersionV1 {
            repo: "model".into(),
            parent: Some(v1.clone()),
            author_account: "bob".into(),
            author_name: "Боб".into(),
            at_unix_ms: 2_000,
            message: "v2".into(),
            changes: vec![change("заметки.txt", None), WorkspaceFileChangeV1 { path: "сборка/корпус.prt".into(), content: Some(new_part.clone()) }],
        })
        .unwrap();
        // Changed here and not saved: the version waits.
        std::fs::write(work.join("заметки.txt"), "мои").unwrap();
        let waiting = follow(&dir, "model", &work, &v2, false).unwrap();
        assert_eq!(waiting.waiting_changed, vec!["заметки.txt".to_string()]);
        assert_eq!(std::fs::read_to_string(work.join("сборка/корпус.prt")).unwrap(), "v1", "nothing of the version yet");
        std::fs::write(work.join("заметки.txt"), "n1").unwrap();
        let done = follow(&dir, "model", &work, &v2, false).unwrap();
        assert!(done.waiting_changed.is_empty() && done.waiting_open.is_empty() && done.problem.is_none(), "{done:?}");
        assert_eq!(std::fs::read_to_string(work.join("сборка/корпус.prt")).unwrap(), "v2");
        assert!(!work.join("заметки.txt").exists());
        assert_eq!(read_ref(&dir, Ref::Applied).unwrap().as_ref(), Some(&v2));
        assert!(!store_object(&dir, &new_part.id).exists(), "a member without an archive keeps it in the folder only");
        assert_eq!(content_file(&dir, Some(&work), &new_part.id, new_part.size), Some(work.join("сборка/корпус.prt")), "served from the folder");
        std::fs::write(work.join("сборка/корпус.prt"), "changed").unwrap();
        assert_eq!(content_file(&dir, Some(&work), &new_part.id, new_part.size), None, "a changed file is not served as the version's");
        for d in [dir, work, other] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_conflict_copy_keeps_folder_and_extension() {
        assert_eq!(conflict_copy_name("сборка/корпус.sldasm", "Петя, 30.10 10-42"), "сборка/корпус (Петя, 30.10 10-42).sldasm");
        assert_eq!(conflict_copy_name("README", "Петя, 30.10 10-42"), "README (Петя, 30.10 10-42)");
        assert_eq!(utc_time(0), (1970, 1, 1, 0, 0));
        assert_eq!(utc_time(1_700_000_000_000), (2023, 11, 14, 22, 13));
        assert_eq!(utc_time(951_782_400_000), (2000, 2, 29, 0, 0), "a leap day");
    }
}
