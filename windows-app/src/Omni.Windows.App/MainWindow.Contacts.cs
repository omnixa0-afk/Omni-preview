using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Text.Json;
using System.Threading;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;
using Windows.Storage.FileProperties;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Contacts;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void ResetPrekeyExchangeUi()
    {
        LocalPrekeyBundleTextBox.Text = "Сведения о ключах скрыты из обычного чата.";
        RemotePrekeyBundleTextBox.Text = string.Empty;
        PrekeyBundleStatusTextBlock.Text = "Статус: обмен ключами выполняется автоматически.";
    }

    private static MenuFlyoutItem CreateContactActionMenuItem(string text, string automationId)
    {
        MenuFlyoutItem item = new()
        {
            Text = text,
        };

        AutomationProperties.SetAutomationId(item, automationId);
        AutomationProperties.SetName(item, text);
        AutomationProperties.SetHelpText(item, text);
        return item;
    }

    private ContactSnapshot AttachCurrentDialogToContactIfAvailable(
        ContactSnapshot contact,
        out string? statusText)
    {
        string? currentPeerKey = GetResolvedCurrentDialogKey()
            ?? windowState.CurrentTransportPeerKey?.Trim();
        if (string.IsNullOrWhiteSpace(currentPeerKey))
        {
            statusText = null;
            return contact;
        }

        try
        {
            ContactSnapshot attached = rustBridge.AttachDialogToContact(contact.ContactId, currentPeerKey);
            statusText = $"Контакт {attached.Petname} добавлен и привязан к текущему диалогу.";
            return attached;
        }
        catch (Exception ex)
        {
            statusText = $"Контакт {contact.Petname} добавлен, но привязать текущий диалог не удалось: {NormalizeUserError(ex)}";
            return contact;
        }
    }

    private ContactSnapshot? TryFindExistingContactByRemoteAccountId(string accountId)
    {
        return GetContactSnapshotsForLookup()
            .FirstOrDefault(contact =>
                string.Equals(contact.RemoteAccountId, accountId, StringComparison.OrdinalIgnoreCase));
    }

    private ContactSnapshot? TryFindExistingContactByRemoteInstanceId(string instanceId)
    {
        return GetContactSnapshotsForLookup()
            .FirstOrDefault(contact =>
                string.Equals(contact.RemoteInstanceId, instanceId, StringComparison.OrdinalIgnoreCase));
    }

    private IReadOnlyList<ContactSnapshot> GetContactSnapshotsForLookup()
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            return [];
        }

        return rustBridge.GetContactsScreenSnapshot().Contacts;
    }

    private ContactSnapshot? TryResolveActiveRemoteContact()
    {
        string? remoteAccountId = lastTransportSnapshot?.RemoteAccountId;
        if (!string.IsNullOrWhiteSpace(remoteAccountId))
        {
            ContactSnapshot? byAccountId = TryFindExistingContactByRemoteAccountId(remoteAccountId);
            if (byAccountId is not null)
            {
                return byAccountId;
            }
        }

        string? remoteInstanceId = lastTransportSnapshot?.RemoteInstanceId;
        if (!string.IsNullOrWhiteSpace(remoteInstanceId))
        {
            return TryFindExistingContactByRemoteInstanceId(remoteInstanceId);
        }

        return null;
    }

    private string BuildConnectStatusText(bool dialogReady)
    {
        ContactSnapshot? activeRemoteContact = TryResolveActiveRemoteContact();
        if (activeRemoteContact is not null &&
            string.Equals(activeRemoteContact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase))
        {
            return "Подключение выполнено. Контакт найден; ждём ping/keepalive в диалоге.";
        }

        return dialogReady
            ? "Подключение выполнено, запрос отправлен. Диалог создан; ждём ping/keepalive."
            : "Подключение выполнено, запрос отправлен. Ждём подтверждения контакта.";
    }

    private void RefreshContactsForActiveRemote()
    {
        ContactSnapshot? contact = TryResolveActiveRemoteContact();
        if (contact is null)
        {
            RefreshContactsList(true, windowState.SelectedContactId);
            RefreshSelectedContactCard();
            return;
        }

        if (!string.Equals(windowState.SelectedContactId, contact.ContactId, StringComparison.Ordinal))
        {
            windowState.SelectedContactRevision = null;
        }

        windowState.SelectedContactId = contact.ContactId;
        RefreshContactsList(true, contact.ContactId);
        RefreshSelectedContactCard();
    }

    private string? TryOpenApprovedActiveRemoteChat(bool navigateToChat)
    {
        ContactSnapshot? contact = TryResolveActiveRemoteContact();
        if (contact is null ||
            !string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase))
        {
            return null;
        }

        return TryOpenApprovedContactChat(contact, navigateToChat);
    }

    private string? TryOpenApprovedContactChat(ContactSnapshot contact, bool navigateToChat)
    {
        try
        {
            ContactChatTarget target = rustBridge.StartChatFromContact(contact.ContactId);
            OpenChatThread(target.DialogKey, target.ContactId);
            RefreshChatsList(true, target.DialogKey);
            if (navigateToChat)
            {
                ShowChatsSection();
            }

            return target.DialogKey;
        }
        catch (Exception ex)
        {
            string? currentDialogKey = GetAttachableCurrentDialogKey();
            if (string.IsNullOrWhiteSpace(currentDialogKey) ||
                !NormalizeUserError(ex).Contains("not linked to a dialog yet", StringComparison.OrdinalIgnoreCase))
            {
                return null;
            }

            try
            {
                ContactSnapshot attached = rustBridge.AttachDialogToContact(contact.ContactId, currentDialogKey);
                ContactChatTarget target = rustBridge.StartChatFromContact(attached.ContactId);
                OpenChatThread(target.DialogKey, target.ContactId);
                RefreshChatsList(true, target.DialogKey);
                if (navigateToChat)
                {
                    ShowChatsSection();
                }

                return target.DialogKey;
            }
            catch
            {
                return null;
            }
        }
    }

    private static bool TryParseContactVerificationPayload(
        string payloadText,
        out ContactVerificationPayloadV1? payload,
        out string error)
    {
        payload = null;
        error = string.Empty;

        try
        {
            using JsonDocument document = JsonDocument.Parse(payloadText);
            JsonElement root = document.RootElement;

            if (root.ValueKind != JsonValueKind.Object)
            {
                error = "Данные контакта должны быть JSON-объектом.";
                return false;
            }

            if (!TryReadUnsignedVersion(root, out ushort rootVersion, out error))
            {
                return false;
            }

            if (rootVersion is not 1)
            {
                error = $"Неподдерживаемая версия данных контакта: {rootVersion}.";
                return false;
            }

            if (!root.TryGetProperty("account", out JsonElement accountElement) ||
                accountElement.ValueKind != JsonValueKind.Object)
            {
                error = "В данных контакта отсутствует или неверно задано поле 'account'.";
                return false;
            }

            if (!TryReadUnsignedVersion(accountElement, out ushort accountVersion, out error))
            {
                return false;
            }

            if (accountVersion is not 1)
            {
                error = $"Неподдерживаемая версия профиля контакта: {accountVersion}.";
                return false;
            }

            if (!TryReadRequiredString(accountElement, "account_id", out string accountId, out error))
            {
                return false;
            }

            if (!TryReadRequiredString(accountElement, "display_name", out string displayName, out error))
            {
                return false;
            }

            if (!root.TryGetProperty("devices", out JsonElement devicesElement) ||
                devicesElement.ValueKind != JsonValueKind.Array)
            {
                error = "В данных контакта отсутствует или неверно задано поле 'devices'.";
                return false;
            }

            List<DevicePublicDescriptorV1> devices = new();
            foreach (JsonElement deviceElement in devicesElement.EnumerateArray())
            {
                if (deviceElement.ValueKind != JsonValueKind.Object)
                {
                    error = "Каждое устройство должно быть JSON-объектом.";
                    return false;
                }

                if (!TryReadUnsignedVersion(deviceElement, out ushort deviceVersion, out error))
                {
                    return false;
                }

                if (deviceVersion is not 1)
                {
                    error = $"Неподдерживаемая версия устройства: {deviceVersion}.";
                    return false;
                }

                if (!TryReadRequiredString(deviceElement, "device_id", out string deviceId, out error))
                {
                    return false;
                }

                if (!TryReadRequiredString(deviceElement, "account_id", out string deviceAccountId, out error))
                {
                    return false;
                }

                if (!TryReadRequiredString(deviceElement, "device_label", out string deviceLabel, out error))
                {
                    return false;
                }

                devices.Add(new DevicePublicDescriptorV1(
                    deviceVersion,
                    deviceId,
                    deviceAccountId,
                    deviceLabel));
            }

            if (!TryReadRequiredString(root, "fingerprint", out string fingerprint, out error))
            {
                return false;
            }

            if (!TryReadRequiredString(root, "safety_number", out string safetyNumber, out error))
            {
                return false;
            }

            if (!TryReadRequiredInt64(root, "exported_at_unix_ms", out long exportedAtUnixMs, out error))
            {
                return false;
            }

            foreach (DevicePublicDescriptorV1 device in devices)
            {
                if (!string.Equals(device.AccountId, accountId, StringComparison.OrdinalIgnoreCase))
                {
                    error = "Идентификатор устройства не совпадает с идентификатором профиля.";
                    return false;
                }
            }

            payload = new ContactVerificationPayloadV1(
                rootVersion,
                new AccountPublicDescriptorV1(accountVersion, accountId, displayName),
                devices,
                fingerprint,
                safetyNumber,
                exportedAtUnixMs);
            return true;
        }
        catch (JsonException ex)
        {
            error = $"Не удалось разобрать данные контакта: {NormalizeUserError(ex)}";
            return false;
        }
    }

    private static bool TryReadRequiredString(
        JsonElement parent,
        string propertyName,
        out string value,
        out string error)
    {
        if (parent.ValueKind == JsonValueKind.Object &&
            parent.TryGetProperty(propertyName, out JsonElement element) &&
            element.ValueKind == JsonValueKind.String)
        {
            value = element.GetString() ?? string.Empty;
            if (!string.IsNullOrWhiteSpace(value))
            {
                error = string.Empty;
                return true;
            }
        }

        value = string.Empty;
        error = $"В данных контакта отсутствует или неверно задано поле '{propertyName}'.";
        return false;
    }

    private static bool TryReadRequiredInt64(
        JsonElement parent,
        string propertyName,
        out long value,
        out string error)
    {
        if (parent.ValueKind == JsonValueKind.Object &&
            parent.TryGetProperty(propertyName, out JsonElement element) &&
            element.TryGetInt64(out value))
        {
            error = string.Empty;
            return true;
        }

        value = 0;
        error = $"В данных контакта отсутствует или неверно задано поле '{propertyName}'.";
        return false;
    }

    private static bool TryReadUnsignedVersion(
        JsonElement parent,
        out ushort value,
        out string error)
    {
        if (parent.ValueKind == JsonValueKind.Object &&
            parent.TryGetProperty("version", out JsonElement element) &&
            element.TryGetUInt16(out value))
        {
            error = string.Empty;
            return true;
        }

        value = 0;
        error = "В данных контакта отсутствует или неверно задано поле 'version'.";
        return false;
    }

    private void RefreshContactsList(bool force = false, string? selectContactId = null)
    {
        if (!force &&
            string.Equals(
                windowState.LastRenderedContactSignature,
                windowState.LastObservedContactRefreshSignature,
                StringComparison.Ordinal) &&
            contactItems.Count > 0)
        {
            RefreshFilteredContactsList(selectContactId ?? windowState.SelectedContactId);
            return;
        }

        ContactsScreenSnapshot snapshot = rustBridge.GetContactsScreenSnapshot();
        windowState.LastObservedContactRefreshSignature = BuildContactsRefreshSignature(snapshot.RefreshSummary);
        ApplyContactsSnapshots(snapshot.Contacts, force, selectContactId);
    }

    private void ApplyContactsSnapshots(
        IReadOnlyList<ContactSnapshot> contacts,
        bool force = false,
        string? selectContactId = null)
    {
        long nowUnixMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        windowState.LastRenderedContactSignature = windowState.LastObservedContactRefreshSignature;
        List<ContactListItem> renderedContacts = [];
        List<ContactListItem> pendingContacts = [];
        List<ContactListItem> approvedContacts = [];
        List<ContactListItem> declinedContacts = [];

        HashSet<string> linkedAccountIds = CurrentlyLinkedAccountIds();
        foreach (ContactSnapshot contact in contacts)
        {
            ContactListItem item = MainWindowPresentation.ToContactItem(contact, nowUnixMs, linkedAccountIds);
            renderedContacts.Add(item);

            if (IsPendingContactItem(item))
            {
                pendingContacts.Add(item);
                continue;
            }

            if (IsDeclinedContactItem(item))
            {
                declinedContacts.Add(item);
                continue;
            }

            if (IsApprovedContactItem(item))
            {
                approvedContacts.Add(item);
            }
        }

        SynchronizeCollection(
            contactItems,
            renderedContacts,
            item => item.ContactId);
        SynchronizeCollection(
            pendingContactItems,
            pendingContacts,
            item => item.ContactId);
        SynchronizeCollection(
            approvedContactItems,
            approvedContacts,
            item => item.ContactId);
        SynchronizeCollection(
            declinedContactItems,
            declinedContacts,
            item => item.ContactId);

        RefreshFilteredContactsList(selectContactId ?? windowState.SelectedContactId);
    }

    private static bool IsPendingContactItem(ContactListItem item)
        => string.Equals(item.RelationshipState, "pending", StringComparison.OrdinalIgnoreCase);

    private static bool IsApprovedContactItem(ContactListItem item)
        => string.Equals(item.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase);

    private static bool IsDeclinedContactItem(ContactListItem item)
        => string.Equals(item.RelationshipState, "declined", StringComparison.OrdinalIgnoreCase);

    private static bool MatchesContactsFilter(ContactListItem item, ContactsFilterMode filterMode)
        => filterMode switch
        {
            ContactsFilterMode.All => true,
            ContactsFilterMode.Pending => IsPendingContactItem(item),
            ContactsFilterMode.Approved => IsApprovedContactItem(item),
            ContactsFilterMode.Declined => IsDeclinedContactItem(item),
            _ => true,
        };

    private static ContactsFilterMode ResolveContactsFilterForItem(ContactListItem item)
        => item switch
        {
            _ when IsPendingContactItem(item) => ContactsFilterMode.Pending,
            _ when IsDeclinedContactItem(item) => ContactsFilterMode.Declined,
            _ when IsApprovedContactItem(item) => ContactsFilterMode.Approved,
            _ => ContactsFilterMode.All,
        };

    private void ApplyContactsWorkspaceMode(ContactsWorkspaceMode mode)
    {
        currentContactsWorkspaceMode = mode;
        // Adding takes the main pane; the list stays on the left (owner, 2026-10-13: the add form was
        // squeezed into the list column).
        bool adding = mode == ContactsWorkspaceMode.Add;
        ContactsDirectoryPanel.Visibility = Visibility.Visible;
        ContactsAddPanel.Visibility = adding ? Visibility.Visible : Visibility.Collapsed;
        ContactsDetailScroll.Visibility = adding ? Visibility.Collapsed : Visibility.Visible;
        ContactsAddToggleIcon.Glyph = adding ? "\uE711" : "\uE710";
        ContactsAddToggleText.Text = adding ? "Закрыть" : "Добавить";
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ContactsAddToggleButton, adding ? "Закрыть добавление" : "Добавить контакт");
    }

    private void ApplyContactsFilterMode(ContactsFilterMode mode)
    {
        currentContactsFilterMode = mode;
        UpdateContactsFilterButtons();
        RefreshFilteredContactsList(allowFilterShiftForSelection: false);
    }

    private void RefreshFilteredContactsList(
        string? selectContactId = null,
        bool allowFilterShiftForSelection = true)
    {
        List<ContactListItem> filteredItems = contactItems
            .Where(item => MatchesContactsFilter(item, currentContactsFilterMode))
            .ToList();
        // A row whose status changed is REPLACED in the list; replacing the selected row drops the
        // ListView's selection for a moment. Unsuppressed, that echo rendered the card as "nothing
        // selected" (body collapsed — the pane's scroll fell to the top) until the reselect below
        // brought it back: the card jumped up every time a contact went on or off line (owner,
        // 2026-10-11). The selection is restored right after, from windowState.
        WithSelectionSyncSuppressed(ref suppressContactSelectionSync, () =>
            SynchronizeCollection(
                filteredContactItems,
                filteredItems,
                item => item.ContactId));
        UpdateContactsFilterButtons();
        bool isEmpty = filteredItems.Count == 0;
        ContactsEmptyStateBorder.Visibility = isEmpty ? Visibility.Visible : Visibility.Collapsed;
        ContactsEmptyStateTextBlock.Text = BuildContactsEmptyStateText();
        SelectContactInFilteredList(selectContactId ?? windowState.SelectedContactId, allowFilterShiftForSelection);
    }

    private void UpdateContactsFilterButtons()
    {
        ContactsFilterAllButton.Content = BuildContactsFilterLabel("Все", contactItems.Count);
        ContactsFilterPendingButton.Content = BuildContactsFilterLabel("Запросы", pendingContactItems.Count);
        ContactsFilterApprovedButton.Content = BuildContactsFilterLabel("Принятые", approvedContactItems.Count);
        ContactsFilterDeclinedButton.Content = BuildContactsFilterLabel("Отказы", declinedContactItems.Count);

        SetPillButtonState(ContactsFilterAllButton, currentContactsFilterMode == ContactsFilterMode.All);
        SetPillButtonState(ContactsFilterPendingButton, currentContactsFilterMode == ContactsFilterMode.Pending);
        SetPillButtonState(ContactsFilterApprovedButton, currentContactsFilterMode == ContactsFilterMode.Approved);
        SetPillButtonState(ContactsFilterDeclinedButton, currentContactsFilterMode == ContactsFilterMode.Declined);
    }

    // A count stops at «999+» (owner, 2026-10-12); the column grows with the pills
    // (Features/Shared/SectionRail.cs), so it never has to grow past four characters.
    private static string BuildContactsFilterLabel(string label, int count)
        => count switch
        {
            <= 0 => label,
            > 999 => $"{label} 999+",
            _ => $"{label} {count}",
        };

    private string BuildContactsEmptyStateText()
        => currentContactsFilterMode switch
        {
            ContactsFilterMode.All => "Пока никого. Нажмите «Добавить» вверху.",
            ContactsFilterMode.Pending => "Новых запросов нет.",
            ContactsFilterMode.Approved => "Принятых пока нет.",
            ContactsFilterMode.Declined => "Отказов нет.",
            _ => "В этой категории пока ничего нет.",
        };

    private void SetPillButtonState(Button button, bool isActive)
    {
        string resourceKey = button.Name.StartsWith("ContactsFilter", StringComparison.Ordinal)
            ? isActive ? "ContactFilterAccentButtonStyle" : "ContactFilterButtonStyle"
            : isActive ? "AccentPillButtonStyle" : "PillButtonStyle";
        if (Application.Current.Resources.TryGetValue(resourceKey, out object? resource) &&
            resource is Style style)
        {
            button.Style = style;
        }
    }

    private bool HasAnySelectedContact()
        => FilteredContactsListView.SelectedItem is not null;

    private void SelectContactInFilteredList(
        string? contactId,
        bool allowFilterShiftForSelection = true)
    {
        WithSelectionSyncSuppressed(ref suppressContactSelectionSync, () =>
        {
            FilteredContactsListView.SelectedItem = null;

            if (string.IsNullOrWhiteSpace(contactId))
            {
                windowState.SelectedContactId = null;
                windowState.SelectedContactRevision = null;
                selectedContactCard = null;
                RenderSelectedContactCard();
                return;
            }

            ContactListItem? selected = contactItems.FirstOrDefault(item =>
                string.Equals(item.ContactId, contactId, StringComparison.Ordinal));
            if (selected is null)
            {
                windowState.SelectedContactId = null;
                windowState.SelectedContactRevision = null;
                selectedContactCard = null;
                RenderSelectedContactCard();
                return;
            }

            if (!string.Equals(windowState.SelectedContactId, selected.ContactId, StringComparison.Ordinal))
            {
                windowState.SelectedContactRevision = null;
            }
            windowState.SelectedContactId = selected.ContactId;
            if (!MatchesContactsFilter(selected, currentContactsFilterMode))
            {
                if (allowFilterShiftForSelection)
                {
                    currentContactsFilterMode = ResolveContactsFilterForItem(selected);
                    UpdateContactsFilterButtons();
                    RefreshFilteredContactsList(selected.ContactId, allowFilterShiftForSelection: false);
                }
                else
                {
                    windowState.SelectedContactId = null;
                    windowState.SelectedContactRevision = null;
                    selectedContactCard = null;
                    RenderSelectedContactCard();
                }

                return;
            }

            ContactListItem? filteredSelected = filteredContactItems.FirstOrDefault(item =>
                string.Equals(item.ContactId, selected.ContactId, StringComparison.Ordinal));
            if (filteredSelected is not null)
            {
                FilteredContactsListView.SelectedItem = filteredSelected;
            }
        });
    }

    private void RefreshSelectedContactCard()
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            selectedContactCard = null;
            windowState.SelectedContactRevision = null;
            RenderSelectedContactCard();
            return;
        }

        try
        {
            EntityRevisionSnapshot? revisionSnapshot = rustBridge.GetContactCardRevision(windowState.SelectedContactId!);
            ApplySelectedContactRefreshPayload(new SelectedContactRefreshPayload(
                windowState.SelectedContactId!,
                rustBridge.GetContactCard(windowState.SelectedContactId!),
                revisionSnapshot?.Revision));
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void ApplySelectedContactRefreshPayload(SelectedContactRefreshPayload payload)
    {
        selectedContactCard = payload.ContactCard;
        windowState.SelectedContactRevision = payload.Revision;
        RenderSelectedContactCard();
    }

    private void RenderSelectedContactCard()
    {
        long nowUnixMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        // No contact selected → show only the header prompt, hide the trust/verification/rename/
        // tools body and the action buttons (otherwise the card showed a "Не проверен" trust
        // verdict + empty rename/verify fields for nothing — the same noise the chat empty state
        // had). The header keeps the "Выберите контакт слева…" prompt.
        bool hasSelection = selectedContactCard is not null;
        SelectedContactDetailBody.Visibility = hasSelection ? Visibility.Visible : Visibility.Collapsed;
        SelectedContactActionPanel.Visibility = hasSelection ? Visibility.Visible : Visibility.Collapsed;
        SelectedContactTitleTextBlock.Text = selectedContactCard is null
            ? contactItems.Count == 0 ? "Контактов пока нет" : "Выберите контакт"
            : MainWindowPresentation.ContactTitle(selectedContactCard.Contact);
        // One status under the name, details under «Подробнее» (MainWindow.Chats.Header.cs).
        RefreshSelectedContactConnection(nowUnixMs);
        SelectedContactAvatarTextBlock.FontFamily = selectedContactCard is null
            ? new FontFamily("Segoe Fluent Icons")
            : new FontFamily("Segoe UI");
        SelectedContactAvatarTextBlock.Text = selectedContactCard is null
            ? "\uE77B"
            : MainWindowPresentation.BuildAvatarText(SelectedContactTitleTextBlock.Text);
        ApplyAvatarVisual(
            SelectedContactAvatarImage,
            SelectedContactAvatarTextBlock,
            selectedContactCard?.Contact.PublicAvatarImagePath,
            SelectedContactAvatarTextBlock.Text,
            loadingRing: SelectedContactAvatarLoadingRing,
            isLoading: selectedContactCard?.Contact is not null &&
                !string.IsNullOrWhiteSpace(selectedContactCard.Contact.PublicAvatarPendingHashHex),
            versionToken: BuildContactAvatarVersionToken(selectedContactCard?.Contact),
            circle: SelectedContactAvatarCircle,
            colorSeed: selectedContactCard?.Contact is { } seeded ? MainWindowPresentation.AvatarSeed(seeded) : string.Empty);
        // Presence dot on the detail-card avatar, mirroring the contact list + chat header.
        ApplyPresenceDot(SelectedContactPresenceDotBorder, SelectedContactPresenceDotEllipse, selectedContactCard?.Contact);
        ContactTrustStateTextBlock.Text = MainWindowPresentation.BuildTrustLine(selectedContactCard);
        ContactTrustWarningTextBlock.Text = MainWindowPresentation.BuildTrustWarning(selectedContactCard);
        ContactTrustWarningTextBlock.Visibility = string.IsNullOrEmpty(ContactTrustWarningTextBlock.Text)
            ? Visibility.Collapsed
            : Visibility.Visible;
        bool verified = string.Equals(selectedContactCard?.TrustState, "verified", StringComparison.OrdinalIgnoreCase);
        ToggleManualVerificationButton.Content = verified ? "Сверить ещё раз" : "Сверить ключи вручную";
        RenderSelectedContactRemoteAccess();
        ContactCardSummaryTextBlock.Text = MainWindowPresentation.BuildContactDetails(selectedContactCard, nowUnixMs);
        ContactCardIdTextBlock.Text = selectedContactCard?.Contact.RemoteAccountId ?? string.Empty;
        RenameContactTextBox.Text = selectedContactCard?.Contact.Petname ?? string.Empty;
        // The tile says «Написать»; a contact not linked yet is said in its tooltip.
        ToolTipService.SetToolTip(
            OpenContactChatButton,
            selectedContactCard is not null &&
            string.Equals(selectedContactCard.Contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase) &&
            string.IsNullOrWhiteSpace(selectedContactCard.Contact.RemoteInstanceId) &&
            !string.IsNullOrWhiteSpace(selectedContactCard.Contact.RemoteAccountId)
                ? "Ждём первого подключения — сообщения уйдут, когда собеседник появится в сети"
                : "Открыть чат");
    }

}
