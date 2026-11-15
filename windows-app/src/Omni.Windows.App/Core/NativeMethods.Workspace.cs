using System;

namespace Omni.Windows.App.Core;

/// Workspaces: a group with git repositories (docs/decisions/omni-workspace-git.md).
internal static partial class NativeMethods
{
    private static OmniGroupsJsonDelegate? omniWorkspaceAddRepo;
    private static OmniGroupsJsonDelegate? omniWorkspaceRepos;
    private static OmniGroupsJsonDelegate? omniWorkspaceBranches;
    private static OmniGroupsJsonDelegate? omniWorkspaceMain;
    private static OmniGroupsJsonDelegate? omniWorkspaceSetMainRule;
    private static OmniGroupsJsonDelegate? omniWorkspaceApprove;
    private static OmniGroupsJsonDelegate? omniWorkspaceView;
    private static OmniGroupsJsonDelegate? omniWorkspacePull;
    private static OmniGroupsJsonDelegate? omniWorkspacePublishBranch;
    private static OmniGroupsJsonDelegate? omniWorkspaceTakeRepo;
    private static OmniGroupsJsonDelegate? omniWorkspaceSendToMyDevices;
    private static OmniGroupsJsonDelegate? omniWorkspaceTakeFromDevice;
    private static OmniGroupsJsonDelegate? omniWorkspaceSaveVersion;
    private static OmniGroupsJsonDelegate? omniWorkspaceFiles;
    private static OmniGroupsJsonDelegate? omniWorkspaceAgents;

    private static void LoadWorkspaceExports()
    {
        LoadExport(ref omniWorkspaceAddRepo, nameof(omni_workspace_add_repo));
        LoadExport(ref omniWorkspaceRepos, nameof(omni_workspace_repos));
        LoadExport(ref omniWorkspaceBranches, nameof(omni_workspace_branches));
        LoadExport(ref omniWorkspaceMain, nameof(omni_workspace_main));
        LoadExport(ref omniWorkspaceSetMainRule, nameof(omni_workspace_set_main_rule));
        LoadExport(ref omniWorkspaceApprove, nameof(omni_workspace_approve));
        LoadExport(ref omniWorkspaceView, nameof(omni_workspace_view));
        LoadExport(ref omniWorkspacePull, nameof(omni_workspace_pull));
        LoadExport(ref omniWorkspacePublishBranch, nameof(omni_workspace_publish_branch));
        LoadExport(ref omniWorkspaceTakeRepo, nameof(omni_workspace_take_repo));
        LoadExport(ref omniWorkspaceSendToMyDevices, nameof(omni_workspace_send_to_my_devices));
        LoadExport(ref omniWorkspaceTakeFromDevice, nameof(omni_workspace_take_from_device));
        LoadExport(ref omniWorkspaceSaveVersion, nameof(omni_workspace_save_version));
        LoadExport(ref omniWorkspaceFiles, nameof(omni_workspace_files));
        LoadExport(ref omniWorkspaceAgents, nameof(omni_workspace_agents));
    }

    internal static IntPtr omni_workspace_add_repo(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceAddRepo!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_repos(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceRepos!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_branches(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceBranches!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_main(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceMain!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_set_main_rule(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceSetMainRule!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_approve(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceApprove!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_view(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceView!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_pull(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspacePull!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_publish_branch(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspacePublishBranch!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_take_repo(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceTakeRepo!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_send_to_my_devices(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceSendToMyDevices!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_take_from_device(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceTakeFromDevice!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_save_version(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceSaveVersion!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_files(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceFiles!(handle, requestJson);
    }

    internal static IntPtr omni_workspace_agents(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniWorkspaceAgents!(handle, requestJson);
    }
}
