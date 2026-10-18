// Helper-process entry point (multi-call / self-exec). Windows cannot start a process from memory,
// so a SINGLE-FILE host has no sibling `omni-rendezvous-helper.exe` / `omni-dht-helper.exe` to spawn
// — and the helpers must stay separate PROCESSES (that isolation is the whole point: key-free,
// untrusted, independently proxyable, crash-sandboxed; see the helper crates' manifests). The way
// out is the multi-call pattern: the host re-execs ITSELF with `--run-helper <kind> …`, its startup
// notices the flag before any UI comes up, and routes here. The courier bodies live in the helper
// crates' libs, linked into this DLL, so the file count on disk stays exactly one.

/// Run helper `kind` ("rendezvous" | "dht") with `args_json` (a JSON array of the arguments AFTER
/// the flag, e.g. `["--listen","127.0.0.1:0"]`) and return the process exit code for the caller to
/// exit with. BLOCKS for the child's lifetime — this thread IS the helper. No `OmniAppHandle`: a
/// helper holds no keys and never touches the app.
#[no_mangle]
pub extern "C" fn omni_run_helper(kind: *const c_char, args_json: *const c_char) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| run_helper_impl(kind, args_json))) {
        Ok(code) => code,
        Err(_) => {
            eprintln!("panic crossed FFI boundary in omni_run_helper");
            101
        }
    }
}
