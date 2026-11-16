//! The `OMNI_TRACE_FILE` diagnostic sink, in one place.
//!
//! This sink had grown two independent implementations — `omni-app/trace_log.rs` and
//! `omni-network/tcp_transport.rs::tcp_trace` — writing the same tab-separated format to the same
//! file from the same environment variable. A third was about to appear in `omni-transport`, which
//! is what prompted collapsing them here instead. `omni-net-primitives` is the lowest crate both
//! networking layers already depend on.
//!
//! Off unless `OMNI_TRACE_FILE` is set at process start or the host turns it on
//! ([`set_trace_file`], the shell's «Журнал для разработчика» switch): while off every call is an
//! early return on one atomic load, no allocation and no I/O. Errors are swallowed by design — a
//! diagnostic affordance must never take down a hot path, and a misconfigured path simply produces
//! no output.
//!
//! The file is capped: past [`TRACE_FILE_MAX_BYTES`] it is renamed to `<name>.1` (replacing the
//! previous one) and a new file starts, so a user who forgets the switch on loses at most two caps
//! of disk, not the disk.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

/// A trace file past this size is rotated (see the module note).
pub const TRACE_FILE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Where the sink writes, and a flag mirroring "somewhere" so the off path is one atomic load.
struct Sink {
    on: AtomicBool,
    path: RwLock<Option<PathBuf>>,
}

/// The sink, seeded from `OMNI_TRACE_FILE` on first use.
fn sink() -> &'static Sink {
    static SINK: OnceLock<Sink> = OnceLock::new();
    SINK.get_or_init(|| {
        let path = std::env::var("OMNI_TRACE_FILE")
            .ok()
            .filter(|raw| !raw.trim().is_empty())
            .map(PathBuf::from);
        Sink { on: AtomicBool::new(path.is_some()), path: RwLock::new(path) }
    })
}

/// Whether the sink is active. Lets a caller skip building an expensive message, or skip spawning a
/// monitor task altogether, when nobody is listening. A monitor skipped this way stays skipped for
/// its connection when the sink is turned on later.
pub fn trace_enabled() -> bool {
    sink().on.load(Ordering::Relaxed)
}

/// Write the trace to `path` from now on, or stop (`None`). Takes effect for the next line.
pub fn set_trace_file(path: Option<PathBuf>) {
    let sink = sink();
    let on = path.is_some();
    *sink.path.write().unwrap_or_else(|poison| poison.into_inner()) = path;
    sink.on.store(on, Ordering::Relaxed);
}

/// The file the trace goes to now, if any.
pub fn trace_file() -> Option<PathBuf> {
    sink().path.read().unwrap_or_else(|poison| poison.into_inner()).clone()
}

/// Append one diagnostic line. No-op when the sink is off.
pub fn trace_line(category: &str, message: &str) {
    if !trace_enabled() {
        return;
    }
    let Some(path) = trace_file() else {
        return;
    };
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    rotate_if_full(&path, TRACE_FILE_MAX_BYTES);
    append_event(&path, unix_ms, category, message);
}

/// Past `cap`, move the file to `<name>.1` (replacing the older one). Two threads crossing the cap
/// together both try; the loser's rename finds nothing and does nothing.
fn rotate_if_full(path: &Path, cap: u64) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= cap) {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
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

    /// The settings switch: on writes, off stops at once, and a full file moves aside to `.1`.
    #[test]
    fn the_sink_turns_on_and_off_at_runtime_and_a_full_file_moves_aside() {
        let dir = std::env::temp_dir().join(format!("omni-trace-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trace.log");

        set_trace_file(Some(path.clone()));
        assert!(trace_enabled());
        trace_line("test/switch", "first");
        set_trace_file(None);
        assert!(!trace_enabled());
        trace_line("test/switch", "while off");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("first") && !written.contains("while off"), "{written}");

        rotate_if_full(&path, 1);
        assert!(!path.exists(), "a full file is moved aside");
        assert!(dir.join("trace.log.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
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
