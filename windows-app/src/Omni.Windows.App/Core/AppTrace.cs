namespace Omni.Windows.App.Core;

/// <summary>
/// Shell-side writer for the same `OMNI_TRACE_FILE` sink the Rust core uses, in the same
/// tab-separated `unix_ms\tcategory\tmessage` format.
///
/// Why the shell needs one at all: the core's trace shows a call reaching `connected` and then a
/// perfectly healthy link with zero media, because everything that decides whether media starts —
/// the capture probe, the capability publish, the native engine load — lives here in C# and used to
/// leave no trace anywhere. A live standalone test burned a full round of guessing on exactly that
/// blind spot (docs/bugs/*, media/engine-load).
///
/// Deliberately one implementation: two ad-hoc `File.AppendAllText` log writers already exist in the
/// shell (`nav-refresh.log`, the crash artifact) and a third copy was about to appear here.
///
/// The line itself is written BY THE CORE (`omni_trace_write`): the file has one writer. Appending
/// here with `File.AppendAllText` opened the file denying others write access, so whenever the shell
/// and the core wrote at the same moment one of the two lines was lost whole — about one in seventy
/// (docs/bugs/trace-log-loses-records.md; the lost `media/rtcp-feedback` line was a shell line).
/// </summary>
internal static class AppTrace
{
    /// <summary>
    /// `OMNI_TRACE_FILE` set by hand before start (a test stand, a script): it wins over the settings
    /// switch, which then shows it and stays locked. Read once, before <see cref="SetEnabled"/> ever
    /// writes the variable itself.
    /// </summary>
    internal static readonly string? EnvironmentTraceFile =
        Environment.GetEnvironmentVariable("OMNI_TRACE_FILE") is { Length: > 0 } raw && !string.IsNullOrWhiteSpace(raw)
            ? raw
            : null;

    private static volatile bool enabled = EnvironmentTraceFile is not null;

    internal static bool IsEnabled => enabled;

    /// <summary>The file the settings switch writes: `logs\trace.log` in the data folder.</summary>
    internal static string LogFilePath(string appDirectory) => Path.Combine(appDirectory, "logs", "trace.log");

    /// <summary>
    /// The settings switch «Журнал для разработчика»: the core starts or stops writing at once. The
    /// variable is set too, so a helper process started later (the courier, the DHT engine) traces
    /// into the same file; one already running keeps what it had. No-op under a hand-set variable.
    /// </summary>
    internal static void SetEnabled(string appDirectory, bool on)
    {
        if (EnvironmentTraceFile is not null)
        {
            return;
        }

        string? path = null;
        if (on)
        {
            path = LogFilePath(appDirectory);
            Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        }

        NativeMethods.omni_trace_set_file(path);
        Environment.SetEnvironmentVariable("OMNI_TRACE_FILE", path);
        enabled = on;
    }

    /// <summary>
    /// Append one line. Silently does nothing when the sink is off, and never throws — a diagnostic
    /// must not be able to break the thing it is reporting on.
    /// </summary>
    internal static void Write(string category, string message)
    {
        try
        {
            if (!enabled)
            {
                return;
            }

            NativeMethods.omni_trace_write(category, message);
        }
        catch
        {
            // Intentionally swallowed.
        }
    }

    /// <summary>Shorthand for reporting a swallowed exception on a path that must not throw.</summary>
    internal static void WriteFailure(string category, string what, Exception ex)
        => Write(category, $"{what} FAILED: {ex.GetType().Name}: {ex.Message}");
}
