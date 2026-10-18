using System;
using Microsoft.UI.Xaml;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    // The line above the message box says what an action just did ("Файл прикреплён", "Ошибка
    // отправки: …") and then goes away. It used to stay until something overwrote it — and what
    // overwrote it was the delivery status of the latest outgoing message, copied there on every
    // transcript refresh ("Доставлено" under a chat whose last message was incoming; owner removed it
    // 2026-10-10: the ticks in the bubble already say it).
    private static readonly TimeSpan ActionStatusLifetime = TimeSpan.FromSeconds(10);
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? actionStatusTimer;

    private void SetActionStatus(string text)
    {
        ActionStatusTextBlock.Text = text;
        bool shown = !string.IsNullOrWhiteSpace(text);
        ActionStatusTextBlock.Visibility = shown ? Visibility.Visible : Visibility.Collapsed;
        actionStatusTimer ??= CreateActionStatusTimer();
        actionStatusTimer.Stop();
        if (shown)
        {
            actionStatusTimer.Start();
        }
    }

    private Microsoft.UI.Dispatching.DispatcherQueueTimer CreateActionStatusTimer()
    {
        Microsoft.UI.Dispatching.DispatcherQueueTimer timer = DispatcherQueue.CreateTimer();
        timer.Interval = ActionStatusLifetime;
        timer.IsRepeating = false;
        timer.Tick += (_, _) =>
        {
            ActionStatusTextBlock.Text = string.Empty;
            ActionStatusTextBlock.Visibility = Visibility.Collapsed;
        };
        return timer;
    }

    private void SetContactsStatus(string text)
        => ContactsStatusTextBlock.Text = text;

    private void SetActionAndContactsStatus(string text)
    {
        SetContactsStatus(text);
        SetActionStatus(text);
    }

    private void SetActionAndContactManagementStatus(string text)
    {
        ContactManagementStatusTextBlock.Text = text;
        SetActionStatus(text);
    }

    private static void WithSelectionSyncSuppressed(ref bool suppressFlag, Action action)
    {
        bool previous = suppressFlag;
        suppressFlag = true;
        try
        {
            action();
        }
        finally
        {
            suppressFlag = previous;
        }
    }
}
