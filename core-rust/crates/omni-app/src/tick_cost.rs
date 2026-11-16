//! What the mesh and queue ticks spend their time on, a line a minute in the trace: they run for as
//! long as the app does, so what they cost is what the app costs doing nothing.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const REPORT_EVERY: Duration = Duration::from_secs(60);

struct Book {
    last: Instant,
    since: Instant,
    spent: BTreeMap<&'static str, Duration>,
    ticks: u32,
}

thread_local! {
    static BOOK: RefCell<Book> = RefCell::new(Book {
        last: Instant::now(),
        since: Instant::now(),
        spent: BTreeMap::new(),
        ticks: 0,
    });
}

/// A tick starts: the time spent waiting for it is nobody's.
pub(crate) fn begin() {
    BOOK.with_borrow_mut(|book| {
        book.last = Instant::now();
        book.ticks += 1;
    });
}

/// The time since the last mark went on `section`.
pub(crate) fn mark(section: &'static str) {
    BOOK.with_borrow_mut(|book| {
        let now = Instant::now();
        *book.spent.entry(section).or_default() += now - book.last;
        book.last = now;
    });
}

pub(crate) fn report_if_due(label: &str) {
    BOOK.with_borrow_mut(|book| {
        if book.since.elapsed() < REPORT_EVERY {
            return;
        }

        let mut spent: Vec<(&str, Duration)> = book.spent.iter().map(|(name, took)| (*name, *took)).collect();
        spent.sort_by_key(|(_, took)| std::cmp::Reverse(*took));
        let total: Duration = spent.iter().map(|(_, took)| *took).sum();
        let parts: Vec<String> = spent.iter().map(|(name, took)| format!("{name}={}ms", took.as_millis())).collect();
        crate::omni_app_trace_log(
            label,
            &format!(
                "ticks={} total={}ms over {}s: {}",
                book.ticks,
                total.as_millis(),
                book.since.elapsed().as_secs(),
                parts.join(" ")
            ),
        );
        book.spent.clear();
        book.ticks = 0;
        book.since = Instant::now();
    });
}
