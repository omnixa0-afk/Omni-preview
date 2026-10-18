using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Presentation;
using Windows.Storage;
using Windows.Storage.Pickers;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    /// Our own avatar and name in the pane footer: the way to the profile, not a picture viewer.
    private void OnIdentityFooterTapped(object sender, Microsoft.UI.Xaml.Input.TappedRoutedEventArgs e)
    {
        e.Handled = true;
        AuthenticatedNavigationView.SelectedItem = SettingsNavigationItem;
        SettingsSectionHost.ShowSection("profile");
    }

    private void RenderIdentityAvatar()
    {
        LocalAccountSnapshot? account = windowState.Onboarding.LocalAccount;
        string fallbackText = MainWindowPresentation.BuildAvatarText(account?.Descriptor.DisplayName);
        ApplyAvatarVisual(
            IdentityAvatarImage,
            IdentityAvatarTextBlock,
            account?.PublicAvatarImagePath,
            fallbackText,
            loadingRing: IdentityAvatarLoadingRing,
            isLoading: isUpdatingPublicProfile,
            versionToken: BuildLocalAvatarVersionToken(account),
            decodePixelSize: 64,
            circle: IdentityAvatarCircle,
            colorSeed: account?.Descriptor.AccountId ?? string.Empty);
    }

    // The account whose name the editor was last filled with. Until it is filled once, the empty box
    // is no draft: it read as one ("" differs from the name), so the editor stayed empty under a
    // profile called LaptopStand1 forever (seen 2026-10-10).
    private string? publicNameEditorFilledForAccountId;

    private void RenderLocalPublicProfileEditor(bool preserveDraft = true)
    {
        LocalAccountSnapshot? account = windowState.Onboarding.LocalAccount;
        string? accountId = account?.Descriptor.AccountId;
        bool firstFill = accountId is not null && !string.Equals(publicNameEditorFilledForAccountId, accountId, StringComparison.Ordinal);
        publicNameEditorFilledForAccountId = accountId;
        bool preserveEditorDraft = preserveDraft && !firstFill && HasPendingPublicProfileEditorChanges();
        string persistedDisplayName = account?.Descriptor.DisplayName ?? string.Empty;
        if (!preserveEditorDraft &&
            !string.Equals(SettingsPublicDisplayNameTextBox.Text, persistedDisplayName, StringComparison.Ordinal))
        {
            SettingsPublicDisplayNameTextBox.Text = persistedDisplayName;
        }

        string effectiveDisplayName = preserveEditorDraft
            ? SettingsPublicDisplayNameTextBox.Text.Trim()
            : persistedDisplayName;
        if (string.IsNullOrWhiteSpace(effectiveDisplayName))
        {
            effectiveDisplayName = persistedDisplayName;
        }

        ApplyAvatarVisual(
            SettingsProfileAvatarImage,
            SettingsProfileAvatarTextBlock,
            GetPendingPublicAvatarImagePath(account),
            MainWindowPresentation.BuildAvatarText(effectiveDisplayName),
            loadingRing: SettingsProfileAvatarLoadingRing,
            isLoading: isUpdatingPublicProfile,
            versionToken: BuildSettingsAvatarVersionToken(account),
            decodePixelSize: 160,
            circle: SettingsProfileAvatarCircle,
            colorSeed: account?.Descriptor.AccountId ?? string.Empty);

        SettingsProfileEditorStatusTextBlock.Text = BuildLocalPublicProfileEditorStatusText(account);
    }

    private string? GetPendingPublicAvatarImagePath(LocalAccountSnapshot? account)
    {
        if (!string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath))
        {
            return pendingPublicAvatarSourcePath;
        }

        if (pendingPublicAvatarClearRequested)
        {
            return null;
        }

        return account?.PublicAvatarImagePath;
    }

    private bool HasPendingPublicProfileEditorChanges()
    {
        LocalAccountSnapshot? account = windowState.Onboarding.LocalAccount;
        if (account is null)
        {
            return false;
        }

        if (!string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath) || pendingPublicAvatarClearRequested)
        {
            return true;
        }

        string draftDisplayName = SettingsPublicDisplayNameTextBox.Text.Trim();
        return !string.Equals(
            draftDisplayName,
            account.Descriptor.DisplayName,
            StringComparison.Ordinal);
    }

    private string BuildLocalPublicProfileEditorStatusText(LocalAccountSnapshot? account)
    {
        if (account is null)
        {
            return "Войдите в локальный профиль, чтобы задать публичное имя и аватар.";
        }

        if (!string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath))
        {
            string avatarName = string.IsNullOrWhiteSpace(pendingPublicAvatarDisplayName)
                ? Path.GetFileName(pendingPublicAvatarSourcePath)
                : pendingPublicAvatarDisplayName;
            return $"Выбран новый аватар: {avatarName}. Сохраните профиль, чтобы отправить обновление контактам.";
        }

        if (pendingPublicAvatarClearRequested)
        {
            return "Фото уберётся, когда вы нажмёте «Сохранить».";
        }

        string draftDisplayName = SettingsPublicDisplayNameTextBox.Text.Trim();
        if (!string.IsNullOrWhiteSpace(draftDisplayName) &&
            !string.Equals(draftDisplayName, account.Descriptor.DisplayName, StringComparison.Ordinal))
        {
            return "Имя изменено — нажмите «Сохранить», и контакты увидят новое.";
        }

        // Saved: nothing to say — the date of the last save told nobody anything (owner, 2026-10-13).
        return string.Empty;
    }

    private static string BuildStagedPublicAvatarFileName(string originalFileName)
    {
        string extension = Path.GetExtension(originalFileName);
        if (string.IsNullOrWhiteSpace(extension) || extension.Length > 16)
        {
            extension = ".avatar";
        }

        return $"omni-public-avatar-{Guid.NewGuid():N}{extension}";
    }

    private async Task<string> StagePublicAvatarSourceAsync(string sourcePath)
    {
        string stagedFileName = BuildStagedPublicAvatarFileName(Path.GetFileName(sourcePath));
        return await StageFilePathToTempAsync(sourcePath, stagedFileName);
    }

    private async Task<string> StagePublicAvatarSourceAsync(StorageFile file)
    {
        string stagedFileName = BuildStagedPublicAvatarFileName(file.Name);
        return await StageStorageFileToTempAsync(file, stagedFileName);
    }

    private void DeletePendingPublicAvatarSourceFile()
    {
        string? sourcePath = pendingPublicAvatarSourcePath;
        if (string.IsNullOrWhiteSpace(sourcePath))
        {
            return;
        }

        try
        {
            string tempFolderPath = Path.GetFullPath(GetUiTempDirectoryPath())
                .TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar)
                + Path.DirectorySeparatorChar;
            string fullSourcePath = Path.GetFullPath(sourcePath);
            if (!fullSourcePath.StartsWith(tempFolderPath, StringComparison.OrdinalIgnoreCase))
            {
                return;
            }

            if (File.Exists(fullSourcePath))
            {
                File.Delete(fullSourcePath);
            }
        }
        catch
        {
            // Best-effort cleanup only.
        }
    }

    private void ApplyAvatarVisual(
        Image imageControl,
        TextBlock fallbackTextBlock,
        string? imagePath,
        string fallbackText,
        int decodePixelSize = 96,
        ProgressRing? loadingRing = null,
        bool isLoading = false,
        string? versionToken = null,
        Border? circle = null,
        string? colorSeed = null)
    {
        // The circle under the photo takes the person's own colour; the initials on it are white
        // (AvatarPalette).
        if (circle is not null && colorSeed is not null)
        {
            circle.Background = AvatarPalette.BrushFor(colorSeed);
            fallbackTextBlock.Foreground = WhiteBrush;
        }

        string cacheKey = string.Join(
            '|',
            imagePath?.Trim() ?? string.Empty,
            decodePixelSize,
            versionToken?.Trim() ?? string.Empty);
        if (!avatarImageCache.TryGetValue(cacheKey, out BitmapImage? avatarImage))
        {
            avatarImage = MainWindowPresentation.BuildAvatarImage(imagePath, decodePixelSize, versionToken);
            avatarImageCache[cacheKey] = avatarImage;
        }
        imageControl.Source = avatarImage;
        imageControl.Visibility = avatarImage is null ? Visibility.Collapsed : Visibility.Visible;
        fallbackTextBlock.Text = fallbackText;
        fallbackTextBlock.Visibility = avatarImage is null ? Visibility.Visible : Visibility.Collapsed;
        if (loadingRing is not null)
        {
            loadingRing.IsActive = isLoading;
            loadingRing.Visibility = isLoading ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    private static readonly Microsoft.UI.Xaml.Media.SolidColorBrush WhiteBrush = new(Microsoft.UI.Colors.White);

    private static string? BuildLocalAvatarVersionToken(LocalAccountSnapshot? account)
    {
        if (account is null)
        {
            return null;
        }

        return !string.IsNullOrWhiteSpace(account.PublicAvatarHashHex)
            ? account.PublicAvatarHashHex
            : account.PublicProfileUpdatedAtUnixMs?.ToString();
    }

    private static string? BuildContactAvatarVersionToken(ContactSnapshot? contact)
    {
        if (contact is null)
        {
            return null;
        }

        return !string.IsNullOrWhiteSpace(contact.PublicAvatarPendingHashHex)
            ? contact.PublicAvatarPendingHashHex
            : !string.IsNullOrWhiteSpace(contact.PublicAvatarHashHex)
                ? contact.PublicAvatarHashHex
                : contact.PublicProfileUpdatedAtUnixMs?.ToString();
    }

    private string? BuildSettingsAvatarVersionToken(LocalAccountSnapshot? account)
    {
        if (!string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath))
        {
            return pendingPublicAvatarSourcePath;
        }

        if (pendingPublicAvatarClearRequested)
        {
            return "cleared";
        }

        return BuildLocalAvatarVersionToken(account);
    }

    private async void OnSettingsChooseAvatarClicked(object sender, RoutedEventArgs e)
    {
        if (windowState.Onboarding.LocalAccount is null)
        {
            SettingsProfileEditorStatusTextBlock.Text = "Сначала войдите в локальный профиль.";
            return;
        }

        try
        {
            string? avatarPath = ShowDesktopOpenFileDialog(
                "Выберите изображение аватара",
                [
                    new DesktopFileDialogFilter("Изображения (*.jpg;*.jpeg;*.png;*.bmp)", "*.jpg;*.jpeg;*.png;*.bmp"),
                    new DesktopFileDialogFilter("Все файлы (*.*)", "*.*"),
                ],
                Environment.GetFolderPath(Environment.SpecialFolder.MyPictures));

            if (string.IsNullOrWhiteSpace(avatarPath))
            {
                SettingsProfileEditorStatusTextBlock.Text = "Выбор аватара отменён.";
                return;
            }

            FileInfo file = new(avatarPath);
            if (!file.Exists || (file.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
            {
                throw new InvalidOperationException("Выберите существующий файл изображения, а не папку.");
            }

            DeletePendingPublicAvatarSourceFile();
            pendingPublicAvatarSourcePath = await StagePublicAvatarSourceAsync(file.FullName);
            pendingPublicAvatarDisplayName = file.Name;
            pendingPublicAvatarClearRequested = false;
            RenderLocalPublicProfileEditor();
            SettingsStatusTextBlock.Text = $"Выбран новый аватар: {file.Name}.";
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            string errorText = NormalizeUserError(ex);
            SettingsProfileEditorStatusTextBlock.Text = $"Не удалось выбрать аватар: {errorText}";
            SettingsStatusTextBlock.Text = SettingsProfileEditorStatusTextBlock.Text;
        }
    }

    private void OnSettingsClearAvatarClicked(object sender, RoutedEventArgs e)
    {
        if (windowState.Onboarding.LocalAccount is null)
        {
            SettingsProfileEditorStatusTextBlock.Text = "Сначала войдите в локальный профиль.";
            return;
        }

        DeletePendingPublicAvatarSourceFile();
        pendingPublicAvatarSourcePath = null;
        pendingPublicAvatarDisplayName = null;
        pendingPublicAvatarClearRequested = true;
        RenderLocalPublicProfileEditor();
        SettingsStatusTextBlock.Text = "Фото уберётся, когда вы нажмёте «Сохранить».";
        UpdateControlStates();
    }

    private async void OnSettingsSavePublicProfileClicked(object sender, RoutedEventArgs e)
    {
        LocalAccountSnapshot? currentAccount = windowState.Onboarding.LocalAccount;
        if (currentAccount is null)
        {
            SettingsProfileEditorStatusTextBlock.Text = "Сначала войдите в локальный профиль.";
            return;
        }

        string displayName = SettingsPublicDisplayNameTextBox.Text.Trim();
        if (string.IsNullOrWhiteSpace(displayName))
        {
            displayName = currentAccount.Descriptor.DisplayName;
        }
        if (string.IsNullOrWhiteSpace(displayName))
        {
            SettingsProfileEditorStatusTextBlock.Text = "Введите публичное имя профиля.";
            return;
        }

        if (!HasPendingPublicProfileEditorChanges())
        {
            SettingsProfileEditorStatusTextBlock.Text = "Изменений пока нет.";
            return;
        }

        isUpdatingPublicProfile = true;
        UpdateControlStates();
        RenderIdentityAvatar();
        RenderLocalPublicProfileEditor();

        try
        {
            LocalAccountSnapshot updatedAccount = await Task.Run(() =>
                rustBridge.UpdateLocalPublicProfile(
                    displayName,
                    pendingPublicAvatarSourcePath,
                    pendingPublicAvatarClearRequested));
            windowState.Onboarding.LocalAccount = updatedAccount;
            DeletePendingPublicAvatarSourceFile();
            pendingPublicAvatarSourcePath = null;
            pendingPublicAvatarDisplayName = null;
            pendingPublicAvatarClearRequested = false;
            UpdateIdentityHeader();
            RenderLocalPublicProfileEditor(preserveDraft: false);
            SettingsProfileEditorStatusTextBlock.Text = "Сохранено — контакты увидят при следующей связи.";
            SettingsStatusTextBlock.Text = SettingsProfileEditorStatusTextBlock.Text;
        }
        catch (Exception ex)
        {
            string errorText = NormalizeUserError(ex);
            SettingsProfileEditorStatusTextBlock.Text = $"Не удалось обновить публичный профиль: {errorText}";
            SettingsStatusTextBlock.Text = SettingsProfileEditorStatusTextBlock.Text;
        }
        finally
        {
            isUpdatingPublicProfile = false;
            UpdateControlStates();
            RenderIdentityAvatar();
            RenderLocalPublicProfileEditor();
        }
    }
}
