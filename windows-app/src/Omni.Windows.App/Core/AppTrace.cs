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
    /// Append one line. Silently does nothing when the sink is off, and never throws — a diagnostic
    /// must not be able to break the thing it is reporting on.
    /// </summary>
    internal static void Write(string category, string message)
    {
        try
        {
            string? sink = Environment.GetEnvironmentVariable("OMNI_TRACE_FILE");
            if (string.IsNullOrWhiteSpace(sink))
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
