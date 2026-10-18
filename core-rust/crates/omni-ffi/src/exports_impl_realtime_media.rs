use std::ffi::CStr;

#[allow(dead_code)]
fn parse_utf8_arg(value: *const c_char, name: &str) -> OmniResult<String> {
    if value.is_null() {
        return Err(OmniError::InvalidInput(format!("{name} must not be null")));
    }

    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|error| OmniError::InvalidInput(format!("{name} is not valid UTF-8: {error}")))
        .map(ToOwned::to_owned)
}

/// Wave 2: parses dialog_key passed as raw UTF-8 bytes (no null terminator).
/// Zero-copy — returns a `&str` borrow over the caller's pinned buffer.
fn parse_dialog_key_bytes(ptr: *const u8, len: usize) -> OmniResult<&'static str> {
    if ptr.is_null() {
        return Err(OmniError::InvalidInput(
            "dialog_key must not be null".to_string(),
        ));
    }
    if len == 0 {
        return Err(OmniError::InvalidInput(
            "dialog_key must not be empty".to_string(),
        ));
    }
    // Safety: caller (C#) guarantees `ptr..ptr+len` is a pinned, valid UTF-8
    // buffer for the duration of this call. The `'static` lifetime is fine
    // because we only use the slice inside this stack frame.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes)
        .map_err(|error| OmniError::InvalidInput(format!("dialog_key is not valid UTF-8: {error}")))
}

/// One call's media send path from the media engine into the core.
///
/// The engine's transport (libwebrtc's pacer, the RTCP of the receive half, the audio encoder)
/// calls [`omni_realtime_media_packet_sink`] with this target's ID as its context, once per packet,
/// on whatever thread produced it. Everything the send needs from storage was resolved when the
/// target was created, so the per-packet path runs without the FFI serializer (see
/// `OmniApp::send_realtime_media_packet`).
///
/// The context is an ID into [`packet_targets`], not a pointer: an engine object that outlives its
/// call (a receiver still being built on a worker when the call stops) then calls into a missing
/// entry and is ignored, instead of into freed memory.
///
/// Before 2026-09-26 the shell drained the engine's queue on a timer and sent batches; see
/// docs/bugs/media-rode-the-shells-timer.md.
pub struct RealtimeMediaPacketTarget {
    handle: *mut OmniAppHandle,
    route: omni_app::RealtimeMediaRoute,
    /// Stand knob: drop this share of outgoing packets before they reach the core
    /// (`OMNI_MEDIA_TEST_PACKET_LOSS_PERCENT`, 0..=100). Read once, at creation.
    test_loss_percent: u32,
    test_rng: std::sync::atomic::AtomicU64,
    accepted: std::sync::atomic::AtomicU64,
    refused: std::sync::atomic::AtomicU64,
    test_dropped: std::sync::atomic::AtomicU64,
}

// SAFETY: `handle` points at the app, which outlives every call's media path (the shell destroys
// the target before it releases the app), and the per-packet path only reaches state behind its
// own locks (see `OmniApp::send_realtime_media_packet`).
unsafe impl Send for RealtimeMediaPacketTarget {}
unsafe impl Sync for RealtimeMediaPacketTarget {}

impl RealtimeMediaPacketTarget {
    /// xorshift64 — a stand knob needs spread, not quality.
    fn next_percent(&self) -> u32 {
        use std::sync::atomic::Ordering;
        let mut state = self.test_rng.load(Ordering::Relaxed);
        loop {
            let mut next = state;
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            match self
                .test_rng
                .compare_exchange_weak(state, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return (next % 100) as u32,
                Err(current) => state = current,
            }
        }
    }

    fn send(&self, packet: &[u8]) {
        use std::sync::atomic::Ordering;
        if packet.is_empty() {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.test_loss_percent > 0 && self.next_percent() < self.test_loss_percent {
            self.test_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: see the Send/Sync note — the app outlives the target.
        let app = unsafe { &(*self.handle).inner };
        match app.send_realtime_media_packet(&self.route, packet) {
            Ok(()) => self.accepted.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.refused.fetch_add(1, Ordering::Relaxed),
        };
    }
}

type PacketTargets = parking_lot::RwLock<std::collections::HashMap<u64, std::sync::Arc<RealtimeMediaPacketTarget>>>;

fn packet_targets() -> &'static PacketTargets {
    static TARGETS: std::sync::OnceLock<PacketTargets> = std::sync::OnceLock::new();
    TARGETS.get_or_init(Default::default)
}

fn packet_target(id: u64) -> Option<std::sync::Arc<RealtimeMediaPacketTarget>> {
    packet_targets().read().get(&id).cloned()
}

/// The engine's receive entry, same shape as the send sink (`OmniWebRtcMediaPacketSink`).
pub type OmniRealtimeMediaInboundSink =
    unsafe extern "C" fn(context: *mut std::ffi::c_void, packet: *const u8, packet_len: usize);

fn realtime_media_set_inbound_sink_impl(
    handle: *mut OmniAppHandle,
    dialog_key: *const u8,
    dialog_key_len: usize,
    sink: Option<OmniRealtimeMediaInboundSink>,
    context: *mut std::ffi::c_void,
) -> OmniResult<()> {
    let dialog_key = parse_dialog_key_bytes(dialog_key, dialog_key_len)?;
    let sink = sink.map(|sink| {
        // The context is the engine's, opaque here; carried as an integer so the closure is Send.
        let context = context as usize;
        std::sync::Arc::new(move |packet: &[u8]| {
            // SAFETY: the shell keeps the engine behind `context` alive until it unwires this sink,
            // and unwiring waits for in-flight deliveries (RealtimeMediaInboundSinks).
            unsafe { sink(context as *mut std::ffi::c_void, packet.as_ptr(), packet.len()) }
        }) as omni_app::RealtimeMediaInboundSink
    });
    require_handle(handle)?
        .inner
        .set_realtime_media_inbound_sink(dialog_key, sink)
}

fn read_test_loss_percent() -> u32 {
    // Fault injection is a test-stand knob: release builds ignore it unless built with `dev-tools`.
    if !cfg!(any(debug_assertions, feature = "dev-tools")) {
        return 0;
    }
    std::env::var("OMNI_MEDIA_TEST_PACKET_LOSS_PERCENT")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map(|value| value.min(100))
        .unwrap_or(0)
}

fn realtime_media_packet_target_create_impl(
    handle: *mut OmniAppHandle,
    dialog_key: *const u8,
    dialog_key_len: usize,
    out_target_id: *mut u64,
) -> OmniResult<()> {
    if out_target_id.is_null() {
        return Err(OmniError::InvalidInput("out_target_id must not be null".to_string()));
    }
    unsafe { *out_target_id = 0 };
    let dialog_key = parse_dialog_key_bytes(dialog_key, dialog_key_len)?;
    let route = require_handle(handle)?.inner.realtime_media_route(dialog_key)?;
    let seed = (current_unix_ms_for_ffi() as u64) | 1;
    let target = std::sync::Arc::new(RealtimeMediaPacketTarget {
        handle,
        route,
        test_loss_percent: read_test_loss_percent(),
        test_rng: std::sync::atomic::AtomicU64::new(seed),
        accepted: std::sync::atomic::AtomicU64::new(0),
        refused: std::sync::atomic::AtomicU64::new(0),
        test_dropped: std::sync::atomic::AtomicU64::new(0),
    });
    // IDs start at 1: 0 is "no target", the value the out parameter holds on failure.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    packet_targets().write().insert(id, target);
    unsafe { *out_target_id = id };
    Ok(())
}

fn current_unix_ms_for_ffi() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(1)
}


