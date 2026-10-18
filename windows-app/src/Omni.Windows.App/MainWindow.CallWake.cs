using Omni.Windows.App.Core;

namespace Omni.Windows.App;

/// <summary>
/// The core wakes the shell the moment a call changes (<c>omni_calls_wait_change</c>), instead of
/// the shell finding out on its next poll — 2 s with no call, 3 s with the window hidden. That poll
/// was most of the delay before an unattended remote-control session showed a picture: the owner's
/// core accepted in ~0.1 s, and the hidden shell started media ~1.1 s later (live 2026-10-06).
/// The periodic poll stays as the backstop; this only makes it come early.
/// </summary>
public sealed partial class MainWindow
{
    private const uint CallWakeTimeoutMs = 30_000;
    private Thread? callWakeThread;

    private void StartCallChangeWatcher()
    {
        if (callWakeThread is not null)
        {
            return;
        }

        callWakeThread = new Thread(() =>
        {
            ulong revision = NativeMethods.omni_calls_wait_change(0, 0);
            while (!shutdownStarted)
            {
                ulong next = NativeMethods.omni_calls_wait_change(revision, CallWakeTimeoutMs);
                if (next == revision)
                {
                    continue; // timed out — nothing changed
                }

                revision = next;
                _ = DispatcherQueue.TryEnqueue(() =>
                {
                    if (shutdownStarted)
                    {
                        return;
                    }

                    RefreshCallInbox();
                    nextCallRefreshAt = DateTimeOffset.UtcNow + GetCallRefreshCadence();
                });
            }
        })
        {
            IsBackground = true,
            Name = "omni-call-wake",
        };
        callWakeThread.Start();
    }
}
