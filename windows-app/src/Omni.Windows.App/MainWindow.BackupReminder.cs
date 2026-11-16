using System;
using System.Threading.Tasks;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private async void RefreshBackupReminder()
    {
        try
        {
            AccountExportManifestV1? manifest = await Task.Run(() => rustBridge.GetCurrentAccountExportManifest());
            if (manifest is not null)
            {
                SettingsExportStatusTextBlock.Text = BackupReminder.Describe(manifest.LastBackupAtUnixMs, DateTimeOffset.Now);
            }
        }
        catch
        {
            // Строка подсказки: ядро ещё холодное — перечитаем при следующем показе.
        }
    }
}
