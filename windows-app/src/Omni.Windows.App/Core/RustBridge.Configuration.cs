namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public TransportMode GetConfiguredTransportMode()
        => sidecarSettingsCache.ReadEnum(
            GetTransportModeSidecarPath(appDirectory),
            TransportMode.Quic,
            "transport_mode",
            TransportModeExtensions.FromDbValue);

    public string CreateIsolatedWorkspaceDirectory(string workspaceId)
    {
        string workspaceDirectory = Path.Combine(
            GetWorkspaceRootDirectory(appDirectory),
            "workspaces",
            SanitizeWorkspaceId(workspaceId));
        Directory.CreateDirectory(workspaceDirectory);
        ApplyConfiguredSidecarsToDirectory(workspaceDirectory);
        return workspaceDirectory;
    }

    public string CreateSameHostCompanionWorkspaceDirectory(string workspaceId)
        => CreateIsolatedWorkspaceDirectory(workspaceId);

    public string CreateClonedAppDirectory(string cloneId)
    {
        string cloneDirectory = Path.Combine(
            GetCloneRootDirectory(appDirectory),
            SanitizeWorkspaceId(cloneId));

        CloneAppDirectorySnapshot(appDirectory, cloneDirectory);
        ApplyConfiguredSidecarsToDirectory(cloneDirectory);
        return cloneDirectory;
    }
}
