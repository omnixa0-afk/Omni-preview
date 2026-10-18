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
    private static readonly TimeSpan UiTempFileRetention = TimeSpan.FromDays(1);

    private string? GetApprovedCurrentDialogKey()
    {
        if (!CanComposeInCurrentChat())
        {
            return null;
        }

        return currentChatHeaderSummary?.DialogKey;
    }

    private void UpdateDraftAttachmentUi()
    {
        if (HasPendingAttachmentDraft())
        {
            // «Фото»: shown in the chat, re-encoded; unticked, the picture goes byte for byte as a file.
            ChatComposerBorder.ShowDraft(
                pendingAttachmentSourcePath!,
                pendingAttachmentFileName ?? Path.GetFileName(pendingAttachmentSourcePath!),
                MainWindowPresentation.FormatFileSize(pendingAttachmentFileSize),
                MainWindowPresentation.CanSendAsPhoto(pendingAttachmentFileName, pendingAttachmentFileSize),
                pendingAttachmentAsPhoto);
        }
        else
        {
            ChatComposerBorder.ClearDraft();
        }
        ChatDropOverlayBorder.Visibility = Visibility.Collapsed;
    }

    private void ClearPendingAttachmentDraft(bool preserveStatusText = false)
    {
        DeletePendingAttachmentSourceFile();
        pendingAttachmentSourcePath = null;
        pendingAttachmentFileName = null;
        pendingAttachmentFileSize = 0;
        pendingAttachmentAsPhoto = false;
        UpdateDraftAttachmentUi();
        if (!preserveStatusText)
        {
            SetActionStatus(string.Empty);
        }
        UpdateControlStates();
    }

    private void ClearPendingAttachmentDraftIfCurrent(string sourcePath, bool preserveStatusText = false)
    {
        if (string.IsNullOrWhiteSpace(pendingAttachmentSourcePath))
        {
            return;
        }

        if (!string.Equals(
                Path.GetFullPath(pendingAttachmentSourcePath),
                Path.GetFullPath(sourcePath),
                StringComparison.OrdinalIgnoreCase))
        {
            return;
        }

        ClearPendingAttachmentDraft(preserveStatusText);
    }

    private void SetPendingAttachmentDraft(string sourcePath, string? displayName)
    {
        FileInfo fileInfo = new(sourcePath);
        if (!fileInfo.Exists || (fileInfo.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
        {
            throw new InvalidOperationException("Выберите существующий файл, а не папку.");
        }

        if (!string.IsNullOrWhiteSpace(pendingAttachmentSourcePath) &&
            !string.Equals(
                Path.GetFullPath(pendingAttachmentSourcePath),
                fileInfo.FullName,
                StringComparison.OrdinalIgnoreCase))
        {
            DeletePendingAttachmentSourceFile();
        }

        pendingAttachmentSourcePath = fileInfo.FullName;
        pendingAttachmentFileName = string.IsNullOrWhiteSpace(displayName) ? fileInfo.Name : displayName;
        pendingAttachmentFileSize = fileInfo.Length;
        // A picture goes as a photo unless the user unticks it (Telegram's «Сжать изображение»).
        pendingAttachmentAsPhoto = MainWindowPresentation.CanSendAsPhoto(pendingAttachmentFileName, pendingAttachmentFileSize);
        UpdateDraftAttachmentUi();
        SetActionStatus(string.Empty);
        UpdateControlStates();
    }

    private string GetUiTempDirectoryPath()
        => Path.Combine(appDirectory, "ui-temp");

    private string GetUiTempFilePath(string stagedFileName)
    {
        string tempDirectory = GetUiTempDirectoryPath();
        Directory.CreateDirectory(tempDirectory);
        return Path.Combine(tempDirectory, stagedFileName);
    }

    private void CleanupUiTempDirectory()
    {
        try
        {
            string tempDirectory = GetUiTempDirectoryPath();
            if (!Directory.Exists(tempDirectory))
            {
                return;
            }

            string? currentDraftPath = string.IsNullOrWhiteSpace(pendingAttachmentSourcePath)
                ? null
                : Path.GetFullPath(pendingAttachmentSourcePath);
            DateTimeOffset cutoff = DateTimeOffset.UtcNow - UiTempFileRetention;
            foreach (string filePath in Directory.EnumerateFiles(tempDirectory, "omni-attachment-*", SearchOption.TopDirectoryOnly))
            {
                string fullPath = Path.GetFullPath(filePath);
                if (currentDraftPath is not null &&
                    string.Equals(fullPath, currentDraftPath, StringComparison.OrdinalIgnoreCase))
                {
                    continue;
                }

                FileInfo fileInfo = new(fullPath);
                if (fileInfo.LastWriteTimeUtc > cutoff.UtcDateTime)
                {
                    continue;
                }

                try
                {
                    fileInfo.Delete();
                }
                catch
                {
                    // Best-effort cleanup only.
                }
            }
        }
        catch
        {
            // Best-effort cleanup only.
        }
    }

    private Task<string> StageAttachmentSourceAsync(string sourcePath)
    {
        return Task.FromResult(ValidateLocalAttachmentSourcePath(sourcePath));
    }

    private async Task<string> StageAttachmentSourceAsync(StorageFile file)
    {
        if (!string.IsNullOrWhiteSpace(file.Path) && File.Exists(file.Path))
        {
            return ValidateLocalAttachmentSourcePath(file.Path);
        }

        string stagedFileName = BuildStagedAttachmentFileName(file.Name);
        return await StageStorageFileToTempAsync(file, stagedFileName);
    }

    private static string ValidateLocalAttachmentSourcePath(string sourcePath)
    {
        if (string.IsNullOrWhiteSpace(sourcePath))
        {
            throw new InvalidOperationException("\u041f\u0443\u0442\u044c \u043a \u0432\u044b\u0431\u0440\u0430\u043d\u043d\u043e\u043c\u0443 \u0444\u0430\u0439\u043b\u0443 \u043f\u0443\u0441\u0442.");
        }

        FileInfo sourceFileInfo = new(sourcePath);
        if (!sourceFileInfo.Exists ||
            (sourceFileInfo.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
        {
            throw new InvalidOperationException("\u0412\u044b\u0431\u0435\u0440\u0438\u0442\u0435 \u0441\u0443\u0449\u0435\u0441\u0442\u0432\u0443\u044e\u0449\u0438\u0439 \u0444\u0430\u0439\u043b, \u0430 \u043d\u0435 \u043f\u0430\u043f\u043a\u0443.");
        }

        return sourceFileInfo.FullName;
    }

    private async Task<string> StageFilePathToTempAsync(string sourcePath, string stagedFileName)
    {
        if (string.IsNullOrWhiteSpace(sourcePath))
        {
            throw new InvalidOperationException("Путь к выбранному файлу пуст.");
        }

        FileInfo sourceFileInfo = new(sourcePath);
        if (!sourceFileInfo.Exists ||
            (sourceFileInfo.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
        {
            throw new InvalidOperationException("Выберите существующий файл, а не папку.");
        }

        string stagedPath = GetUiTempFilePath(stagedFileName);
        using FileStream sourceStream = new(
            sourceFileInfo.FullName,
            FileMode.Open,
            FileAccess.Read,
            FileShare.ReadWrite | FileShare.Delete,
            bufferSize: 81920,
            useAsync: true);
        using FileStream destinationStream = new(
            stagedPath,
            FileMode.Create,
            FileAccess.Write,
            FileShare.None,
            bufferSize: 81920,
            useAsync: true);

        await sourceStream.CopyToAsync(destinationStream);
        await destinationStream.FlushAsync();
        return stagedPath;
    }

    private async Task<string> StageStorageFileToTempAsync(StorageFile file, string stagedFileName)
    {
        if (!string.IsNullOrWhiteSpace(file.Path) && File.Exists(file.Path))
        {
            return ValidateLocalAttachmentSourcePath(file.Path);
        }

        string stagedPath = GetUiTempFilePath(stagedFileName);
        using Stream sourceStream = (await file.OpenReadAsync()).AsStreamForRead();
        using FileStream destinationStream = new(
            stagedPath,
            FileMode.Create,
            FileAccess.Write,
            FileShare.None,
            bufferSize: 81920,
            useAsync: true);

        await sourceStream.CopyToAsync(destinationStream);
        await destinationStream.FlushAsync();
        return stagedPath;
    }

    private void DeletePendingAttachmentSourceFile()
    {
        string? sourcePath = pendingAttachmentSourcePath;
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

    private async Task PickAttachmentFileAsync()
    {
        if (string.IsNullOrWhiteSpace(GetAttachableCurrentDialogKey()))
        {
            SetActionStatus("Сначала откройте чат, чтобы прикрепить файл.");
            return;
        }

        string? sourcePath = ShowDesktopOpenFileDialog(
            "Выберите файл",
            [new DesktopFileDialogFilter("Все файлы (*.*)", "*.*")],
            Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments));

        if (string.IsNullOrWhiteSpace(sourcePath))
        {
            SetActionStatus("Выбор файла отменён.");
            return;
        }

        FileInfo file = new(sourcePath);
        if (!file.Exists || (file.Attributes & System.IO.FileAttributes.Directory) == System.IO.FileAttributes.Directory)
        {
            throw new InvalidOperationException("Выберите существующий файл, а не папку.");
        }

        string stagedPath = await StageAttachmentSourceAsync(file.FullName);
        SetPendingAttachmentDraft(stagedPath, file.Name);
        if (GetApprovedCurrentDialogKey() is null)
        {
            SetActionStatus($"Файл {file.Name} подготовлен как черновик. Отправка станет доступна после открытия проверенного чата.");
        }
    }

    private ChatMessageRecord? TryGetCurrentChatMessage(string messageId)
    {
        if (string.IsNullOrWhiteSpace(GetResolvedCurrentDialogKey()))
        {
            return null;
        }

        return currentChatMessagesById.GetValueOrDefault(messageId);
    }

    private async Task RequestAttachmentDownloadAsync(string messageId, AttachmentRecord attachment)
    {
        string extension = Path.GetExtension(attachment.FileName);
        if (string.IsNullOrWhiteSpace(extension))
        {
            extension = ".bin";
        }
        string suggestedFileName = $"{Path.GetFileNameWithoutExtension(attachment.FileName)}{extension}";
        string? destinationPath = ShowDesktopSaveFileDialog(
            "Сохранить файл",
            suggestedFileName,
            [new DesktopFileDialogFilter($"Файл (*{extension})", $"*{extension}")],
            defaultExtension: extension,
            initialDirectory: Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments));
        if (string.IsNullOrWhiteSpace(destinationPath))
        {
            SetActionStatus("Сохранение файла отменено.");
            return;
        }

        await StartAttachmentDownloadAsync(messageId, destinationPath);
    }

    /// <summary>Скачивание в выбранный путь — общий хвост кнопки и тестового канала.</summary>
    private async Task StartAttachmentDownloadAsync(string messageId, string destinationPath)
    {
        ChatMessageRecord updated = await Task.Run(() =>
            rustBridge.RequestAttachmentDownload(messageId, destinationPath));
        SetActionStatus(updated.Attachment is null
            ? "Копия файла сохранена."
            : updated.Attachment.LocalBlobAvailable &&
              !updated.Attachment.State.Equals("requested", StringComparison.OrdinalIgnoreCase) &&
              !updated.Attachment.State.Equals("downloading", StringComparison.OrdinalIgnoreCase) &&
              !updated.Attachment.State.Equals("waiting_for_reconnect", StringComparison.OrdinalIgnoreCase) &&
              !updated.Attachment.State.Equals("verifying", StringComparison.OrdinalIgnoreCase)
                ? "Копия файла сохранена."
            : updated.Attachment.State.Equals("requested", StringComparison.OrdinalIgnoreCase)
                ? "Запрос на загрузку отправлен."
                : "Загрузка файла началась.");
        RefreshMessages();
        RefreshQueueDiagnostics();
        WakeChatRefresh();
        UpdateControlStates();
    }

    private async Task RevokeAttachmentAccessAsync(string messageId)
    {
        ChatMessageRecord updated = await Task.Run(() => rustBridge.RevokeAttachmentAccess(messageId));
        SetActionStatus(updated.Attachment?.State.ToLowerInvariant() switch
        {
            "revoked" => "Доступ к файлу отозван.",
            "failed" when updated.Attachment.LastError == "cancelled" => "Подготовка файла отменена.",
            _ => "Доступ к файлу изменён.",
        });
        RefreshMessages();
        RefreshQueueDiagnostics();
        WakeChatRefresh();
        UpdateControlStates();
    }

}
