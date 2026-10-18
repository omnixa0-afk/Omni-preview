using System;
using System.Collections.Generic;
using System.Linq;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// Remote access overview in Settings (docs/decisions/remote-access-for-contacts.md): read-only list
// of who may control this computer, plus "revoke all". The per-contact grant is edited in the
// contact card; this surface only shows and withdraws. No session, no input.
public sealed partial class MainWindow
{
    private void RenderRemoteAccessSettings()
    {
        List<ContactRemoteAccessRecord> granted;
        Dictionary<string, string> names;
        try
        {
            granted = rustBridge.ListContactRemoteAccess()
                .Where(record => record.GrantedMode is not null)
                .ToList();
            names = rustBridge.ListContacts()
                .GroupBy(contact => contact.ContactId, StringComparer.Ordinal)
                .ToDictionary(group => group.Key, group => group.First().Petname, StringComparer.Ordinal);
        }
        catch (Exception)
        {
            // Profile not ready — leave the last rendered state; the next settings render retries.
            return;
        }

        SettingsRemoteAccessList.ItemsSource = granted
            .Select(record =>
            {
                string name = names.TryGetValue(record.ContactId, out string? petname) && !string.IsNullOrWhiteSpace(petname)
                    ? petname
                    : record.ContactId;
                string mode = record.GrantedMode == "unattended" ? "без вопроса" : "с подтверждением";
                return $"{name} — {mode}";
            })
            .ToList();

        SettingsRemoteAccessSummaryTextBlock.Text = granted.Count == 0
            ? "Никому — разрешение выдаётся в карточке контакта."
            : $"Разрешено контактам: {granted.Count}. Забрать у одного — в его карточке.";
        // Nothing to revoke — no button (a grey one only asked why).
        SettingsRevokeAllRemoteAccessButton.Visibility = granted.Count > 0 ? Microsoft.UI.Xaml.Visibility.Visible : Microsoft.UI.Xaml.Visibility.Collapsed;
    }

    private void OnRevokeAllRemoteAccessClicked(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
    {
        int revoked = 0;
        try
        {
            foreach (ContactRemoteAccessRecord record in rustBridge.ListContactRemoteAccess().Where(r => r.GrantedMode is not null))
            {
                rustBridge.SetContactRemoteAccessGranted(record.ContactId, null);
                revoked++;
            }

            SettingsRemoteAccessStatusTextBlock.Text = revoked == 0
                ? "Нечего отзывать."
                : $"Удалённый доступ отозван у всех ({revoked}).";
        }
        catch (Exception ex)
        {
            SettingsRemoteAccessStatusTextBlock.Text = $"Не удалось отозвать доступ: {NormalizeUserError(ex)}";
        }

        RenderRemoteAccessSettings();
        // The open contact card, if any, reflects the change too.
        RenderSelectedContactRemoteAccessIfSelected();
    }

    private void RenderSelectedContactRemoteAccessIfSelected()
    {
        if (selectedContactCard is not null)
        {
            RenderSelectedContactRemoteAccess();
        }
    }
}
