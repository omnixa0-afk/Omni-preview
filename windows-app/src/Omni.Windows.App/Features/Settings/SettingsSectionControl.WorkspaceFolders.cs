using System;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Settings;

public sealed partial class SettingsSectionControl
{
    private RustBridge? workspaceBridge;
    private Func<Task<string?>>? workspacePickFolder;

    internal void ConfigureWorkspaceFolders(RustBridge bridge, Func<Task<string?>> pickFolder)
    {
        workspaceBridge = bridge;
        workspacePickFolder = pickFolder;
    }

    private async Task RefreshWorkspaceFoldersAsync()
    {
        if (workspaceBridge is null) return;
        try { SettingsWorkspaceRootText.Text = await WorkspaceFolderPreferences.EnsureAsync(workspaceBridge); }
        catch (Exception ex) { SettingsWorkspaceRootText.Text = $"Не удалось прочитать папку проектов: {ex.Message}"; }
    }

    private async void OnWorkspaceRootClicked(object sender, RoutedEventArgs e)
    {
        if (workspaceBridge is null || workspacePickFolder is null) return;
        try
        {
            string? folder = await workspacePickFolder();
            if (folder is null) return;
            RustBridge core = workspaceBridge;
            await Task.Run(() => core.WorkspaceFiles(new("root", "", Folder: folder)));
            await RefreshWorkspaceFoldersAsync();
        }
        catch (Exception ex) { SettingsWorkspaceRootText.Text = $"Не удалось сменить папку проектов: {ex.Message}"; }
    }
}
