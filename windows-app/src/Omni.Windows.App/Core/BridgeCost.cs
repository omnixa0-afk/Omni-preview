using System.Collections.Concurrent;
using System.Diagnostics;
using System.Linq;

namespace Omni.Windows.App.Core;

/// <summary>
/// What the shell asks of the core while nobody does anything, a few lines a minute in the trace:
/// calls, time and bytes per operation, and what the runtime did meanwhile. Nothing when the trace is off.
/// </summary>
internal static class BridgeCost
{
    private sealed class Entry
    {
        public long Calls;
        public long Ticks;
        public long Bytes;
    }

    private const int ReportedOperations = 14;
    private static readonly long ReportEveryTicks = Stopwatch.Frequency * 60;
    private static readonly ConcurrentDictionary<string, Entry> Entries = new();
    private static long lastReportAt = Stopwatch.GetTimestamp();
    private static long lastAllocated = GC.GetTotalAllocatedBytes();
    private static long lastPoolItems = ThreadPool.CompletedWorkItemCount;
    private static int lastCollections = GC.CollectionCount(0);

    internal static long Start() => AppTrace.IsEnabled ? Stopwatch.GetTimestamp() : 0;

    internal static void Record(string operation, long startedAt, long bytes = 0)
    {
        if (startedAt == 0)
        {
            return;
        }

        long now = Stopwatch.GetTimestamp();
        Entry entry = Entries.GetOrAdd(operation, static _ => new Entry());
        Interlocked.Increment(ref entry.Calls);
        Interlocked.Add(ref entry.Ticks, now - startedAt);
        Interlocked.Add(ref entry.Bytes, bytes);

        long last = Interlocked.Read(ref lastReportAt);
        if (now - last >= ReportEveryTicks && Interlocked.CompareExchange(ref lastReportAt, now, last) == last)
        {
            Report();
        }
    }

    private static void Report()
    {
        var rows = Entries
            .Select(pair => (
                Name: pair.Key,
                Calls: Interlocked.Exchange(ref pair.Value.Calls, 0),
                Ticks: Interlocked.Exchange(ref pair.Value.Ticks, 0),
                Bytes: Interlocked.Exchange(ref pair.Value.Bytes, 0)))
            .Where(row => row.Calls > 0)
            .OrderByDescending(row => row.Ticks)
            .ToList();
        double Ms(long ticks) => ticks * 1000.0 / Stopwatch.Frequency;

        long allocated = GC.GetTotalAllocatedBytes();
        long poolItems = ThreadPool.CompletedWorkItemCount;
        int collections = GC.CollectionCount(0);
        AppTrace.Write(
            "ui/bridge-cost",
            $"calls={rows.Sum(row => row.Calls)} total={Ms(rows.Sum(row => row.Ticks)):F0}ms "
            + $"bytes={rows.Sum(row => row.Bytes)} allocated={(allocated - lastAllocated) / 1024}KB "
            + $"pool-items={poolItems - lastPoolItems} gen0={collections - lastCollections}: "
            + string.Join(' ', rows.Take(ReportedOperations).Select(row =>
                $"{row.Name}={row.Calls}x/{Ms(row.Ticks):F0}ms/{row.Bytes / 1024}KB")));
        lastAllocated = allocated;
        lastPoolItems = poolItems;
        lastCollections = collections;
    }
}
