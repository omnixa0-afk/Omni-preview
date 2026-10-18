using System;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private bool backupReminderWired;

    /// <summary>Дата последней копии перечитывается при каждом показе настроек.</summary>
    private void InitializeBackupReminder()
    {
        if (backupReminderWired)
        {
            return;
        }

        SettingsSectionRoot.RegisterPropertyChangedCallback(UIElement.VisibilityProperty, (_, _) =>
        {
            if (SettingsSectionRoot.Visibility == Visibility.Visible)
            {
                DispatcherQueue.TryEnqueue(RefreshBackupReminder);
            }
        });
        backupReminderWired = true;
    }

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
