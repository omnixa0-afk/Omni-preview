// The shell's lines for the `OMNI_TRACE_FILE` sink go through the core, so the file has ONE writer.
//
// The shell used to append on its own (`File.AppendAllText`), which opens the file denying others
// write access. Whichever side held the file at that moment made the other side's open fail, and
// both swallow errors by design — a whole line vanished, about one in seventy
// (docs/bugs/trace-log-loses-records.md, the `media/rtcp-feedback` case was a shell line).

/// Append one `category`/`message` line to the trace sink on the shell's behalf. A no-op when the
/// sink is off, on null pointers and on invalid UTF-8 in neither field (lossy). No app handle: the
/// shell traces before the core is up, too. Never panics across the boundary.
#[no_mangle]
// FFI fn: C caller owns pointer validity; null-checked + catch_unwind. See `omni_app_free`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn omni_trace_write(category: *const c_char, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if category.is_null() || message.is_null() {
            return;
        }
        if !omni_net_primitives::trace::trace_enabled() {
            return;
        }
        let category = unsafe { CStr::from_ptr(category) }.to_string_lossy();
        let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        let (category, message) = shell_trace_fields(&category, &message);
        omni_net_primitives::trace::trace_line(&category, &message);
    }));
}

/// One record per line, three tab-separated fields: a newline or a tab inside a shell field (an
/// exception text, say) would split the record or shift its columns, so they become spaces.
fn shell_trace_fields(category: &str, message: &str) -> (String, String) {
    let flatten = |field: &str| field.replace(['\r', '\n', '\t'], " ");
    (flatten(category), flatten(message))
}
