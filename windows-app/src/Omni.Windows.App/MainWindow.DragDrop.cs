using System;
using System.Collections.Generic;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Shared;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace Omni.Windows.App;

/// Files and pictures dragged into Omni (owner, 2026-10-30: «drag and drop где бы это могло
/// пригодиться»): onto the open 1:1 chat — its thread and its composer — and onto a row of the chat
/// list, which opens that chat (1:1 or group) with the file as its draft, as in Telegram. A group's
/// own thread and composer take drops in GroupConversationControl; a folder onto Workspaces makes a
/// repository there (WorkspacesSectionControl).
public sealed partial class MainWindow
{
    private void WireDragDrop()
    {
        DroppedFiles.TempDirectory = GetUiTempDirectoryPath();

        // The composer: its text box takes drags of text itself; files go to the chat as on the thread.
        ChatComposerBorder.AllowDrop = true;
        ChatComposerBorder.AddHandler(UIElement.DragOverEvent, new DragEventHandler(OnChatMessagesDragOver), handledEventsToo: true);
        ChatComposerBorder.AddHandler(UIElement.DropEvent, new DragEventHandler(OnChatMessagesDrop), handledEventsToo: true);
        ChatComposerBorder.DragLeave += OnChatMessagesDragLeave;

        ChatsListView.AllowDrop = true;
        ChatsListView.DragOver += OnChatListDragOver;
        ChatsListView.Drop += OnChatListDrop;
    }

    // The chat row under the pointer, if a file may go there (not an invitation, not a group we left).
    private ChatThreadListItem? ChatRowUnder(DragEventArgs e)
    {
        global::Windows.Foundation.Point point = e.GetPosition(null);
        return VisualTreeHelper.FindElementsInHostCoordinates(point, ChatsListView)
            .OfType<ListViewItem>()
            .Select(row => row.Content as ChatThreadListItem)
            .FirstOrDefault(row => row is not null && !row.IsGroupInvite);
    }

    private void OnChatListDragOver(object sender, DragEventArgs e)
    {
        ChatThreadListItem? row = DroppedFiles.CanAttach(e.DataView) ? ChatRowUnder(e) : null;
        e.AcceptedOperation = row is null ? DataPackageOperation.None : DataPackageOperation.Copy;
        if (row is not null)
        {
            e.DragUIOverride.Caption = $"Отправить в «{row.Title}»";
        }
    }

    private async void OnChatListDrop(object sender, DragEventArgs e)
    {
        if (!DroppedFiles.CanAttach(e.DataView) || ChatRowUnder(e) is not { } row)
        {
            return;
        }

        try
        {
            IReadOnlyList<StorageFile> files = await DroppedFiles.ReadAsync(e.DataView);
            // Open the chat as a click on the row does, then attach as a drop on it would.
            ChatsListView.SelectedItem = row;
            if (row.IsGroup)
            {
                GroupConversation.AttachDropped(files);
            }
            else
            {
                await AttachDroppedToCurrentChatAsync(files);
            }
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось обработать перетаскивание: {NormalizeUserError(ex)}");
        }
    }
}
