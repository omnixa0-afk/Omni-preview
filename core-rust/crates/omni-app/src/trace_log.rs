// Lightweight env-controlled file logger for diagnosing the
// unidirectional contact accept bug
// (see docs/decisions/unidirectional-contact-accept-fix.md).
//
// Activated only when OMNI_TRACE_FILE environment variable is set at
// process startup. When unset, every call is an early-return noop —
// no allocation, no I/O.
//
// Output format (one event per line):
//   <unix_ms>\t<category>\t<message>
// Tab-separated so grep / awk / spreadsheet import are unambiguous.
//
// Concurrency: file opened in append mode for each event and closed
// immediately. On Windows, append-mode writes from multiple threads to
// the same file are sequenced by the kernel — fine for our expected
// rate (single-digit/sec during repro). If higher throughput is needed
// later, replace with a channel + dedicated writer thread.
//
// File-internal helper (no `mod` declaration — lib.rs `include!`s us).

// Cache OMNI_TRACE_FILE at first use so std::env isn't hit per call.
// `None` after first read = tracing disabled for life of this process.
fn omni_app_trace_path() -> Option<&'static std::path::PathBuf> {
    static CACHED: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let raw = std::env::var("OMNI_TRACE_FILE").ok()?;
            if raw.is_empty() {
                return None;
            }
            Some(std::path::PathBuf::from(raw))
        })
        .as_ref()
}

fn omni_app_trace_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Append a single line. Errors silently swallowed — logging failure
// must never crash a hot signaling path. A misconfigured path simply
// produces no output, which is acceptable for a debugging affordance.
pub(crate) fn omni_app_trace_log(category: &str, message: &str) {
    let Some(path) = omni_app_trace_path() else {
        return;
    };
    let line = format!(
        "{ms}\t{category}\t{message}\n",
        ms = omni_app_trace_unix_ms()
    );
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    use std::io::Write as _;
    let _ = file.write_all(line.as_bytes());
    // Flush + fsync each line so a live tail (and a crash mid-run) sees every event immediately.
    // Without this, a diagnosing user saw the file "only update when reopened" (2026-07-20 live
    // test), which made the receiver-side trace untrustworthy — the exact events we needed were
    // buffered and invisible. The rate is single-digit lines/sec, so a sync per line is cheap.
    let _ = file.flush();
    let _ = file.sync_data();
}
