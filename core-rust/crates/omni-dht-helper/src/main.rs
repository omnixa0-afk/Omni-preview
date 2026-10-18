//! Thin bin wrapper: the DHT engine helper itself lives in the crate's lib (`run`), so BOTH callers
//! share one body — this standalone `omni-dht-helper.exe` (the normal multi-file layout), and the
//! single-file host, which has no sibling .exe on disk and instead re-execs itself with
//! `--run-helper dht …`, reaching `run` through the FFI (`omni_run_helper`).

fn main() {
    std::process::exit(omni_dht_helper::run(std::env::args().skip(1).collect()));
}
