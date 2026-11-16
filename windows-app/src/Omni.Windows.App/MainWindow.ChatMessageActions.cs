using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Shared;

namespace Omni.Windows.App;

// Deleting, pinning, forwarding and picking messages of the open 1:1 chat.
public sealed partial class MainWindow
{
    private MessageSelection? chatSelection;
    private string messageActionsSeen = string.Empty;

    private MessageSelection ChatSelection => chatSelection ??= new MessageSelection(() => messageItems, ChatSelectionBar);

    private void HookChatMessageActions()
    {
        ChatSelectionBar.CancelRequested += (_, _) => ChatSelection.Stop();
        ChatSelectionBar.CopyRequested += (_, _) =>
        {
            MessageActions.Copy(ChatSelection.Picked);
            ChatSelection.Stop();
        };
        ChatSelectionBar.ForwardRequested += async (_, _) => await ForwardChatMessagesAsync(ChatSelection.Picked);
        ChatSelectionBar.DeleteRequested += async (_, _) => await DeleteChatMessagesAsync(ChatSelection.Picked);
        ChatPinnedBar.Opened += (_, id) => GoToChatMessage(id);
        ChatPinnedBar.UnpinRequested += (_, id) => PinChatMessage(id, false);
    }

    private void OnMessagePinRequested(object? sender, MessageRow row) => PinChatMessage(row.MessageId, !row.IsPinned);

    private void OnMessageSelectRequested(object? sender, MessageRow row) => ChatSelection.Start(row);

    private void OnMessageSelectionToggled(object? sender, MessageRow row) => ChatSelection.Toggle(row);

    private async void OnMessageForwardRequested(object? sender, MessageRow row) => await ForwardChatMessagesAsync([row]);

    private async void OnMessageDeleteRequested(object? sender, MessageRow row) => await DeleteChatMessagesAsync([row]);

    private void PinChatMessage(string messageId, bool pinned)
    {
        try
        {
            rustBridge.PinMessage(messageId, pinned);
            RefreshMessageActions(force: true);
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось закрепить: {ex.Message}");
        }
    }

    private async Task ForwardChatMessagesAsync(List<MessageRow> rows)
    {
        if (rows.Count == 0 || Content.XamlRoot is not { } root)
        {
            return;
        }

        string? problem = await MessageActions.ForwardAsync(root, rustBridge, rows);
        SetActionStatus(problem ?? string.Empty);
        ChatSelection.Stop();
        RefreshMessages();
        WakeChatRefresh();
    }

    private async Task DeleteChatMessagesAsync(List<MessageRow> rows)
    {
        if (rows.Count == 0 || Content.XamlRoot is not { } root)
        {
            return;
        }

        // Only one's own can go from the other side.
        string? everyone = rows.All(r => r.IsOutgoing) ? "Удалить и у собеседника" : null;
        if (await MessageActions.AskDeleteAsync(root, rows.Count, everyone) is not { } forEveryone)
        {
            return;
        }

        try
        {
            foreach (MessageRow row in rows)
            {
                rustBridge.DeleteMessage(row.MessageId, forEveryone);
            }
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось удалить: {ex.Message}");
        }

        ChatSelection.Stop();
        RefreshMessageActions(force: true);
        WakeChatRefresh();
    }

    /// A deletion or a pin, ours or from the other side: they change no line's time, so a feed reading
    /// by cursor never sees them. The core's mark says when to look.
    private void RefreshMessageActions(bool force = false)
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(dialogKey) || !rustBridge.IsInitialized)
        {
            ChatPinnedBar.Show([]);
            return;
        }

        MessageActionsView view = rustBridge.GetMessageActions(dialogKey);
        string seen = $"{dialogKey}|{view.Mark}";
        if (!force && seen == messageActionsSeen)
        {
            return;
        }

        messageActionsSeen = seen;
        ChatPinnedBar.Show(view.Pinned);
        HashSet<string> pinned = view.Pinned.Select(p => p.MessageId).ToHashSet(StringComparer.Ordinal);
        bool removed = false;
        foreach (string id in view.Deleted)
        {
            if (currentChatMessageItemsById.TryGetValue(id, out ChatMessageListItem? gone))
            {
                messageItems.Remove(gone);
                currentChatMessageItemsById.Remove(id);
                currentChatMessagesById.Remove(id);
                currentChatSecurityDetailsByMessageId.Remove(id);
                removed = true;
            }
        }

        foreach (ChatMessageListItem item in messageItems)
        {
            item.IsPinned = pinned.Contains(item.MessageId);
        }

        if (removed)
        {
            UpdateTranscriptEmptyState();
            TranscriptLayout.Apply(messageItems, DateTime.Today, transcriptPeer);
        }
    }

    private void GoToChatMessage(string messageId)
    {
        if (messageItems.FirstOrDefault(item => item.MessageId == messageId) is { } target)
        {
            MessagesListView.ScrollIntoView(target, ScrollIntoViewAlignment.Leading);
        }
        else
        {
            SetActionStatus("Это сообщение выше в истории: прокрутите к нему.");
        }
    }
}
