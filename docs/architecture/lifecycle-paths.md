---
tags: [architecture, lifecycle, hot-paths, manual]
status: living
---

# Hot lifecycle paths

Manually curated. Updated when call chains shift across refactors.

The goal is to give readers a ready-made chain of functions for the common operations, so they do
not have to grep for them each time. Chains list **function → function**, not line numbers, so they
survive refactors.

## Sign-out / profile switch

`OmniApp::sign_out` (or `prepare_sign_out` → `confirm_sign_out`)
→ `ProfileLifecycleCoordinator::stop_runtime_context` (`app_profile_lifecycle.rs`)
→ `transport.stop_discovery` + `transport.stop_listener` + `clear_transport_local_identity`
→ **All static-registry clears:**
- `clear_runtime_chat_sessions_for_active_profile` (`runtime_chat_sessions.rs`)
- `clear_call_runtime_for_profile` (`app_calls.rs`)
- `clear_realtime_media_negotiation_for_profile` (`app_realtime_media.rs`)
- `clear_failed_challenge_probe_cooldowns_for_profile` (`runtime_queue.rs`)
- `clear_verification_challenges_for_profile` / `clear_public_profile_syncs_for_profile` /
  `clear_attachment_buffers_for_profile` (`runtime_transport_event_helpers.rs`)

**Invariant**: a new profile never sees stale state from the previous one.
**Bug class**: a new `static` that is not added to `stop_runtime_context` makes memory grow
across profile switches.

## Send text message

`OmniApp::send_text` (`app_chat_messaging.rs`)
→ `resolve_canonical_dialog_key_for_selector` + active snapshot resolution
→ Approval gate: `storage.resolve_contact_card_for_dialog` → must be `Approved`
→ `resume_chat_session_lease_for_profile` (sets `is_open_lease=true`)
→ `touch_runtime_chat_session_interaction(Message)`
→ `lock_app_session_guard` (poison-recovering, see `session-lock-recovery.md`)
→ `storage.insert_outgoing_message` (MessageStatus::Queued)
→ `attempt_send_existing_message` (`runtime_queue.rs`)
→ `send_existing_message_encrypted`:
  - resolve/bootstrap session
  - check `runtime_chat_session_open_lease_waits_for_keepalive`
    (see `open-lease-keepalive-wait.md`)
  - `build_encrypted_text_envelope` or `build_encrypted_attachment_offer_envelope`
  - `transport.send_wire_envelope_for_dialog_key`
  - `storage.upsert_message_security_metadata`
→ Status: `Sent` / `Retrying` / `Failed`

## Receive encrypted wire envelope

Transport callback → `ReceivedTransportEvent::WireEnvelope`
→ `handle_incoming_wire_envelope` (`runtime_transport_events.rs`)
→ `decrypt_wire_payload` → `WirePayloadV1` variant dispatch
→ Per-variant handler (e.g. `handle_incoming_text`, `handle_incoming_attachment_offer`,
  `handle_incoming_realtime_media_capabilities`, etc.)
→ For attachments: approval gate via `attachment_control_event_allowed`
  (TM-002 closed); the offer is never refused. The per-dialog bound (TM-005) only stops a photo
  from requesting itself past 8 downloads in flight.

## Receive realtime media datagram

Transport callback → `ReceivedTransportEvent::RealtimeMediaDatagram`
→ `push_incoming_realtime_media_datagram` (`app_realtime_media.rs`)
→ `dialog_has_active_media_negotiation` (registry check)
  - **dropped silently** if there is no active negotiation (defense in depth, TM-001/TM-007)
→ `RealtimeMediaDatagramQueues::push` (per-dialog FIFO, bounded)

Drain side (called from C# via FFI):
`OmniApp::drain_realtime_media_datagrams` → `ensure_realtime_media_dialog_is_approved`
(on downgrade: clear the queue, return empty) → `RealtimeMediaDatagramQueues::drain`

## Realtime media negotiation

**Publish (local):**
`OmniApp::publish_realtime_media_capabilities`
→ `normalize_realtime_media_capabilities` + validate
→ `store_local_realtime_media_capabilities` (registry update)
→ `refresh_realtime_media_selection` (computes the selection if both sides are present)
→ sends a `RealtimeMediaCapabilitiesV1` event over the reliable wire
→ **If offerer** (`is_local_offerer_for_dialog` = lexicographically smaller account_id),
  also sends a `RealtimeMediaSelectionV1` event. See `media-negotiation-role.md`.

**Receive (peer capabilities):**
`handle_incoming_realtime_media_capabilities` → `store_remote_realtime_media_capabilities`
→ `refresh_realtime_media_selection` → **if offerer**, emit the selection

**Receive (peer selection):**
`handle_incoming_realtime_media_selection` → `store_remote_realtime_media_selection`
(overrides any local selection)

## Call end (decline / hangup / missed / connection lost)

All paths converge on `end_runtime_call` (`app_calls.rs`):
- `OmniApp::decline_call` → `end_runtime_call(CallOutcome::Declined, reason)`
- `OmniApp::hang_up_call` → `end_runtime_call(Completed|Cancelled)`
- `tick_call_runtime` timeout → `end_runtime_call(Missed)`
- `handle_call_session_closed` reconnect grace expired → `end_runtime_call(ConnectionLost)`
- `handle_call_event_v1` peer cancel/hangup → `end_runtime_call(...)`

`end_runtime_call`:
- marks `CallSummary` as Ended and sets the outcome/decline_reason
- `storage.upsert_call_session`
- `set_current_call_runtime(db_path, None)` clears `CALL_RUNTIME_REGISTRY`
- `clear_realtime_media_negotiation_for_dialog` clears the media registry
  (see `call-end-cleanup.md`)

**Not cleared in `end_runtime_call`** (cleared later on `SessionClosed`):
- `RealtimeMediaDatagramQueues`
- `RealtimeMediaControlQueues`

C# side teardown: `CallVideoService.StopAsync` cancels `mediaCancellation` and
awaits the tracked tasks (`activeFrameProcessingTask`, `localPreviewLoopTask`,
`remoteRenderLoopTask`, `captureReconfigureTask`) before the native dispose.
See `csharp-call-task-lifecycle.md` and `webrtc-quality-scaler-adoption.md`.

## Queue worker tick (every 250 ms)

`run_queue_worker` loop → `process_queue_tick` (`runtime_queue.rs`):
1. `is_profile_signed_in` short-circuit
2. `apply_failed_challenge_probe_cooldowns_for_db_path` (drains a static registry)
3. `transport.snapshot`
4. Under `lock_app_session_guard`:
   - `promote_overdue_sent_messages` (batched in one transaction,
     see `storage-batching.md`)
   - `pause_inflight_attachment_downloads` (if disconnected)
   - `rebind_pending_dialog_scope` (legacy → new dialog key migration)
   - `process_pending_for_active_dialog` (send queued messages)
   - `process_pending_attachment_downloads_for_active_dialog`
   - `send_current_public_profile_sync_for_session` (when connected and a session exists)
   - `resume_attachment_preparations` (every 10 s: outgoing files left in `preparing` with no pass
     reading them, e.g. after a restart; `app_attachment_preparation.rs`)
   - `process_contact_presence_tick` (per-contact heartbeat, with presence ping dedup)
   - `maintain_open_chat_session_leases`
   - `expire_stale_contact_presence`
5. Outside the lock: `maybe_probe_pending_dialog_routes`, `maybe_probe_discovered_friends`

## Attachment download

**Sender side** (response to `AttachmentDownloadRequest`):
`process_pending_attachment_downloads_for_active_dialog` selects the in-flight transfers
→ chunked send: `attempt_send_attachment_download_request` →
`send_attachment_over_sink` (resume offset, leaf-aligned) → per 16 MiB block: read, check against
the stored content tree, then Start / Chunk (the first of a block carries its proof) / Complete frames.

**Receiver side** (see `docs/decisions/attachment-content-tree.md`):
- Bulk stream frames → `persist_attachment_transfer_chunk_with_writer`:
  - verify the block proof against the offer's content id, and each chunk against its leaf
  - `write_checked_attachment_chunk` at its offset (one writer per transfer)
  - checkpoint: `sync_data` → `put_attachment_tree_blocks` → `mark_attachment_downloading`
- On complete: `finish_attachment_transfer` →
  `complete_attachment_download_from_temp_with_verified_tree` → move to the user path

## FFI ingress (C# → Rust)

C# `RustBridge.Method` → `NativeMethods.omni_xxx_impl` P/Invoke
→ Rust `omni_xxx_impl` function (`omni-ffi/src/exports_impl_xxx.rs`)
→ JSON parse via `parse_json_request<RequestT>`
→ `require_handle(handle)` → `OmniAppHandle.inner` (= `OmniApp`)
→ `app.method_name(...)` call into orchestration
→ the result is serialized to JSON and returned as `*mut c_char`
→ C# `ReadUtf8Response` (which calls `omni_string_free` in a finally block)

**Binary pump (media datagrams), not JSON:**
- C# pins the payload bytes → `omni_realtime_media_send_datagram(handle, dialog_key, *const u8, len)`
- Rust copies the bytes via `to_vec()` (per packet) →
  `transport.send_realtime_media_datagram_for_dialog_key`

## Transport callback dispatch

Inside `OmniApp::init`, the transport is given a callback. Variants:
- `WireEnvelope` → decrypt → dispatch to the handler for each `WirePayloadV1`
- `RealtimeMediaDatagram` → `push_incoming_realtime_media_datagram`
- `AttachmentTransferFrame` / `AttachmentTransferStream` → attachment handlers
- `SessionClosed` → `clear_realtime_media_runtime_for_context` + `handle_session_closed`
- `SessionActivated` → bootstrap / contact-state updates

All run on the **transport-runtime thread**. They use `runtime_event_storage_for_callback`,
`transport_handle_for_callback` and `session_lock_for_callback`, cloned from
`OmniApp` init. Locks are acquired per call.

## Failure mode index

| Symptom | Where to look |
|---|---|
| Messages stuck at "Queued" status | `process_pending_for_active_dialog`, `runtime_chat_session_open_lease_waits_for_keepalive` (see `open-lease-keepalive-wait.md`) |
| Static registry leak across profiles | the `stop_runtime_context` clear chain: was a new `static` added? |
| Datagram dropped silently | `dialog_has_active_media_negotiation` returns false: no negotiation, peer not approved, or the call has ended |
| AccessViolation on native dispose | the C# StopAsync await chain: is a tracked task missing? `csharp-call-task-lifecycle.md` |
| Both peers emit different selections | the `is_local_offerer_for_dialog` gate: `media-negotiation-role.md` |
| Attachment transfer stuck | the sum of temp bytes is near the quota, or the 24 h TTL: `attachment-transfer-quotas.md` |
| Session lock poisoning stalls the queue | recovery via `lock_app_session_guard`: `session-lock-recovery.md` |

## Maintenance

This file is maintained manually. It is updated when:
- a function name in a hot path changes (rename refactor)
- a new registry appears: it is added to the sign-out chain and the symbol map
- a new transport-callback variant appears: the dispatch section is updated
- a new static appears: it is added to the cleanup section

The file does not aim to be exhaustive (`code-map.md` already covers that). It is a guide for
following cross-cutting concerns through the code.
