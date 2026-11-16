use serde::Deserialize;

/// `work_path`: the person's working repository to bind (its `omni` remote), none to only publish.
/// `files`: the folder holds files (models, drawings, documents), not code — it needs `work_path`.
#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceAddRepoRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    #[serde(default)]
    pub(crate) work_path: Option<String>,
    #[serde(default)]
    pub(crate) files: bool,
}

/// «Сохранить версию» of a files repository; an empty `message` is "Версия".
#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceSaveVersionRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    #[serde(default)]
    pub(crate) message: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceRepoRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceMainRuleRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    pub(crate) needs_approval: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceApproveRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    pub(crate) oid: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceBranchRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    pub(crate) branch: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceTakeRepoRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    pub(crate) folder: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceDeviceBranchRequest {
    pub(crate) group_id: String,
    pub(crate) repo: String,
    pub(crate) node: String,
    pub(crate) branch: String,
}
