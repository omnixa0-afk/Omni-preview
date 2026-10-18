using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Settings;

public sealed partial class SettingsSectionControl : UserControl
{
    public SettingsSectionControl()
    {
        InitializeComponent();
        SettingsNavList.SelectionChanged += (_, _) => ShowSelectedSection();
        SettingsNavList.SelectedIndex = 0;
        // One left-column width for every section (Shared.SectionRail).
        Loaded += (_, _) =>
        {
            Shared.SectionRail.Changed -= ApplyRailWidth;
            Shared.SectionRail.Changed += ApplyRailWidth;
            ApplyRailWidth();
        };
        Unloaded += (_, _) => Shared.SectionRail.Changed -= ApplyRailWidth;
    }

    private void ApplyRailWidth() => SettingsNavColumn.Width = new GridLength(Shared.SectionRail.Width);

    /// «Убрать из списка» on a revoked device; the device id is the argument.
    internal event EventHandler<string>? HideRevokedDeviceRequested;

    /// «Отозвать» in a device's row; the device id is the argument.
    internal event EventHandler<string>? RevokeDeviceRequested;

    /// Open a section by its tag (profile, devices, privacy, network, calls, general, advanced).
    internal void ShowSection(string tag)
    {
        foreach (object item in SettingsNavList.Items)
        {
            if (item is ListViewItem listed && Equals(listed.Tag, tag))
            {
                SettingsNavList.SelectedItem = listed;
                return;
            }
        }
    }

    private void ShowSelectedSection()
    {
        string tag = (SettingsNavList.SelectedItem as ListViewItem)?.Tag as string ?? "profile";
        foreach (StackPanel section in new[]
                 {
                     SettingsSection_profile, SettingsSection_devices, SettingsSection_privacy,
                     SettingsSection_network, SettingsSection_calls, SettingsSection_general,
                     SettingsSection_advanced,
                 })
        {
            section.Visibility = section.Name == $"SettingsSection_{tag}" ? Visibility.Visible : Visibility.Collapsed;
        }

        SettingsContentScroll.ChangeView(null, 0, null, disableAnimation: true);
    }

    private void OnRevokeDeviceRowClicked(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: string deviceId })
        {
            RevokeDeviceRequested?.Invoke(this, deviceId);
        }
    }

    private void OnHideRevokedDeviceClicked(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: string deviceId })
        {
            HideRevokedDeviceRequested?.Invoke(this, deviceId);
        }
    }
}
