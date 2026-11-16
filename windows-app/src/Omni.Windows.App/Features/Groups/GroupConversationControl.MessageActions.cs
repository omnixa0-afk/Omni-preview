using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Shared;

namespace Omni.Windows.App.Features.Groups;

// Deleting, pinning, forwarding and picking lines of the group.
public sealed partial class GroupConversationControl
{
    private MessageSelection? selection;
    private bool messageActionsHooked;

    private MessageSelection Selection => selection ??= new MessageSelection(() => messages, GroupSelectionBar);

    private void HookMessageActions()
    {
        if (messageActionsHooked)
        {
            return;
        }

        messageActionsHooked = true;
        GroupSelectionBar.CancelRequested += (_, _) => Selection.Stop();
        GroupSelectionBar.CopyRequested += (_, _) =>
        {
            MessageActions.Copy(Selection.Picked);
            Selection.Stop();
        };
        GroupSelectionBar.ForwardRequested += async (_, _) => await ForwardAsync(Selection.Picked);
        GroupSelectionBar.DeleteRequested += async (_, _) => await DeleteAsync(Selection.Picked);
        GroupPinnedBar.Opened += (_, id) => GoToLine(id);
        GroupPinnedBar.UnpinRequested += (_, id) => Pin(id, false);
    }

    private void OnMessagePinRequested(object? sender, MessageRow row) => Pin(row.MessageId, !row.IsPinned);

    private void OnMessageSelectRequested(object? sender, MessageRow row) => Selection.Start(row);

    private void OnMessageSelectionToggled(object? sender, MessageRow row) => Selection.Toggle(row);

    private async void OnMessageForwardRequested(object? sender, MessageRow row) => await ForwardAsync([row]);

    private async void OnMessageDeleteRequested(object? sender, MessageRow row) => await DeleteAsync([row]);

    private void Pin(string messageId, bool pinned)
    {
        if (bridge is null)
        {
            return;
        }

        try
        {
            bridge.PinMessage(messageId, pinned);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось закрепить: {ex.Message}");
        }
    }

    private async Task ForwardAsync(List<MessageRow> rows)
    {
        if (rows.Count == 0 || bridge is null)
        {
            return;
        }

        string? problem = await MessageActions.ForwardAsync(XamlRoot, bridge, rows);
        SetStatus(problem ?? string.Empty);
        Selection.Stop();
        RefreshMessages();
    }

    private async Task DeleteAsync(List<MessageRow> rows)
    {
        if (rows.Count == 0 || bridge is null)
        {
            return;
        }

        string? everyone = rows.All(r => r.IsOutgoing) ? "Удалить у всех в группе" : null;
        if (await MessageActions.AskDeleteAsync(XamlRoot, rows.Count, everyone) is not { } forEveryone)
        {
            return;
        }

        try
        {
            foreach (MessageRow row in rows)
            {
                bridge.DeleteMessage(row.MessageId, forEveryone);
            }
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось удалить: {ex.Message}");
        }

        Selection.Stop();
        RefreshMessages();
    }

    private void GoToLine(string messageId)
    {
        if (messages.FirstOrDefault(item => item.MessageId == messageId) is { } target)
        {
            GroupMessagesList.ScrollIntoView(target, ScrollIntoViewAlignment.Leading);
        }
    }
}
