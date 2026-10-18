using System.Diagnostics;
using System.Threading;
using Microsoft.UI.Dispatching;

namespace Omni.Windows.App;

internal sealed class RefreshCoordinator : IDisposable
{
    private readonly record struct PendingRefreshRequest(BackgroundRefreshRequest Request, long RequestId);

    private readonly DispatcherQueue dispatcherQueue;
    private readonly Func<BackgroundRefreshRequest, CancellationToken, Action<BackgroundRefreshResult>, Task> fetchAsync;
    private readonly Action<BackgroundRefreshResult> apply;
    private readonly object syncRoot = new();
    private PendingRefreshRequest? pendingRequest;
    private CancellationTokenSource? activeRefreshCts;
    private int workerActive;
    private long latestRequestId;

    public RefreshCoordinator(
        DispatcherQueue dispatcherQueue,
        Func<BackgroundRefreshRequest, CancellationToken, Action<BackgroundRefreshResult>, Task> fetchAsync,
        Action<BackgroundRefreshResult> apply)
    {
        this.dispatcherQueue = dispatcherQueue;
        this.fetchAsync = fetchAsync;
        this.apply = apply;
    }

    public void RequestRefresh(BackgroundRefreshRequest request)
    {
        long requestId = Interlocked.Increment(ref latestRequestId);
        lock (syncRoot)
        {
            pendingRequest = new PendingRefreshRequest(request, requestId);
            activeRefreshCts?.Cancel();
        }

        if (Interlocked.CompareExchange(ref workerActive, 1, 0) == 0)
        {
            _ = ProcessLoopAsync();
        }
    }

    public void CancelPending()
    {
        lock (syncRoot)
        {
            pendingRequest = null;
            activeRefreshCts?.Cancel();
        }
    }

    public void Dispose()
    {
        CancelPending();
        lock (syncRoot)
        {
            activeRefreshCts?.Dispose();
            activeRefreshCts = null;
        }
    }

    private async Task ProcessLoopAsync()
    {
        try
        {
            while (true)
            {
                PendingRefreshRequest? queuedRequest;
                CancellationTokenSource refreshCts;

                lock (syncRoot)
                {
                    queuedRequest = pendingRequest;
                    pendingRequest = null;

                    if (queuedRequest is null)
                    {
                        activeRefreshCts?.Dispose();
                        activeRefreshCts = null;
                        return;
                    }

                    activeRefreshCts?.Dispose();
                    activeRefreshCts = new CancellationTokenSource();
                    refreshCts = activeRefreshCts;
                }

                PendingRefreshRequest request = queuedRequest.Value;
                try
                {
                    await fetchAsync(
                        request.Request,
                        refreshCts.Token,
                        result =>
                        {
                            if (refreshCts.IsCancellationRequested ||
                                request.RequestId != Volatile.Read(ref latestRequestId))
                            {
                                return;
                            }

                            dispatcherQueue.TryEnqueue(() =>
                            {
                                if (refreshCts.IsCancellationRequested ||
                                    request.RequestId != Volatile.Read(ref latestRequestId))
                                {
                                    return;
                                }

                                apply(result);
                            });
                        });
                }
                catch (OperationCanceledException)
                {
                    // Ignore canceled refresh iterations.
                }
                catch (Exception ex)
                {
                    Debug.WriteLine($"Background refresh failed: {ex}");
                }

                lock (syncRoot)
                {
                    if (pendingRequest is null)
                    {
                        activeRefreshCts?.Dispose();
                        activeRefreshCts = null;
                        return;
                    }
                }
            }
        }
        finally
        {
            Interlocked.Exchange(ref workerActive, 0);

            lock (syncRoot)
            {
                if (pendingRequest is not null &&
                    Interlocked.CompareExchange(ref workerActive, 1, 0) == 0)
                {
                    _ = ProcessLoopAsync();
                }
            }
        }
    }
}
