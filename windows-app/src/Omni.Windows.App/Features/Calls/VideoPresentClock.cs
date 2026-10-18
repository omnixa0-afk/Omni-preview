using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The clock the remote-video presenter runs on: a steady ~60 Hz tick, independent of the network.
///
/// It exists because presentation used to be driven by the inbound datagram batch. Frames reached
/// the swapchain in bursts shaped by packet arrival — which is not what the eye wants and not what
/// any other client does. Telegram and Discord paint video on the display's clock; a burst of
/// packets fills a jitter buffer, it does not lurch the picture. The playout decision (which frame
/// is due) already lives in the native jitter buffer; this only decides how often we ask it.
///
/// 60 Hz rather than the panel's exact refresh: video is 15-30 fps, so a due frame reaches the
/// swapchain within one tick of becoming due, and DWM composites whatever is there at its own rate.
/// A waitable timer created with CREATE_WAITABLE_TIMER_HIGH_RESOLUTION keeps that tick accurate
/// without raising the process-wide timer resolution (a plain .NET timer quantises to ~15.6 ms,
/// which is its own source of unevenness at these intervals).
/// </summary>
internal sealed class VideoPresentClock : IDisposable
{
    internal const int DefaultHz = 60;

    private const uint CreateWaitableTimerHighResolution = 0x00000002;
    private const uint TimerAllAccess = 0x1F0003;
    private const int CancellationWaitIndex = 1;

    private readonly int intervalMs;
    private readonly CancellationToken cancellation;
    private readonly AutoResetEvent? timer;
    private readonly WaitHandle[] waits;

    internal VideoPresentClock(CancellationToken cancellation, int hz = DefaultHz)
    {
        this.cancellation = cancellation;
        intervalMs = Math.Max(1, 1000 / Math.Max(1, hz));
        timer = TryCreatePeriodicTimer(intervalMs);
        waits = timer is null
            ? [cancellation.WaitHandle]
            : [timer, cancellation.WaitHandle];
    }

    /// <summary>Is the tick coming from a real kernel timer, or from the fallback sleep?</summary>
    internal bool IsHighResolution => timer is not null;

    /// <summary>
    /// Blocks until the next tick. Returns false once cancellation is signalled — so the presenter
    /// loop is a plain <c>while (clock.WaitForNextTick())</c> with no second exit condition.
    /// A timer wait that times out still counts as a tick: a missed tick must not stop the picture.
    /// </summary>
    internal bool WaitForNextTick()
    {
        if (cancellation.IsCancellationRequested)
        {
            return false;
        }

        if (timer is null)
        {
            // No timer object — the cancellation handle doubles as the sleep, so a stop is still
            // observed within one interval instead of at the end of it.
            return !cancellation.WaitHandle.WaitOne(intervalMs);
        }

        return WaitHandle.WaitAny(waits, intervalMs * 4) != CancellationWaitIndex;
    }

    public void Dispose() => timer?.Dispose();

    /// <summary>
    /// A periodic waitable timer wrapped as a <see cref="WaitHandle"/> so it can be waited on
    /// together with the cancellation handle. Null when the timer cannot be created — the caller
    /// falls back to a plain interval sleep, which is less even but never leaves video unpresented.
    /// </summary>
    private static AutoResetEvent? TryCreatePeriodicTimer(int intervalMs)
    {
        IntPtr raw = CreateWaitableTimerExW(IntPtr.Zero, null, CreateWaitableTimerHighResolution, TimerAllAccess);
        if (raw == IntPtr.Zero)
        {
            // Pre-1803 flag support / policy refusal: a normal waitable timer still paces better
            // than nothing.
            raw = CreateWaitableTimerExW(IntPtr.Zero, null, 0, TimerAllAccess);
        }

        if (raw == IntPtr.Zero)
        {
            return null;
        }

        // Negative due time = relative, in 100 ns units. Period is milliseconds.
        long dueTime = -10_000L * intervalMs;
        if (!SetWaitableTimer(raw, ref dueTime, intervalMs, IntPtr.Zero, IntPtr.Zero, false))
        {
            CloseHandle(raw);
            return null;
        }

        AutoResetEvent wrapper = new(false);
        // Setting SafeWaitHandle does not close the handle it replaces — close the throwaway event
        // the constructor just made, or it leaks for the length of the call.
        wrapper.SafeWaitHandle.Dispose();
        wrapper.SafeWaitHandle = new SafeWaitHandle(raw, ownsHandle: true);
        return wrapper;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateWaitableTimerExW(
        IntPtr timerAttributes,
        string? timerName,
        uint flags,
        uint desiredAccess);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetWaitableTimer(
        IntPtr timer,
        ref long dueTime,
        int period,
        IntPtr completionRoutine,
        IntPtr completionArg,
        [MarshalAs(UnmanagedType.Bool)] bool resume);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr handle);
}
