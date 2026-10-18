// Impl behind `omni_run_helper` (see ffi_exports_helpers.rs).

/// Exit code for a malformed invocation — same `2` the helper bins use for a usage error, so the
/// self-exec path and the standalone .exe report identically.
const HELPER_USAGE_EXIT_CODE: i32 = 2;

fn run_helper_impl(kind: *const c_char, args_json: *const c_char) -> i32 {
    if kind.is_null() {
        eprintln!("omni_run_helper: kind must not be null");
        return HELPER_USAGE_EXIT_CODE;
    }

    let kind = match unsafe { CStr::from_ptr(kind) }.to_str() {
        Ok(kind) => kind,
        Err(error) => {
            eprintln!("omni_run_helper: kind is not valid UTF-8: {error}");
            return HELPER_USAGE_EXIT_CODE;
        }
    };

    let args: Vec<String> = match parse_json_request(args_json, "args_json") {
        Ok(args) => args,
        Err(error) => {
            eprintln!("omni_run_helper: {error}");
            return HELPER_USAGE_EXIT_CODE;
        }
    };

    run_helper_kind(kind, args)
}

/// The kind → courier switch, split out from the C-ABI shim so it is testable from Rust.
fn run_helper_kind(kind: &str, args: Vec<String>) -> i32 {
    match kind {
        "rendezvous" => omni_rendezvous_helper::run(args),
        "dht" => omni_dht_helper::run(args),
        other => {
            eprintln!("omni_run_helper: unknown helper kind {other:?} (want rendezvous|dht)");
            HELPER_USAGE_EXIT_CODE
        }
    }
}
