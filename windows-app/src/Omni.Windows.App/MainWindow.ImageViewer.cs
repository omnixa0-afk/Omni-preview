using Omni.Windows.App.Core;

namespace Omni.Windows.App;

/// <summary>
/// Opening a picture on the whole window (Features/Shared/ImageViewer): a photo in the chat, an
/// avatar. One viewer for every place (docs/decisions/media-messages.md §2).
/// </summary>
public sealed partial class MainWindow
{
    private bool imageViewerWired;

    private void OpenImageViewer(string? path, string caption, string fileName)
    {
        if (string.IsNullOrWhiteSpace(path) || !File.Exists(path))
        {
            return;
        }

        if (!imageViewerWired)
        {
            imageViewerWired = true;
            ImageViewerHost.SaveRequested += SaveViewedImageAs;
        }

        ImageViewerHost.Show(path, caption, fileName);
    }

    private void SaveViewedImageAs(string sourcePath, string suggestedFileName)
    {
        string extension = Path.GetExtension(suggestedFileName) is { Length: > 0 } ext ? ext : ".jpg";
        string? destinationPath = ShowDesktopSaveFileDialog(
            "Сохранить изображение",
            suggestedFileName,
            [new DesktopFileDialogFilter($"Изображение (*{extension})", $"*{extension}")],
            defaultExtension: extension,
            initialDirectory: Environment.GetFolderPath(Environment.SpecialFolder.MyPictures));
        if (string.IsNullOrWhiteSpace(destinationPath))
        {
            return;
        }

        try
        {
            // The plaintext, not the file: a photo in Omni's storage is encrypted at rest.
            byte[] bytes = MediaFiles.ReadAllBytes(sourcePath)
                ?? throw new IOException("the picture can no longer be read");
            File.WriteAllBytes(destinationPath, bytes);
            SetActionStatus($"Сохранено: {Path.GetFileName(destinationPath)}");
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            SetActionStatus($"Не удалось сохранить: {NormalizeUserError(ex)}");
        }
    }

    /// The avatar's full picture: the image file the avatar is drawn from, if there is one.
    private void OpenAvatarViewer(string? avatarPath, string ownerName)
        => OpenImageViewer(avatarPath, ownerName, $"{SanitizeFileName(ownerName)}.jpg");
}
