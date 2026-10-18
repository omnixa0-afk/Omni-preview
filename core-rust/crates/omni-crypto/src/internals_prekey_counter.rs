fn next_prekey_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static PREKEY_COUNTER: AtomicU32 = AtomicU32::new(1);
    PREKEY_COUNTER.fetch_add(1, Ordering::Relaxed)
}
