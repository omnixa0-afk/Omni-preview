use std::ffi::{c_char, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use omni_app::OmniApp;
use omni_protocol::PrekeyBundleV1;
use omni_types::{
    AttachmentDownloadRequest, ListMessagesPageRequest,
    ListMessagesRequest, OmniError, OmniResult, SendAttachmentRequest, SendTextRequest,
};

mod ffi_support;
mod requests_account_contacts;
mod requests_calls;
mod requests_core;
mod requests_daemon;
mod requests_discoverability;
mod requests_groups;
mod requests_transport;

pub(crate) use ffi_support::*;
pub(crate) use requests_account_contacts::*;
pub(crate) use requests_calls::*;
pub(crate) use requests_core::*;
pub(crate) use requests_daemon::*;
pub(crate) use requests_discoverability::*;
pub(crate) use requests_groups::*;
pub(crate) use requests_transport::*;

#[no_mangle]
pub extern "C" fn omni_app_init(config_json: *const c_char) -> *mut OmniAppHandle {
    match catch_unwind(AssertUnwindSafe(|| init_impl(config_json))) {
        Ok(Ok(handle)) => {
            set_last_init_error(None);
            Box::into_raw(Box::new(handle))
        }
        Ok(Err(error)) => {
            // Причину НЕЛЬЗЯ ронять в `_`. Раньше именно так и было, и оболочке оставалось написать
            // «Rust core вернул пустой дескриптор при инициализации» — сообщение, по которому
            // невозможно понять ничего. Живой случай 2026-08-14: профиль перестал открываться, и
            // настоящая причина (исчерпанный пул одноразовых предключей) читалась только по трассе.
            set_last_init_error(Some(error.to_string()));
            ptr::null_mut()
        }
        Err(_) => {
            set_last_init_error(Some("panic crossed FFI boundary".to_string()));
            ptr::null_mut()
        }
    }
}

/// Почему последняя попытка инициализации вернула пустой дескриптор. Пустая строка — причины нет
/// (инициализация не падала). Строку освобождать через `omni_string_free`.
#[no_mangle]
pub extern "C" fn omni_app_last_init_error() -> *mut c_char {
    let message = last_init_error().unwrap_or_default();
    CString::new(message)
        .unwrap_or_else(|_| CString::new("").expect("empty c-string"))
        .into_raw()
}

fn last_init_error_slot() -> &'static std::sync::Mutex<Option<String>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

fn set_last_init_error(message: Option<String>) {
    if let Ok(mut slot) = last_init_error_slot().lock() {
        *slot = message;
    }
}

fn last_init_error() -> Option<String> {
    last_init_error_slot().lock().ok().and_then(|slot| slot.clone())
}

#[no_mangle]
pub extern "C" fn omni_app_inspect_database_bootstrap(config_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        inspect_database_bootstrap_impl(config_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Whether the database opens on this machine as it is: `{"state":"openable"|"needs_recovery_phrase"}`.
/// Call before `omni_app_inspect_database_bootstrap` / `omni_app_init`: a locked database fails both.
#[no_mangle]
pub extern "C" fn omni_app_database_lock_state(config_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| database_lock_state_impl(config_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

/// Open a database from another machine with the recovery phrase of a profile in it (`{"mnemonic"}`)
/// and seal its key to this Windows account. `null` on success, an error envelope otherwise.
#[no_mangle]
pub extern "C" fn omni_app_unlock_database_with_mnemonic(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        unlock_database_with_mnemonic_impl(config_json, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_app_reset_local_data(config_json: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| reset_local_data_impl(config_json))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_app_export_database_backup_bundle(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        export_database_backup_bundle_impl(config_json, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_app_import_database_backup_bundle(
    config_json: *const c_char,
    request_json: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        import_database_backup_bundle_impl(config_json, request_json)
    })) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

// FFI free fn: the C caller owns pointer validity (it got `handle` from us); we null-check +
// catch_unwind. Kept non-`unsafe` for consistency with the rest of the C-ABI surface.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "C" fn omni_app_free(handle: *mut OmniAppHandle) {
    if handle.is_null() {
        return;
    }

    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        // The embedded daemon lets go first (waiting for a dispatch in flight).
        requests_daemon::release_daemon_handle(handle);
        drop(Box::from_raw(handle));
    }));
}

/// Phase 3 — start the **embedded daemon**: serve the Omni daemon protocol (line + framed,
/// with an events plane) against THIS handle's in-process `OmniApp`, on `endpoint` (a Windows
/// named pipe `\\.\pipe\…` or `host:port`). An external client then drives the live app and the
/// shell reflects it via its normal background poll — no GUI automation. One listener per process;
/// a later start (the core reopened after a backup import) points it at the new handle, and
/// `omni_app_free` takes it away from the old one first.
/// A client `shutdown` does NOT stop the shell. See docs/decisions/embedded-daemon-in-shell.md.
#[no_mangle]
pub extern "C" fn omni_daemon_start(
    handle: *mut OmniAppHandle,
    endpoint: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| daemon_start_impl(handle, endpoint))) {
        Ok(result) => to_result_payload(result),
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_app_ping(handle: *mut OmniAppHandle) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| ping_impl(handle))) {
        Ok(payload) => payload,
        Err(_) => to_c_string_payload(&ErrorEnvelope {
            ok: false,
            error: OmniError::Internal("panic crossed FFI boundary".to_string()),
        }),
    }
}

#[no_mangle]
pub extern "C" fn omni_configure_platform_secret_wrap_key_hex(
    key_hex: *const c_char,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        configure_platform_secret_wrap_key_hex_impl(key_hex)
    })) {
        Ok(Ok(())) => ptr::null_mut(),
        Ok(Err(error)) => to_plain_c_string(&error.to_string()),
        Err(_) => to_plain_c_string("panic crossed FFI boundary"),
    }
}

#[no_mangle]
pub extern "C" fn omni_transport_set_link_cost(cost: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| set_link_cost_impl(cost))) {
        Ok(Ok(())) => ptr::null_mut(),
        Ok(Err(error)) => to_plain_c_string(&error.to_string()),
        Err(_) => to_plain_c_string("panic crossed FFI boundary"),
    }
}

#[no_mangle]
pub extern "C" fn omni_transport_set_congestion_control(mode: *const c_char) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| set_congestion_control_impl(mode))) {
        Ok(Ok(())) => ptr::null_mut(),
        Ok(Err(error)) => to_plain_c_string(&error.to_string()),
        Err(_) => to_plain_c_string("panic crossed FFI boundary"),
    }
}

#[no_mangle]
pub extern "C" fn omni_transport_apply_congestion_control_now(
    handle: *mut OmniAppHandle,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| apply_congestion_control_now_impl(handle))) {
        Ok(Ok(closed)) => to_plain_c_string(&format!("closed={closed}")),
        Ok(Err(error)) => to_plain_c_string(&format!("error: {error}")),
        Err(_) => to_plain_c_string("error: panic crossed FFI boundary"),
    }
}

#[no_mangle]
pub extern "C" fn omni_transport_effective_congestion_control() -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(effective_congestion_control_impl)) {
        Ok(name) => to_plain_c_string(name),
        Err(_) => to_plain_c_string("unknown"),
    }
}

include!("ffi_exports_transport.rs");

include!("ffi_exports_account_contacts.rs");

include!("ffi_exports_calls.rs");

include!("ffi_exports_realtime_media.rs");

include!("ffi_exports_mesh.rs");

include!("ffi_exports_groups.rs");

include!("ffi_exports_screen_control.rs");

include!("ffi_exports_discoverability.rs");

include!("ffi_exports_helpers.rs");

include!("ffi_exports_trace.rs");

#[no_mangle]
// FFI free fn: C caller owns pointer validity; null-checked + catch_unwind. See `omni_app_free`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn omni_string_free(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }

    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        drop(CString::from_raw(ptr));
    }));
}

include!("exports_impl.rs");

include!("tests.rs");
