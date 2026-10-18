using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.Features.Profiles;
using Omni.Windows.App.ViewModels;
using Windows.Storage;
using Windows.Storage.Pickers;
using WinRT.Interop;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void PopulateSavedProfiles(SavedProfileList savedProfiles)
    {
        savedProfileItems.Clear();
        foreach (LocalProfileSummary profile in savedProfiles.Profiles)
        {
            savedProfileItems.Add(new SavedProfileListItem(
                profile.ProfileId,
                profile.DisplayName,
                profile.AccountId,
                profile.State,
                MapProfileState(profile.State)));
        }

        if (!string.IsNullOrWhiteSpace(windowState.Onboarding.SelectedProfileId))
        {
            SavedProfilesListView.SelectedItem = savedProfileItems.FirstOrDefault(
                item => item.ProfileId == windowState.Onboarding.SelectedProfileId);
        }

        // The only profile here is the one to open: «Войти» stayed grey until its card was clicked.
        if (SavedProfilesListView.SelectedItem is null && savedProfileItems.Count == 1)
        {
            SavedProfilesListView.SelectedItem = savedProfileItems[0];
        }
    }

    private static string MapProfileState(string state)
        => state.ToLowerInvariant() switch
        {
            "signed_in" => "\u0410\u043a\u0442\u0438\u0432\u0435\u043d",
            "signed_out_locked" => "\u0417\u0430\u0431\u043b\u043e\u043a\u0438\u0440\u043e\u0432\u0430\u043d",
            "deleting" => "\u0423\u0434\u0430\u043b\u044f\u0435\u0442\u0441\u044f",
            "deleted" => "\u0423\u0434\u0430\u043b\u0451\u043d",
            _ => state,
        };

    private string? GetCurrentProfileId()
        => windowState.Onboarding.ActiveProfileId;

    private string BuildCurrentProfileSummaryText()
    {
        // Data this profile holds \u2014 a privacy-relevant overview, not the internal lifecycle dump
        // it used to be (profile/account id duplicated the identity summary above; lock state is
        // implied by being here signed-in). Footprint counts only.
        return currentProfileFootprintSummary is null
            ? string.Empty
            : $"Здесь контактов: {currentProfileFootprintSummary.ContactsCount}, сообщений: {currentProfileFootprintSummary.MessagesCount}.";
    }

    private void RefreshCurrentLifecycleContracts(bool includeFootprint = false)
    {
        LifecycleRefreshPayload lifecycle = FetchLifecycleRefreshPayload(false);
        ApplyLifecycleRefreshPayload(lifecycle);

        if (includeFootprint || ShouldRefreshCurrentProfileFootprintImmediately())
        {
            ApplyFootprintRefreshPayload(FetchFootprintRefreshPayload(lifecycle.ActiveProfileId));
        }
    }

    private bool ShouldRefreshCurrentProfileFootprintImmediately()
        => windowState.Onboarding.ShellMode == AppShellMode.Authenticated &&
           windowState.CurrentSection is AuthenticatedShellSection.Settings or
               AuthenticatedShellSection.Diagnostics;

    private void RefreshCurrentProfileFootprintIfVisible()
    {
        if (!ShouldRefreshCurrentProfileFootprintImmediately())
        {
            return;
        }

        ApplyFootprintRefreshPayload(FetchFootprintRefreshPayload(GetCurrentProfileId()));
    }

    private LifecycleRefreshPayload FetchLifecycleRefreshPayload(bool _includeFootprint)
    {
        try
        {
            LifecycleFastSnapshot snapshot = rustBridge.GetLifecycleFastSnapshot();
            return new LifecycleRefreshPayload(
                snapshot.ActiveProfileId,
                snapshot.LocalAccount,
                snapshot.LocalDevice,
                snapshot.GateSummary,
                snapshot.LifecycleSummary);
        }
        catch
        {
            return new LifecycleRefreshPayload(
                null,
                null,
                null,
                null,
                null);
        }
    }

    private void ApplyLifecycleRefreshPayload(LifecycleRefreshPayload payload)
    {
        windowState.Onboarding.ActiveProfileId = payload.ActiveProfileId;
        ShowLocalAccount(payload.LocalAccount);
        windowState.Onboarding.LocalDevice = payload.LocalDevice;
        currentProfileGateSummary = payload.GateSummary;
        currentProfileLifecycleSummary = payload.LifecycleSummary;
        UpdateIdentityHeader();
    }

    private FootprintRefreshPayload FetchFootprintRefreshPayload(string? profileId)
    {
        if (string.IsNullOrWhiteSpace(profileId))
        {
            return new FootprintRefreshPayload(null, null);
        }

        try
        {
            return new FootprintRefreshPayload(
                profileId,
                rustBridge.GetLocalProfileFootprintSummary(profileId));
        }
        catch
        {
            return new FootprintRefreshPayload(profileId, null);
        }
    }

    private void ApplyFootprintRefreshPayload(FootprintRefreshPayload payload)
    {
        if (!string.Equals(payload.ProfileId, GetCurrentProfileId(), StringComparison.Ordinal))
        {
            return;
        }

        currentProfileFootprintSummary = payload.FootprintSummary;
        UpdateIdentityHeader();
    }

    private void RefreshPasswordProtectionSummary()
    {
        ApplyPasswordProtectionRefreshPayload(FetchPasswordProtectionRefreshPayload(GetCurrentProfileId()));
    }

    private PasswordProtectionRefreshPayload FetchPasswordProtectionRefreshPayload(string? profileId)
    {
        if (string.IsNullOrWhiteSpace(profileId))
        {
            return new PasswordProtectionRefreshPayload(null, null, null);
        }

        try
        {
            return new PasswordProtectionRefreshPayload(
                profileId,
                rustBridge.GetPasswordProtectionSummary(profileId),
                null);
        }
        catch (Exception ex)
        {
            return new PasswordProtectionRefreshPayload(
                profileId,
                null,
                NormalizeUserError(ex));
        }
    }

    private void ApplyPasswordProtectionRefreshPayload(PasswordProtectionRefreshPayload payload)
    {
        currentPasswordProtectionSummary = payload.Summary;
        currentPasswordProtectionErrorText = payload.ErrorText;
        bool passwordSet = payload.Summary?.Enabled == true;
        SettingsSetPasswordPanel.Visibility = passwordSet ? Visibility.Collapsed : Visibility.Visible;
        SettingsChangePasswordPanel.Visibility = passwordSet ? Visibility.Visible : Visibility.Collapsed;

        if (string.IsNullOrWhiteSpace(payload.ProfileId))
        {
            currentPasswordProtectionSummary = null;
            currentPasswordProtectionErrorText = null;
            SettingsPasswordSummaryTextBlock.Text = "Пароль не задан.";
            RenderRecoverySummary();
            return;
        }

        if (!string.IsNullOrWhiteSpace(payload.ErrorText))
        {
            SettingsPasswordSummaryTextBlock.Text = MainWindowPresentation.BuildPasswordProtectionSummaryText(
                null,
                payload.ErrorText);
            RenderRecoverySummary();
            return;
        }

        if (payload.Summary is null || !payload.Summary.Enabled)
        {
            SettingsPasswordSummaryTextBlock.Text = MainWindowPresentation.BuildPasswordProtectionSummaryText(
                payload.Summary,
                null);
            RenderRecoverySummary();
            return;
        }

        SettingsPasswordSummaryTextBlock.Text = MainWindowPresentation.BuildPasswordProtectionSummaryText(
            payload.Summary,
            null);
        RenderRecoverySummary();
    }

    private void InitializePicker(object picker)
    {
        InitializeWithWindow.Initialize(picker, WindowNative.GetWindowHandle(this));
    }

    private void PopulateImportAccountScreen(string fileName, string? summaryText = null)
    {
        ImportAccountFileNameTextBlock.Text = fileName;
        ImportAccountFileSummaryTextBlock.Text = summaryText ?? "Резервная копия выбрана. Введите фразу восстановления для проверки.";
        ImportAccountMnemonicTextBox.Text = string.Empty;
        ImportAccountStatusTextBlock.Text = "Резервная копия выбрана. Введите фразу восстановления.";
    }

    private static string BuildDatabaseBackupSummaryText(AccountExportManifestV1 manifest)
    {
        return
            $"Профиль: {manifest.DisplayName}\n" +
            $"ID аккаунта: {manifest.AccountId}\n" +
            $"ID профиля: {manifest.ProfileId}\n" +
            $"Название устройства: {manifest.DeviceLabel}\n" +
            $"Дата резервной копии: {DateTimeOffset.FromUnixTimeMilliseconds(manifest.ExportedAtUnixMs):dd.MM.yyyy HH:mm:ss}";
    }

    private static string BuildDatabaseBackupFileName(AccountExportManifestV1 manifest)
    {
        string displayName = SanitizeFileName(manifest.DisplayName);
        string accountId = SanitizeFileName(manifest.AccountId);
        string timestamp = DateTimeOffset.Now.ToString("yyyyMMdd-HHmmss");
        return $"omni-backup-{displayName}-{accountId}-{timestamp}.omnibackup";
    }

    private void ClearPendingImportAccountSelection(bool keepStatus = false)
    {
        windowState.Onboarding.PendingImportFileName = null;
        windowState.Onboarding.PendingImportFilePath = null;
        backupImportSession.Clear();
        ImportAccountFileNameTextBlock.Text = "Файл не выбран.";
        ImportAccountFileSummaryTextBlock.Text = string.Empty;
        ImportAccountMnemonicTextBox.Text = string.Empty;
        if (!keepStatus)
        {
            ImportAccountStatusTextBlock.Text = "Выберите файл копии и введите фразу восстановления.";
        }
        UpdateControlStates();
    }

    private void QueueDangerousActionPreview(DangerousActionPreview preview)
    {
        // The confirmation dialog shows the preview; the card no longer repeats it.
        pendingDangerousActionPreview = preview;
        UpdateControlStates();
    }

    private void ClearDangerousActionPreview()
    {
        pendingDangerousActionPreview = null;
        UpdateControlStates();
    }

    private void SuspendAuthenticatedRefresh()
    {
        StopAuthenticatedRefreshLoop();
    }

    private void ResumeAuthenticatedRefreshIfNeeded(bool shouldResume)
    {
        if (shouldResume &&
            windowState.Onboarding.ShellMode == AppShellMode.Authenticated &&
            !IsAuthenticatedRefreshLoopRunning())
        {
            StartAuthenticatedRefreshLoop();
        }
    }

    private bool IsDeletedProfileExitComplete(string profileId)
    {
        try
        {
            return rustBridge.GetLocalProfileSummary(profileId) is null;
        }
        catch
        {
            return false;
        }
    }

    private bool IsSignedOutProfileExitComplete(string profileId)
    {
        try
        {
            LocalProfileSummary? summary = rustBridge.GetLocalProfileSummary(profileId);
            if (summary is null)
            {
                return true;
            }

            return rustBridge.GetActiveProfileId() is null &&
                summary.State.Equals("signed_out_locked", StringComparison.OrdinalIgnoreCase);
        }
        catch
        {
            return false;
        }
    }

    private async Task<(bool Confirmed, string? ConfirmationToken)> ConfirmDangerousActionAsync(
        DangerousActionPreview preview,
        string primaryButtonText)
    {
        if (!preview.SupportedNow)
        {
            SettingsStatusTextBlock.Text = preview.UnsupportedReason ?? "Действие сейчас недоступно.";
            return (false, null);
        }

        string consequenceText = preview.Consequences.Count == 0
            ? string.Empty
            : "\n\n" + string.Join("\n", preview.Consequences.Select(value => $"• {value}"));
        StackPanel dialogContent = new()
        {
            Spacing = 12,
        };
        dialogContent.Children.Add(new TextBlock
        {
            Text = $"{preview.Summary}{consequenceText}",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        if (preview.RequiresConfirmation)
        {
            dialogContent.Children.Add(new TextBlock
            {
                Text = $"Нажатие кнопки «{primaryButtonText}» подтвердит это действие.",
                TextWrapping = TextWrapping.WrapWholeWords,
            });
        }
        ContentDialog dialog = new()
        {
            Title = preview.Title,
            Content = dialogContent,
            PrimaryButtonText = primaryButtonText,
            CloseButtonText = "Отмена",
            XamlRoot = Content.XamlRoot,
        };

        bool confirmed = await dialog.ShowAsync() == ContentDialogResult.Primary;
        return (
            confirmed,
            confirmed && preview.RequiresConfirmation
                ? preview.ConfirmationToken
                : null);
    }

    private void ReleaseActiveProfileAvatarResourcesForDelete()
    {
        DeletePendingPublicAvatarSourceFile();
        pendingPublicAvatarSourcePath = null;
        pendingPublicAvatarDisplayName = null;
        pendingPublicAvatarClearRequested = false;
        IdentityAvatarImage.Source = null;
        SettingsProfileAvatarImage.Source = null;
        avatarImageCache.Clear();
    }

    private void ReleaseProfileBoundShellResourcesForDelete()
    {
        CancelTranscriptLoads();
        chatRefreshService.ResetAllDialogState();
        ClearCurrentChatMessageCache();
        messageItems.Clear();
        chatThreadItems.Clear();
        contactItems.Clear();
        filteredContactItems.Clear();
        pendingContactItems.Clear();
        approvedContactItems.Clear();
        declinedContactItems.Clear();
        CloseGroupConversation(clearSelection: true);
        ChatsListView.SelectedItem = null;
        FilteredContactsListView.SelectedItem = null;
        selectedContactCard = null;
        currentChatHeaderSummary = null;
        currentChatSecuritySummary = null;
        currentChatSessionSummary = null;
        lastTransportSnapshot = null;
        lastQueueDiagnosticsSnapshot = null;
        windowState.LastRenderedChatSignature = string.Empty;
        windowState.LastRenderedContactSignature = string.Empty;
        windowState.LastObservedContactRefreshSignature = string.Empty;
        lastRenderedPeerSignature = string.Empty;
        lastRenderedDiscoveryNetworkSettingsSignature = string.Empty;
        pendingChatAutoScrollToLatest = false;
    }

    private void RestoreActiveProfileVisualsAfterDeleteFailure()
    {
        try
        {
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
            windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
            RefreshCurrentLifecycleContracts(includeFootprint: true);
            UpdateIdentityHeader();
            RenderIdentityAvatar();
            RenderLocalPublicProfileEditor(preserveDraft: false);
        }
        catch
        {
            // Best-effort recovery only. The original delete error is more important than a stale avatar preview.
        }
    }

    private void ReturnToInitialShellAfterProfileExit(string statusText)
    {
        StopAuthenticatedRefreshLoop();
        TryStopMeshOverlay();
        ReleaseProfileBoundShellResourcesForDelete();
        windowState.CurrentPeerKey = null;
        windowState.CurrentTransportPeerKey = null;
        windowState.SelectedContactId = null;
        windowState.Onboarding.ActiveProfileId = null;
        windowState.Onboarding.SelectedProfileId = null;
        windowState.Onboarding.PendingRecoveryPhraseV2 = null;
        windowState.Onboarding.LocalAccount = null;
        windowState.Onboarding.LocalDevice = null;
        currentProfileGateSummary = null;
        currentProfileLifecycleSummary = null;
        currentProfileFootprintSummary = null;
        currentPasswordProtectionSummary = null;
        currentPasswordProtectionErrorText = null;
        savedProfileItems.Clear();
        SavedProfilesListView.SelectedItem = null;
        ClearDangerousActionPreview();
        ReleaseActiveProfileAvatarResourcesForDelete();

        SavedProfileList savedProfiles = rustBridge.ListLocalProfiles();
        PopulateSavedProfiles(savedProfiles);
        windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
        windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
        RefreshCurrentLifecycleContracts();
        windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
        UpdateIdentityHeader();

        bool hasSavedProfiles = savedProfiles.Profiles.Count > 0;
        bool hasSignedInProfile = currentProfileLifecycleSummary?.IsUnlocked == true;
        if (!hasSavedProfiles)
        {
            ShowOnboardingStep(OnboardingStep.Welcome, statusText);
            return;
        }

        if (!hasSignedInProfile)
        {
            ShowOnboardingStep(OnboardingStep.ProfileChooser, statusText);
            return;
        }

        EnterAuthenticatedShell(statusText);
    }

    private async void OnSettingsSignOutClicked(object sender, RoutedEventArgs e)
    {
        string? profileId = GetCurrentProfileId();
        if (string.IsNullOrWhiteSpace(profileId))
        {
            SettingsStatusTextBlock.Text = "Не удалось определить активный профиль для выхода.";
            return;
        }

        bool shouldResumeRefresh = IsAuthenticatedRefreshLoopRunning();
        SuspendAuthenticatedRefresh();
        bool completed = false;

        try
        {
            DangerousActionPreview? preview = await Task.Run(() => rustBridge.PreviewSignOutProfile(profileId));
            if (preview is null)
            {
                SettingsStatusTextBlock.Text = "Не удалось подготовить подтверждение выхода.";
                return;
            }

            QueueDangerousActionPreview(preview);
            (bool confirmed, string? confirmationToken) =
                await ConfirmDangerousActionAsync(preview, "Выйти");
            if (!confirmed)
            {
                SettingsStatusTextBlock.Text = "Выход отменён.";
                return;
            }
            if (preview.RequiresConfirmation && string.IsNullOrWhiteSpace(confirmationToken))
            {
                SettingsStatusTextBlock.Text = "Не удалось подготовить подтверждение для выхода.";
                return;
            }

            try
            {
                await Task.Run(() => rustBridge.ConfirmSignOutProfile(profileId, confirmationToken));
                try
                {
                    ReturnToInitialShellAfterProfileExit("Вы вышли из профиля.");
                }
                catch
                {
                    await BootAsync();
                    OnboardingStatusTextBlock.Text = "Вы вышли из профиля.";
                }
                completed = true;
            }
            catch when (IsSignedOutProfileExitComplete(profileId))
            {
                try
                {
                    ReturnToInitialShellAfterProfileExit("Вы вышли из профиля.");
                }
                catch
                {
                    await BootAsync();
                    OnboardingStatusTextBlock.Text = "Вы вышли из профиля.";
                }
                completed = true;
            }
        }
        catch (Exception ex)
        {
            SettingsStatusTextBlock.Text = $"Не удалось подготовить выход: {NormalizeUserError(ex)}";
        }
        finally
        {
            ResumeAuthenticatedRefreshIfNeeded(shouldResumeRefresh && !completed);
            UpdateControlStates();
        }
    }

    private async void OnSettingsDeleteLocalProfileClicked(object sender, RoutedEventArgs e)
    {
        string? profileId = GetCurrentProfileId();
        if (string.IsNullOrWhiteSpace(profileId))
        {
            SettingsStatusTextBlock.Text = "Не удалось определить активный профиль для удаления.";
            return;
        }

        bool shouldResumeRefresh = IsAuthenticatedRefreshLoopRunning();
        SuspendAuthenticatedRefresh();
        bool completed = false;
        bool profileDeleted = false;
        bool avatarResourcesReleased = false;

        try
        {
            DangerousActionPreview? preview = await Task.Run(() => rustBridge.PreviewDeleteLocalProfile(profileId));
            if (preview is null)
            {
                SettingsStatusTextBlock.Text = "Не удалось подготовить подтверждение удаления.";
                return;
            }

            QueueDangerousActionPreview(preview);
            (bool confirmed, string? confirmationToken) =
                await ConfirmDangerousActionAsync(preview, "Удалить с устройства");
            if (!confirmed)
            {
                SettingsStatusTextBlock.Text = "Удаление отменено.";
                return;
            }
            if (preview.RequiresConfirmation && string.IsNullOrWhiteSpace(confirmationToken))
            {
                SettingsStatusTextBlock.Text = "Не удалось подготовить подтверждение для удаления.";
                return;
            }

            try
            {
                ReleaseProfileBoundShellResourcesForDelete();
                ReleaseActiveProfileAvatarResourcesForDelete();
                avatarResourcesReleased = true;
                await Task.Run(() => rustBridge.ConfirmDeleteLocalProfile(profileId, confirmationToken));
                profileDeleted = true;
                try
                {
                    ReturnToInitialShellAfterProfileExit("Профиль удалён с этого компьютера.");
                }
                catch
                {
                    await BootAsync();
                    OnboardingStatusTextBlock.Text = "Профиль удалён с этого компьютера.";
                }
                completed = true;
            }
            catch when (IsDeletedProfileExitComplete(profileId))
            {
                profileDeleted = true;
                try
                {
                    ReturnToInitialShellAfterProfileExit("Профиль удалён с этого компьютера.");
                }
                catch
                {
                    await BootAsync();
                    OnboardingStatusTextBlock.Text = "Профиль удалён с этого компьютера.";
                }
                completed = true;
            }
        }
        catch (Exception ex)
        {
            if (avatarResourcesReleased && !profileDeleted)
            {
                RestoreActiveProfileVisualsAfterDeleteFailure();
            }
            SettingsStatusTextBlock.Text = $"Не удалось удалить профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            ResumeAuthenticatedRefreshIfNeeded(shouldResumeRefresh && !completed);
            UpdateControlStates();
        }
    }

    private async void OnSettingsSetPasswordClicked(object sender, RoutedEventArgs e)
    {
        string? profileId = GetCurrentProfileId();
        if (string.IsNullOrWhiteSpace(profileId))
        {
            SettingsStatusTextBlock.Text = "\u041d\u0435\u043b\u044c\u0437\u044f \u0443\u0441\u0442\u0430\u043d\u043e\u0432\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c \u0431\u0435\u0437 \u0430\u043a\u0442\u0438\u0432\u043d\u043e\u0433\u043e \u043f\u0440\u043e\u0444\u0438\u043b\u044f.";
            return;
        }

        string password = SettingsNewPasswordBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(password))
        {
            SettingsStatusTextBlock.Text = "Введите пароль.";
            return;
        }

        try
        {
            await Task.Run(() => rustBridge.SetLocalProfilePassword(profileId, password));
            SettingsNewPasswordBox.Password = string.Empty;
            RefreshPasswordProtectionSummary();
            SettingsStatusTextBlock.Text = "Пароль задан.";
        }
        catch (Exception ex)
        {
            SettingsStatusTextBlock.Text = $"\u041d\u0435 \u0443\u0434\u0430\u043b\u043e\u0441\u044c \u0443\u0441\u0442\u0430\u043d\u043e\u0432\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c: {NormalizeUserError(ex)}";
        }
    }

    private async void OnSettingsChangePasswordClicked(object sender, RoutedEventArgs e)
    {
        string? profileId = GetCurrentProfileId();
        if (string.IsNullOrWhiteSpace(profileId))
        {
            SettingsStatusTextBlock.Text = "\u041d\u0435\u043b\u044c\u0437\u044f \u0438\u0437\u043c\u0435\u043d\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c \u0431\u0435\u0437 \u0430\u043a\u0442\u0438\u0432\u043d\u043e\u0433\u043e \u043f\u0440\u043e\u0444\u0438\u043b\u044f.";
            return;
        }

        string currentPassword = SettingsCurrentPasswordBox.Password.Trim();
        string newPassword = SettingsChangePasswordBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(currentPassword) || string.IsNullOrWhiteSpace(newPassword))
        {
            SettingsStatusTextBlock.Text = "\u0412\u0432\u0435\u0434\u0438\u0442\u0435 \u0442\u0435\u043a\u0443\u0449\u0438\u0439 \u0438 \u043d\u043e\u0432\u044b\u0439 \u043f\u0430\u0440\u043e\u043b\u044c.";
            return;
        }

        try
        {
            await Task.Run(() => rustBridge.ChangeLocalProfilePassword(profileId, currentPassword, newPassword));
            SettingsCurrentPasswordBox.Password = string.Empty;
            SettingsChangePasswordBox.Password = string.Empty;
            RefreshPasswordProtectionSummary();
            SettingsStatusTextBlock.Text = "Пароль изменён.";
        }
        catch (Exception ex)
        {
            SettingsStatusTextBlock.Text = $"\u041d\u0435 \u0443\u0434\u0430\u043b\u043e\u0441\u044c \u0438\u0437\u043c\u0435\u043d\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c: {NormalizeUserError(ex)}";
        }
    }

    private async void OnSettingsRemovePasswordClicked(object sender, RoutedEventArgs e)
    {
        string? profileId = GetCurrentProfileId();
        if (string.IsNullOrWhiteSpace(profileId))
        {
            SettingsStatusTextBlock.Text = "\u041d\u0435\u043b\u044c\u0437\u044f \u0443\u0434\u0430\u043b\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c \u0431\u0435\u0437 \u0430\u043a\u0442\u0438\u0432\u043d\u043e\u0433\u043e \u043f\u0440\u043e\u0444\u0438\u043b\u044f.";
            return;
        }

        string currentPassword = SettingsCurrentPasswordBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(currentPassword))
        {
            SettingsStatusTextBlock.Text = "\u0412\u0432\u0435\u0434\u0438\u0442\u0435 \u0442\u0435\u043a\u0443\u0449\u0438\u0439 \u043f\u0430\u0440\u043e\u043b\u044c \u0434\u043b\u044f \u0443\u0434\u0430\u043b\u0435\u043d\u0438\u044f.";
            return;
        }

        try
        {
            await Task.Run(() => rustBridge.RemoveLocalProfilePassword(profileId, currentPassword));
            SettingsCurrentPasswordBox.Password = string.Empty;
            RefreshPasswordProtectionSummary();
            SettingsStatusTextBlock.Text = "Пароль убран.";
        }
        catch (Exception ex)
        {
            SettingsStatusTextBlock.Text = $"\u041d\u0435 \u0443\u0434\u0430\u043b\u043e\u0441\u044c \u0443\u0434\u0430\u043b\u0438\u0442\u044c \u043f\u0430\u0440\u043e\u043b\u044c: {NormalizeUserError(ex)}";
        }
    }

    private static string NormalizeUserError(Exception ex)
    {
        string message = ex.Message;
        if (message.Contains("connection attempt timed out", StringComparison.OrdinalIgnoreCase))
        {
            return "Внешний адрес не ответил. Проверьте, что порт открыт через UPnP, проброс порта или firewall.";
        }

        if (message.Contains("transport backend did not respond", StringComparison.OrdinalIgnoreCase))
        {
            return "Транспорт не успел завершить подключение. Проверьте доступность адреса и попробуйте ещё раз.";
        }

        if (message.Contains("invalid", StringComparison.OrdinalIgnoreCase) &&
            message.Contains("password", StringComparison.OrdinalIgnoreCase))
        {
            return "\u041d\u0435\u0432\u0435\u0440\u043d\u044b\u0439 \u043f\u0430\u0440\u043e\u043b\u044c. \u041f\u0440\u043e\u0432\u0435\u0440\u044c\u0442\u0435 \u0442\u0435\u043a\u0443\u0449\u0438\u0439 \u043f\u0430\u0440\u043e\u043b\u044c \u0438 \u043f\u043e\u043f\u0440\u043e\u0431\u0443\u0439\u0442\u0435 \u0435\u0449\u0451 \u0440\u0430\u0437.";
        }

        if (message.Contains("mnemonic", StringComparison.OrdinalIgnoreCase))
        {
            return "\u041d\u0435\u0432\u0435\u0440\u043d\u0430\u044f \u043c\u043d\u0435\u043c\u043e\u043d\u0438\u0447\u0435\u0441\u043a\u0430\u044f \u0444\u0440\u0430\u0437\u0430. \u041f\u0440\u043e\u0432\u0435\u0440\u044c\u0442\u0435 \u0444\u0440\u0430\u0437\u0443 \u0432\u043e\u0441\u0441\u0442\u0430\u043d\u043e\u0432\u043b\u0435\u043d\u0438\u044f \u0438 \u043f\u043e\u043f\u0440\u043e\u0431\u0443\u0439\u0442\u0435 \u0435\u0449\u0451 \u0440\u0430\u0437.";
        }

        if (message.Contains("profile", StringComparison.OrdinalIgnoreCase) &&
            message.Contains("not", StringComparison.OrdinalIgnoreCase))
        {
            return "\u041f\u0440\u043e\u0444\u0438\u043b\u044c \u043d\u0435 \u043d\u0430\u0439\u0434\u0435\u043d \u0438\u043b\u0438 \u0443\u0436\u0435 \u0443\u0434\u0430\u043b\u0451\u043d.";
        }

        return message;
    }

    /// <summary>
    /// Папка данных перенесена с другого компьютера: ключ базы запечатан той машиной, открыть её
    /// здесь можно фразой восстановления одного из профилей в ней. <c>true</c> — база открыта или
    /// стёрта («Начать заново»), загрузку можно повторить; <c>false</c> — загрузка остановлена.
    /// </summary>
    // The open «Открыть данные фразой» dialog, for the test channel (`boot.lock`, `boot.unlock`).
    private ContentDialog? bootLockDialog;
    private TextBox? bootLockPhraseBox;
    private string? bootLockErrorText;
    private ContentDialogResult? bootLockForcedResult;

    private async Task<bool> OpenDatabaseFromAnotherMachineAsync()
    {
        const string LockedText = "Данные Omni перенесены с другого компьютера и зашифрованы его ключом.";
        // Boot runs from the first activation, before the window's content is loaded (no XamlRoot
        // yet), or without a window at all (OMNI_START_HIDDEN): the question waits for the window.
        if (Content is FrameworkElement root && root.XamlRoot is null)
        {
            BootStatusTextBlock.Text = $"{LockedText} Нужна фраза восстановления.";
            TaskCompletionSource loaded = new();
            void OnLoaded(object sender, RoutedEventArgs e) => loaded.TrySetResult();
            root.Loaded += OnLoaded;
            if (root.XamlRoot is null)
            {
                await loaded.Task;
            }

            root.Loaded -= OnLoaded;
        }

        string? errorText = null;
        while (true)
        {
            TextBox phraseBox = new()
            {
                PlaceholderText = "24 слова через пробел",
                TextWrapping = TextWrapping.Wrap,
                IsSpellCheckEnabled = false,
                MinHeight = 64,
            };
            StackPanel body = new() { Spacing = 12 };
            body.Children.Add(new TextBlock
            {
                Text = $"{LockedText} Введите фразу восстановления любого профиля, который был на том компьютере, — после этого данные будут открываться здесь без фразы.",
                TextWrapping = TextWrapping.WrapWholeWords,
            });
            body.Children.Add(phraseBox);
            if (errorText is not null)
            {
                TextBlock errorBlock = new()
                {
                    Text = errorText,
                    TextWrapping = TextWrapping.WrapWholeWords,
                };
                if (Application.Current.Resources.TryGetValue("SystemFillColorCriticalBrush", out object? brush)
                    && brush is Microsoft.UI.Xaml.Media.Brush critical)
                {
                    errorBlock.Foreground = critical;
                }

                body.Children.Add(errorBlock);
            }

            ContentDialog dialog = new()
            {
                Title = "Открыть данные фразой",
                Content = body,
                PrimaryButtonText = "Открыть",
                SecondaryButtonText = "Начать заново",
                CloseButtonText = "Выйти",
                DefaultButton = ContentDialogButton.Primary,
                XamlRoot = Content.XamlRoot,
            };
            bootLockDialog = dialog;
            bootLockPhraseBox = phraseBox;
            bootLockErrorText = errorText;
            bootLockForcedResult = null;
            ContentDialogResult result = await dialog.ShowAsync();
            result = bootLockForcedResult ?? result;
            bootLockDialog = null;
            bootLockPhraseBox = null;
            if (result == ContentDialogResult.Primary)
            {
                string phrase = phraseBox.Text;
                try
                {
                    BootStatusTextBlock.Text = "Открываем данные фразой восстановления...";
                    await Task.Run(() => rustBridge.UnlockDatabaseWithMnemonic(phrase));
                    return true;
                }
                catch (Exception ex)
                {
                    errorText = NormalizeUserError(ex);
                    continue;
                }
            }

            if (result == ContentDialogResult.Secondary)
            {
                ContentDialog confirm = new()
                {
                    Title = "Начать заново?",
                    Content = new TextBlock
                    {
                        Text = "Перенесённые данные (профили, переписка, файлы) будут удалены с этого компьютера. Профиль можно будет восстановить по фразе, но переписка из этих данных пропадёт.",
                        TextWrapping = TextWrapping.WrapWholeWords,
                    },
                    PrimaryButtonText = "Удалить и начать заново",
                    CloseButtonText = "Назад",
                    DefaultButton = ContentDialogButton.Close,
                    XamlRoot = Content.XamlRoot,
                };
                if (await confirm.ShowAsync() != ContentDialogResult.Primary)
                {
                    continue;
                }

                BootStatusTextBlock.Text = "Удаляем перенесённые данные...";
                await Task.Run(() => rustBridge.ResetLocalData());
                return true;
            }

            Application.Current.Exit();
            return false;
        }
    }

    private static bool ShouldOfferDatabaseResetForInitFailure(string message)
        => message.Contains("пустой дескриптор", StringComparison.OrdinalIgnoreCase)
           || message.Contains("initialized", StringComparison.OrdinalIgnoreCase)
           || message.Contains("инициал", StringComparison.OrdinalIgnoreCase);
}


