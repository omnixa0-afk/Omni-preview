using System;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// Settings → «Для опытных» → «Журнал для разработчика». The app applied the stored choice before the
// window (App.xaml.cs); this keeps the switch and its line in step and saves a change, which the core
// takes at once — no restart. See Core/TraceLogPreference.cs.
public sealed partial class MainWindow
{
    private bool suppressTraceLogToggle;

    private void InitializeTraceLogSetting()
    {
        suppressTraceLogToggle = true;
        try
        {
            SettingsTraceLogToggle.IsOn = AppTrace.IsEnabled;
            // A hand-set variable decides; a switch that moves and changes nothing would lie.
            SettingsTraceLogToggle.IsEnabled = AppTrace.EnvironmentTraceFile is null;
        }
        finally
        {
            suppressTraceLogToggle = false;
        }

        RefreshTraceLogStatus();
    }

    private void OnTraceLogToggled(object sender, RoutedEventArgs e)
    {
        if (suppressTraceLogToggle)
        {
            return;
        }

        bool on = SettingsTraceLogToggle.IsOn;
        try
        {
            JsonObject document = ReadDevicePreferencesDocument();
            document[TraceLogPreference.Key] = on;
            WriteDevicePreferencesDocument(document);
            AppTrace.SetEnabled(appDirectory, on);
            AppTrace.Write("app/trace-log", "turned on in settings");
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("app/trace-log", "switch", ex);
        }

        RefreshTraceLogStatus();
    }

    private void RefreshTraceLogStatus()
        => SettingsTraceLogStatusTextBlock.Text =
            TraceLogPreference.StatusText(appDirectory, AppTrace.IsEnabled, AppTrace.EnvironmentTraceFile);
}
