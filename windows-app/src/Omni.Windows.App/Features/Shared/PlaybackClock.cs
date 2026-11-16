using System.Diagnostics;

namespace Omni.Windows.App.Features.Shared;

/// A player's position, smooth. The system player reports it in coarse steps, so a progress line fed
/// from it directly jumps; this runs on its own clock between reports and only follows a report that
/// differs by more than ordinary jitter (a seek, a stall).
internal sealed class PlaybackClock
{
    /// Reports wander by about this much around the true position.
    private const double JitterMs = 300;

    private readonly Stopwatch running = new();
    private double anchorMs;

    /// `reportedMs`: what the player says now. The result never goes back on ordinary jitter.
    internal double Read(double reportedMs, bool playing)
    {
        if (!playing)
        {
            running.Reset();
            anchorMs = reportedMs;
            return reportedMs;
        }

        double smooth = anchorMs + running.Elapsed.TotalMilliseconds;
        if (!running.IsRunning || Math.Abs(smooth - reportedMs) > JitterMs)
        {
            anchorMs = reportedMs;
            running.Restart();
            return reportedMs;
        }

        return smooth;
    }
}
