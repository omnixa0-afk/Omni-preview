using System;
using System.IO;
using System.Threading.Tasks;

namespace Omni.Windows.App.Core;

internal static class WorkspaceFolderPreferences
{
    internal static async Task<string> EnsureAsync(RustBridge bridge)
    {
        WorkspaceFilesDetails current = await Task.Run(() => bridge.WorkspaceFiles(new("root", "", Folder: "")));
        if (!string.IsNullOrEmpty(current.Root)) return current.Root;
        string root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments), "Omni Workspaces");
        await Task.Run(() => bridge.WorkspaceFiles(new("root", "", Folder: root)));
        return root;
    }
}
