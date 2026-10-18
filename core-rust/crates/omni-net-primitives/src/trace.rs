//! The `OMNI_TRACE_FILE` diagnostic sink, in one place.
//!
//! This sink had grown two independent implementations — `omni-app/trace_log.rs` and
//! `omni-network/tcp_transport.rs::tcp_trace` — writing the same tab-separated format to the same
//! file from the same environment variable. A third was about to appear in `omni-transport`, which
//! is what prompted collapsing them here instead. `omni-net-primitives` is the lowest crate both
//! networking layers already depend on.
//!
//! Off unless `OMNI_TRACE_FILE` is set at process start: every call is then an early return, no
//! allocation and no I/O. Errors are swallowed by design — a diagnostic affordance must never take
//! down a hot path, and a misconfigured path simply produces no output.
//!
//! Format, one event per line: `<unix_ms>\t<category>\t<message>` — tabs so grep/awk/spreadsheet
//! import are unambiguous.
//!
//! Concurrency: opened in append mode per event and closed immediately. Each line reaches the file
//! as ONE append write, and the kernel sequences those between threads, which is fine at the rates
//! these traces run at. If a caller ever needs more, replace this with a channel and a writer
//! thread — not with a fourth copy.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Resolved once; `None` means tracing is off for the life of the process.
fn trace_path() -> Option<&'static PathBuf> {
    static CACHED: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let raw = std::env::var("OMNI_TRACE_FILE").ok()?;
            if raw.trim().is_empty() {
                return None;
            }
            Some(PathBuf::from(raw))
        })
        .as_ref()
}

/// Whether the sink is active. Lets a caller skip building an expensive message, or skip spawning a
/// monitor task altogether, when nobody is listening.
pub fn trace_enabled() -> bool {
    trace_path().is_some()
}

/// Append one diagnostic line. No-op when the sink is off.
pub fn trace_line(category: &str, message: &str) {
    let Some(path) = trace_path() else {
        return;
    };
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    append_event(path, unix_ms, category, message);
}

/// The whole line is formatted first and handed to the file in ONE write. `write!` straight into an
/// unbuffered `File` issues a write per formatting piece — the time, a tab, the category, … — and two
/// threads tracing at the same moment interleaved those pieces. Live 2026-10-03 the dial race and the
/// QUIC tracer produced `17910146654371791014665437\t\tdial/racequic/conn-open…`: neither event
/// parseable.
fn append_event(path: &Path, unix_ms: i64, category: &str, message: &str) {
    append_whole_line(path, &format!("{unix_ms}\t{category}\t{message}\n"));
}

/// Append an already formatted `line` to `path` in ONE write, so a concurrent writer can't land in
/// the middle of it. For any trace file sink — the `OMNI_NET_TRACE` one in `omni-network` too.
pub fn append_whole_line(path: &Path, line: &str) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = file.write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `<unix_ms>\t<category>\t<message>` and nothing else — what every reader of the trace splits on.
    fn well_formed(line: &str) -> bool {
        let mut fields = line.splitn(3, '\t');
        let (Some(ms), Some(category), Some(message)) = (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        ms.len() == 13
            && ms.bytes().all(|b| b.is_ascii_digit())
            && !category.is_empty()
            && !message.contains('\t')
    }

    #[test]
    fn lines_traced_from_many_threads_at_once_stay_whole() {
        // The checker first: the line the live trace actually produced must be rejected, or the
        // positive half below would prove nothing.
        assert!(!well_formed(
            "17910146654371791014665437\t\tdial/racequic/conn-open\t\tdir=out remote=x"
        ));
        assert!(well_formed("1791014661802\tdial/race\twon=1.2.3.4:5 started=1 of 8"));

        let path = std::env::temp_dir().join(format!("omni-trace-test-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        const THREADS: usize = 8;
        const LINES: usize = 400;
        std::thread::scope(|scope| {
            for thread in 0..THREADS {
                let path = &path;
                scope.spawn(move || {
                    let category = format!("cat/{thread}");
                    for n in 0..LINES {
                        let message = format!("t{thread} n{n} {}", "x".repeat(120));
                        append_event(path, 1_791_014_661_802, &category, &message);
                    }
                });
            }
        });
        let written = std::fs::read_to_string(&path).expect("the trace is written");
        let _ = std::fs::remove_file(&path);
        let lines: Vec<&str> = written.lines().collect();
        let broken: Vec<&&str> = lines.iter().filter(|line| !well_formed(line)).take(3).collect();
        assert!(broken.is_empty(), "interleaved lines: {broken:?}");
        assert_eq!(lines.len(), THREADS * LINES, "a line was lost or two were glued");
    }
}
