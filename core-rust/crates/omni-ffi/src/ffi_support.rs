use std::ffi::{c_char, CStr, CString};
use std::sync::Arc;

use omni_app::{AppInitRequest, OmniApp};
use omni_crypto::configure_platform_secret_wrap_key_hex;
use omni_types::{OmniError, OmniResult};
use parking_lot::ReentrantMutex;
use serde::{de::DeserializeOwned, Serialize};

#[repr(C)]
pub struct OmniAppHandle {
    pub(crate) inner: OmniApp,
    /// This app's entry in the media send path's table of live apps (`live_media_apps`): a send
    /// target names the app by it, never by the handle's address.
    pub(crate) media_app_id: u64,
}

#[derive(Serialize)]
pub(crate) struct ErrorEnvelope {
    pub(crate) ok: bool,
    pub(crate) error: OmniError,
}

/// Wave 2c concurrency fix (2026-05-16): every FFI entry point gets exactly
/// one of these guards via `require_handle`. The guard owns a re-entrant
/// lock on `inner.serializer`, so concurrent ThreadPool callers serialize
/// against each other while the same thread can re-enter (some Rust paths
/// nest OmniApp method calls).
///
/// Without this, `rusqlite::Connection`'s internal RefCell panicked
/// ("RefCell already mutably borrowed") whenever two FFI calls overlapped.
///
/// Deref + DerefMut to `OmniAppHandle` so callers keep working with
/// `require_handle(handle)?.inner.x()` unchanged.
pub(crate) struct HandleGuard {
    handle: *mut OmniAppHandle,
    // The Arc keeps the mutex alive for as long as the guard exists.
    _arc: Arc<ReentrantMutex<()>>,
    // Held to drop in correct order. Lifetime transmuted to 'static —
    // the backing mutex lives inside `_arc` above which outlives the
    // guard (drop order: _lock first, then _arc).
    _lock: parking_lot::lock_api::ReentrantMutexGuard<
        'static,
        parking_lot::RawMutex,
        parking_lot::RawThreadId,
        (),
    >,
}

impl std::ops::Deref for HandleGuard {
    type Target = OmniAppHandle;
    fn deref(&self) -> &OmniAppHandle {
        // SAFETY: handle was non-null when stored (require_handle checked).
        unsafe { &*self.handle }
    }
}

impl std::ops::DerefMut for HandleGuard {
    fn deref_mut(&mut self) -> &mut OmniAppHandle {
        unsafe { &mut *self.handle }
    }
}

/// The handles `omni_app_init` made and `omni_app_free` has not freed yet (their addresses). A call
/// takes its handle's lock only while the handle is here, and checks again once it holds it;
/// `omni_app_free` takes the handle out first, then waits for the call holding the lock, then frees
/// it. A call that was waiting for the lock, or comes with the handle afterwards, is refused instead
/// of reading freed memory, and a second free does nothing (blind audit 2026-11-10). Process-wide,
/// not a profile's: a handle is the whole core.
static LIVE_HANDLES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

fn live_handles() -> std::sync::MutexGuard<'static, Vec<usize>> {
    LIVE_HANDLES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn register_live_handle(handle: *mut OmniAppHandle) {
    live_handles().push(handle as usize);
}

/// Take `handle` out of the live ones: its lock, cloned while it was still live, or `None` when it
/// was not live (freed already, or never made here).
pub(crate) fn retire_live_handle(handle: *mut OmniAppHandle) -> Option<Arc<ReentrantMutex<()>>> {
    let mut live = live_handles();
    let at = live.iter().position(|known| *known == handle as usize)?;
    live.swap_remove(at);
    // SAFETY: it was live, and the free that retires it is the only one that frees it.
    Some(unsafe { (*handle).inner.serializer.clone() })
}

pub(crate) fn handle_is_live(handle: *mut OmniAppHandle) -> bool {
    live_handles().contains(&(handle as usize))
}

const HANDLE_FREED: &str = "the core was closed: this handle is no longer valid";

pub(crate) fn require_handle(handle: *mut OmniAppHandle) -> OmniResult<HandleGuard> {
    if handle.is_null() {
        return Err(OmniError::InvalidInput("handle must not be null".to_string()));
    }
    // The lock is cloned out while the handle is known live (`omni_app_free` cannot free it in
    // between: it retires it under the same lock first), then taken.
    let arc: Arc<ReentrantMutex<()>> = {
        let live = live_handles();
        if !live.contains(&(handle as usize)) {
            return Err(OmniError::InvalidInput(HANDLE_FREED.to_string()));
        }
        // SAFETY: live, so not freed; a free retires it under `live` before touching it.
        unsafe { (*handle).inner.serializer.clone() }
    };
    let lock_ptr: *const ReentrantMutex<()> = Arc::as_ptr(&arc);
    let lock: parking_lot::ReentrantMutexGuard<'_, ()> = unsafe { (*lock_ptr).lock() };
    // SAFETY: extend lifetime to 'static. The Arc is owned by the guard
    // (field above) and dropped AFTER the lock, so the storage stays
    // valid for as long as the guard exists.
    let lock_static: parking_lot::lock_api::ReentrantMutexGuard<
        'static,
        parking_lot::RawMutex,
        parking_lot::RawThreadId,
        (),
    > = unsafe { std::mem::transmute(lock) };
    let guard = HandleGuard {
        handle,
        _arc: arc,
        _lock: lock_static,
    };
    // Freed while this call waited for the lock: the free took it out, then waited for whoever
    // held the lock — not for this call.
    if !handle_is_live(handle) {
        return Err(OmniError::InvalidInput(HANDLE_FREED.to_string()));
    }
    Ok(guard)
}

pub(crate) fn parse_json_request<T: DeserializeOwned>(
    request_json: *const c_char,
    request_name: &str,
) -> OmniResult<T> {
    if request_json.is_null() {
        return Err(OmniError::InvalidInput(format!(
            "{request_name} must not be null"
        )));
    }

    let request_json = unsafe { CStr::from_ptr(request_json) }
        .to_str()
        .map_err(|error| {
            OmniError::InvalidInput(format!("{request_name} is not valid UTF-8: {error}"))
        })?;

    serde_json::from_str(request_json)
        .map_err(|error| OmniError::InvalidInput(format!("invalid {request_name} json: {error}")))
}

pub(crate) fn parse_init_request(
    config_json: *const c_char,
    operation: &str,
) -> OmniResult<AppInitRequest> {
    if config_json.is_null() {
        return Err(OmniError::InvalidInput(
            "config_json must not be null".to_string(),
        ));
    }

    let config_json = unsafe { CStr::from_ptr(config_json) }
        .to_str()
        .map_err(|error| {
            OmniError::InvalidInput(format!("config_json is not valid UTF-8: {error}"))
        })?;

    serde_json::from_str(config_json)
        .map_err(|error| OmniError::InvalidInput(format!("invalid {operation} json: {error}")))
}

pub(crate) fn to_result_payload<T: Serialize>(result: OmniResult<T>) -> *mut c_char {
    match result {
        Ok(value) => to_c_string_payload(&value),
        Err(error) => to_c_string_payload(&ErrorEnvelope { ok: false, error }),
    }
}

pub(crate) fn to_c_string_payload<T: Serialize>(value: &T) -> *mut c_char {
    let payload = match serde_json::to_string(value) {
        Ok(payload) => payload,
        Err(error) => format!(
            "{{\"ok\":false,\"error\":{{\"kind\":\"internal\",\"message\":\"failed to serialize payload: {error}\"}}}}"
        ),
    };

    match CString::new(payload) {
        Ok(c_string) => c_string.into_raw(),
        Err(_) => unsafe {
            CString::from_vec_unchecked(
                b"{\"ok\":false,\"error\":{\"kind\":\"internal\",\"message\":\"payload contained interior null byte\"}}"
                    .to_vec(),
            )
        }
        .into_raw(),
    }
}

/// Audit P1 / P2: error envelope for binary-pump exports (datagram send,
/// drain, batch_free). Until now pump errors used `to_plain_c_string` with
/// raw text while JSON-control exports used `to_c_string_payload(ErrorEnvelope)`.
/// Unified to JSON envelope so the C# `ReadNativeErrorAndFree` parses one
/// shape everywhere. C# helper falls back to raw string for backward-compat.
pub(crate) fn to_error_envelope_c_string(error: OmniError) -> *mut c_char {
    to_c_string_payload(&ErrorEnvelope { ok: false, error })
}

pub(crate) fn to_plain_c_string(message: &str) -> *mut c_char {
    match CString::new(message) {
        Ok(value) => value.into_raw(),
        Err(_) => unsafe {
            CString::from_vec_unchecked(b"ffi message contained interior null byte".to_vec())
        }
        .into_raw(),
    }
}

/// Оболочка сообщает, считают ли трафик на текущем канале. Ядро само этого знать не может:
/// на Windows ответ даёт WinRT, на Android — ConnectivityManager.
/// Значения: `metered` / `unmetered` / `unknown`. См. `select_congestion_control`.
pub(crate) fn set_link_cost_impl(cost: *const c_char) -> OmniResult<()> {
    if cost.is_null() {
        return Err(OmniError::InvalidInput(
            "link cost must not be null".to_string(),
        ));
    }
    let cost = unsafe { CStr::from_ptr(cost) }
        .to_str()
        .map_err(|error| OmniError::InvalidInput(format!("link cost is not valid UTF-8: {error}")))?;
    let parsed = match cost.trim().to_ascii_lowercase().as_str() {
        "metered" => omni_app::LinkCost::Metered,
        "unmetered" => omni_app::LinkCost::Unmetered,
        "unknown" => omni_app::LinkCost::Unknown,
        other => {
            // Молча принять неизвестное значение нельзя: оно решает, чем мы едем.
            return Err(OmniError::InvalidInput(format!(
                "link cost must be metered/unmetered/unknown, got {other:?}"
            )));
        }
    };
    omni_app::set_link_cost(parsed);
    Ok(())
}

/// Ручной выбор контроллера перегрузки из настроек: `auto` / `bbr` / `cubic` / `newreno`.
/// Применяется к соединениям, которые поднимутся ПОСЛЕ вызова — quinn не умеет менять контроллер
/// на живом соединении, поэтому оболочка обязана переустановить меш, если хочет эффекта сейчас.
pub(crate) fn set_congestion_control_impl(mode: *const c_char) -> OmniResult<()> {
    if mode.is_null() {
        return Err(OmniError::InvalidInput(
            "congestion control must not be null".to_string(),
        ));
    }
    let mode = unsafe { CStr::from_ptr(mode) }.to_str().map_err(|error| {
        OmniError::InvalidInput(format!("congestion control is not valid UTF-8: {error}"))
    })?;
    let parsed = omni_app::parse_congestion_preference(mode).ok_or_else(|| {
        // Молча принять неизвестное значение нельзя: оно решает, чем мы едем.
        OmniError::InvalidInput(format!(
            "congestion control must be auto/bbr/cubic/newreno, got {mode:?}"
        ))
    })?;
    omni_app::set_congestion_preference(parsed);
    Ok(())
}

/// Применить выбранный контроллер к ЖИВОМУ мешу: пересобрать конфиги интерфейсов и закрыть
/// соединения, которые едут на прежнем. Возвращает число закрытых соединений — ноль означает
/// «применять было не к чему», и оболочка не должна показывать это как «применено к линку».
pub(crate) fn apply_congestion_control_now_impl(handle: *mut OmniAppHandle) -> OmniResult<usize> {
    require_handle(handle)?.inner.apply_congestion_control_now()
}

/// Что применится к следующему соединению прямо сейчас — с учётом `OMNI_QUIC_CC` и стоимости
/// канала. Нужно, чтобы настройка в UI не врала на «авто» и не молчала под переменной окружения.
pub(crate) fn effective_congestion_control_impl() -> &'static str {
    match omni_app::effective_congestion_control() {
        omni_app::CongestionChoice::Bbr => "bbr",
        omni_app::CongestionChoice::Cubic => "cubic",
        omni_app::CongestionChoice::NewReno => "newreno",
    }
}

pub(crate) fn configure_platform_secret_wrap_key_hex_impl(
    key_hex: *const c_char,
) -> OmniResult<()> {
    if key_hex.is_null() {
        return Err(OmniError::InvalidInput(
            "platform secret key must not be null".to_string(),
        ));
    }

    let key_hex = unsafe { CStr::from_ptr(key_hex) }
        .to_str()
        .map_err(|error| {
            OmniError::InvalidInput(format!("platform key is not valid UTF-8: {error}"))
        })?;

    configure_platform_secret_wrap_key_hex(key_hex)
}
