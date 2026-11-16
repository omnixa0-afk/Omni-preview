using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

/// Workspaces: a group with git repositories (docs/decisions/omni-workspace-git.md).
internal sealed partial class RustBridge
{
    /// Add a repository to the workspace group: the core makes this profile's published bare
    /// repository and, given `workPath`, binds that working repository to it (remote `omni`). With
    /// `files` the folder holds files (models, drawings, documents): a git repository is made in it
    /// when there is none, and its versions are saved with <see cref="SaveWorkspaceVersion"/>.
    public WorkspaceRepoView AddWorkspaceRepo(string groupId, string repo, string? workPath, bool files = false)
        => CallJson(
            () => NativeMethods.omni_workspace_add_repo(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceAddRepoRequest(groupId, repo, workPath, files),
                    OmniJsonSerializerContext.Default.WorkspaceAddRepoRequest)),
            OmniJsonSerializerContext.Default.WorkspaceRepoView,
            "workspace_add_repo");

    public IReadOnlyList<WorkspaceRepoView> ListWorkspaceRepos(string groupId)
        => CallJson(
            () => NativeMethods.omni_workspace_repos(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            OmniJsonSerializerContext.Default.ListWorkspaceRepoView,
            "workspace_repos") ?? new List<WorkspaceRepoView>();

    /// The branches members announced in a repository and whether their commits are here yet.
    public IReadOnlyList<WorkspaceBranchView> ListWorkspaceBranches(string groupId, string repo)
        => CallJson(
            () => NativeMethods.omni_workspace_branches(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceRepoRequest(groupId, repo),
                    OmniJsonSerializerContext.Default.WorkspaceRepoRequest)),
            OmniJsonSerializerContext.Default.ListWorkspaceBranchView,
            "workspace_branches") ?? new List<WorkspaceBranchView>();

    public WorkspaceMainView GetWorkspaceMain(string groupId, string repo)
        => CallJson(
            () => NativeMethods.omni_workspace_main(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceRepoRequest(groupId, repo),
                    OmniJsonSerializerContext.Default.WorkspaceRepoRequest)),
            OmniJsonSerializerContext.Default.WorkspaceMainView,
            "workspace_main");

    /// Admin only: whether the shared `main` of a code project waits for another member's approval.
    public void SetWorkspaceMainRule(string groupId, string repo, bool needsApproval)
        => CallUnit(
            () => NativeMethods.omni_workspace_set_main_rule(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceMainRuleRequest(groupId, repo, needsApproval),
                    OmniJsonSerializerContext.Default.WorkspaceMainRuleRequest)),
            "workspace_set_main_rule");

    public void ApproveWorkspaceCommit(string groupId, string repo, string oid)
        => CallUnit(
            () => NativeMethods.omni_workspace_approve(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceApproveRequest(groupId, repo, oid),
                    OmniJsonSerializerContext.Default.WorkspaceApproveRequest)),
            "workspace_approve");

    /// The workspace screen: every repository with its shared `main`, what waits, members' branches.
    public WorkspaceScreen GetWorkspaceScreen(string groupId)
        => CallJsonLong(
            () => NativeMethods.omni_workspace_view(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            OmniJsonSerializerContext.Default.WorkspaceScreen,
            "workspace_view");

    /// «Забрать»: fetch, then bring `main` forward when safe. The outcome: updated | up_to_date |
    /// dirty | not_on_main | diverged | no_main.
    public string PullWorkspaceRepo(string groupId, string repo)
        => CallJsonLong(
            () => NativeMethods.omni_workspace_pull(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceRepoRequest(groupId, repo),
                    OmniJsonSerializerContext.Default.WorkspaceRepoRequest)),
            OmniJsonSerializerContext.Default.String,
            "workspace_pull") ?? "no_main";

    public void PublishWorkspaceBranch(string groupId, string repo, string branch)
        => CallUnit(
            () => NativeMethods.omni_workspace_publish_branch(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceBranchRequest(groupId, repo, branch),
                    OmniJsonSerializerContext.Default.WorkspaceBranchRequest)),
            "workspace_publish_branch");

    /// «Забрать сюда»: a repository members have, into `folder` (made a git repository if needed).
    public WorkspaceRepoView TakeWorkspaceRepo(string groupId, string repo, string folder)
        => CallJson(
            () => NativeMethods.omni_workspace_take_repo(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceTakeRepoRequest(groupId, repo, folder),
                    OmniJsonSerializerContext.Default.WorkspaceTakeRepoRequest)),
            OmniJsonSerializerContext.Default.WorkspaceRepoView,
            "workspace_take_repo");

    /// «На мои устройства»: the branch goes to the account's other devices only.
    public void SendWorkspaceBranchToMyDevices(string groupId, string repo, string branch)
        => CallUnit(
            () => NativeMethods.omni_workspace_send_to_my_devices(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceBranchRequest(groupId, repo, branch),
                    OmniJsonSerializerContext.Default.WorkspaceBranchRequest)),
            "workspace_send_to_my_devices");

    /// «Забрать» a branch another device of ours sent; the outcome as for <see cref="PullWorkspaceRepo"/>.
    public string TakeWorkspaceBranchFromDevice(string groupId, string repo, string node, string branch)
        => CallJsonLong(
            () => NativeMethods.omni_workspace_take_from_device(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceDeviceBranchRequest(groupId, repo, node, branch),
                    OmniJsonSerializerContext.Default.WorkspaceDeviceBranchRequest)),
            OmniJsonSerializerContext.Default.String,
            "workspace_take_from_device") ?? "no_main";

    /// «Сохранить версию» of a files repository: the new commit, or null when nothing changed.
    public string? SaveWorkspaceVersion(string groupId, string repo, string message)
        => CallOptionalJsonLong(
            () => NativeMethods.omni_workspace_save_version(
                handle,
                JsonSerializer.Serialize(
                    new WorkspaceSaveVersionRequest(groupId, repo, message),
                    OmniJsonSerializerContext.Default.WorkspaceSaveVersionRequest)),
            OmniJsonSerializerContext.Default.String,
            "workspace_save_version");

    internal sealed record WorkspaceSaveVersionRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("message")] string Message);

    internal sealed record WorkspaceDeviceBranchRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("node")] string Node,
        [property: JsonPropertyName("branch")] string Branch);

    internal sealed record WorkspaceBranchRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("branch")] string Branch);

    internal sealed record WorkspaceTakeRepoRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("folder")] string Folder);

    internal sealed record WorkspaceAddRepoRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("work_path")] string? WorkPath,
        [property: JsonPropertyName("files")] bool Files);

    internal sealed record WorkspaceRepoRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo);

    internal sealed record WorkspaceMainRuleRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("needs_approval")] bool NeedsApproval);

    internal sealed record WorkspaceApproveRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("repo")] string Repo,
        [property: JsonPropertyName("oid")] string Oid);
}

internal sealed record WorkspaceRepoView(
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("repo")] string Repo,
    [property: JsonPropertyName("published_path")] string PublishedPath,
    [property: JsonPropertyName("work_path")] string? WorkPath);

internal sealed record WorkspaceBranchView(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("ref_name")] string RefName,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("fetched")] bool Fetched);

internal sealed record WorkspaceScreen(
    [property: JsonPropertyName("i_am_admin")] bool IAmAdmin,
    [property: JsonPropertyName("i_am_maintainer")] bool IAmMaintainer,
    [property: JsonPropertyName("repos")] List<WorkspaceRepoState> Repos,
    // Another device of our account is in the group: «На мои устройства» has somewhere to go.
    [property: JsonPropertyName("i_have_other_devices")] bool IHaveOtherDevices = false,
    [property: JsonPropertyName("custodian_of")] List<string>? CustodianOf = null,
    [property: JsonPropertyName("custody_asked")] List<string>? CustodyAsked = null,
    // Our agents' questions to us nobody answered yet: the count on the agents' tab.
    [property: JsonPropertyName("agent_asks")] int AgentAsks = 0,
    // «Что нового»: what others did here lately, newest first.
    [property: JsonPropertyName("news")] List<WorkspaceNews>? News = null);

/// A line of «Что нового»: what happened and what it is about — a project, the tasks project or
/// "#agents"; empty when about the workspace as a whole.
internal sealed record WorkspaceNews(
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs,
    [property: JsonPropertyName("text")] string Text,
    [property: JsonPropertyName("about")] string About = "");

internal sealed record WorkspaceRepoState(
    [property: JsonPropertyName("repo")] string Repo,
    // False: members have it, this device has not taken it yet.
    [property: JsonPropertyName("here")] bool Here,
    [property: JsonPropertyName("work_path")] string? WorkPath,
    [property: JsonPropertyName("main")] WorkspaceCommit? Main,
    // Code: main moves only to an approved commit; files: members' versions wait for review.
    [property: JsonPropertyName("needs_approval")] bool NeedsApproval,
    [property: JsonPropertyName("main_behind")] int MainBehind,
    [property: JsonPropertyName("waiting")] List<WorkspaceTip> Waiting,
    [property: JsonPropertyName("forks")] List<WorkspaceTip> Forks,
    [property: JsonPropertyName("branches")] List<WorkspaceTip> Branches,
    [property: JsonPropertyName("local_branches")] List<WorkspaceLocalBranch> LocalBranches,
    // What our other devices sent «на мои устройства».
    [property: JsonPropertyName("device_branches")] List<WorkspaceDeviceBranch>? DeviceBranches = null,
    // The repository holds files (models, drawings, documents): versions, not branches.
    [property: JsonPropertyName("files")] bool Files = false,
    // Files: the shared `main` is version N.
    [property: JsonPropertyName("version")] int Version = 0,
    // Files: what changed in the folder and is not saved (the first 20), and how many.
    [property: JsonPropertyName("changed")] List<string>? Changed = null,
    [property: JsonPropertyName("changed_count")] int ChangedCount = 0,
    [property: JsonPropertyName("sync")] WorkspaceFilesSync? Sync = null,
    // Files: how many files the shared version has, their size together, the latest versions.
    [property: JsonPropertyName("file_count")] int FileCount = 0,
    [property: JsonPropertyName("total_bytes")] long TotalBytes = 0,
    [property: JsonPropertyName("recent")] List<WorkspaceCommit>? Recent = null,
    // Files: our saved versions the shared one lacks yet (null: all we saved is in).
    [property: JsonPropertyName("own")] WorkspaceOwnVersion? Own = null,
    // Files: the folder holds the shared version (false while it catches up).
    [property: JsonPropertyName("folder_current")] bool FolderCurrent = false,
    // Files: the project's folders this device does not take into its working folder.
    [property: JsonPropertyName("left_out")] List<string>? LeftOut = null,
    // Files: what our account holds («Беру в работу»).
    [property: JsonPropertyName("taken")] List<string>? Taken = null,
    // Files: what we changed or took that someone else holds (they took it first).
    [property: JsonPropertyName("held_elsewhere")] List<WorkspaceHeld>? HeldElsewhere = null,
    // Files: the shared version's files kept with conflict copies beside them.
    [property: JsonPropertyName("conflicts")] List<string>? Conflicts = null,
    // Files: revisions «На нормоконтроле» someone else released.
    [property: JsonPropertyName("revisions_on_check")] List<string>? RevisionsOnCheck = null,
    // The workspace's tasks project: «Задачи», not a project card.
    [property: JsonPropertyName("is_tasks")] bool IsTasks = false);

/// A file someone else holds that we changed or took too.
internal sealed record WorkspaceHeld(
    [property: JsonPropertyName("path")] string Path,
    [property: JsonPropertyName("name")] string Name);

/// Our saves outside the shared version: the latest, how many, and waiting | rejected | joining.
internal sealed record WorkspaceOwnVersion(
    [property: JsonPropertyName("commit")] WorkspaceCommit Commit,
    [property: JsonPropertyName("count")] int Count,
    [property: JsonPropertyName("state")] string State);

/// Where a files repository's folder is against the shared version.
internal sealed record WorkspaceFilesSync(
    // Open in a program: the next version waits for them to be closed.
    [property: JsonPropertyName("waiting_open")] List<string> WaitingOpen,
    // Changed here and not saved, and changed by the next version too.
    [property: JsonPropertyName("waiting_changed")] List<string> WaitingChanged,
    [property: JsonPropertyName("content_missing_bytes")] long ContentMissingBytes,
    [property: JsonPropertyName("content_total_bytes")] long ContentTotalBytes,
    [property: JsonPropertyName("problem")] string? Problem);

/// A branch another device of ours sent «на мои устройства». State: new | newer | same | older |
/// diverged | unknown — against the same branch here.
internal sealed record WorkspaceDeviceBranch(
    [property: JsonPropertyName("device")] string Device,
    [property: JsonPropertyName("node")] string Node,
    [property: JsonPropertyName("branch")] string Branch,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("commit")] WorkspaceCommit? Commit,
    [property: JsonPropertyName("state")] string State);

internal sealed record WorkspaceCommit(
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("author")] string Author,
    [property: JsonPropertyName("at_unix_ms")] long AtUnixMs);

internal sealed record WorkspaceTip(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("is_me")] bool IsMe,
    [property: JsonPropertyName("branch")] string Branch,
    [property: JsonPropertyName("oid")] string Oid,
    // Null: the commit is not here yet.
    [property: JsonPropertyName("commit")] WorkspaceCommit? Commit);

internal sealed record WorkspaceLocalBranch(
    [property: JsonPropertyName("branch")] string Branch,
    [property: JsonPropertyName("oid")] string Oid,
    [property: JsonPropertyName("subject")] string Subject,
    [property: JsonPropertyName("published")] bool Published,
    [property: JsonPropertyName("on_my_devices")] bool OnMyDevices = false);

/// `Waiting` and `Forks` are (account, commit) pairs.
internal sealed record WorkspaceMainView(
    [property: JsonPropertyName("oid")] string? Oid,
    [property: JsonPropertyName("needs_approval")] bool NeedsApproval,
    [property: JsonPropertyName("waiting")] List<List<string>> Waiting,
    [property: JsonPropertyName("forks")] List<List<string>> Forks);
