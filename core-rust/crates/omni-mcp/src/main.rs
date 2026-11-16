//! Thin bin wrapper: the server lives in the crate's lib (`run`), so both callers share one body —
//! this standalone `omni-mcp.exe` and a single-file host re-run with `--run-helper mcp …` (reaching
//! `run` through the FFI, `omni_run_helper`).

fn main() {
    std::process::exit(omni_mcp::run(std::env::args().skip(1).collect()));
}
