//! Thin bin wrapper: the relay lives in the crate's lib (`run`), so both callers share one body —
//! this standalone `omni-git-helper.exe` and a single-file host re-run with `--run-helper git …`
//! (reaching `run` through the FFI, `omni_run_helper`).

fn main() {
    std::process::exit(omni_git_helper::run(std::env::args().skip(1).collect()));
}
