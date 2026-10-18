using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Profiles;
using Omni.Windows.App.ViewModels;
using Windows.Storage;
using Windows.Storage.Pickers;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void OnShowCreateAccountClicked(object sender, RoutedEventArgs e)
    {
        windowState.Onboarding.SelectedProfileId = null;
        SavedProfilesListView.SelectedItem = null;
        ShowOnboardingStep(OnboardingStep.CreateAccount, string.Empty);
    }

    private void OnShowRestoreAccountClicked(object sender, RoutedEventArgs e)
    {
        windowState.Onboarding.SelectedProfileId = null;
        SavedProfilesListView.SelectedItem = null;
        windowState.Onboarding.RecoveryPreview = null;
        RestoreScenarioSummaryTextBlock.Text = "Фразу проверим, прежде чем что-то менять на компьютере.";
        ShowOnboardingStep(OnboardingStep.RestoreAccount, string.Empty);
    }

    private void OnShowImportAccountClicked(object sender, RoutedEventArgs e)
    {
        windowState.Onboarding.SelectedProfileId = null;
        SavedProfilesListView.SelectedItem = null;
        ShowOnboardingStep(OnboardingStep.ImportAccount, string.Empty);
        if (!string.IsNullOrWhiteSpace(windowState.Onboarding.PendingImportFilePath))
        {
            PopulateImportAccountScreen(
                windowState.Onboarding.PendingImportFileName ?? "Выбранный файл");
        }
    }

    private void OnBackToWelcomeClicked(object sender, RoutedEventArgs e)
    {
        ClearPendingImportAccountSelection();
        if (savedProfileItems.Count > 0)
        {
            ShowOnboardingStep(OnboardingStep.ProfileChooser, string.Empty);
            return;
        }

        ShowOnboardingStep(OnboardingStep.Welcome, string.Empty);
    }

    private void OnSavedProfilesSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (SavedProfilesListView.SelectedItem is SavedProfileListItem item)
        {
            windowState.Onboarding.SelectedProfileId = item.ProfileId;
        }
        else
        {
            windowState.Onboarding.SelectedProfileId = null;
        }

        UpdateControlStates();
    }

    private async void OnOpenSavedProfileClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.Onboarding.SelectedProfileId))
        {
            OnboardingStatusTextBlock.Text = "Выберите профиль для входа.";
            return;
        }

        SavedProfileListItem? selected = savedProfileItems.FirstOrDefault(
            value => value.ProfileId == windowState.Onboarding.SelectedProfileId);
        if (selected is null)
        {
            OnboardingStatusTextBlock.Text = "Выберите профиль для входа.";
            return;
        }

        try
        {
            await Task.Run(() => rustBridge.ReopenLocalProfile(selected.ProfileId));
            RefreshCurrentLifecycleContracts();
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
            windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
            UpdateIdentityHeader();
            UnlockPasswordBox.Password = string.Empty;
            UnlockMnemonicTextBox.Text = string.Empty;

            UnlockProfileTitleTextBlock.Text = $"Профиль: {selected.DisplayName}";
            EnterAuthenticatedShell("Профиль открыт.");
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось открыть профиль: {NormalizeUserError(ex)}";
        }
    }

    private async void OnDeleteSavedProfileFromChooserClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(windowState.Onboarding.SelectedProfileId))
        {
            OnboardingStatusTextBlock.Text = "Выберите профиль для удаления с устройства.";
            return;
        }

        string profileId = windowState.Onboarding.SelectedProfileId;

        try
        {
            DangerousActionPreview? preview = rustBridge.PreviewDeleteLocalProfile(profileId);
            if (preview is not null)
            {
                OnboardingStatusTextBlock.Text = preview.Title;
            }

            if (preview is null)
            {
                OnboardingStatusTextBlock.Text = "Не удалось подготовить подтверждение удаления.";
                return;
            }

            (bool confirmed, string? confirmationToken) =
                await ConfirmDangerousActionAsync(preview, "Удалить с устройства");
            if (!confirmed)
            {
                OnboardingStatusTextBlock.Text = "Удаление отменено.";
                return;
            }
            if (preview.RequiresConfirmation && string.IsNullOrWhiteSpace(confirmationToken))
            {
                OnboardingStatusTextBlock.Text = "Не удалось подготовить подтверждение для удаления.";
                return;
            }

            bool deleteCompleted = false;
            try
            {
                ReleaseProfileBoundShellResourcesForDelete();
                ReleaseActiveProfileAvatarResourcesForDelete();
                rustBridge.ConfirmDeleteLocalProfile(profileId, confirmationToken);
                deleteCompleted = true;
            }
            catch when (IsDeletedProfileExitComplete(profileId))
            {
                deleteCompleted = true;
            }

            if (!deleteCompleted)
            {
                throw new InvalidOperationException("Не удалось удалить профиль с устройства.");
            }

            try
            {
                ReturnToInitialShellAfterProfileExit("Профиль удалён с этого компьютера.");
            }
            catch
            {
                await BootAsync();
                OnboardingStatusTextBlock.Text = "Профиль удалён с этого компьютера.";
            }
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось удалить профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            UpdateControlStates();
        }
    }

    private void OnBackToChooserClicked(object sender, RoutedEventArgs e)
        => ShowOnboardingStep(OnboardingStep.ProfileChooser, string.Empty);

    private async void OnUnlockWithPasswordClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(GetCurrentProfileId()))
        {
            OnboardingStatusTextBlock.Text = "Сначала откройте профиль из списка.";
            return;
        }

        string password = UnlockPasswordBox.Password.Trim();
        if (string.IsNullOrWhiteSpace(password))
        {
            OnboardingStatusTextBlock.Text = "Введите пароль для входа.";
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();

            await Task.Run(() => rustBridge.UnlockCurrentProfileWithPassword(password));
            RefreshCurrentLifecycleContracts();
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
            windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
            UpdateIdentityHeader();
            EnterAuthenticatedShell("Профиль разблокирован по паролю.");
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось разблокировать профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            isOnboardingBusy = false;
            UnlockPasswordBox.Password = string.Empty;
            UpdateControlStates();
        }
    }

    private async void OnUnlockWithMnemonicClicked(object sender, RoutedEventArgs e)
    {
        if (string.IsNullOrWhiteSpace(GetCurrentProfileId()))
        {
            OnboardingStatusTextBlock.Text = "Сначала откройте профиль из списка.";
            return;
        }

        string mnemonic = UnlockMnemonicTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(mnemonic))
        {
            OnboardingStatusTextBlock.Text = "Введите фразу восстановления.";
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();

            await Task.Run(() => rustBridge.UnlockCurrentProfileWithMnemonic(mnemonic));
            RefreshCurrentLifecycleContracts();
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
            windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
            UpdateIdentityHeader();
            EnterAuthenticatedShell("Профиль разблокирован по фразе восстановления.");
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось разблокировать профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            isOnboardingBusy = false;
            UnlockMnemonicTextBox.Text = string.Empty;
            UpdateControlStates();
        }
    }

    private async void OnCreateAccountClicked(object sender, RoutedEventArgs e)
    {
        string displayName = CreateDisplayNameTextBox.Text.Trim();
        string deviceLabel = CreateDeviceLabelTextBox.Text.Trim();

        if (string.IsNullOrWhiteSpace(displayName))
        {
            OnboardingStatusTextBlock.Text = "Введите имя.";
            return;
        }

        if (string.IsNullOrWhiteSpace(deviceLabel))
        {
            OnboardingStatusTextBlock.Text = "Введите название устройства.";
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();

            CreateAccountResponse response = await Task.Run(() => rustBridge.CreateAccount(displayName, deviceLabel));
            LocalAccountSnapshot account = response.Account;
            LocalDeviceSnapshot? device = rustBridge.GetLocalDevice();

            windowState.Onboarding.LocalAccount = account;
            windowState.Onboarding.LocalDevice = device;
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.PendingRecoveryPhraseV2 = response.InitialRecoveryPhraseV2;
            PopulateSavedProfiles(rustBridge.ListLocalProfiles());
            RefreshCurrentLifecycleContracts();
            UpdateIdentityHeader();
            PopulateRecoveryScreen(account, response.InitialRecoveryPhraseV2);
            ShowOnboardingStep(OnboardingStep.RecoveryPhrase, string.Empty);
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось создать локальный профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            isOnboardingBusy = false;
            UpdateControlStates();
        }
    }

    private async void OnRestoreAccountClicked(object sender, RoutedEventArgs e)
    {
        string displayName = RestoreDisplayNameTextBox.Text.Trim();
        string mnemonic = RestoreRecoveryPhraseTextBox.Text.Trim();
        string deviceLabel = RestoreDeviceLabelTextBox.Text.Trim();

        if (string.IsNullOrWhiteSpace(displayName) ||
            string.IsNullOrWhiteSpace(mnemonic) ||
            string.IsNullOrWhiteSpace(deviceLabel))
        {
            OnboardingStatusTextBlock.Text = "Заполните все поля восстановления.";
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();
            RecoveryInputSummary recoveryPreview = await Task.Run(() =>
                rustBridge.InspectRecoveryInput(mnemonic, displayName, deviceLabel));
            windowState.Onboarding.RecoveryPreview = recoveryPreview;
            string previewText = Presentation.RecoveryText.ForPreview(recoveryPreview.Conflict, recoveryPreview.CanRestore);
            RestoreScenarioSummaryTextBlock.Text = previewText;

            if (!recoveryPreview.CanRestore)
            {
                OnboardingStatusTextBlock.Text = previewText;
                return;
            }

            LocalAccountSnapshot account = await Task.Run(() => rustBridge.RestoreLocalProfileFromMnemonic(
                displayName,
                mnemonic,
                deviceLabel));
            LocalDeviceSnapshot? device = rustBridge.GetLocalDevice();

            windowState.Onboarding.LocalAccount = account;
            windowState.Onboarding.LocalDevice = device;
            windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
            windowState.Onboarding.PendingRecoveryPhraseV2 = null;
            PopulateSavedProfiles(rustBridge.ListLocalProfiles());
            RefreshCurrentLifecycleContracts();
            UpdateIdentityHeader();
            EnterAuthenticatedShell("Новый локальный профиль восстановлен.");
        }
        catch (Exception ex)
        {
            OnboardingStatusTextBlock.Text = $"Не удалось восстановить локальный профиль: {NormalizeUserError(ex)}";
        }
        finally
        {
            isOnboardingBusy = false;
            UpdateControlStates();
        }
    }

    private async void OnSaveRecoveryClicked(object sender, RoutedEventArgs e)
    {
        LocalAccountSnapshot? accountSnapshot = windowState.Onboarding.LocalAccount;
        RecoveryPhraseV2? recoveryPhraseV2 = windowState.Onboarding.PendingRecoveryPhraseV2;
        if (recoveryPhraseV2 is null)
        {
            recoveryPhraseV2 = await PromptForRecoveryPhraseAsync(showRecoveryDialog: false);
        }

        if (recoveryPhraseV2 is null)
        {
            return;
        }

        try
        {
            string recoveryDirectory = GetRecoveryDirectory();
            Directory.CreateDirectory(recoveryDirectory);

            string accountId =
                accountSnapshot?.Descriptor.AccountId ?? "account";
            string fileName = $"omni-recovery-{SanitizeFileName(accountId)}.json";
            string filePath = Path.Combine(recoveryDirectory, fileName);
            string payload = JsonSerializer.Serialize(
                recoveryPhraseV2,
                OmniJsonSerializerContext.Default.RecoveryPhraseV2);
            File.WriteAllText(filePath, payload);

            SetRecoverySaveStatus($"Файл сохранён: {filePath}");
        }
        catch (Exception ex)
        {
            SetRecoverySaveStatus($"Не удалось сохранить файл: {ex.Message}");
        }
    }

    private async void OnExportAccountClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            AccountExportManifestV1? manifest = await Task.Run(() => rustBridge.GetCurrentAccountExportManifest());
            if (manifest is null)
            {
                SettingsExportStatusTextBlock.Text = "Сначала откройте локальный профиль.";
                return;
            }

            FolderPicker picker = new();
            picker.FileTypeFilter.Add("*");
            InitializePicker(picker);

            StorageFolder? folder = await picker.PickSingleFolderAsync();
            if (folder is null)
            {
                SettingsExportStatusTextBlock.Text = "Выбор папки отменён.";
                return;
            }

            string fileName = BuildDatabaseBackupFileName(manifest);
            string destinationPath = Path.Combine(folder.Path, fileName);
            await Task.Run(() => rustBridge.ExportCurrentDatabaseBackupBundle(destinationPath));

            SettingsExportStatusTextBlock.Text = $"Резервная копия сохранена: {destinationPath}. Открыть её можно только этим аккаунтом или по фразе восстановления.";
        }
        catch (Exception ex)
        {
            SettingsExportStatusTextBlock.Text = $"Не удалось сохранить резервную копию: {NormalizeUserError(ex)}";
        }
    }

    private async void OnChooseImportAccountFileClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            string? importPath = ShowDesktopOpenFileDialog(
                "Выберите резервную копию аккаунта",
                [
                    new DesktopFileDialogFilter("Резервные копии Omni (*.omnibackup;*.db)", "*.omnibackup;*.db;*.sqlite;*.sqlite3"),
                    new DesktopFileDialogFilter("Все файлы (*.*)", "*.*"),
                ],
                Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments));

            if (string.IsNullOrWhiteSpace(importPath))
            {
                ImportAccountStatusTextBlock.Text = "Выбор резервной копии отменён.";
                return;
            }

            FileInfo file = new(importPath);
            if (!file.Exists || (file.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
            {
                throw new InvalidOperationException("Выберите существующий файл резервной копии.");
            }

            windowState.Onboarding.PendingImportFilePath = file.FullName;
            windowState.Onboarding.PendingImportFileName = file.Name;
            backupImportSession.SelectFile(file.FullName);
            PopulateImportAccountScreen(file.Name);
            ShowOnboardingStep(OnboardingStep.ImportAccount, "Копия выбрана — введите фразу восстановления.");
        }
        catch (Exception ex)
        {
            ImportAccountStatusTextBlock.Text = $"Не удалось открыть резервную копию: {NormalizeUserError(ex)}";
            ClearPendingImportAccountSelection(keepStatus: true);
        }
    }

    private async void OnImportAccountClicked(object sender, RoutedEventArgs e)
    {
        string? backupPath = windowState.Onboarding.PendingImportFilePath;
        if (string.IsNullOrWhiteSpace(backupPath))
        {
            ImportAccountStatusTextBlock.Text = "Сначала выберите резервную копию.";
            return;
        }

        string mnemonic = ImportAccountMnemonicTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(mnemonic))
        {
            ImportAccountStatusTextBlock.Text = "Введите фразу восстановления.";
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();

            AccountBackupInspection backupInspection = await Task.Run(() =>
                rustBridge.InspectAccountBackupManifest(backupPath, mnemonic));
            backupImportSession.RecordInspection(backupPath, backupInspection);

            ImportAccountStatusTextBlock.Text = "Резервная копия проверена. Заменяем локальную базу данных...";
            await Task.Run(() => rustBridge.ImportDatabaseBackup(
                backupPath,
                backupImportSession.GetRequiredInspectionToken(backupPath)));
            ClearPendingImportAccountSelection(keepStatus: true);
            await BootAsync();
            ImportAccountStatusTextBlock.Text = $"Резервная копия восстановлена: {backupInspection.Manifest.DisplayName}.";
        }
        catch (Exception ex)
        {
            ImportAccountStatusTextBlock.Text = $"Не удалось восстановить резервную копию: {NormalizeUserError(ex)}";
        }
        finally
        {
            isOnboardingBusy = false;
            UpdateControlStates();
        }
    }

    private void OnRecoveryContinueClicked(object sender, RoutedEventArgs e)
    {
        if (windowState.Onboarding.LocalAccount is null)
        {
            OnboardingStatusTextBlock.Text = "Сначала создайте профиль.";
            return;
        }

        windowState.Onboarding.PendingRecoveryPhraseV2 = null;
        EnterAuthenticatedShell(string.Empty);
    }

    private async void OnRevealRecoveryClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            RecoveryPhraseV2? recoveryPhraseV2 = await PromptForRecoveryPhraseAsync(showRecoveryDialog: true);
            if (recoveryPhraseV2 is not null)
            {
                SetRecoverySaveStatus("Данные восстановления показаны только в отдельном окне.");
            }
        }
        catch (Exception ex)
        {
            SetRecoverySaveStatus($"Не удалось показать данные восстановления: {NormalizeUserError(ex)}");
        }
        finally
        {
            UpdateControlStates();
        }
    }

    private async Task<RecoveryPhraseV2?> PromptForRecoveryPhraseAsync(bool showRecoveryDialog)
    {
        if (windowState.Onboarding.PendingRecoveryPhraseV2 is not null)
        {
            if (showRecoveryDialog)
            {
                await ShowRecoveryPhraseDialogAsync(windowState.Onboarding.PendingRecoveryPhraseV2);
            }

            return windowState.Onboarding.PendingRecoveryPhraseV2;
        }

        if (currentPasswordProtectionSummary?.Enabled != true)
        {
            SetRecoverySaveStatus("Сначала включите локальную парольную защиту, чтобы снова открыть данные восстановления.");
            return null;
        }

        if (Content is not FrameworkElement rootElement || rootElement.XamlRoot is null)
        {
            throw new InvalidOperationException("Окно подтверждения пароля пока не готово.");
        }

        PasswordBox passwordBox = new()
        {
            PlaceholderText = "Пароль",
        };
        StackPanel dialogContent = new()
        {
            Spacing = 12,
        };
        dialogContent.Children.Add(new TextBlock
        {
            Text = showRecoveryDialog
                ? "Введите локальный пароль, чтобы временно показать данные восстановления."
                : "Введите локальный пароль, чтобы сохранить данные восстановления в файл.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        dialogContent.Children.Add(passwordBox);

        ContentDialog dialog = new()
        {
            Title = "Подтвердите локальный пароль",
            Content = dialogContent,
            PrimaryButtonText = "Продолжить",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = rootElement.XamlRoot,
        };

        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return null;
        }

        string password = passwordBox.Password.Trim();
        passwordBox.Password = string.Empty;
        if (string.IsNullOrWhiteSpace(password))
        {
            SetRecoverySaveStatus("Пароль.");
            return null;
        }

        RecoveryPhraseV2 recoveryPhraseV2 = await Task.Run(() =>
            rustBridge.RevealCurrentProfileRecoveryPhraseWithPassword(password));

        if (showRecoveryDialog)
        {
            await ShowRecoveryPhraseDialogAsync(recoveryPhraseV2);
        }

        return recoveryPhraseV2;
    }

    private async Task ShowRecoveryPhraseDialogAsync(RecoveryPhraseV2 recoveryPhraseV2)
    {
        if (Content is not FrameworkElement rootElement || rootElement.XamlRoot is null)
        {
            throw new InvalidOperationException("Окно просмотра данных восстановления пока не готово.");
        }

        TextBox mnemonicTextBox = new()
        {
            IsReadOnly = true,
            Text = recoveryPhraseV2.Mnemonic,
        };
        TextBox payloadTextBox = new()
        {
            AcceptsReturn = true,
            IsReadOnly = true,
            MinHeight = 140,
            TextWrapping = TextWrapping.Wrap,
            Text = JsonSerializer.Serialize(
                recoveryPhraseV2,
                OmniJsonSerializerContext.Default.RecoveryPhraseV2),
        };
        StackPanel dialogContent = new()
        {
            Spacing = 12,
        };
        dialogContent.Children.Add(new TextBlock
        {
            Text = "Данные восстановления показаны только для этого действия и не сохраняются на экране настроек.",
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        dialogContent.Children.Add(new TextBlock
        {
            Text = "Фраза восстановления",
        });
        dialogContent.Children.Add(mnemonicTextBox);
        dialogContent.Children.Add(new TextBlock
        {
            Text = "Полное представление для сохранения",
        });
        dialogContent.Children.Add(payloadTextBox);

        ContentDialog dialog = new()
        {
            Title = "Данные восстановления",
            Content = dialogContent,
            CloseButtonText = "Закрыть",
            DefaultButton = ContentDialogButton.Close,
            XamlRoot = rootElement.XamlRoot,
        };

        await dialog.ShowAsync();
        mnemonicTextBox.Text = string.Empty;
        payloadTextBox.Text = string.Empty;
    }

    private void SetRecoverySaveStatus(string message)
    {
        RecoverySaveStatusTextBlock.Text = message;
        SettingsStatusTextBlock.Text = message;
    }

    private string GetRecoveryDirectory()
        => Path.Combine(rustBridge.AppDirectory, "recovery");

    private static string SanitizeFileName(string value)
    {
        HashSet<char> invalidChars = Path.GetInvalidFileNameChars().ToHashSet();
        return new string(value.Select(ch => invalidChars.Contains(ch) ? '_' : ch).ToArray());
    }
    private void PopulateRecoveryScreen(LocalAccountSnapshot account, RecoveryPhraseV2 recoveryPhraseV2)
    {
        RecoveryDisplayNameTextBlock.Text = $"Профиль: {account.Descriptor.DisplayName}";
        RecoveryAccountIdTextBlock.Text = $"ID профиля: {account.Descriptor.AccountId}";
        RecoveryPhraseTextBox.Text = recoveryPhraseV2.Mnemonic;
        RecoveryPayloadTextBox.Text = JsonSerializer.Serialize(
            recoveryPhraseV2,
            OmniJsonSerializerContext.Default.RecoveryPhraseV2);
        RecoverySaveStatusTextBlock.Text =
            "Сохраните фразу восстановления. Без неё и без устройств восстановить профиль не получится.";
    }

}
