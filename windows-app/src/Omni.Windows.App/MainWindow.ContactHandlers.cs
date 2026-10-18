using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Contacts;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void OnAddContactByIdentifierClicked(object sender, RoutedEventArgs e)
    {
        string accountId = AddContactIdentifierTextBox.Text.Trim();
        string petname = AddContactPetnameTextBox.Text.Trim();
        string? publicDisplayName = string.IsNullOrWhiteSpace(AddContactDisplayNameTextBox.Text)
            ? null
            : AddContactDisplayNameTextBox.Text.Trim();

        if (string.IsNullOrWhiteSpace(accountId) || string.IsNullOrWhiteSpace(petname))
        {
            ContactsStatusTextBlock.Text = "Введите идентификатор профиля и локальное имя.";
            return;
        }

        try
        {
            ContactSnapshot? existingContact = TryFindExistingContactByRemoteAccountId(accountId);
            if (existingContact is not null)
            {
                ContactSnapshot existingMatch = existingContact;
                string relationshipState = existingMatch.RelationshipState?.Trim().ToLowerInvariant() ?? "unknown";
                ContactsStatusTextBlock.Text = relationshipState switch
                {
                    "approved" => $"Контакт {existingMatch.Petname} уже в списке контактов.",
                    "pending" => $"Запрос для {existingMatch.Petname} уже сохранён в списке запросов и ждёт решения.",
                    "declined" => $"Контакт {existingMatch.Petname} уже отклонён.",
                    _ => $"Контакт {existingMatch.Petname} уже есть в списке.",
                };
                AddContactIdentifierTextBox.Text = string.Empty;
                AddContactPetnameTextBox.Text = string.Empty;
                AddContactDisplayNameTextBox.Text = string.Empty;
                windowState.SelectedContactRevision = null;
                windowState.SelectedContactId = existingMatch.ContactId;
                ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
                RefreshContactsList(true, existingMatch.ContactId);
                RefreshSelectedContactCard();
                SaveUiState();
                UpdateControlStates();
                return;
            }

            ContactSnapshot contact = rustBridge.AddContactByIdentifier(accountId, petname, publicDisplayName);
            ContactsStatusTextBlock.Text = $"Контакт {contact.Petname} сохранён. Пока нет живой встречи, он отмечен как «Ещё не встречались».";
            AddContactIdentifierTextBox.Text = string.Empty;
            AddContactPetnameTextBox.Text = string.Empty;
            AddContactDisplayNameTextBox.Text = string.Empty;
            windowState.SelectedContactRevision = null;
            windowState.SelectedContactId = contact.ContactId;
            ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            SaveUiState();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactsStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnAddContactFromPayloadClicked(object sender, RoutedEventArgs e)
    {
        string payloadText = AddContactPayloadTextBox.Text.Trim();
        string petname = AddContactPetnameTextBox.Text.Trim();

        if (string.IsNullOrWhiteSpace(payloadText) || string.IsNullOrWhiteSpace(petname))
        {
            ContactsStatusTextBlock.Text = "Введите данные контакта и локальное имя.";
            return;
        }

        try
        {
            if (!TryParseContactVerificationPayload(payloadText, out ContactVerificationPayloadV1? verificationPayload, out string parseError))
            {
                ContactsStatusTextBlock.Text = parseError;
                return;
            }

            ImportableContactPayloadV1 payload = new(
                verificationPayload!.Version,
                verificationPayload.Account,
                verificationPayload.Devices,
                verificationPayload.Fingerprint,
                verificationPayload.SafetyNumber,
                verificationPayload.ExportedAtUnixMs);
            ContactSnapshot? existingContact = TryFindExistingContactByRemoteAccountId(payload.Account.AccountId);
            if (existingContact is not null)
            {
                ContactSnapshot existingMatch = AttachCurrentDialogToContactIfAvailable(existingContact, out string? existingAttachStatus);
                string existingStatusText = $"Контакт {existingMatch.Petname} уже есть в списке.";
                if (!string.IsNullOrWhiteSpace(existingAttachStatus))
                {
                    existingStatusText = $"{existingStatusText}\n{existingAttachStatus}";
                }

                ContactsStatusTextBlock.Text = existingStatusText;
                AddContactPayloadTextBox.Text = string.Empty;
                AddContactPetnameTextBox.Text = string.Empty;
                windowState.SelectedContactRevision = null;
                windowState.SelectedContactId = existingMatch.ContactId;
                ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
                RefreshContactsList(true, existingMatch.ContactId);
                RefreshSelectedContactCard();
                RefreshChatsList(true);
                UpdateControlStates();
                return;
            }

            ContactSnapshot contact = rustBridge.AddContactFromPayload(payload, petname);
            string statusText = $"Контакт {contact.Petname} импортирован в список контактов. Подтвердите его вручную, если хотите закрепить проверку.";

            contact = AttachCurrentDialogToContactIfAvailable(contact, out string? attachStatus);
            if (!string.IsNullOrWhiteSpace(attachStatus))
            {
                statusText = $"{statusText}\n{attachStatus}";
            }

            ContactsStatusTextBlock.Text = statusText;
            AddContactPayloadTextBox.Text = string.Empty;
            AddContactPetnameTextBox.Text = string.Empty;
            windowState.SelectedContactRevision = null;
            windowState.SelectedContactId = contact.ContactId;
            ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            RefreshChatsList(true);
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactsStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnShowLocalPrekeyBundleClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            PrekeyBundleV1 bundle = rustBridge.GetLocalPrekeyBundle();
            LocalPrekeyBundleTextBox.Text = JsonSerializer.Serialize(
                bundle,
                OmniJsonSerializerContext.Default.PrekeyBundleV1);
            string remoteStatus = currentChatSecuritySummary?.HasRemotePrekeyBundle == true
                ? "Статус: ключи уже согласованы."
                : "Статус: обмен ключами выполняется автоматически.";
            PrekeyBundleStatusTextBlock.Text = $"Ваш пакет готов к отправке. {remoteStatus}";
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            PrekeyBundleStatusTextBlock.Text = NormalizeUserError(ex);
        }
    }

    private void OnRemotePrekeyBundleTextChanged(object sender, TextChangedEventArgs e)
        => UpdateControlStates();

    private void OnImportRemotePrekeyBundleClicked(object sender, RoutedEventArgs e)
    {
        string payloadText = RemotePrekeyBundleTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(payloadText))
        {
            PrekeyBundleStatusTextBlock.Text = "Для передачи ключей используйте автоматический статус соединения.";
            return;
        }

        try
        {
            PrekeyBundleV1? bundle = JsonSerializer.Deserialize(payloadText, OmniJsonSerializerContext.Default.PrekeyBundleV1);
            if (bundle is null)
            {
                PrekeyBundleStatusTextBlock.Text = "Не удалось разобрать пакет.";
                return;
            }

            rustBridge.StoreRemotePrekeyBundle(bundle);
            PrekeyBundleStatusTextBlock.Text = "Защищённая связь готова — можно писать.";
            RefreshChatHeaderSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            PrekeyBundleStatusTextBlock.Text = NormalizeUserError(ex);
        }
    }

    private void OnContactsSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressContactSelectionSync || sender is not ListView listView)
        {
            return;
        }

        if (listView.SelectedItem is not ContactListItem item)
        {
            if (!HasAnySelectedContact())
            {
                windowState.SelectedContactRevision = null;
                windowState.SelectedContactId = null;
                selectedContactCard = null;
                RenderSelectedContactCard();
                UpdateControlStates();
            }
            return;
        }

        // A contact picked while adding: its page takes the main pane back.
        if (currentContactsWorkspaceMode == ContactsWorkspaceMode.Add)
        {
            ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
        }

        SelectContactInFilteredList(item.ContactId);
        RefreshSelectedContactCard();
        WakeSelectedContactRefresh();
        SaveUiState();
        UpdateControlStates();
    }

    private void OnContactsAddToggleClicked(object sender, RoutedEventArgs e)
        => ApplyContactsWorkspaceMode(currentContactsWorkspaceMode == ContactsWorkspaceMode.Add
            ? ContactsWorkspaceMode.Directory
            : ContactsWorkspaceMode.Add);

    private void OnContactsFilterModeClicked(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: string tag })
        {
            return;
        }

        ContactsFilterMode? mode = tag switch
        {
            "all" => ContactsFilterMode.All,
            "pending" => ContactsFilterMode.Pending,
            "approved" => ContactsFilterMode.Approved,
            "declined" => ContactsFilterMode.Declined,
            _ => null,
        };

        if (mode is ContactsFilterMode resolvedMode)
        {
            ApplyContactsFilterMode(resolvedMode);
        }
    }

    private void OnRenameContactClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        string petname = RenameContactTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(petname))
        {
            ContactManagementStatusTextBlock.Text = "Введите новое имя.";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.RenameContact(windowState.SelectedContactId, petname);
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = $"Контакт переименован: {contact.Petname}.";
            RefreshContactsList(true, contact.ContactId);
            RefreshChatsList(true, windowState.CurrentPeerKey);
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnContactActionsClicked(object sender, RoutedEventArgs e)
    {
        if (selectedContactCard is null || sender is not FrameworkElement element)
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        contactActionsFlyout.Items.Clear();

        string relationshipState = selectedContactCard.Contact.RelationshipState?.Trim().ToLowerInvariant() ?? "approved";
        string pendingRequestDirection = selectedContactCard.Contact.PendingRequestDirection?.Trim().ToLowerInvariant() ?? string.Empty;
        bool isIncomingPendingRequest = relationshipState == "pending" && pendingRequestDirection == "incoming";
        if (relationshipState == "declined" || isIncomingPendingRequest)
        {
            MenuFlyoutItem approveItem = CreateContactActionMenuItem("Принять", "ContactActionApproveMenuItem");
            approveItem.Click += OnApproveContactClicked;
            contactActionsFlyout.Items.Add(approveItem);
        }

        if (relationshipState == "approved" || isIncomingPendingRequest)
        {
            MenuFlyoutItem declineItem = CreateContactActionMenuItem("Отклонить", "ContactActionDeclineMenuItem");
            declineItem.Click += OnDeclineContactClicked;
            contactActionsFlyout.Items.Add(declineItem);
        }

        if (relationshipState == "approved")
        {
            // Friend-proxy (Axis 2): per-contact consent to carry THIS friend's mesh traffic
            // through our node. Off by default, revocable. See friend-proxy-by-consent.md.
            MenuFlyoutItem relayConsentItem = CreateContactActionMenuItem(
                selectedContactCard.Contact.FriendRelayConsent
                    ? "Запретить маршрут через меня"
                    : "Разрешить маршрут через меня",
                selectedContactCard.Contact.FriendRelayConsent
                    ? "ContactActionRevokeFriendRelayMenuItem"
                    : "ContactActionAllowFriendRelayMenuItem");
            relayConsentItem.Click += selectedContactCard.Contact.FriendRelayConsent
                ? OnRevokeFriendRelayConsentClicked
                : OnAllowFriendRelayConsentClicked;
            contactActionsFlyout.Items.Add(relayConsentItem);
        }

        MenuFlyoutItem forgetItem = CreateContactActionMenuItem("Забыть", "ContactActionForgetMenuItem");
        forgetItem.Click += OnForgetContactClicked;
        contactActionsFlyout.Items.Add(forgetItem);

        contactActionsFlyout.ShowAt(element);
    }

    private void OnApproveContactClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.ApproveContact(windowState.SelectedContactId);
            ContactChatTarget? chatTarget = null;
            try
            {
                chatTarget = rustBridge.StartChatFromContact(contact.ContactId);
                OpenChatThread(chatTarget.DialogKey, chatTarget.ContactId);
            }
            catch
            {
                // Approval can race the transport projection; the contact refresh below will catch up.
            }
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = $"Контакт {contact.Petname} принят.";
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            RefreshChatsList(true, chatTarget?.DialogKey ?? windowState.CurrentPeerKey);
            RefreshChatHeaderSummary();
            if (chatTarget is not null)
            {
                AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                windowState.CurrentSection = AuthenticatedShellSection.Chats;
                ChatsSectionRoot.Visibility = Visibility.Visible;
                ContactsSectionRoot.Visibility = Visibility.Collapsed;
                SettingsSectionRoot.Visibility = Visibility.Collapsed;
                DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
            }
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnAllowFriendRelayConsentClicked(object sender, RoutedEventArgs e)
        => SetFriendRelayConsentForSelectedContact(true);

    private void OnRevokeFriendRelayConsentClicked(object sender, RoutedEventArgs e)
        => SetFriendRelayConsentForSelectedContact(false);

    private void SetFriendRelayConsentForSelectedContact(bool consent)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.SetContactFriendRelayConsent(windowState.SelectedContactId, consent);
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = consent
                ? $"Контакту {contact.Petname} разрешён маршрут через этот узел."
                : $"Контакту {contact.Petname} запрещён маршрут через этот узел.";
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnDeclineContactClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.DeclineContact(windowState.SelectedContactId);
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = $"Контакт {contact.Petname} отклонён.";
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            RefreshChatsList(true, windowState.CurrentPeerKey);
            RefreshChatHeaderSummary();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private async void OnForgetContactClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        if (!await ConfirmForgetContactAsync(selectedContactCard?.Contact.Petname))
        {
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.ForgetContact(windowState.SelectedContactId);
            CloseChatContactInfo();
            if (IsActiveChatForContact(contact))
            {
                CloseCurrentChatSelection();
            }

            windowState.SelectedContactRevision = null;
            windowState.SelectedContactId = null;
            selectedContactCard = null;
            ContactManagementStatusTextBlock.Text = $"Контакт {contact.Petname} удалён из памяти.";
            RefreshContactsList(true);
            RefreshSelectedContactCard();
            RefreshChatHeaderSummary();
            RefreshChatsList(true);
            RefreshMessages(force: true);
            RefreshQueueDiagnostics();
            UpdateCurrentPeerText();
            UpdateControlStates();
            SaveUiState();
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    /// «Забыть» deletes the contact and the whole chat with it, here and on our other devices; it
    /// went at one click from the chat's «⋯» and from the card (seen 2026-10-11).
    private async Task<bool> ConfirmForgetContactAsync(string? name)
    {
        string who = string.IsNullOrWhiteSpace(name) ? "контакт" : $"«{name}»";
        ContentDialog dialog = new()
        {
            Title = $"Забыть {who}?",
            Content = new TextBlock
            {
                Text = "Контакт и вся переписка с ним удалятся с этого устройства и с других ваших устройств. "
                    + "Чтобы снова переписываться, нужно будет заново добавить друг друга.",
                TextWrapping = TextWrapping.WrapWholeWords,
            },
            PrimaryButtonText = "Забыть и удалить переписку",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Close,
            XamlRoot = Content.XamlRoot,
        };
        try
        {
            return await dialog.ShowAsync() == ContentDialogResult.Primary;
        }
        catch (Exception)
        {
            // Another dialog is already open (WinUI allows one at a time): do nothing rather than delete.
            return false;
        }
    }

    private void OnBlockContactClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.BlockContact(windowState.SelectedContactId);
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = $"Контакт {contact.Petname} заблокирован.";
            RefreshContactsList(true, contact.ContactId);
            RefreshChatsList(true, windowState.CurrentPeerKey);
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnOpenContactChatClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        try
        {
            ContactChatTarget target = rustBridge.StartChatFromContact(windowState.SelectedContactId);
            OpenChatThread(target.DialogKey, target.ContactId);
            RefreshChatsList(true, target.DialogKey);
            AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
            windowState.CurrentSection = AuthenticatedShellSection.Chats;
            ChatsSectionRoot.Visibility = Visibility.Visible;
            ContactsSectionRoot.Visibility = Visibility.Collapsed;
            SettingsSectionRoot.Visibility = Visibility.Collapsed;
            DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
            SetActionStatus($"Диалог для контакта {target.Title} открыт.");
        }
        catch (Exception ex)
        {
            string rawErrorText = ex.Message;
            string errorText = NormalizeUserError(ex);
            string? currentDialogKey = GetAttachableCurrentDialogKey();
            if (!string.IsNullOrWhiteSpace(currentDialogKey))
            {
                try
                {
                    ContactSnapshot contact = rustBridge.AttachDialogToContact(
                        windowState.SelectedContactId,
                        currentDialogKey);
                    ContactChatTarget target = rustBridge.StartChatFromContact(contact.ContactId);
                    OpenChatThread(target.DialogKey, target.ContactId);
                    RefreshChatsList(true, target.DialogKey);
                    AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                    windowState.CurrentSection = AuthenticatedShellSection.Chats;
                    ChatsSectionRoot.Visibility = Visibility.Visible;
                    ContactsSectionRoot.Visibility = Visibility.Collapsed;
                    SettingsSectionRoot.Visibility = Visibility.Collapsed;
                    DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
                    SetActionStatus($"Диалог для контакта {target.Title} привязан и открыт.");
                    return;
                }
                catch (Exception attachEx)
                {
                    ContactManagementStatusTextBlock.Text = $"Не удалось привязать диалог к контакту: {NormalizeUserError(attachEx)}";
                    return;
                }
            }

            ContactManagementStatusTextBlock.Text = string.IsNullOrWhiteSpace(rawErrorText) ? errorText : rawErrorText;
        }
    }

    private void OnConfirmContactVerificationClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.SelectedContactId))
        {
            ContactManagementStatusTextBlock.Text = "Сначала выберите контакт.";
            return;
        }

        string payloadText = VerifyContactPayloadTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(payloadText))
        {
            ContactManagementStatusTextBlock.Text = "Вставьте данные для проверки контакта.";
            return;
        }

        try
        {
            if (!TryParseContactVerificationPayload(payloadText, out ContactVerificationPayloadV1? payload, out string parseError))
            {
                ContactManagementStatusTextBlock.Text = parseError;
                return;
            }

            selectedContactCard = rustBridge.ConfirmContactVerification(windowState.SelectedContactId, payload!, "manual_compare");
            windowState.SelectedContactRevision = null;
            ContactManagementStatusTextBlock.Text = "Ключи совпали — контакт сверен.";
            VerifyContactPayloadTextBox.Text = string.Empty;
            ManualVerificationPanel.Visibility = Visibility.Collapsed;
            RenderSelectedContactCard();
            RefreshChatHeaderSummary();
            RefreshMessages(force: true);
            RefreshContactsList(true, windowState.SelectedContactId);
            RefreshChatsList(true, windowState.CurrentPeerKey);

            try
            {
                ContactChatTarget target = rustBridge.StartChatFromContact(windowState.SelectedContactId);
                OpenChatThread(target.DialogKey, target.ContactId);
                RefreshChatsList(true, target.DialogKey);
                AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                windowState.CurrentSection = AuthenticatedShellSection.Chats;
                ChatsSectionRoot.Visibility = Visibility.Visible;
                ContactsSectionRoot.Visibility = Visibility.Collapsed;
                SettingsSectionRoot.Visibility = Visibility.Collapsed;
                DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
            }
                catch (Exception attachEx)
                {
                    string errorText = NormalizeUserError(attachEx);
                    string? currentDialogKey = GetAttachableCurrentDialogKey();
                    if (!string.IsNullOrWhiteSpace(currentDialogKey) &&
                        errorText.Contains("not linked to a dialog yet", StringComparison.OrdinalIgnoreCase))
                    {
                    try
                    {
                        ContactSnapshot contact = rustBridge.AttachDialogToContact(
                            windowState.SelectedContactId,
                            currentDialogKey);
                        ContactChatTarget target = rustBridge.StartChatFromContact(contact.ContactId);
                        OpenChatThread(target.DialogKey, target.ContactId);
                        RefreshChatsList(true, target.DialogKey);
                        AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                        windowState.CurrentSection = AuthenticatedShellSection.Chats;
                        ChatsSectionRoot.Visibility = Visibility.Visible;
                        ContactsSectionRoot.Visibility = Visibility.Collapsed;
                        SettingsSectionRoot.Visibility = Visibility.Collapsed;
                        DiagnosticsSectionRoot.Visibility = Visibility.Collapsed;
                        SetActionStatus($"Диалог для контакта {target.Title} привязан и открыт.");
                        return;
                    }
                    catch (Exception retryEx)
                    {
                        ContactManagementStatusTextBlock.Text =
                            $"Не удалось привязать диалог к контакту: {NormalizeUserError(retryEx)}";
                        return;
                    }
                }

                ContactManagementStatusTextBlock.Text = $"Не удалось открыть чат после проверки: {errorText}";
            }
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = ex.Message;
        }
    }

    private void OnQuickAddContactClicked(object sender, RoutedEventArgs e)
    {
        string? currentDialogKey = GetAttachableCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(currentDialogKey))
        {
            ContactsStatusTextBlock.Text = "Сначала подключитесь к узлу или выберите диалог.";
            return;
        }

        string petname = QuickAddContactPetnameTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(petname))
        {
            ContactsStatusTextBlock.Text = "Как назвать контакт?";
            return;
        }

        try
        {
            ContactSnapshot contact = rustBridge.QuickAddContactFromDialog(currentDialogKey, petname);
            QuickAddContactPetnameTextBox.Text = string.Empty;
            ContactsStatusTextBlock.Text = $"Контакт {contact.Petname} сохранён в списке контактов.";
            windowState.SelectedContactRevision = null;
            windowState.SelectedContactId = contact.ContactId;
            ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
            RefreshContactsList(true, contact.ContactId);
            RefreshSelectedContactCard();
            RefreshChatsList(true);
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            ContactsStatusTextBlock.Text = ex.Message;
        }
    }

}
