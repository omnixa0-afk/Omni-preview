// The core's diagnostic trace line — the `OMNI_TRACE_FILE` sink of `omni_net_primitives::trace`.
//
// This file used to keep its own copy of the sink (its own env read, its own append), one of the
// copies `omni_net_primitives::trace` was written to replace. With the trace switchable from the
// settings, two copies would mean two switches; the line now goes through the one sink. The copy
// also fsynced every line, for a live tail that "only updated when reopened" (2026-07-20) — a plain
// append is visible to readers at once; an fsync per line only bought disk flushes on the hot path.
//
// File-internal helper (no `mod` declaration — lib.rs `include!`s us).

pub(crate) fn omni_app_trace_log(category: &str, message: &str) {
    omni_net_primitives::trace::trace_line(category, message);
}
