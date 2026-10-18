---
tags: [architecture, ffi, auto-generated]
---

# FFI Surface

**Auto-generated. Do not edit.** Regenerate with `scripts/regen-docs.ps1`.

Every `pub extern "C" fn omni_*` export from `omni-ffi`. C# side imports
these via `[DllImport]` in `windows-app/src/Omni.Windows.App/Core/NativeMethods.*.cs`.

Total: **215 exports** across 26 domain prefixes.

## `omni_account_*` (5 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_account_device_revoke` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:933 |
| `omni_account_devices_acknowledge` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:904 |
| `omni_account_devices_hide_revoked` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:917 |
| `omni_account_devices_list` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:877 |
| `omni_account_move_working_key` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:889 |

## `omni_app_*` (10 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_app_database_lock_state` | `(config_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:93 |
| `omni_app_export_database_backup_bundle` | `(config_json: *const c_char, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:133 |
| `omni_app_free` | `(handle: *mut OmniAppHandle)` | core-rust/crates/omni-ffi/src/lib.rs:168 |
| `omni_app_import_database_backup_bundle` | `(config_json: *const c_char, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:149 |
| `omni_app_init` | `(config_json: *const c_char) -> *mut OmniAppHandle` | core-rust/crates/omni-ffi/src/lib.rs:31 |
| `omni_app_inspect_database_bootstrap` | `(config_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:78 |
| `omni_app_last_init_error` | `() -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:55 |
| `omni_app_ping` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:202 |
| `omni_app_reset_local_data` | `(config_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:122 |
| `omni_app_unlock_database_with_mnemonic` | `(config_json: *const c_char, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:106 |

## `omni_calls_*` (13 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_calls_accept_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:48 |
| `omni_calls_clear_realtime_media_negotiation` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:121 |
| `omni_calls_decline_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:2 |
| `omni_calls_get_inbox_snapshot` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:78 |
| `omni_calls_get_realtime_media_negotiation` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:105 |
| `omni_calls_get_screen_control_state` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_screen_control.rs:49 |
| `omni_calls_hang_up_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:18 |
| `omni_calls_place_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:34 |
| `omni_calls_publish_realtime_media_capabilities` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:89 |
| `omni_calls_request_remote_control` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:139 |
| `omni_calls_set_local_mic_muted` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:62 |
| `omni_calls_set_screen_control` | `(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_screen_control.rs:37 |
| `omni_calls_wait_change` | `(since: u64, timeout_ms: u32) -> u64` | core-rust/crates/omni-ffi/src/ffi_exports_calls.rs:156 |

## `omni_chat_*` (18 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_chat_end_session` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1139 |
| `omni_chat_get_header_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1074 |
| `omni_chat_get_message_delta` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1213 |
| `omni_chat_get_message_refresh_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1197 |
| `omni_chat_get_message_security_details` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1229 |
| `omni_chat_get_message_security_details_batch` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1245 |
| `omni_chat_get_screen_snapshot` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1090 |
| `omni_chat_get_security_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1181 |
| `omni_chat_get_thread_list_refresh_summary` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1166 |
| `omni_chat_list_messages` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:88 |
| `omni_chat_list_messages_page` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:104 |
| `omni_chat_list_thread_projections` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1155 |
| `omni_chat_mark_read` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1123 |
| `omni_chat_open_session_lease` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1106 |
| `omni_chat_request_attachment_download` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:56 |
| `omni_chat_revoke_attachment_access` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:72 |
| `omni_chat_send_attachment` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:40 |
| `omni_chat_send_text` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:24 |

## `omni_configure_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_configure_platform_secret_wrap_key_hex` | `(key_hex: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:213 |

## `omni_contacts_*` (23 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_contacts_add_by_identifier` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:718 |
| `omni_contacts_add_from_payload` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:750 |
| `omni_contacts_approve` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:798 |
| `omni_contacts_attach_dialog_to_contact` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1293 |
| `omni_contacts_block` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:782 |
| `omni_contacts_confirm_verification` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1058 |
| `omni_contacts_decline` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:947 |
| `omni_contacts_forget` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:963 |
| `omni_contacts_forget_and_reset_peer` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:979 |
| `omni_contacts_get_card` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:995 |
| `omni_contacts_get_card_revision` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1011 |
| `omni_contacts_get_refresh_summary` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:696 |
| `omni_contacts_get_remote_access` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:834 |
| `omni_contacts_get_screen_snapshot` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:707 |
| `omni_contacts_get_verification_state` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1042 |
| `omni_contacts_list` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:685 |
| `omni_contacts_list_remote_access` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:849 |
| `omni_contacts_quick_add_from_dialog` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1277 |
| `omni_contacts_rename` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:766 |
| `omni_contacts_request_by_identifier` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:734 |
| `omni_contacts_set_friend_relay_consent` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:816 |
| `omni_contacts_set_remote_access_granted` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:862 |
| `omni_contacts_start_chat` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1261 |

## `omni_daemon_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_daemon_start` | `(handle: *mut OmniAppHandle, endpoint: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:188 |

## `omni_dht_*` (9 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_dht_bootstrap_add` | `(handle: *mut OmniAppHandle, addr: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:491 |
| `omni_dht_bootstrap_bundle_export` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:523 |
| `omni_dht_bootstrap_bundle_import` | `(handle: *mut OmniAppHandle, token: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:537 |
| `omni_dht_bootstrap_list_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:479 |
| `omni_dht_bootstrap_remove` | `(handle: *mut OmniAppHandle, addr: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:506 |
| `omni_dht_entry_node_endpoint_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:426 |
| `omni_dht_entry_node_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:396 |
| `omni_dht_entry_node_set` | `(handle: *mut OmniAppHandle, mode: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:409 |
| `omni_dht_rendezvous_status_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:439 |

## `omni_discoverability_*` (7 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_discoverability_find_by_omni_id` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:42 |
| `omni_discoverability_get_mode` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:15 |
| `omni_discoverability_get_name_findable` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:60 |
| `omni_discoverability_get_omni_id` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:4 |
| `omni_discoverability_search_by_name` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:85 |
| `omni_discoverability_set_mode` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:26 |
| `omni_discoverability_set_name_findable` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:71 |

## `omni_groups_*` (28 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_groups_accept_invite` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:177 |
| `omni_groups_call_inbox` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:318 |
| `omni_groups_call_snapshot` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:120 |
| `omni_groups_cancel_invite` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:359 |
| `omni_groups_create` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:5 |
| `omni_groups_decline_invite` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:191 |
| `omni_groups_delete_for_everyone` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:219 |
| `omni_groups_dismiss_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:373 |
| `omni_groups_invite` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:78 |
| `omni_groups_join_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:329 |
| `omni_groups_leave` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:205 |
| `omni_groups_leave_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:106 |
| `omni_groups_list` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:19 |
| `omni_groups_list_invites` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:166 |
| `omni_groups_list_members` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:30 |
| `omni_groups_list_messages` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:304 |
| `omni_groups_list_sent_invites` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:344 |
| `omni_groups_publish_media_capabilities` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:150 |
| `omni_groups_remove_member` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:247 |
| `omni_groups_rename` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:261 |
| `omni_groups_send_attachment` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:46 |
| `omni_groups_send_text` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:62 |
| `omni_groups_set_admin` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:290 |
| `omni_groups_set_avatar` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:233 |
| `omni_groups_set_call_media_state` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:134 |
| `omni_groups_set_call_screen_control` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_screen_control.rs:62 |
| `omni_groups_set_newcomer_history` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:276 |
| `omni_groups_start_call` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_groups.rs:92 |

## `omni_identity_*` (46 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_identity_change_local_profile_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:510 |
| `omni_identity_clear_active_profile` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:247 |
| `omni_identity_confirm_delete_current_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:669 |
| `omni_identity_confirm_delete_local_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:653 |
| `omni_identity_confirm_sign_out_current_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:382 |
| `omni_identity_confirm_sign_out_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:398 |
| `omni_identity_create_account` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:64 |
| `omni_identity_delete_local_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:637 |
| `omni_identity_export_current_database_backup` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:17 |
| `omni_identity_export_current_database_backup_bundle` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:33 |
| `omni_identity_get_active_profile` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:193 |
| `omni_identity_get_current_account_export_manifest` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:2 |
| `omni_identity_get_current_profile_gate_summary` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:290 |
| `omni_identity_get_current_profile_lifecycle_summary` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:321 |
| `omni_identity_get_lifecycle_fast_snapshot` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:336 |
| `omni_identity_get_local_account` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:128 |
| `omni_identity_get_local_device` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:155 |
| `omni_identity_get_local_prekey_bundle` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:166 |
| `omni_identity_get_local_profile_footprint_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:558 |
| `omni_identity_get_local_profile_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:258 |
| `omni_identity_get_my_verification_payload` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:1027 |
| `omni_identity_get_password_protection_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:542 |
| `omni_identity_get_profile_gate_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:274 |
| `omni_identity_get_profile_lifecycle_summary` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:305 |
| `omni_identity_inspect_account_backup_manifest` | `(request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:49 |
| `omni_identity_inspect_recovery_input` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:112 |
| `omni_identity_list_local_profiles` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:204 |
| `omni_identity_prepare_delete_current_profile` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:606 |
| `omni_identity_prepare_sign_out_current_profile` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:367 |
| `omni_identity_preview_delete_local_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:590 |
| `omni_identity_preview_revoke_current_device_and_wipe` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:621 |
| `omni_identity_preview_sign_out_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:574 |
| `omni_identity_remove_local_profile_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:526 |
| `omni_identity_reopen_local_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:231 |
| `omni_identity_restore_account_from_export_manifest` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:80 |
| `omni_identity_restore_account_from_input` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:96 |
| `omni_identity_reveal_current_profile_recovery_phrase_with_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:446 |
| `omni_identity_select_active_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:215 |
| `omni_identity_set_local_profile_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:494 |
| `omni_identity_sign_out_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:351 |
| `omni_identity_store_remote_prekey_bundle` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:177 |
| `omni_identity_unlock_current_profile_with_mnemonic` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:478 |
| `omni_identity_unlock_current_profile_with_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:430 |
| `omni_identity_unlock_profile_with_mnemonic` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:462 |
| `omni_identity_unlock_profile_with_password` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:414 |
| `omni_identity_update_local_public_profile` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_account_contacts.rs:139 |

## `omni_local_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_local_transport_endpoint_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:466 |

## `omni_media_*` (2 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_media_open_copy` | `(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:157 |
| `omni_media_read_file` | `(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:144 |

## `omni_mesh_*` (15 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_mesh_carrier_policy_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:116 |
| `omni_mesh_carrier_policy_set` | `(handle: *mut OmniAppHandle, policy: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:129 |
| `omni_mesh_connect_active_call` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:37 |
| `omni_mesh_connection_privacy_mode_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:83 |
| `omni_mesh_connection_privacy_mode_set` | `(handle: *mut OmniAppHandle, mode: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:98 |
| `omni_mesh_connection_state` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:67 |
| `omni_mesh_dial_reach_token` | `(handle: *mut OmniAppHandle, token: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:194 |
| `omni_mesh_direct_tcp_port_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:148 |
| `omni_mesh_direct_tcp_port_set` | `(handle: *mut OmniAppHandle, port: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:162 |
| `omni_mesh_generate_pairing_invite` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:52 |
| `omni_mesh_generate_reach_token` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:180 |
| `omni_mesh_register_pairing_invite` | `(handle: *mut OmniAppHandle, token: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:212 |
| `omni_mesh_rendezvous_reach_contact` | `(handle: *mut OmniAppHandle, account_id: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:236 |
| `omni_mesh_start` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:11 |
| `omni_mesh_stop` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:23 |

## `omni_network_*` (5 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_network_node_add` | `(handle: *mut OmniAppHandle, endpoint: *const c_char, label: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:270 |
| `omni_network_node_list` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:256 |
| `omni_network_node_remove` | `(handle: *mut OmniAppHandle, id: i64) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:289 |
| `omni_network_relay_bundle_import` | `(handle: *mut OmniAppHandle, token: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:579 |
| `omni_network_relay_set_shared` | `(handle: *mut OmniAppHandle, id: i64, shared: i32,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:555 |

## `omni_pinned_*` (3 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_pinned_relay_clear` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:164 |
| `omni_pinned_relay_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:139 |
| `omni_pinned_relay_set` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:150 |

## `omni_publish_*` (2 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_publish_plan_set` | `(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:382 |
| `omni_publish_plans_get` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:369 |

## `omni_queue_*` (2 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_queue_diagnostics` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:131 |
| `omni_queue_snapshot` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:120 |

## `omni_realtime_media_*` (5 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_realtime_media_packet_sink` | `(context: *mut std::ffi::c_void, packet: *const u8, packet_len: usize,)` | core-rust/crates/omni-ffi/src/ffi_exports_realtime_media.rs:40 |
| `omni_realtime_media_packet_target_counters` | `(target_id: u64, out_accepted: *mut u64, out_refused: *mut u64, out_test_dropped: *mut u64,)` | core-rust/crates/omni-ffi/src/ffi_exports_realtime_media.rs:61 |
| `omni_realtime_media_packet_target_create` | `(handle: *mut OmniAppHandle, dialog_key: *const u8, dialog_key_len: usize, out_target_id: *mut u64,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_realtime_media.rs:7 |
| `omni_realtime_media_packet_target_destroy` | `(target_id: u64)` | core-rust/crates/omni-ffi/src/ffi_exports_realtime_media.rs:26 |
| `omni_realtime_media_set_inbound_sink` | `(handle: *mut OmniAppHandle, dialog_key: *const u8, dialog_key_len: usize, sink: Option<OmniRealtimeMediaInboundSink>, context: *mut std::ffi::c_void,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_realtime_media.rs:88 |

## `omni_relay_*` (3 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_relay_directory_get_role` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:101 |
| `omni_relay_directory_search` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:126 |
| `omni_relay_directory_set_role` | `(handle: *mut OmniAppHandle, request_json: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_discoverability.rs:112 |

## `omni_rendezvous_*` (5 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_rendezvous_provider_add` | `(handle: *mut OmniAppHandle, url: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:318 |
| `omni_rendezvous_provider_list` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:304 |
| `omni_rendezvous_provider_remove` | `(handle: *mut OmniAppHandle, id: i64,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:334 |
| `omni_rendezvous_provider_set_mode` | `(handle: *mut OmniAppHandle, id: i64, mode: *const c_char,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:352 |
| `omni_rendezvous_respawn` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_mesh.rs:453 |

## `omni_run_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_run_helper` | `(kind: *const c_char, args_json: *const c_char) -> i32` | core-rust/crates/omni-ffi/src/ffi_exports_helpers.rs:14 |

## `omni_screen_*` (2 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_screen_control_send` | `(handle: *mut OmniAppHandle, request_json: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_screen_control.rs:25 |
| `omni_screen_control_set_sink` | `(handle: *mut OmniAppHandle, sink: Option<OmniScreenControlSink>, context: *mut std::ffi::c_void,) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_screen_control.rs:9 |

## `omni_string_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_string_free` | `(ptr: *mut c_char)` | core-rust/crates/omni-ffi/src/lib.rs:285 |

## `omni_trace_*` (1 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_trace_write` | `(category: *const c_char, message: *const c_char)` | core-rust/crates/omni-ffi/src/ffi_exports_trace.rs:14 |

## `omni_transport_*` (6 exports)

| Export | Signature | File:Line |
|---|---|---|
| `omni_transport_apply_congestion_control_now` | `(handle: *mut OmniAppHandle,) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:244 |
| `omni_transport_discovery_refresh` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:13 |
| `omni_transport_effective_congestion_control` | `() -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:255 |
| `omni_transport_set_congestion_control` | `(mode: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:235 |
| `omni_transport_set_link_cost` | `(cost: *const c_char) -> *mut c_char` | core-rust/crates/omni-ffi/src/lib.rs:226 |
| `omni_transport_snapshot` | `(handle: *mut OmniAppHandle) -> *mut c_char` | core-rust/crates/omni-ffi/src/ffi_exports_transport.rs:2 |

