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

/// Where the embedded daemon listens. A later start, whatever endpoint it names, is answered with
/// this one: it said «listening» at the endpoint it asked for while the daemon stayed where it was
/// (blind audit 2026-11-10).
static DAEMON_ENDPOINT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn daemon_endpoint() -> std::sync::MutexGuard<'static, Option<String>> {
    DAEMON_ENDPOINT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    // a backup import (one listener per process: a second start only points it here). Pointed at
    // it under the daemon's lock, and only while it is live: `omni_app_free` retires a handle, then
    // takes this lock to let go of it, so a handle freed meanwhile is refused here instead of being
    // kept and read later (blind audit 2026-11-10). A second live core does not take the daemon
    // from the first: that is not the backup import's case, whose old core is freed first.
    {
        let mut current = DAEMON_HANDLE.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !crate::handle_is_live(handle) {
            return Err(OmniError::InvalidInput("the core was closed: this handle is no longer valid".to_string()));
        }
        let served = *current;
        if served != 0 && served != handle as usize && crate::handle_is_live(served as *mut OmniAppHandle) {
            return Err(OmniError::InvalidInput("the daemon serves another core that is still open".to_string()));
        }
        *current = handle as usize;
    }
    if DAEMON_STARTED.swap(true, Ordering::SeqCst) {
        let listening = daemon_endpoint().clone().unwrap_or(endpoint);
        return Ok(DaemonStartedResponse { ok: true, endpoint: listening });
    }
    *daemon_endpoint() = Some(endpoint.clone());

    let app = Arc::new(HandleApp);
    // The embedded daemon must NEVER kill the shell: a client `shutdown` is a no-op (it still
    // closes that one connection — `serve` wrote "OK bye" — but the daemon keeps running). The
    // shell owns its own lifecycle.
    let on_shutdown: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

    // The shell listens on a named pipe (current user only). A TCP endpoint (a dev override of
    // OMNI_DAEMON_PIPE) is served only with a token the starter set in OMNI_DAEMON_TOKEN, and
    // only on loopback (V-11).
    let tcp = omni_daemon::TcpPolicy::from_env(false);

    let serve_endpoint = endpoint.clone();
    let (failed_tx, failed_rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("omni-embedded-daemon".to_string())
        .spawn(move || {
            if let Err(error) = omni_daemon::serve_endpoint(&serve_endpoint, app, on_shutdown, tcp) {
                // Bind failure: release the guard so a later retry on a different endpoint works.
                *daemon_endpoint() = None;
                DAEMON_STARTED.store(false, Ordering::SeqCst);
                eprintln!("omni embedded daemon: cannot listen on {serve_endpoint}: {error}");
                let _ = failed_tx.send(error.to_string());
            }
        })
        .map_err(|error| OmniError::Internal(format!("failed to spawn daemon thread: {error}")))?;

    // A listener that cannot bind fails at once: wait that long before saying it listens. It said
    // «ok» for an endpoint taken by another program (blind audit 2026-11-10).
    if let Ok(error) = failed_rx.recv_timeout(std::time::Duration::from_millis(500)) {
        return Err(OmniError::Transport(format!("the daemon cannot listen on {endpoint}: {error}")));
    }
    Ok(DaemonStartedResponse { ok: true, endpoint })
}
