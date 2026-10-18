//! Embedded daemon (Phase 3): serve the Omni daemon protocol against the shell's in-process
//! `OmniApp` (the one behind this FFI handle), so an external client drives the live app and
//! the shell reflects it — no GUI automation. See docs/decisions/embedded-daemon-in-shell.md.

use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use omni_app::OmniApp;
use omni_types::{OmniError, OmniResult};
use serde::Serialize;

use crate::OmniAppHandle;

/// Only one embedded daemon per process (one shell = one core = one endpoint).
static DAEMON_STARTED: AtomicBool = AtomicBool::new(false);

/// The handle the embedded daemon serves now (its address; 0 = none). The daemon runs for the
/// process lifetime, the handle does not: importing a backup frees the core and opens another
/// (docs/bugs/the-daemon-touched-a-freed-core.md). A dispatch holds the read lock while it uses the
/// app; `omni_app_free` takes the write lock — waiting for a dispatch in flight — and clears it
/// before the app is dropped; the next `omni_daemon_start` points the daemon at the new handle.
static DAEMON_HANDLE: RwLock<usize> = RwLock::new(0);

/// The daemon lets go of `handle` before it is freed.
pub(crate) fn release_daemon_handle(handle: *mut OmniAppHandle) {
    let mut current = DAEMON_HANDLE.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *current == handle as usize {
        *current = 0;
    }
}

/// The daemon's view of the shell's core: whatever handle is current, per dispatch. All access
/// goes through `OmniApp::serializer` (the daemon lib locks it around every dispatch and drain),
/// exactly as `require_handle` does for every other FFI call.
struct HandleApp;

impl omni_daemon::AppAccess for HandleApp {
    fn with_app(&self, f: &mut dyn FnMut(&OmniApp)) {
        let current = DAEMON_HANDLE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *current != 0 {
            // SAFETY: the handle stays alive while the read lock is held — `omni_app_free`
            // clears it under the write lock before dropping it.
            f(unsafe { &(*(*current as *mut OmniAppHandle)).inner })
        }
    }
}

#[derive(Serialize)]
pub(crate) struct DaemonStartedResponse {
    pub(crate) ok: bool,
    pub(crate) endpoint: String,
}

pub(crate) fn daemon_start_impl(
    handle: *mut OmniAppHandle,
    endpoint: *const c_char,
) -> OmniResult<DaemonStartedResponse> {
    if handle.is_null() {
        return Err(OmniError::InvalidInput("handle must not be null".to_string()));
    }
    if endpoint.is_null() {
        return Err(OmniError::InvalidInput("endpoint must not be null".to_string()));
    }
    let endpoint = unsafe { CStr::from_ptr(endpoint) }
        .to_str()
        .map_err(|error| OmniError::InvalidInput(format!("endpoint is not valid UTF-8: {error}")))?
        .to_string();

    // The daemon serves this handle from now on — the first one, or the core opened again after
    // a backup import (one listener per process: a second start only points it here).
    *DAEMON_HANDLE.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = handle as usize;
    if DAEMON_STARTED.swap(true, Ordering::SeqCst) {
        return Ok(DaemonStartedResponse { ok: true, endpoint });
    }

    let app = Arc::new(HandleApp);
    // The embedded daemon must NEVER kill the shell: a client `shutdown` is a no-op (it still
    // closes that one connection — `serve` wrote "OK bye" — but the daemon keeps running). The
    // shell owns its own lifecycle.
    let on_shutdown: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

    let serve_endpoint = endpoint.clone();
    std::thread::Builder::new()
        .name("omni-embedded-daemon".to_string())
        .spawn(move || {
            if let Err(error) = omni_daemon::serve_endpoint(&serve_endpoint, app, on_shutdown) {
                // Bind failure: release the guard so a later retry on a different endpoint works.
                DAEMON_STARTED.store(false, Ordering::SeqCst);
                eprintln!("omni embedded daemon: cannot listen on {serve_endpoint}: {error}");
            }
        })
        .map_err(|error| OmniError::Internal(format!("failed to spawn daemon thread: {error}")))?;

    Ok(DaemonStartedResponse { ok: true, endpoint })
}
