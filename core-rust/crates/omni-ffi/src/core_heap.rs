//! How much of the process's heap is the core's own (Rust) memory, for a test stand: everything else in
//! the native heap belongs to the UI framework and the drivers. Only with the `dev-tools` feature: an
//! atomic on every allocation is not worth carrying in a product.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Once;
use std::time::Duration;

const REPORT_EVERY: Duration = Duration::from_secs(10);
const MEGABYTE: f64 = 1_048_576.0;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static REPORTING: Once = Once::new();

struct CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn grew(by: u64) {
    let live = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(live, Relaxed);
}

// SAFETY: every call goes to `System` as it came; only the counters are added.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let block = System.alloc(layout);
        if !block.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            grew(layout.size() as u64);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        System.dealloc(block, layout);
        LIVE.fetch_sub(layout.size() as u64, Relaxed);
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let block = System.alloc_zeroed(layout);
        if !block.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            grew(layout.size() as u64);
        }
        block
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = System.realloc(block, layout, new_size);
        if !moved.is_null() {
            ALLOCATIONS.fetch_add(1, Relaxed);
            grew(new_size as u64);
            LIVE.fetch_sub(layout.size() as u64, Relaxed);
        }
        moved
    }
}

/// A line in the trace every few seconds, from the first core up in this process on.
pub(crate) fn start_reporting() {
    REPORTING.call_once(|| {
        let _ = std::thread::Builder::new().name("omni-core-heap".to_string()).spawn(|| {
            let mut allocations_before = 0;
            loop {
                std::thread::sleep(REPORT_EVERY);
                let allocations = ALLOCATIONS.load(Relaxed);
                omni_net_primitives::trace::trace_line(
                    "core/heap",
                    &format!(
                        "live={:.1}MB peak={:.1}MB allocations={}",
                        LIVE.load(Relaxed) as f64 / MEGABYTE,
                        PEAK.swap(LIVE.load(Relaxed), Relaxed) as f64 / MEGABYTE,
                        allocations - allocations_before,
                    ),
                );
                allocations_before = allocations;
            }
        });
    });
}
