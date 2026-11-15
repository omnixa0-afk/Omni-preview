using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private async void OnSendClicked(object sender, RoutedEventArgs e)
    {
        string body = MessageInputTextBox.Text.Trim();
        string? peerSelector = GetApprovedCurrentDialogKey();
        bool hasAttachment = HasPendingAttachmentDraft();
        string? attachmentSourcePath = pendingAttachmentSourcePath;
        string? attachmentFileName = pendingAttachmentFileName;
        bool attachmentAsPhoto = pendingAttachmentAsPhoto;
        string? answering = chatReplyTo?.MessageId;

        if (string.IsNullOrWhiteSpace(peerSelector))
        {
            SetActionStatus("Чтобы отправить сообщение, откройте проверенный контакт.");
            return;
        }

        if (string.IsNullOrWhiteSpace(body) && !hasAttachment)
        {
            SetActionStatus("Введите сообщение или прикрепите файл.");
            return;
        }

        try
        {
            pendingChatAutoScrollToLatest = true;
            if (hasAttachment)
            {
                SetActionStatus("Подготовка файла к отправке...");
                UpdateControlStates();
            }

            ChatMessageRecord message = hasAttachment
                ? await Task.Run(() => rustBridge.SendAttachment(
                    peerSelector,
                    body,
                    attachmentSourcePath!,
                    attachmentFileName,
                    attachmentAsPhoto,
                    answering))
                : rustBridge.SendText(peerSelector, body, answering);
            MessageInputTextBox.Text = string.Empty;
            SetChatReply(null);
            if (hasAttachment)
            {
                ClearPendingAttachmentDraftIfCurrent(attachmentSourcePath!, preserveStatusText: true);
            }
            // The bubble shows where the message is (sending, delivered…); a line under the input
            // saying "queued" stayed up after it was delivered.
            SetActionStatus(string.Empty);
            RefreshMessages();
            RefreshQueueDiagnostics();
            WakeChatRefresh();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Ошибка отправки: {ex.Message}");
            RefreshMessages();
            RefreshQueueDiagnostics();
            WakeChatRefresh();
        }

        RefreshTransportStatus();
        UpdateControlStates();
    }

    /// A voice message from the composer: it answers what the text would have answered.
    private async void OnChatVoiceRecorded(object? sender, Features.Shared.VoiceRecording recording)
    {
        string? peerSelector = GetApprovedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(peerSelector))
        {
            SetActionStatus("Чтобы отправить голосовое, откройте проверенный контакт.");
            return;
        }

        string? answering = chatReplyTo?.MessageId;
        try
        {
            pendingChatAutoScrollToLatest = true;
            await Task.Run(() =>
            {
                string file = rustBridge.StageVoiceRecording(recording.Packets, recording.Waveform);
                rustBridge.SendAttachment(peerSelector, string.Empty, file, null, false, answering, asVoice: true);
            });
            SetChatReply(null);
            SetActionStatus(string.Empty);
        }
        catch (Exception ex)
        {
            SetActionStatus($"Голосовое не отправилось: {NormalizeUserError(ex)}");
        }

        RefreshMessages();
        RefreshQueueDiagnostics();
        WakeChatRefresh();
    }

    /// A video message from the composer: it answers what the text would have answered.
    private async void OnChatVideoNoteRecorded(object? sender, Features.Shared.VideoNoteRecording recording)
    {
        string? peerSelector = GetApprovedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(peerSelector))
        {
            SetActionStatus("Чтобы отправить видеосообщение, откройте проверенный контакт.");
            return;
        }

        string? answering = chatReplyTo?.MessageId;
        try
        {
            pendingChatAutoScrollToLatest = true;
            await Task.Run(() =>
            {
                string file = rustBridge.StageVideoNote(recording.Video, recording.Preview);
                rustBridge.SendAttachment(peerSelector, string.Empty, file, null, false, answering, asVideoNote: true);
            });
            SetChatReply(null);
            SetActionStatus(string.Empty);
        }
        catch (Exception ex)
        {
            SetActionStatus($"Видеосообщение не отправилось: {NormalizeUserError(ex)}");
        }

        RefreshMessages();
        RefreshQueueDiagnostics();
        WakeChatRefresh();
    }

    // The message the next one answers, shown over the field until it goes or the cross is pressed.
    private Features.Shared.MessageRow? chatReplyTo;

    private void OnMessageReplyRequested(object? sender, Features.Shared.MessageRow row) => SetChatReply(row);

    private void OnMessageReactionChosen(object? sender, string emoji)
    {
        if (sender is not Features.Shared.MessageBubble { Row: { } row })
        {
            return;
        }

        try
        {
            rustBridge.ReactToMessage(row.MessageId, emoji);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось поставить реакцию: {ex.Message}");
        }
    }

    private void SetChatReply(Features.Shared.MessageRow? row)
    {
        chatReplyTo = row;
        if (row is null)
        {
            ChatComposerBorder.ClearReply();
            return;
        }

        string text = row.Body.Length > 0 ? row.Body : row.AttachmentLabel;
        ChatComposerBorder.ShowReply(row.IsOutgoing ? "Вы" : ChatIdentityTitleTextBlock.Text, text.Length > 0 ? text : "Сообщение");
    }

    /// A quote goes to its message when the open part of the transcript holds it.
    private void OnMessageQuoteOpened(object? sender, Features.Shared.MessageRow row)
    {
        if (messageItems.FirstOrDefault(item => item.MessageId == row.QuotedMessageId) is { } target)
        {
            MessagesListView.ScrollIntoView(target, ScrollIntoViewAlignment.Leading);
        }
        else
        {
            SetActionStatus("Это сообщение выше в истории: прокрутите к нему.");
        }
    }

    private void OnMessageInputTextChanged(object sender, TextChangedEventArgs e)
        => UpdateControlStates();

    private bool HasPendingAttachmentDraft()
        => !string.IsNullOrWhiteSpace(pendingAttachmentSourcePath);

    private static string BuildStagedAttachmentFileName(string originalFileName)
    {
        string extension = Path.GetExtension(originalFileName);
        if (string.IsNullOrWhiteSpace(extension) || extension.Length > 16)
        {
            extension = ".bin";
        }

        return $"omni-attachment-{Guid.NewGuid():N}{extension}";
    }

    private async void OnAttachFilesButtonClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            await PickAttachmentFileAsync();
            WakeChatRefresh();
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось прикрепить файл: {NormalizeUserError(ex)}");
            UpdateControlStates();
        }
    }

    private void OnRemoveDraftAttachmentClicked(object sender, RoutedEventArgs e)
    {
        ClearPendingAttachmentDraft();
        WakeChatRefresh();
    }

    private void OnChatMessagesDragLeave(object sender, DragEventArgs e)
    {
        ChatDropOverlayBorder.Visibility = Visibility.Collapsed;
    }

    private void OnChatMessagesDragOver(object sender, DragEventArgs e)
    {
        bool canAccept =
            !string.IsNullOrWhiteSpace(GetAttachableCurrentDialogKey()) &&
            Features.Shared.DroppedFiles.CanAttach(e.DataView);

        if (!canAccept)
        {
            e.AcceptedOperation = DataPackageOperation.None;
            ChatDropOverlayBorder.Visibility = Visibility.Collapsed;
            return;
        }

        e.AcceptedOperation = DataPackageOperation.Copy;
        e.DragUIOverride.Caption = "Отпустите, чтобы прикрепить файл";
        ChatDropOverlayBorder.Visibility = Visibility.Visible;
    }

    private async void OnChatMessagesDrop(object sender, DragEventArgs e)
    {
        ChatDropOverlayBorder.Visibility = Visibility.Collapsed;

        if (string.IsNullOrWhiteSpace(GetAttachableCurrentDialogKey()) ||
            !Features.Shared.DroppedFiles.CanAttach(e.DataView))
        {
            return;
        }

        try
        {
            await AttachDroppedToCurrentChatAsync(await Features.Shared.DroppedFiles.ReadAsync(e.DataView));
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось обработать перетаскивание: {NormalizeUserError(ex)}");
            WakeChatRefresh();
        }
    }

    /// Files dropped on the open 1:1 chat (its thread, its composer, its row in the list): the first
    /// becomes the draft, as «Прикрепить» makes it.
    private async Task AttachDroppedToCurrentChatAsync(IReadOnlyList<StorageFile> files)
    {
        if (files.Count == 0)
        {
            SetActionStatus("Можно перетащить файл или картинку, а не папку.");
            return;
        }

        StorageFile file = files[0];
        string stagedPath = await StageAttachmentSourceAsync(file);
        SetPendingAttachmentDraft(stagedPath, file.Name);
        if (GetApprovedCurrentDialogKey() is null)
        {
            SetActionStatus($"Файл {file.Name} подготовлен как черновик. Отправка станет доступна после открытия проверенного чата.");
        }
        else if (Features.Shared.DroppedFiles.MoreThanOneNote(files) is { } note)
        {
            SetActionStatus(note);
        }
        WakeChatRefresh();
    }

    private void OnAttachmentPhotoClicked(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement { Tag: string messageId } ||
            TryGetCurrentChatMessage(messageId) is not { Attachment: { } attachment } message)
        {
            return;
        }

        if (string.IsNullOrWhiteSpace(attachment.LocalCopyPath))
        {
            SetActionStatus("Фото ещё загружается.");
            return;
        }

        string author = message.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase)
            ? "Вы"
            : ChatIdentityTitleTextBlock.Text;
        OpenImageViewer(
            attachment.LocalCopyPath,
            $"{author} · {MainWindowPresentation.FormatBubbleTime(message.CreatedAtUnixMs)}",
            attachment.FileName);
    }

    private async void OnAttachmentCardClicked(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement element ||
            element.Tag is not string messageId)
        {
            return;
        }

        ChatMessageRecord? message = TryGetCurrentChatMessage(messageId);
        if (message?.Attachment is null)
        {
            SetActionStatus("Сведения о файле недоступны.");
            return;
        }

        ShowAttachmentActions(element, message.Id, message.Attachment);
    }

    /// The menu of a file in a bubble — the 1:1 chat's and a group's alike.
    private void ShowAttachmentActions(FrameworkElement element, string messageId, AttachmentRecord attachment)
    {
        attachmentActionsFlyout.Items.Clear();

        if (attachment.LocalSavedPath is { Length: > 0 } savedPath && File.Exists(savedPath))
        {
            MenuFlyoutItem openItem = new() { Text = "Открыть" };
            openItem.Click += async (_, _) => await OpenLocalFileAsync(savedPath, attachment.FileName, attachment.OpensAsProgram);
            attachmentActionsFlyout.Items.Add(openItem);
        }

        bool hasSavedLocalCopy = !string.IsNullOrWhiteSpace(attachment.LocalSavedPath);
        if (MainWindowPresentation.CanDownloadAttachment(attachment))
        {
            MenuFlyoutItem downloadItem = new()
            {
                Text = hasSavedLocalCopy ? "Сохранить ещё раз" : "Скачать",
            };
            downloadItem.Click += async (_, _) => await RequestAttachmentDownloadAsync(messageId, attachment);
            attachmentActionsFlyout.Items.Add(downloadItem);
        }

        if (MainWindowPresentation.IsAttachmentPreparing(attachment))
        {
            // Nothing was offered yet: the core stops reading the file and the message fails.
            MenuFlyoutItem cancelItem = new()
            {
                Text = "Отменить подготовку",
            };
            cancelItem.Click += async (_, _) => await RevokeAttachmentAccessAsync(messageId);
            attachmentActionsFlyout.Items.Add(cancelItem);
        }

        if (MainWindowPresentation.CanRevokeAttachmentAccess(attachment))
        {
            if (attachmentActionsFlyout.Items.Count > 0)
            {
                attachmentActionsFlyout.Items.Add(new MenuFlyoutSeparator());
            }
            MenuFlyoutItem revokeItem = new()
            {
                Text = "Отозвать доступ",
            };
            revokeItem.Click += async (_, _) => await RevokeAttachmentAccessAsync(messageId);
            attachmentActionsFlyout.Items.Add(revokeItem);
        }

        if (attachmentActionsFlyout.Items.Count == 0)
        {
            MenuFlyoutItem unavailableItem = new()
            {
                Text = attachment.LastError ?? "Файл недоступен",
                IsEnabled = false,
            };
            attachmentActionsFlyout.Items.Add(unavailableItem);
        }

        attachmentActionsFlyout.Items.Add(new MenuFlyoutSeparator());
        attachmentActionsFlyout.Items.Add(new MenuFlyoutItem { Text = "Отмена" });
        attachmentActionsFlyout.ShowAt(element);
    }

    private async Task OpenLocalFileAsync(string path, string fileName, bool opensAsProgram)
    {
        if (opensAsProgram && !await ConfirmOpenProgramAsync(fileName))
        {
            return;
        }

        try
        {
            // A file in Omni's storage (a photo that downloaded by itself) is encrypted at rest: the
            // outside program gets a plain copy, cleared at the next start.
            string openable = await Task.Run(() => MediaFiles.OpenablePath(path, fileName));
            global::Windows.Storage.StorageFile file = await global::Windows.Storage.StorageFile.GetFileFromPathAsync(openable);
            await global::Windows.System.Launcher.LaunchFileAsync(file);
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось открыть файл: {ex.Message}");
        }
    }

    /// <summary>
    /// A received file that runs as a program or a script (V-18): opening it is the user's decision,
    /// taken knowing what it is — the name alone may pass for a document.
    /// </summary>
    private async Task<bool> ConfirmOpenProgramAsync(string fileName)
    {
        ContentDialog dialog = new()
        {
            Title = "Это программа",
            Content = new TextBlock
            {
                Text = $"«{fileName}» запустится как программа или сценарий и получит те же права, что и вы. "
                    + "Открывайте, только если ждали именно эту программу от этого человека.",
                TextWrapping = TextWrapping.WrapWholeWords,
            },
            PrimaryButtonText = "Запустить",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Close,
            XamlRoot = Content.XamlRoot,
            RequestedTheme = Presentation.MainWindowPresentation.ActiveTheme,
        };
        try
        {
            return await dialog.ShowAsync() == ContentDialogResult.Primary;
        }
        catch (Exception)
        {
            // Another dialog is already open (WinUI allows one at a time): do not run it.
            return false;
        }
    }

    private void OnOpenContactsFromChatsClicked(object sender, RoutedEventArgs e)
    {
        // «Добавить контакт» goes straight to adding; «Открыть контакты» (people, no conversation yet) to the list.
        AuthenticatedNavigationView.SelectedItem = ContactsNavigationItem;
        ApplyContactsWorkspaceMode(chatPartnerCount > 0 ? ContactsWorkspaceMode.Directory : ContactsWorkspaceMode.Add);
    }

    private void OnChatHeaderMenuClicked(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement element)
        {
            chatHeaderMenuFlyout.ShowAt(element);
        }
    }

    private void OnEndCurrentChatSessionClicked(object sender, RoutedEventArgs e)
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(dialogKey) || !CanEndCurrentChatSession())
        {
            SetActionStatus("Сначала откройте одобренный чат с активной или возобновляемой сессией.");
            return;
        }

        try
        {
            currentChatSessionSummary = rustBridge.EndChatSession(dialogKey);
            RenderChatSessionSummary(currentChatSessionSummary);
            SetActionStatus("Сессия завершена. Новое подключение поднимется при следующем явном действии.");
            RefreshChatHeaderSummary();
            RefreshQueueDiagnostics();
            WakeChatRefresh();
            RefreshTransportStatus();
            UpdateControlStates();
        }
        catch (Exception ex)
        {
            SetActionStatus(ex.Message);
        }
    }

    private async void OnDeleteCurrentChatContactClicked(object sender, RoutedEventArgs e)
    {
        if (currentChatHeaderSummary?.ContactCard is null)
        {
            SetActionStatus("Сначала откройте чат, привязанный к контакту.");
            return;
        }

        if (!await ConfirmForgetContactAsync(currentChatHeaderSummary.ContactCard.Contact.Petname))
        {
            return;
        }

        try
        {
            ContactSnapshot removed = rustBridge.ForgetContactAndResetPeer(currentChatHeaderSummary.ContactCard.Contact.ContactId);
            CloseChatContactInfo();
            CloseCurrentChatSelection();
            windowState.SelectedContactId = null;
            selectedContactCard = null;
            ContactsStatusTextBlock.Text = $"Контакт {removed.Petname} удалён из памяти.";
            SetActionStatus($"Контакт {removed.Petname} удалён из памяти.");
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
            SetActionStatus(ex.Message);
        }
    }

}
