using System.Runtime.InteropServices;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// Coarse processor load for the capability passport ("low" / "medium" / "high"): it decides
/// whether this machine may encode video in software at all
/// (docs/decisions/per-pair-video-codec-selection.md §2, §4.4). Whole-system busy time since the
/// previous reading (GetSystemTimes); the first reading of a process measures over a short sample.
/// </summary>
internal static class ProcessorLoad
{
    private static readonly object Sync = new();
    private static ulong lastIdle;
    private static ulong lastTotal;

    internal static string Read()
    {
        double? busy = SampleBusyFraction();
        return Classify(busy);
    }

    /// <summary>Under 35 % busy is "low", from 75 % "high"; unknown counts as "medium".</summary>
    internal static string Classify(double? busyFraction)
        => busyFraction switch
        {
            null => "medium",
            < 0.35 => "low",
            >= 0.75 => "high",
            _ => "medium",
        };

    private static double? SampleBusyFraction()
    {
        lock (Sync)
        {
            if (!TryReadTimes(out ulong idle, out ulong total))
            {
                return null;
            }

            if (lastTotal == 0)
            {
                // No previous reading: take a short one now.
                Thread.Sleep(150);
                lastIdle = idle;
                lastTotal = total;
                if (!TryReadTimes(out idle, out total))
                {
                    return null;
                }
            }

            ulong idleDelta = idle - lastIdle;
            ulong totalDelta = total - lastTotal;
            lastIdle = idle;
            lastTotal = total;
            return totalDelta == 0 ? null : 1.0 - (double)idleDelta / totalDelta;
        }
    }

    private static bool TryReadTimes(out ulong idle, out ulong total)
    {
        idle = 0;
        total = 0;
        if (!GetSystemTimes(out long idleTime, out long kernelTime, out long userTime))
        {
            return false;
        }

        // Kernel time includes idle time.
        idle = (ulong)idleTime;
        total = (ulong)kernelTime + (ulong)userTime;
        return true;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetSystemTimes(out long idleTime, out long kernelTime, out long userTime);
}
