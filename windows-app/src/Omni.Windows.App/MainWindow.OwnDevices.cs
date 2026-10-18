using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

// «Мои устройства» in Settings (docs/decisions/multi-device-equal-nodes.md, «Устройства»): the
// devices of this account, and taking one of them back. The core does the work: the revoked device
// is dropped from every route, our other devices and our contacts are told, and the revoked device
// erases its profile when it learns. This surface lists and asks.
public sealed partial class MainWindow
{
    private void RenderOwnDevicesSettings()
    {
        List<OwnDeviceView> devices;
        try
        {
            devices = rustBridge.ListOwnDevices().ToList();
        }
        catch (Exception)
        {
            // Profile not ready — leave the last rendered state; the next settings render retries.
            return;
        }

        SettingsDevicesList.ItemsSource = devices
            .Where(device => !device.Revoked)
            .OrderByDescending(device => device.IsThisDevice)
            .ThenByDescending(device => device.IsNew)
            .Select(DescribeOwnDevice)
            .ToList();
        List<RevokedDeviceRow> revoked = devices
            .Where(device => device.Revoked)
            .Select(device => new RevokedDeviceRow(
                device.DeviceId,
                device.DeviceLabel,
                device.RevokedAtUnixMs is long at ? $"отозвано {MainWindowPresentation.FormatMessageTime(at)}" : "отозвано"))
            .ToList();
        SettingsRevokedDevicesList.ItemsSource = revoked;
        SettingsRevokedDevicesHeaderTextBlock.Text = $"Отозванные ({revoked.Count})";
        SettingsRevokedDevicesExpander.Visibility = revoked.Count > 0 ? Visibility.Visible : Visibility.Collapsed;

        int others = devices.Count(device => !device.IsThisDevice && !device.Revoked);
        List<OwnDeviceView> added = devices.Where(device => device.IsNew).ToList();
        SettingsNewDeviceInfoBar.IsOpen = added.Count > 0;
        SettingsNewDeviceInfoBar.Message = added.Count > 0
            ? $"{string.Join(", ", added.Select(device => $"«{device.DeviceLabel}»"))}. Если это не вы — отзовите: устройство сотрёт свой профиль, как только выйдет в сеть."
            : string.Empty;
        SettingsDevicesSummaryTextBlock.Text = others == 0
            ? "В аккаунте только это устройство. Другое появится здесь, когда вы скопируете папку профиля на другой компьютер."
            : "Отозванное устройство перестаёт получать сообщения и стирает свой профиль, как только выходит в сеть.";
        UpdateNewDevicesBadge(added);
    }

    private DateTime lastErasedProfileCheckUtc;
    private DateTime lastNewDevicesCheckUtc;

    // A dot on «Настройки» while a device of the account has appeared that the owner has not seen:
    // the notice «Добавлено устройство X» must not wait for a visit to Settings. Looked at every few
    // seconds from the refresh loop, like the erased-profile check.
    private void RefreshNewDevicesBadgeIfDue()
    {
        if (windowState.Onboarding.ActiveProfileId is null
            || DateTime.UtcNow - lastNewDevicesCheckUtc < TimeSpan.FromSeconds(5))
        {
            return;
        }

        lastNewDevicesCheckUtc = DateTime.UtcNow;
        try
        {
            UpdateNewDevicesBadge(rustBridge.ListOwnDevices().Where(device => device.IsNew).ToList());
            RefreshOwnProfileIfChangedElsewhere();
        }
        catch (Exception)
        {
            // Profile not ready; the next pass retries.
        }
    }

    // Another device of ours changed the name or avatar and the core took it
    // (multi-device-equal-nodes.md, «Синхронизация профиля»): show it. A draft in the editor stays.
    private void RefreshOwnProfileIfChangedElsewhere()
    {
        LocalAccountSnapshot? shown = windowState.Onboarding.LocalAccount;
        LocalAccountSnapshot? current = rustBridge.GetLocalAccount();
        if (shown is null || current is null
            || (current.PublicProfileUpdatedAtUnixMs == shown.PublicProfileUpdatedAtUnixMs
                && current.PublicAvatarHashHex == shown.PublicAvatarHashHex
                && current.Descriptor.DisplayName == shown.Descriptor.DisplayName))
        {
            return;
        }

        ShowLocalAccount(current);
        UpdateIdentityHeader();
    }

    // Take a fresh snapshot of our account. The profile editor still holding the name shown until now
    // is no draft: it follows the new one — otherwise a later «Сохранить» writes the old name back with
    // a fresh moment, and that wins on every device and at every contact (stand 2026-10-10: the laptop
    // undid a rename made on its copy).
    private void ShowLocalAccount(LocalAccountSnapshot? current)
    {
        LocalAccountSnapshot? shown = windowState.Onboarding.LocalAccount;
        bool editorFollows = shown is not null
            && current is not null
            && shown.Descriptor.DisplayName != current.Descriptor.DisplayName
            && string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath)
            && !pendingPublicAvatarClearRequested
            && string.Equals(SettingsPublicDisplayNameTextBox.Text.Trim(), shown.Descriptor.DisplayName, StringComparison.Ordinal);
        windowState.Onboarding.LocalAccount = current;
        if (editorFollows)
        {
            SettingsPublicDisplayNameTextBox.Text = current!.Descriptor.DisplayName;
        }
    }

    // A bare «1» on the gear said nothing (owner, 2026-10-11: «непонятно какое сообщение»): the
    // tooltip names the device, and a click with the badge on goes straight to «Устройства».
    private void UpdateNewDevicesBadge(IReadOnlyList<OwnDeviceView> added)
    {
        string tip = added.Count == 0
            ? "Настройки"
            : added.Count == 1
                ? $"Настройки — в аккаунте новое устройство «{added[0].DeviceLabel}». Это вы? Проверьте в «Устройства»."
                : $"Настройки — в аккаунте новые устройства ({added.Count}): {string.Join(", ", added.Select(device => $"«{device.DeviceLabel}»"))}. Проверьте в «Устройства».";
        if (SettingsNavigationItem.InfoBadge is InfoBadge shown && shown.Value == added.Count)
        {
            return;
        }

        SettingsNavigationItem.InfoBadge = added.Count > 0 ? new InfoBadge { Value = added.Count } : null;
        ToolTipService.SetToolTip(SettingsNavigationItem, tip);
        AutomationProperties.SetHelpText(SettingsNavigationItem, added.Count > 0 ? tip : string.Empty);
    }

    private bool HasNewDevicesBadge => SettingsNavigationItem.InfoBadge is not null;

    private void OnAcknowledgeOwnDevicesClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            rustBridge.AcknowledgeOwnDevices();
            SettingsDevicesStatusTextBlock.Text = string.Empty;
        }
        catch (Exception ex)
        {
            SettingsDevicesStatusTextBlock.Text = $"Не удалось сохранить: {NormalizeUserError(ex)}";
        }

        RenderOwnDevicesSettings();
    }

    // The account took THIS device back and the core erased the profile from under the shell
    // (Storage::wipe_profile_if_taken_back). Nothing tells the shell, so the refresh loop looks every
    // few seconds: a profile that was active and is gone sends the window back to the start screen,
    // the same way deleting it by hand does.
    private bool TryReturnToOnboardingIfProfileWasErased()
    {
        // Keyed on the shell being signed in, not on the remembered active profile: the periodic
        // lifecycle refresh copies the core's active profile (null once erased) into the window state,
        // and when it ran before this check the check never fired again — the window stayed signed in
        // over an erased profile (stand 2026-10-10, copy-4).
        if (windowState.Onboarding.ShellMode != ViewModels.AppShellMode.Authenticated)
        {
            return false;
        }

        if (DateTime.UtcNow - lastErasedProfileCheckUtc < TimeSpan.FromSeconds(5))
        {
            return false;
        }

        lastErasedProfileCheckUtc = DateTime.UtcNow;
        // A profile that was erased answers "already deleted" to most calls, so ask for the one fact
        // that stays readable: which profile is active.
        string? activeProfileId;
        try
        {
            activeProfileId = rustBridge.GetActiveProfileId();
        }
        catch
        {
            return false;
        }

        if (activeProfileId is not null)
        {
            return false;
        }

        const string message = "Аккаунт отозвал это устройство: профиль стёрт. Создайте профиль или восстановитесь по фразе.";
        try
        {
            ReturnToInitialShellAfterProfileExit(message);
        }
        catch
        {
            _ = BootAsync();
        }

        return true;
    }

    private static OwnDeviceRow DescribeOwnDevice(OwnDeviceView device) => new(
        device.DeviceId,
        device.DeviceLabel,
        device.IsThisDevice ? "Это устройство" : device.Online ? "В сети" : "Не в сети",
        device.IsNew ? Visibility.Visible : Visibility.Collapsed,
        // Only another device that has not been taken back can be revoked.
        device.IsThisDevice ? Visibility.Collapsed : Visibility.Visible);

    /// A row of «Устройства» (bound by name in SettingsSectionControl.xaml).
    public sealed record OwnDeviceRow(string DeviceId, string Label, string StateText, Visibility NewVisibility, Visibility RevokeVisibility);

    /// A row of the folded «Отозванные» list (bound by name in SettingsSectionControl.xaml).
    public sealed record RevokedDeviceRow(string DeviceId, string Label, string WhenText);

    private void OnHideRevokedDeviceRequested(object? sender, string deviceId)
        => HideRevokedOwnDevices(deviceId);

    private void OnHideAllRevokedDevicesClicked(object sender, RoutedEventArgs e)
        => HideRevokedOwnDevices(null);

    // Out of the list only: the revocation stays (runtime_device_revocation.rs).
    private void HideRevokedOwnDevices(string? deviceId)
    {
        try
        {
            rustBridge.HideRevokedOwnDevices(deviceId);
            SettingsDevicesStatusTextBlock.Text = string.Empty;
        }
        catch (Exception ex)
        {
            SettingsDevicesStatusTextBlock.Text = $"Не удалось убрать из списка: {NormalizeUserError(ex)}";
        }

        RenderOwnDevicesSettings();
    }

    // «Ключ аккаунта украден»: the account moves to a new working key and keeps only the devices the
    // owner ticks. The rest are out of the account and told nothing — they are the ones moved away from.
    private async void OnMoveAccountKeyClicked(object sender, RoutedEventArgs e)
    {
        List<OwnDeviceView> others;
        try
        {
            others = rustBridge.ListOwnDevices().Where(device => !device.IsThisDevice && !device.Revoked).ToList();
        }
        catch (Exception ex)
        {
            SettingsDevicesStatusTextBlock.Text = $"Не удалось получить список устройств: {NormalizeUserError(ex)}";
            return;
        }

        StackPanel content = new() { Spacing = 12 };
        content.Children.Add(new TextBlock
        {
            Text = "Аккаунт перейдёт на новый ключ. Omni-ID, контакты и переписка сохранятся, а вор со старым ключом больше не сможет ни выдать себе устройство, ни отозвать ваши.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        content.Children.Add(new TextBlock
        {
            Text = "Оставьте только те устройства, которым доверяете. Остальные будут исключены из аккаунта — им ничего не сообщается. Это устройство остаётся всегда.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        List<(CheckBox Box, string DeviceId)> choices = new();
        foreach (OwnDeviceView device in others)
        {
            // A device that only just appeared is the suspicious one: not kept unless the owner says so.
            CheckBox box = new() { Content = device.DeviceLabel, IsChecked = !device.IsNew };
            choices.Add((box, device.DeviceId));
            content.Children.Add(box);
        }

        if (others.Count == 0)
        {
            content.Children.Add(new TextBlock
            {
                Text = "Других устройств в аккаунте нет.",
                TextWrapping = TextWrapping.WrapWholeWords,
            });
        }

        TextBox phraseBox = new()
        {
            PlaceholderText = "Фраза восстановления (нужна, только если профиль скопирован с другого компьютера)",
            TextWrapping = TextWrapping.Wrap,
            AcceptsReturn = false,
        };
        content.Children.Add(phraseBox);
        PasswordBox? passwordBox = null;
        if (currentPasswordProtectionSummary?.Enabled == true)
        {
            passwordBox = new PasswordBox { PlaceholderText = "Пароль профиля" };
            content.Children.Add(passwordBox);
        }

        ContentDialog dialog = new()
        {
            Title = "Ключ аккаунта украден",
            Content = content,
            PrimaryButtonText = "Перейти на новый ключ",
            CloseButtonText = "Отмена",
            XamlRoot = Content.XamlRoot,
        };
        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        List<string> keep = choices.Where(choice => choice.Box.IsChecked == true).Select(choice => choice.DeviceId).ToList();
        string? phrase = string.IsNullOrWhiteSpace(phraseBox.Text) ? null : phraseBox.Text;
        string? password = passwordBox?.Password;
        try
        {
            AccountKeyMoveSummary summary = await Task.Run(() => rustBridge.MoveAccountToNewWorkingKey(keep, phrase, password));
            SettingsDevicesStatusTextBlock.Text =
                $"Аккаунт перенесён на новый ключ (версия {summary.NewWorkingKeyVersion}). Устройств в аккаунте: {summary.KeptDevices}.";
        }
        catch (Exception ex)
        {
            SettingsDevicesStatusTextBlock.Text = $"Не удалось перейти на новый ключ: {NormalizeUserError(ex)}";
        }

        RenderOwnDevicesSettings();
    }

    private async void RevokeOwnDevice(string deviceId)
    {
        string label = (SettingsDevicesList.ItemsSource as IEnumerable<OwnDeviceRow>)?
            .FirstOrDefault(row => row.DeviceId == deviceId)?.Label ?? deviceId;
        StackPanel content = new() { Spacing = 12 };
        content.Children.Add(new TextBlock
        {
            Text = $"Устройство «{label}» будет исключено из аккаунта.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        content.Children.Add(new TextBlock
        {
            Text = "• Ваши остальные устройства и контакты перестанут слать ему сообщения и слушать его.\n"
                 + "• Когда оно выйдет в сеть, оно СРАЗУ и безвозвратно сотрёт свой профиль.\n"
                 + "• Отозвать можно только с устройства, у которого в этой папке есть ключ аккаунта.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        // A folder under a password keeps the account key under it: the password signs this one act.
        PasswordBox? passwordBox = null;
        if (currentPasswordProtectionSummary?.Enabled == true)
        {
            passwordBox = new PasswordBox { PlaceholderText = "Пароль профиля" };
            content.Children.Add(new TextBlock
            {
                Text = "Профиль защищён паролем — он нужен, чтобы подписать отзыв.",
                TextWrapping = TextWrapping.WrapWholeWords,
            });
            content.Children.Add(passwordBox);
        }

        ContentDialog dialog = new()
        {
            Title = "Отозвать устройство",
            Content = content,
            PrimaryButtonText = "Отозвать",
            CloseButtonText = "Отмена",
            XamlRoot = Content.XamlRoot,
        };
        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        try
        {
            string? password = passwordBox?.Password;
            await Task.Run(() => rustBridge.RevokeOwnDevice(deviceId, password));
            SettingsDevicesStatusTextBlock.Text = $"Устройство «{label}» отозвано.";
        }
        catch (Exception ex)
        {
            SettingsDevicesStatusTextBlock.Text = $"Не удалось отозвать устройство: {NormalizeUserError(ex)}";
        }

        RenderOwnDevicesSettings();
    }
}
