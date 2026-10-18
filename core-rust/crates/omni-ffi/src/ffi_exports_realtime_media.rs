/// Create one call's media send target (see `RealtimeMediaPacketTarget`). Resolves the route
/// under the FFI serializer, once. On success `*out_target_id` is non-zero; it is the context to
/// hand the engine next to [`omni_realtime_media_packet_sink`], and is released with
/// [`omni_realtime_media_packet_target_destroy`]. A 1:1 dialog without an approved contact is
/// refused here.
#[no_mangle]
pub extern "C" fn omni_realtime_media_packet_target_create(
    handle: *mut OmniAppHandle,
    dialog_key: *const u8,
    dialog_key_len: usize,
    out_target_id: *mut u64,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        realtime_media_packet_target_create_impl(handle, dialog_key, dialog_key_len, out_target_id)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}

/// Release a target. Packets the engine still hands over for this ID afterwards are ignored.
#[no_mangle]
pub extern "C" fn omni_realtime_media_packet_target_destroy(target_id: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        packet_targets().write().remove(&target_id);
    }));
}

/// The media engine's packet sink (`OmniWebRtcMediaPacketSink` in omni_webrtc_media.h): called
/// once per outgoing RTP/RTCP packet with a target ID as `context`. Never fails and never unwinds —
/// the outcome is counted on the target (`_counters`). An unknown ID (the target is already gone)
/// or a null packet is ignored.
#[no_mangle]
// A C callback handed to the engine as a plain function pointer, so it cannot be `unsafe fn`; the
// pointer is null-checked and only read for `packet_len` bytes the engine guarantees.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn omni_realtime_media_packet_sink(
    context: *mut std::ffi::c_void,
    packet: *const u8,
    packet_len: usize,
) {
    if packet.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(target) = packet_target(context as usize as u64) else {
            return;
        };
        // SAFETY: the engine passes a buffer valid for the duration of this call.
        let packet = unsafe { std::slice::from_raw_parts(packet, packet_len) };
        target.send(packet);
    }));
}

/// Packets the core took (`accepted`), refused (no negotiation yet, no link, bad packet), and the
/// stand knob dropped on purpose (`test_dropped`). Any out pointer may be null.
#[no_mangle]
pub extern "C" fn omni_realtime_media_packet_target_counters(
    target_id: u64,
    out_accepted: *mut u64,
    out_refused: *mut u64,
    out_test_dropped: *mut u64,
) {
    use std::sync::atomic::Ordering;
    let Some(target) = packet_target(target_id) else {
        return;
    };
    for (out, counter) in [
        (out_accepted, &target.accepted),
        (out_refused, &target.refused),
        (out_test_dropped, &target.test_dropped),
    ] {
        if !out.is_null() {
            unsafe { *out = counter.load(Ordering::Relaxed) };
        }
    }
}

/// Wire the media engine that receives this dialog's inbound media: every datagram that passes the
/// media gate is handed to `sink(context, …)` at once, on the thread that delivered it (the mesh
/// pump). A null `sink` unwires; that call returns only when no delivery is in flight, so the
/// engine behind `context` may be destroyed right after. Replaces the per-dialog queue the shell
/// drained on a timer — docs/bugs/media-rode-the-shells-timer.md.
#[no_mangle]
pub extern "C" fn omni_realtime_media_set_inbound_sink(
    handle: *mut OmniAppHandle,
    dialog_key: *const u8,
    dialog_key_len: usize,
    sink: Option<OmniRealtimeMediaInboundSink>,
    context: *mut std::ffi::c_void,
) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        realtime_media_set_inbound_sink_impl(handle, dialog_key, dialog_key_len, sink, context)
    })) {
        Ok(Ok(())) => std::ptr::null_mut(),
        Ok(Err(error)) => to_error_envelope_c_string(error),
        Err(_) => to_error_envelope_c_string(OmniError::Internal(
            "panic crossed FFI boundary".to_string(),
        )),
    }
}


