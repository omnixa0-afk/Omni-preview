using System;
using System.Text.Json.Nodes;
using System.Threading.Tasks;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// Background mode (docs/decisions/remote-access-for-contacts.md, stage 2): with it on, the X hides the
// window and Omni stays in the notification area, so contacts with standing remote access (and calls,
// messages) still reach this computer. "Выход" in the icon's menu is the way out. Off (the default),
// the X quits as it always did. A per-device preference in device-preferences.json
// ("run_in_background"), like the theme. No autostart — the owner starts Omni after a reboot.
public sealed partial class MainWindow
{
    private const string RunInBackgroundPreferenceKey = "run_in_background";
    private bool runInBackground;
    private bool suppressRunInBackgroundToggle;
    private TrayIcon? trayIcon;
    private bool backgroundOfferShownThisRun;

    private void InitializeBackgroundMode()
    {
        runInBackground = LoadRunInBackgroundPreference();
        suppressRunInBackgroundToggle = true;
        try
        {
            SettingsRunInBackgroundToggle.IsOn = runInBackground;
        }
        finally
        {
            suppressRunInBackgroundToggle = false;
        }

        ApplyBackgroundMode();
    }

    private void OnRunInBackgroundToggled(object sender, RoutedEventArgs e)
    {
        if (suppressRunInBackgroundToggle)
        {
            return;
        }

        SetRunInBackground(SettingsRunInBackgroundToggle.IsOn);
    }

    private void SetRunInBackground(bool enabled)
    {
        runInBackground = enabled;
        try
        {
            JsonObject document = ReadDevicePreferencesDocument();
            document[RunInBackgroundPreferenceKey] = enabled;
            WriteDevicePreferencesDocument(document);
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("app/background", "save preference", ex);
        }

        if (SettingsRunInBackgroundToggle.IsOn != enabled)
        {
            suppressRunInBackgroundToggle = true;
            try
            {
                SettingsRunInBackgroundToggle.IsOn = enabled;
            }
            finally
            {
                suppressRunInBackgroundToggle = false;
            }
        }

        ApplyBackgroundMode();
    }

    // The icon is shown for as long as background mode is on, window open or not: the user always
    // sees that Omni keeps running after the X, and always has "Выход" at hand.
    private void ApplyBackgroundMode()
    {
        if (runInBackground)
        {
            if (trayIcon is null)
            {
                trayIcon = new TrayIcon("Omni — работает в фоне");
                trayIcon.OpenRequested += ShowFromBackground;
                trayIcon.ExitRequested += ExitFromBackground;
            }

            trayIcon.Show();
        }
        else
        {
            trayIcon?.Hide();
            if (!AppWindow.IsVisible)
            {
                ShowFromBackground(); // switched off while hidden: never leave an invisible app
            }
        }
    }

    /// <summary>
    /// Right after access is granted: "in any moment" needs Omni alive with the window closed. Asked
    /// once per run and only while background mode is off — a decline is respected, not nagged.
    /// </summary>
    private async Task OfferBackgroundModeAfterGrantAsync(string contactName)
    {
        if (runInBackground || backgroundOfferShownThisRun || Content.XamlRoot is null)
        {
            return;
        }

        backgroundOfferShownThisRun = true;
        ContentDialog dialog = new()
        {
            Title = "Оставлять Omni работать в фоне?",
            Content = new TextBlock
            {
                Text = $"Чтобы «{contactName}» мог подключиться, когда окно Omni закрыто, оставьте Omni работать в фоне: " +
                       "крестик спрячет окно к часам, а выйти можно будет из меню значка. После перезагрузки Omni нужно запустить самому.",
                TextWrapping = TextWrapping.WrapWholeWords,
            },
            PrimaryButtonText = "Включить",
            CloseButtonText = "Не сейчас",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = Content.XamlRoot,
        };

        try
        {
            if (await dialog.ShowAsync() == ContentDialogResult.Primary)
            {
                SetRunInBackground(true);
            }
        }
        catch (Exception ex)
        {
            // Another dialog was already open (WinUI allows one at a time): skip the offer this time;
            // the Settings toggle is always there.
            backgroundOfferShownThisRun = false;
            AppTrace.WriteFailure("app/background", "offer dialog", ex);
        }
    }

    /// <summary>The X, with background mode on: hide instead of quitting. True if it was handled.</summary>
    private bool TryHideToBackground(AppWindowClosingEventArgs args)
    {
        if (!runInBackground || trayIcon is null)
        {
            return false;
        }

        args.Cancel = true;
        AppWindow.Hide();
        AppTrace.Write("app/background", "window hidden; running in background");
        return true;
    }

    internal void ShowFromBackground()
    {
        AppWindow.Show();
        Activate();
    }

    private void ExitFromBackground()
    {
        AppTrace.Write("app/background", "exit from the notification-area menu");
        BeginShutdown();
    }

    /// <summary>Quit for real: the X without background mode, or "Выход" by the notification icon.</summary>
    private void BeginShutdown()
    {
        if (shutdownStarted)
        {
            return;
        }

        shutdownStarted = true;
        AppTrace.Write("app/shutdown", "closing: stopping media");
        _ = ShutDownThenCloseAsync();
    }

    private void DisposeTrayIcon()
    {
        trayIcon?.Dispose();
        trayIcon = null;
    }

    private bool LoadRunInBackgroundPreference()
    {
        try
        {
            return ReadDevicePreferencesDocument()[RunInBackgroundPreferenceKey]?.GetValue<bool>() ?? false;
        }
        catch (Exception ex)
        {
            // Unreadable value: off is the safe default — the X then quits, as the user expects.
            AppTrace.WriteFailure("app/background", "load preference", ex);
            return false;
        }
    }
}
