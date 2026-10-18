using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

/// <summary>
/// Unread counters (docs/decisions/unread-counters.md): a pill on each chat row and the total on the
/// «Чаты» rail item. The core counts — the shell only says which chat is on screen. A chat is read
/// while it is open in the focused window; its pill goes at once, the core's mark follows.
/// </summary>
public sealed partial class MainWindow
{
    // Per row key, from the last unfiltered list: the rail's total must not shrink with a search.
    private readonly Dictionary<string, int> unreadByDialog = new(StringComparer.Ordinal);

    /// The chat the user is looking at: open, in the Chats section, in the focused window.
    private string? OnScreenChatDialogKey()
    {
        if (!IsWindowInFront() || windowState.CurrentSection != AuthenticatedShellSection.Chats)
        {
            return null;
        }

        if (currentGroupRowKey is not null)
        {
            return currentGroupRowKey.StartsWith(GroupInviteRowPrefix, StringComparison.Ordinal) ? null : currentGroupRowKey;
        }

        return GetResolvedCurrentDialogKey();
    }

    /// Asked of Windows, not of `isWindowActive`: that flag starts true and a window launched without
    /// focus never gets the Deactivated that would clear it — a chat open in such a window read itself
    /// (live 2026-10-17, stand L started by a scheduled task).
    private bool IsWindowInFront()
    {
        nint hwnd = WindowHandle;
        return GetForegroundWindow() == hwnd && !IsIconic(hwnd);
    }

    /// On screen at all: shown and not minimized (focus aside).
    private bool IsWindowInSight()
    {
        nint hwnd = WindowHandle;
        return IsWindowVisible(hwnd) && !IsIconic(hwnd);
    }

    // Taken once (the refresh loop asks every tick); the window keeps its handle for life.
    private nint windowHandle;

    private nint WindowHandle => windowHandle != 0
        ? windowHandle
        : windowHandle = WinRT.Interop.WindowNative.GetWindowHandle(this);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern bool IsWindowVisible(nint hwnd);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern nint GetForegroundWindow();

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern bool IsIconic(nint hwnd);

    /// A fresh list from the core (before the search filter): take its counts, read the chat on screen.
    private void NoteUnreadCounts(IReadOnlyList<ChatThreadListItem> threads)
    {
        unreadByDialog.Clear();
        foreach (ChatThreadListItem thread in threads)
        {
            unreadByDialog[thread.DialogKey] = thread.UnreadCount;
        }

        string? onScreen = OnScreenChatDialogKey();
        ChatThreadListItem? open = onScreen is null
            ? null
            : threads.FirstOrDefault(thread => string.Equals(thread.DialogKey, onScreen, StringComparison.Ordinal));
        if (open is not null)
        {
            ReadChat(open);
        }

        UpdateChatsUnreadBadge();
    }

    /// The chat on screen may have changed (opened, window focused): read it in the shown list.
    private void ReadChatOnScreen()
    {
        string? onScreen = OnScreenChatDialogKey();
        if (onScreen is null)
        {
            return;
        }

        ChatThreadListItem? open = chatThreadItems.FirstOrDefault(
            thread => string.Equals(thread.DialogKey, onScreen, StringComparison.Ordinal));
        if (open is not null)
        {
            ReadChat(open);
            UpdateChatsUnreadBadge();
        }
    }

    private void ReadChat(ChatThreadListItem thread)
    {
        if (thread.UnreadCount == 0)
        {
            return;
        }

        thread.ClearUnread();
        unreadByDialog[thread.DialogKey] = 0;
        string dialogKey = thread.DialogKey;
        // The core's write off the UI thread; the next list refresh sees the moved mark.
        _ = Task.Run(() =>
        {
            try
            {
                rustBridge.MarkChatRead(dialogKey);
            }
            catch (Exception ex)
            {
                // The pill comes back on the next refresh if the mark did not move — nothing is lost.
                AppendNavRefreshLog("chat.mark_read", ex);
            }
        });
    }

    private void UpdateChatsUnreadBadge()
    {
        int total = unreadByDialog.Values.Sum();
        if (ChatsNavigationItem.InfoBadge is InfoBadge shown && shown.Value == total)
        {
            return;
        }

        ChatsNavigationItem.InfoBadge = total > 0 ? new InfoBadge { Value = total } : null;
    }
}
