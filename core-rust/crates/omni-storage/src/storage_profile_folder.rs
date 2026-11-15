// What the core keeps in a profile's folder beside its database (class C, audit 2026-11-09 #6).
//
// Every folder is named here, and its path comes only from here: a test reads the sources for a path
// built from the database's folder anywhere else (`every_path_in_the_profile_folder_comes_from_its_registry`
// in omni-app). Deleting the last profile and resetting the device remove all of them. A folder
// outside this list was a trace no one removed: `workspaces` (projects' content, in the clear),
// `workspace-tasks` and the plain media copies of `tmp/open` were. The database's own files beside it
// (journals, key, settings sidecars) are found by their name's prefix (`local_data_file_targets`).

/// A folder the core keeps in a profile's folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFolder {
    /// Files of chats, sent and received.
    Attachments,
    /// Avatars and profile pictures.
    ProfileMedia,
    /// Projects' versions, content and published repositories.
    Workspaces,
    /// The default folder of a workspace's task project.
    WorkspaceTasks,
    /// Scratch: plain copies of media handed to outside programs (`tmp/open`).
    Scratch,
}

impl ProfileFolder {
    pub const ALL: [ProfileFolder; 5] = [
        ProfileFolder::Attachments,
        ProfileFolder::ProfileMedia,
        ProfileFolder::Workspaces,
        ProfileFolder::WorkspaceTasks,
        ProfileFolder::Scratch,
    ];

    fn name(self) -> &'static str {
        match self {
            ProfileFolder::Attachments => "attachments",
            ProfileFolder::ProfileMedia => "profile-media",
            ProfileFolder::Workspaces => "workspaces",
            ProfileFolder::WorkspaceTasks => "workspace-tasks",
            ProfileFolder::Scratch => "tmp",
        }
    }

    /// The folder for the profile database at `db_path`.
    pub fn path(self, db_path: &Path) -> OmniResult<PathBuf> {
        Ok(profile_folder_of(db_path)?.join(self.name()))
    }
}

/// A backup being packed: `.omni-backup-<tag>` in the profile's folder (`app_backup_file`).
pub const BACKUP_STAGING_PREFIX: &str = ".omni-backup-";

/// Where a backup of the profile at `db_path` is packed (`<it>/.omni-backup-<tag>`).
pub fn backup_staging_root(db_path: &Path) -> OmniResult<PathBuf> {
    profile_folder_of(db_path)
}

/// The name of the profile's folder, for a trace line that says which profile it was.
pub fn profile_folder_label(db_path: &Path) -> String {
    profile_folder_of(db_path)
        .ok()
        .and_then(|dir| dir.file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

fn profile_folder_of(db_path: &Path) -> OmniResult<PathBuf> {
    db_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| OmniError::Storage("the profile database has no folder".to_string()))
}

/// Remove every folder of [`ProfileFolder`] and every backup being packed, for the profile database
/// at `db_path`.
fn remove_profile_folders(db_path: &Path) -> OmniResult<()> {
    let gone = |path: &Path| match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OmniError::Storage(format!("failed to delete '{}': {error}", path.display()))),
    };
    for folder in ProfileFolder::ALL {
        gone(&folder.path(db_path)?)?;
    }
    if let Ok(entries) = fs::read_dir(profile_folder_of(db_path)?) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(BACKUP_STAGING_PREFIX) {
                gone(&entry.path())?;
            }
        }
    }
    Ok(())
}
