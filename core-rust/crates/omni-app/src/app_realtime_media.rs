const REALTIME_MEDIA_NEGOTIATION_SESSION_PREFIX: &str = "media-v2";

/// Inbound realtime-media counters for the trace (see the gate in
/// `push_incoming_realtime_media_datagram_for_dialog`). Process-wide and advisory — they exist to
/// answer "no audio/video: is media arriving, and is the approval gate eating it?", which the
/// media path could not answer at all (it had zero instrumentation).
static MEDIA_DATAGRAMS_DROPPED_NO_NEGOTIATION: AtomicU64 = AtomicU64::new(0);
static MEDIA_DATAGRAMS_DROPPED_NO_SINK: AtomicU64 = AtomicU64::new(0);
static MEDIA_DATAGRAMS_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static MEDIA_DATAGRAMS_SENT: AtomicU64 = AtomicU64::new(0);

/// Outbound counterpart of the inbound media counters, reported at the same cadence and naming the
/// LANE the batch took (mesh link vs legacy transport). Together the three counters localize a
/// silent call: nothing sent ⇒ capture/shell side; sent but nothing received ⇒ the carrier; received
/// but dropped ⇒ the media approval gate.
/// Timestamp + total at the previous sampled line, so each line can state the RATE over the
/// window it samples instead of only the one batch that happened to trip it.
static MEDIA_DATAGRAM_OUT_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
static MEDIA_DATAGRAM_OUT_LAST_TOTAL: AtomicU64 = AtomicU64::new(0);
static MEDIA_DATAGRAM_IN_LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);

fn trace_media_datagram_out(dialog_key: &str, lane: &str, bytes: usize) {
    let sent = MEDIA_DATAGRAMS_SENT.fetch_add(1, Ordering::Relaxed) + 1;
    if sent == 1 || sent.is_multiple_of(250) {
        // This line is SAMPLED — one per 250 packets — so `bytes` describes only the packet that
        // tripped the sample, never the traffic. Reading it as the traffic has already cost a
        // wrong diagnosis once (2026-08-11: a healthy screen share at ~100 packets/s was read as
        // "almost nothing reaches the wire"). The window rate below is the number to read.
        let now_ms = current_unix_ms().max(0) as u64;
        let previous_ms = MEDIA_DATAGRAM_OUT_LAST_LOG_MS.swap(now_ms, Ordering::Relaxed);
        let previous_total = MEDIA_DATAGRAM_OUT_LAST_TOTAL.swap(sent, Ordering::Relaxed);
        let window_ms = now_ms.saturating_sub(previous_ms);
        let window_packets = sent.saturating_sub(previous_total);
        let rate = if previous_ms == 0 || window_ms == 0 {
            0
        } else {
            window_packets.saturating_mul(1_000) / window_ms
        };
        omni_app_trace_log(
            "media/datagram-out",
            &format!(
                "dialog={dialog_key} lane={lane} bytes={bytes} total={sent} \
                 window={window_packets}p/{window_ms}ms rate={rate}p/s"
            ),
        );
    }
}

/// Where one call's media packets go, resolved once when the call's media path is wired
/// (`OmniApp::realtime_media_route`) so that the per-packet send never touches storage.
#[derive(Debug, Clone)]
pub struct RealtimeMediaRoute {
    db_path: String,
    dialog_key: String,
    /// The 1:1 peer's mesh node; `None` for a group dialog or a contact with no known identity key
    /// (media then goes over the legacy transport session).
    mesh_node: Option<MeshNodeAddress>,
}

/// The receive entry of one call's media engine: takes one inbound packet, on the thread that
/// delivered it.
pub type RealtimeMediaInboundSink = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// Where each call's inbound media goes: dialog → its media engine, called at once for every
/// datagram that passes the media gate.
///
/// Until 2026-09-26 inbound media sat in a per-dialog queue that the shell drained on a timer
/// (~15.6 ms on Windows), and the arrival time libwebrtc's congestion controller saw was that
/// tick, not the packet's. See docs/bugs/media-rode-the-shells-timer.md.
///
/// The sink is called UNDER the read lock on purpose: `set(dialog, None)` then returns only when no
/// call is in flight, so the engine behind the sink can be torn down right after.
#[derive(Default)]
struct RealtimeMediaInboundSinks {
    by_dialog: std::sync::RwLock<std::collections::HashMap<String, RealtimeMediaInboundSink>>,
}

impl RealtimeMediaInboundSinks {
    fn set(&self, dialog_key: &str, sink: Option<RealtimeMediaInboundSink>) -> OmniResult<()> {
        let dialog_key = normalize_dialog_key(dialog_key);
        if dialog_key.trim().is_empty() {
            return Err(OmniError::InvalidInput("dialog_key must not be empty".to_string()));
        }
        let mut sinks = self.by_dialog.write().map_err(|_| {
            OmniError::Internal("realtime media sink registry lock is poisoned".to_string())
        })?;
        match sink {
            Some(sink) => {
                sinks.insert(dialog_key, sink);
            }
            None => {
                sinks.remove(&dialog_key);
            }
        }
        Ok(())
    }

    /// `false` when the dialog has no sink (no call is wired to receive it).
    fn deliver(&self, dialog_key: &str, payload: &[u8]) -> bool {
        let dialog_key = normalize_dialog_key(dialog_key);
        let Ok(sinks) = self.by_dialog.read() else {
            return false;
        };
        match sinks.get(&dialog_key) {
            Some(sink) => {
                sink(payload);
                true
            }
            None => false,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct RealtimeMediaNegotiationRuntime {
    by_dialog: std::collections::HashMap<String, RealtimeMediaDialogNegotiation>,
}

#[derive(Debug, Default, Clone)]
struct RealtimeMediaDialogNegotiation {
    local_capabilities: Option<RealtimeMediaCapabilitiesV1>,
    remote_capabilities: Option<RealtimeMediaCapabilitiesV1>,
    selected: Option<RealtimeMediaSelectionV1>,
    diagnostic: Option<String>,
    updated_at_unix_ms: i64,
    /// Group dialog: our full passport (the selection above is built from it narrowed to one codec).
    group_full_capabilities: Option<RealtimeMediaCapabilitiesV1>,
    /// Group dialog: the codec we send to everybody, and when it was picked (the 10-second rule).
    group_send_codec: Option<(omni_types::RealtimeVideoCodec, i64)>,
}

static REALTIME_MEDIA_NEGOTIATION_REGISTRY: std::sync::OnceLock<
    Mutex<std::collections::HashMap<String, RealtimeMediaNegotiationRuntime>>,
> = std::sync::OnceLock::new();

impl OmniApp {
    pub fn publish_realtime_media_capabilities(
        &self,
        capabilities: RealtimeMediaCapabilitiesV1,
    ) -> OmniResult<RealtimeMediaNegotiationSnapshot> {
        let _ = self.ensure_runtime_profile_active("publish_realtime_media_capabilities")?;
        let existing_id = local_capabilities_id_for_dialog(
            &self.storage.db_path().to_string_lossy(),
            &normalize_dialog_key(&capabilities.dialog_key),
        );
        let capabilities = normalize_realtime_media_capabilities(with_capabilities_id(
            capabilities,
            existing_id,
        ))?;
        let dialog_key = capabilities.dialog_key.clone();

        // This call sits between "the call was answered" and "the peer hears us", so each stage
        // of it is timed: the whole publish was measured at 5 s on a live pair while the stages
        // inside it were invisible. See docs/bugs/slow-media-start.md.
        let entered_at_ms = current_unix_ms();
        let mut snapshot = store_local_realtime_media_capabilities(
            &self.storage.db_path().to_string_lossy(),
            capabilities.clone(),
            current_unix_ms(),
        )?;
        let stored_at_ms = current_unix_ms();
        let event = omni_types::LogicalEventEnvelopeV1::realtime_media_capabilities(capabilities);
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        let locked_at_ms = current_unix_ms();
        send_realtime_media_event_for_dialog(
            &self.storage,
            &self.transport,
            &dialog_key,
            &event,
            SecurityPayloadKind::RealtimeMediaCapabilities,
        )?;
        let sent_at_ms = current_unix_ms();
        omni_app_trace_log(
            "media/publish-stage",
            &format!(
                "dialog={dialog_key} store={}ms session_lock={}ms send_capabilities={}ms",
                stored_at_ms - entered_at_ms,
                locked_at_ms - stored_at_ms,
                sent_at_ms - locked_at_ms
            ),
        );

        if let Some(selection) = snapshot.selected.clone() {
            // Only the offerer (lex-smaller account_id) emits selection on
            // publish. Mirror of the gate in store_remote_realtime_media_capabilities.
            // See docs/decisions/media-negotiation-role.md.
            if is_local_offerer_for_dialog(&self.storage, &dialog_key)? {
                let event =
                    omni_types::LogicalEventEnvelopeV1::realtime_media_selection(selection);
                send_realtime_media_event_for_dialog(
                    &self.storage,
                    &self.transport,
                    &dialog_key,
                    &event,
                    SecurityPayloadKind::RealtimeMediaSelection,
                )?;
                snapshot = realtime_media_negotiation_snapshot_for_dialog(
                    &self.storage.db_path().to_string_lossy(),
                    &dialog_key,
                )?;
                omni_app_trace_log(
                    "media/publish-stage",
                    &format!(
                        "dialog={dialog_key} send_selection={}ms",
                        current_unix_ms() - sent_at_ms
                    ),
                );
            }
        }

        Ok(snapshot)
    }

    /// Group call: publish our media capabilities and self-select the call's format (there is no 1:1
    /// pairwise negotiation in a group). Our passport also rides our call media to the other
    /// participants (`GROUP_PASSPORT_MAGIC`), so every other sender picks its codec for us from it; and we send everybody the ONE codec they all decode
    /// ([`reselect_group_media`], docs/decisions/per-pair-video-codec-selection.md §9, phase 4a).
    /// Receivers build each sender's decoder from the stream itself (its RTP payload type), so the
    /// pick needs no answer. The dialog_key on `capabilities` must be `group:<id>`; a 1:1 dialog is
    /// rejected so this can't be used to bypass the pairwise negotiation gate.
    pub fn publish_group_media_capabilities(
        &self,
        capabilities: RealtimeMediaCapabilitiesV1,
    ) -> OmniResult<RealtimeMediaNegotiationSnapshot> {
        let _ = self.ensure_runtime_profile_active("publish_group_media_capabilities")?;
        let capabilities =
            normalize_realtime_media_capabilities(with_capabilities_id(capabilities, None))?;
        let Some(group_id) = group_id_from_dialog_key(&capabilities.dialog_key).map(str::to_string) else {
            return Err(OmniError::InvalidInput(
                "publish_group_media_capabilities requires a group:<id> dialog".to_string(),
            ));
        };
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let dialog_key = capabilities.dialog_key.clone();
        {
            let mut registry = lock_realtime_media_negotiation_registry()?;
            let negotiation = registry
                .entry(db_path.clone())
                .or_default()
                .by_dialog
                .entry(dialog_key.clone())
                .or_default();
            negotiation.group_full_capabilities = Some(capabilities.clone());
        }
        crate::app_group_calls::store_own_group_video_capabilities(&db_path, &group_id, &capabilities);
        reselect_group_media(&db_path, &dialog_key);
        realtime_media_negotiation_snapshot_for_dialog(&db_path, &dialog_key)
    }

    pub fn get_realtime_media_negotiation(
        &self,
        dialog_key: impl AsRef<str>,
    ) -> OmniResult<RealtimeMediaNegotiationSnapshot> {
        let _ = self.ensure_runtime_profile_active("get_realtime_media_negotiation")?;
        realtime_media_negotiation_snapshot_for_dialog(
            &self.storage.db_path().to_string_lossy(),
            dialog_key.as_ref(),
        )
    }

    pub fn clear_realtime_media_negotiation(
        &self,
        dialog_key: impl AsRef<str>,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("clear_realtime_media_negotiation")?;
        clear_realtime_media_negotiation_for_dialog(
            &self.storage.db_path().to_string_lossy(),
            dialog_key.as_ref(),
        )
    }

    /// Resolve, ONCE per call, everything the per-packet send needs from storage: whether the dialog
    /// is a group, and the peer's mesh node. See [`Self::send_realtime_media_packet`] for why the
    /// packet path must not touch storage at all.
    pub fn realtime_media_route(&self, dialog_key: impl AsRef<str>) -> OmniResult<RealtimeMediaRoute> {
        let dialog_key = normalize_dialog_key(dialog_key.as_ref());
        if dialog_key.trim().is_empty() {
            return Err(OmniError::InvalidInput("dialog_key must not be empty".to_string()));
        }
        let mesh_node = if group_id_from_dialog_key(&dialog_key).is_some()
            || group_feedback_from_dialog_key(&dialog_key).is_some()
        {
            None
        } else {
            ensure_realtime_media_dialog_is_approved(&self.storage, &dialog_key)?;
            self.mesh_node_for_dialog(&dialog_key)?
        };
        Ok(RealtimeMediaRoute {
            db_path: self.storage.db_path().to_string_lossy().into_owned(),
            dialog_key,
            mesh_node,
        })
    }

    /// Send ONE media packet (RTP or RTCP) — called straight from the media engine's transport,
    /// i.e. from libwebrtc's pacer the moment it releases the packet.
    ///
    /// Touches NO storage, so it runs without the FFI serializer: that lock guards SQLite, and a
    /// pacer queued behind a slow UI query would stall the very timing it exists to keep. The gate
    /// is the in-memory negotiation registry — an entry exists only after
    /// `ensure_realtime_media_dialog_is_approved` passed for this dialog, the equivalence the
    /// inbound gate already relies on (docs/decisions/media-approval-gate.md).
    ///
    /// Until 2026-09-26 the shell drained the engine's queue on a timer (~15.6 ms on Windows) and
    /// sent what it found in one batch: libwebrtc's pacing was re-bunched into bursts, and the send
    /// time the congestion controller recorded was taken BEFORE the packet waited for that tick.
    /// See docs/bugs/media-rode-the-shells-timer.md.
    pub fn send_realtime_media_packet(
        &self,
        route: &RealtimeMediaRoute,
        payload: &[u8],
    ) -> OmniResult<()> {
        let dialog_key = route.dialog_key.as_str();
        // Group call (G9): a `group:<id>` dialog routes into the peer-SFU media
        // path (seal per-sender + fan-out to targets), NOT the 1:1 mesh/legacy
        // path. Approval here is group membership + a live call, enforced inside
        // send_group_media_batch (it drops if there's no active call).
        if let Some(group_id) = group_id_from_dialog_key(dialog_key) {
            return self.send_group_media_batch(group_id, vec![payload.to_vec()]);
        }
        if let Some((group_id, sender_tag)) = group_feedback_from_dialog_key(dialog_key) {
            return self.send_group_feedback(group_id, &sender_tag, payload);
        }
        if !dialog_has_active_media_negotiation(&route.db_path, dialog_key) {
            return Err(OmniError::InvalidInput(
                "realtime media requires an active negotiation with an approved contact"
                    .to_string(),
            ));
        }
        // Media rides the peer's verified mesh link; without one there is nowhere to send it.
        let (Some(node), Some(runtime)) = (route.mesh_node, self.mesh_runtime()) else {
            return Err(OmniError::Transport(format!(
                "no mesh link for realtime media in dialog '{dialog_key}'"
            )));
        };
        if !runtime.has_link(&node) {
            return Err(OmniError::Transport(format!(
                "no mesh link for realtime media in dialog '{dialog_key}'"
            )));
        }
        runtime
            .send_media(node, payload)
            .map_err(|error| OmniError::Transport(format!("mesh media send failed: {error}")))?;
        trace_media_datagram_out(dialog_key, "mesh", payload.len());
        Ok(())
    }


    /// Wire (or, with `None`, unwire) the media engine that receives this dialog's inbound media.
    /// Unwiring returns only when no delivery is in flight.
    pub fn set_realtime_media_inbound_sink(
        &self,
        dialog_key: impl AsRef<str>,
        sink: Option<RealtimeMediaInboundSink>,
    ) -> OmniResult<()> {
        self.realtime_media_inbound.set(dialog_key.as_ref(), sink)
    }

}

fn lock_realtime_media_negotiation_registry() -> OmniResult<
    std::sync::MutexGuard<'static, std::collections::HashMap<String, RealtimeMediaNegotiationRuntime>>,
> {
    REALTIME_MEDIA_NEGOTIATION_REGISTRY
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .map_err(|_| {
            OmniError::Internal(
                "realtime media negotiation registry lock is poisoned".to_string(),
            )
        })
}

pub(crate) fn clear_realtime_media_negotiation_for_profile(db_path: &str) {
    if let Ok(mut registry) = lock_realtime_media_negotiation_registry() {
        registry.remove(db_path);
    }
}

/// Names an announcement the shell left unnamed: the selection labels directions by it.
/// The id this side already announced in this dialog's negotiation, if any.
fn local_capabilities_id_for_dialog(db_path: &str, dialog_key: &str) -> Option<String> {
    let registry = lock_realtime_media_negotiation_registry().ok()?;
    registry
        .get(db_path)?
        .by_dialog
        .get(dialog_key)?
        .local_capabilities
        .as_ref()
        .map(|capabilities| capabilities.capabilities_id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// Names an announcement the shell left unnamed: the selection labels directions by it. The id
/// stays the same for the whole negotiation of a dialog — a mid-call re-announcement (a share that
/// started, a codec ceiling) keeps it, so a selection the peer computed from our previous
/// announcement still names our direction (live 2026-10-13: a fresh id per announcement left a
/// side without its send stream, and its screen share was dropped as "audio only").
fn with_capabilities_id(
    mut capabilities: RealtimeMediaCapabilitiesV1,
    existing: Option<String>,
) -> RealtimeMediaCapabilitiesV1 {
    if capabilities.capabilities_id.trim().is_empty() {
        if let Some(existing) = existing {
            capabilities.capabilities_id = existing;
            return capabilities;
        }
        let bytes = omni_crypto::generate_group_key();
        capabilities.capabilities_id = format!("caps-{}", hex::encode(&bytes[..8]));
    }
    capabilities
}

fn normalize_realtime_media_capabilities(
    mut capabilities: RealtimeMediaCapabilitiesV1,
) -> OmniResult<RealtimeMediaCapabilitiesV1> {
    capabilities.dialog_key = normalize_dialog_key(&capabilities.dialog_key);
    omni_protocol::validate_realtime_media_capabilities_v1(&capabilities)?;
    Ok(capabilities)
}

fn normalize_realtime_media_selection(
    mut selection: RealtimeMediaSelectionV1,
) -> OmniResult<RealtimeMediaSelectionV1> {
    selection.dialog_key = normalize_dialog_key(&selection.dialog_key);
    omni_protocol::validate_realtime_media_selection_v1(&selection)?;
    Ok(selection)
}

fn store_local_realtime_media_capabilities(
    db_path: &str,
    capabilities: RealtimeMediaCapabilitiesV1,
    now_unix_ms: i64,
) -> OmniResult<RealtimeMediaNegotiationSnapshot> {
    let dialog_key = capabilities.dialog_key.clone();
    let mut registry = lock_realtime_media_negotiation_registry()?;
    let state = registry.entry(db_path.to_string()).or_default();
    let negotiation = state.by_dialog.entry(dialog_key.clone()).or_default();
    negotiation.local_capabilities = Some(capabilities);
    negotiation.updated_at_unix_ms = now_unix_ms;
    refresh_realtime_media_selection(&dialog_key, negotiation, now_unix_ms);
    Ok(snapshot_from_realtime_media_negotiation(
        &dialog_key,
        negotiation,
        now_unix_ms,
    ))
}

fn store_remote_realtime_media_capabilities(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    capabilities: &RealtimeMediaCapabilitiesV1,
) -> OmniResult<()> {
    let mut capabilities = normalize_realtime_media_capabilities(capabilities.clone())?;
    let dialog_key = normalize_dialog_key(&receive_context.dialog_key);
    // Dialog keys are local selectors. The encrypted receive context, not the
    // sender's local selector, determines the receiver-side dialog key.
    capabilities.dialog_key = dialog_key.clone();
    ensure_realtime_media_dialog_is_approved(storage, &dialog_key)?;

    let selection_to_send;
    {
        let mut registry = lock_realtime_media_negotiation_registry()?;
        let state = registry
            .entry(storage.db_path().to_string_lossy().to_string())
            .or_default();
        let negotiation = state.by_dialog.entry(dialog_key.clone()).or_default();
        negotiation.remote_capabilities = Some(capabilities);
        let updated_at_unix_ms = current_unix_ms();
        negotiation.updated_at_unix_ms = updated_at_unix_ms;
        refresh_realtime_media_selection(
            &dialog_key,
            negotiation,
            updated_at_unix_ms,
        );
        selection_to_send = negotiation.selected.clone();
    }

    if let Some(selection) = selection_to_send {
        // Only the offerer (lex-smaller account_id) emits selection. Without
        // this gate, both peers race to emit and may pick different codecs
        // (asymmetric selection algorithm). See docs/decisions/media-negotiation-role.md.
        if is_local_offerer_for_dialog(storage, &dialog_key)? {
            let event = omni_types::LogicalEventEnvelopeV1::realtime_media_selection(selection);
            send_realtime_media_event_for_dialog(
                storage,
                transport,
                &dialog_key,
                &event,
                SecurityPayloadKind::RealtimeMediaSelection,
            )?;
        }
    }
    Ok(())
}

fn handle_incoming_realtime_media_capabilities(
    storage: &Storage,
    transport: &TransportManager,
    receive_context: &ReceivedChatContext,
    capabilities: &RealtimeMediaCapabilitiesV1,
) -> OmniResult<()> {
    store_remote_realtime_media_capabilities(storage, transport, receive_context, capabilities)
}

fn store_remote_realtime_media_selection(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    selection: &RealtimeMediaSelectionV1,
) -> OmniResult<()> {
    let mut selection = normalize_realtime_media_selection(selection.clone())?;
    let dialog_key = normalize_dialog_key(&receive_context.dialog_key);
    // Dialog keys are local selectors. The encrypted receive context, not the
    // sender's local selector, determines the receiver-side dialog key.
    selection.dialog_key = dialog_key.clone();
    ensure_realtime_media_dialog_is_approved(storage, &dialog_key)?;

    let mut registry = lock_realtime_media_negotiation_registry()?;
    let state = registry
        .entry(storage.db_path().to_string_lossy().to_string())
        .or_default();
    let negotiation = state.by_dialog.entry(dialog_key).or_default();
    negotiation.selected = Some(selection);
    negotiation.diagnostic = None;
    negotiation.updated_at_unix_ms = current_unix_ms();
    Ok(())
}

fn handle_incoming_realtime_media_selection(
    storage: &Storage,
    receive_context: &ReceivedChatContext,
    selection: &RealtimeMediaSelectionV1,
) -> OmniResult<()> {
    store_remote_realtime_media_selection(storage, receive_context, selection)
}

fn realtime_media_negotiation_snapshot_for_dialog(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<RealtimeMediaNegotiationSnapshot> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "dialog_key must not be empty".to_string(),
        ));
    }
    let now_unix_ms = current_unix_ms();
    let registry = lock_realtime_media_negotiation_registry()?;
    let Some(negotiation) = registry
        .get(db_path)
        .and_then(|state| state.by_dialog.get(&dialog_key))
    else {
        return Ok(RealtimeMediaNegotiationSnapshot {
            dialog_key,
            status: RealtimeMediaNegotiationStatus::Idle,
            local_capabilities: None,
            remote_capabilities: None,
            selected: None,
            send_video: None,
            receive_video: None,
            diagnostic: None,
            updated_at_unix_ms: now_unix_ms,
        });
    };
    Ok(snapshot_from_realtime_media_negotiation(
        &dialog_key,
        negotiation,
        now_unix_ms,
    ))
}

fn clear_realtime_media_negotiation_for_dialog(db_path: &str, dialog_key: &str) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }
    let mut registry = lock_realtime_media_negotiation_registry()?;
    if let Some(state) = registry.get_mut(db_path) {
        state.by_dialog.remove(&dialog_key);
        if state.by_dialog.is_empty() {
            registry.remove(db_path);
        }
    }
    Ok(())
}

fn refresh_realtime_media_selection(
    dialog_key: &str,
    negotiation: &mut RealtimeMediaDialogNegotiation,
    now_unix_ms: i64,
) {
    let (Some(local), Some(remote)) = (
        negotiation.local_capabilities.as_ref(),
        negotiation.remote_capabilities.as_ref(),
    ) else {
        negotiation.selected = None;
        negotiation.diagnostic = None;
        return;
    };

    let (selection, video_note) =
        select_realtime_media_configuration(dialog_key, local, remote, now_unix_ms);
    negotiation.selected = Some(selection);
    // Not an error state: the selection stands and the call runs. The note says why
    // it carries no video, and the shell shows it next to the call.
    negotiation.diagnostic = video_note;
}

/// Agree on a media session. This CANNOT fail: once both peers have announced
/// themselves there is always a session to have, because audio needs no
/// negotiation at all. Only the video half is negotiable, and when there is no
/// video in common the returned note says which part was missing.
///
/// It used to return `Result` and the whole session died with the video — a
/// camera-less peer produced no selection, so neither side ever started its audio
/// pipeline (2026-08-11 live test: two connected calls, complete silence).
fn select_realtime_media_configuration(
    dialog_key: &str,
    local: &RealtimeMediaCapabilitiesV1,
    remote: &RealtimeMediaCapabilitiesV1,
    now_unix_ms: i64,
) -> (RealtimeMediaSelectionV1, Option<String>) {
    let max_datagram_bytes = local
        .max_datagram_bytes
        .min(remote.max_datagram_bytes)
        .min(omni_transport::REALTIME_MEDIA_DATAGRAM_MAX_BYTES as u32)
        // Запас от потолка транспорта — с обоснованием у самой константы.
        .min(omni_transport::REALTIME_MEDIA_DATAGRAM_SAFE_BYTES as u32);
    let (video, video_note) = match select_realtime_video_configuration(local, remote) {
        Ok(video) => (Some(video), None),
        Err(note) => (None, Some(note)),
    };

    let selection = RealtimeMediaSelectionV1 {
        dialog_key: normalize_dialog_key(dialog_key),
        session_id: format!(
            "{}:{}",
            REALTIME_MEDIA_NEGOTIATION_SESSION_PREFIX,
            normalize_dialog_key(dialog_key)
        ),
        max_datagram_bytes,
        video,
        selected_at_unix_ms: now_unix_ms,
    };
    (selection, video_note)
}

fn select_realtime_video_configuration(
    local: &RealtimeMediaCapabilitiesV1,
    remote: &RealtimeMediaCapabilitiesV1,
) -> Result<RealtimeVideoSelectionV1, String> {
    let (Some(local_video), Some(remote_video)) = (local.video.as_ref(), remote.video.as_ref())
    else {
        return Err("audio only: one of the peers has no video pipeline".to_string());
    };
    if !local_video.can_send_video && !remote_video.can_send_video {
        return Err("audio only: neither peer has a camera".to_string());
    }
    let capture_pixel_format =
        select_realtime_media_capture_pixel_format(local_video, remote_video).ok_or_else(|| {
            "audio only: no common realtime video capture pixel format".to_string()
        })?;

    // Each direction on its own (docs/decisions/per-direction-video-format.md): the sender's format,
    // its codec chosen for that format and for what it shows — camera or screen
    // (per-pair-video-codec-selection.md §3) — then the format fitted to what the receiver decodes
    // in that codec, and the bitrate of that format. A share that starts or stops is announced again
    // and the direction is chosen anew.
    let mut streams = Vec::new();
    for (sender, sender_video, receiver) in
        [(local, local_video, remote), (remote, remote_video, local)]
    {
        if !sender_video.can_send_video {
            continue;
        }
        let Some((choice, (width, height, fps))) =
            choose_direction_codec_and_format(sender, sender_video, receiver)
        else {
            continue;
        };
        streams.push(RealtimeVideoStreamV1 {
            sender_capabilities_id: sender.capabilities_id.clone(),
            codec: choice.codec,
            payload_type: omni_types::default_video_payload_type(choice.codec),
            width,
            height,
            fps,
            // The codec's efficiency, then the two ends' levels from Settings → Calls → «Битрейт».
            target_bitrate_bps: target_realtime_video_bitrate_bps(width, height, fps, sender.content) / 100
                * omni_types::video_codec_bitrate_percent(choice.codec)
                / 100
                * omni_types::RealtimeVideoBitrateLevel::percent_for_pair(
                    sender.bitrate_level,
                    receiver.bitrate_level,
                ),
            reason: choice.reason,
        });
    }
    if streams.is_empty() {
        return Err("audio only: no common realtime video codec".to_string());
    }

    Ok(RealtimeVideoSelectionV1 {
        streams,
        capture_pixel_format,
        rtcp_feedback_enabled: local.supports_rtcp_feedback && remote.supports_rtcp_feedback,
    })
}

/// The format a sender asks for: its preference inside its own range, both sides even.
fn sender_video_format(video: &omni_types::RealtimeVideoProfileCapabilityV1) -> (u32, u32, u32) {
    let axis = |min: u32, preferred: u32, max: u32| {
        let max = max.max(1);
        preferred.clamp(min.max(1).min(max), max)
    };
    (
        even_video_side(axis(video.min_width, video.preferred_width, video.max_width)),
        even_video_side(axis(video.min_height, video.preferred_height, video.max_height)),
        axis(video.min_fps, video.preferred_fps, video.max_fps).clamp(1, 60),
    )
}

/// The codec of one direction and the format it goes in. The sender's format when some codec both
/// ends can run takes it whole; otherwise the format shrunk to one codec's limits — the sender's
/// encoder and the receiver's decoder — the largest such format that gets a codec. A 1440p screen
/// to a receiver that decodes up to 1080p goes as 1080p, not as nothing.
fn choose_direction_codec_and_format(
    sender: &RealtimeMediaCapabilitiesV1,
    sender_video: &omni_types::RealtimeVideoProfileCapabilityV1,
    receiver: &RealtimeMediaCapabilitiesV1,
) -> Option<(omni_types::VideoCodecChoice, (u32, u32, u32))> {
    let wanted = sender_video_format(sender_video);
    let mut formats = vec![wanted];
    for line in &sender.codecs {
        let decode = receiver
            .codecs
            .iter()
            .find(|theirs| theirs.codec == line.codec)
            .and_then(|theirs| theirs.decode.as_ref());
        let (w, h, f) = fit_video_format(wanted.0, wanted.1, wanted.2, line.encode.as_ref());
        let fitted = fit_video_format(w, h, f, decode);
        if !formats.contains(&fitted) {
            formats.push(fitted);
        }
    }
    formats.sort_by_key(|(w, h, f)| std::cmp::Reverse(u64::from(*w) * u64::from(*h) * u64::from(*f)));
    formats.into_iter().find_map(|(width, height, fps)| {
        let shape = omni_types::VideoStreamShape {
            content: sender.content,
            width,
            height,
            fps,
            h264_bitrate_bps: target_realtime_video_bitrate_bps(width, height, fps, sender.content),
        };
        omni_types::choose_video_codec(sender, receiver, &shape).map(|choice| (choice, (width, height, fps)))
    })
}

/// Shrink a format to a codec side's limits (0 = no known limit), keeping its proportions.
fn fit_video_format(
    width: u32,
    height: u32,
    fps: u32,
    side: Option<&omni_types::RealtimeVideoCodecSideV1>,
) -> (u32, u32, u32) {
    let Some(decoder) = side else {
        return (width, height, fps);
    };
    let mut scale = 1.0_f64;
    if decoder.max_width > 0 && width > decoder.max_width {
        scale = scale.min(f64::from(decoder.max_width) / f64::from(width));
    }
    if decoder.max_height > 0 && height > decoder.max_height {
        scale = scale.min(f64::from(decoder.max_height) / f64::from(height));
    }
    let fit = |side: u32| even_video_side((f64::from(side) * scale).floor() as u32);
    let fps = if decoder.max_fps > 0 { fps.min(decoder.max_fps) } else { fps };
    (fit(width), fit(height), fps.max(1))
}

/// Encoders want even sides (H.264 4:2:0 fails to open on an odd one).
fn even_video_side(side: u32) -> u32 {
    (side & !1).max(2)
}

fn select_realtime_media_capture_pixel_format(
    local: &RealtimeVideoProfileCapabilityV1,
    remote: &RealtimeVideoProfileCapabilityV1,
) -> Option<RealtimeVideoPixelFormat> {
    // The same for every codec: the encoders take NV12 first.
    let supported_order: &[RealtimeVideoPixelFormat] = &[
        RealtimeVideoPixelFormat::Nv12,
        RealtimeVideoPixelFormat::I420,
        RealtimeVideoPixelFormat::Bgra,
    ];
    supported_order
        .iter()
        .copied()
        .filter(|format| {
            local.pixel_formats.contains(format) && remote.pixel_formats.contains(format)
        })
        .min_by_key(|format| {
            (
                realtime_media_pixel_format_offer_rank(&local.pixel_formats, *format)
                    + realtime_media_pixel_format_offer_rank(&remote.pixel_formats, *format),
                realtime_media_pixel_format_tie_rank(*format),
            )
        })
}

fn realtime_media_pixel_format_offer_rank(
    formats: &[RealtimeVideoPixelFormat],
    format: RealtimeVideoPixelFormat,
) -> usize {
    formats
        .iter()
        .position(|candidate| *candidate == format)
        .unwrap_or(usize::MAX / 4)
}

fn realtime_media_pixel_format_tie_rank(format: RealtimeVideoPixelFormat) -> u8 {
    match format {
        // Prefer the native-friendly raw camera formats when peer offer order ties.
        RealtimeVideoPixelFormat::Nv12 => 0,
        RealtimeVideoPixelFormat::I420 => 1,
        RealtimeVideoPixelFormat::Bgra => 2,
    }
}

/// The H.264 target for a format; each direction scales it by its codec's efficiency. A screen
/// gets the budget of the 30 fps rung at any frame rate: text needs bits per frame, not frames.
///
/// A CEILING, not a rate: the congestion controller sets the rate below it, and an easy picture
/// takes less. Raised 1.5x on 2026-10-18 — at the old rungs (720p30: 4 Mbit/s H.264, i.e. AV1
/// 2.4 / HEVC 2.8) a live call sat at the ceiling with the path idle and QP to spare
/// (docs/decisions/video-quality-ceilings-and-twcc.md, "Потолки подняты"). The 2026-08-15
/// rollback of 8 Mbit/s was the shell's send loop, which media no longer rides.
fn target_realtime_video_bitrate_bps(
    width: u32,
    height: u32,
    fps: u32,
    content: omni_types::VideoContent,
) -> u32 {
    let fps = match content {
        omni_types::VideoContent::Screen => fps.max(30),
        omni_types::VideoContent::Camera => fps,
    };
    let pixels_per_second = width.saturating_mul(height).saturating_mul(fps);
    match pixels_per_second {
        value if value >= 1920 * 1080 * 45 => 15_000_000,
        value if value >= 1920 * 1080 * 24 => 12_000_000,
        value if value >= 1280 * 720 * 24 => 6_000_000,
        value if value >= 854 * 480 * 24 => 3_000_000,
        _ => 1_800_000,
    }
}

fn snapshot_from_realtime_media_negotiation(
    dialog_key: &str,
    negotiation: &RealtimeMediaDialogNegotiation,
    now_unix_ms: i64,
) -> RealtimeMediaNegotiationSnapshot {
    // No `unsupported` state any more: once both peers have announced themselves
    // there is always a session (audio at minimum), and `diagnostic` carries the
    // reason it has no video rather than standing in for a failure.
    let status = if negotiation.selected.is_some() {
        RealtimeMediaNegotiationStatus::Negotiated
    } else if negotiation.local_capabilities.is_some() {
        RealtimeMediaNegotiationStatus::WaitingForRemoteCapabilities
    } else {
        RealtimeMediaNegotiationStatus::Idle
    };

    let local_id = negotiation
        .local_capabilities
        .as_ref()
        .map(|capabilities| capabilities.capabilities_id.as_str())
        .unwrap_or_default();
    let streams = negotiation
        .selected
        .as_ref()
        .and_then(|selection| selection.video.as_ref())
        .map(|video| video.streams.as_slice())
        .unwrap_or_default();
    let send_video = streams
        .iter()
        .find(|stream| !local_id.is_empty() && stream.sender_capabilities_id == local_id)
        .cloned();
    // The peer's direction is the one labelled with the peer's id; without its announcement yet,
    // the one that is not ours.
    let remote_id = negotiation
        .remote_capabilities
        .as_ref()
        .map(|capabilities| capabilities.capabilities_id.as_str())
        .filter(|id| !id.is_empty());
    let receive_video = streams
        .iter()
        .find(|stream| match remote_id {
            Some(remote_id) => stream.sender_capabilities_id == remote_id,
            None => stream.sender_capabilities_id != local_id,
        })
        .cloned();
    RealtimeMediaNegotiationSnapshot {
        dialog_key: normalize_dialog_key(dialog_key),
        status,
        local_capabilities: negotiation.local_capabilities.clone(),
        remote_capabilities: negotiation.remote_capabilities.clone(),
        selected: negotiation.selected.clone(),
        send_video,
        receive_video,
        diagnostic: negotiation.diagnostic.clone(),
        updated_at_unix_ms: if negotiation.updated_at_unix_ms > 0 {
            negotiation.updated_at_unix_ms
        } else {
            now_unix_ms
        },
    }
}

/// Determine if the local profile is the "offerer" for this dialog —
/// i.e. the side responsible for emitting `RealtimeMediaSelectionV1` on the
/// wire. Both peers run `refresh_realtime_media_selection` locally but only
/// the offerer broadcasts; the answerer applies the offerer's selection
/// when it arrives via `store_remote_realtime_media_selection`.
///
/// Role is decided by lex-comparing local vs remote `account_id`: the
/// lexicographically smaller one is the offerer. This is deterministic,
/// symmetric across peers, and stable across reconnects.
///
/// Returns `false` (don't emit) if account ids cannot be resolved — safer to
/// stall negotiation than to have both sides emit concurrently and diverge.
///
/// See docs/decisions/media-negotiation-role.md.
fn is_local_offerer_for_dialog(storage: &Storage, dialog_key: &str) -> OmniResult<bool> {
    let Some(local_account) = storage.get_local_account()? else {
        return Ok(false);
    };
    let Some(contact_card) = storage.resolve_contact_card_for_dialog(dialog_key)? else {
        return Ok(false);
    };
    let Some(remote_account_id) = contact_card.contact.remote_account_id.as_ref() else {
        return Ok(false);
    };
    Ok(local_account.descriptor.account_id.0.as_str() < remote_account_id.0.as_str())
}

fn ensure_realtime_media_dialog_is_approved(storage: &Storage, dialog_key: &str) -> OmniResult<()> {
    let Some(card) = storage.resolve_contact_card_for_dialog(dialog_key)? else {
        return Err(OmniError::InvalidInput(
            "realtime media requires an approved contact".to_string(),
        ));
    };
    if card.contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
        return Err(OmniError::InvalidInput(
            "realtime media requires an approved contact".to_string(),
        ));
    }
    Ok(())
}

/// Hot-path approval proxy for inbound media datagrams.
///
/// Instead of locking the storage Mutex on every received frame (30+/sec)
/// and doing a SQLite lookup, we check whether the dialog has an active
/// media negotiation with a committed selection. A negotiation entry can
/// only be created via `publish_realtime_media_capabilities` or
/// `store_remote_realtime_media_capabilities` — both call
/// `ensure_realtime_media_dialog_is_approved` first. On call-end the
/// registry is cleared via `clear_realtime_media_negotiation_for_dialog`.
///
/// Returning `false` causes the caller to silently drop the datagram —
/// correct behavior for unsolicited UDP-like flow (peer has no expectation
/// of feedback for arbitrary packets).
///
/// See docs/decisions/media-approval-gate.md.
fn dialog_has_active_media_negotiation(db_path: &str, dialog_key: &str) -> bool {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return false;
    }
    let Ok(registry) = lock_realtime_media_negotiation_registry() else {
        return false;
    };
    registry
        .get(db_path)
        .and_then(|state| state.by_dialog.get(&dialog_key))
        .is_some_and(|negotiation| negotiation.selected.is_some())
}

fn send_realtime_media_event_for_dialog(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
    event: &omni_types::LogicalEventEnvelopeV1,
    payload_kind: SecurityPayloadKind,
) -> OmniResult<()> {
    ensure_realtime_media_dialog_is_approved(storage, dialog_key)?;
    let contact = storage
        .resolve_contact_card_for_dialog(dialog_key)?
        .ok_or_else(|| OmniError::InvalidInput("contact was not found".to_string()))?;
    let remote_account_id = contact.contact.remote_account_id.ok_or_else(|| {
        OmniError::InvalidInput(
            "realtime media requires a contact with a remote account id".to_string(),
        )
    })?;
    let session = storage
        .get_session_for_dialog(&remote_account_id, dialog_key)?
        .ok_or_else(|| {
            OmniError::Transport(
                "no established realtime media signaling session is available".to_string(),
            )
        })?;
    let mut outgoing = build_encrypted_logical_event_envelope(
        &session,
        event,
        payload_kind,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    Ok(())
}

fn push_incoming_realtime_media_datagram(
    sinks: &RealtimeMediaInboundSinks,
    db_path: &str,
    receive_context: &ReceivedChatContext,
    payload: &[u8],
) -> OmniResult<()> {
    push_incoming_realtime_media_datagram_for_dialog(
        sinks,
        db_path,
        &receive_context.dialog_key,
        payload,
    )
}

/// The `group:<id>` group id, if `dialog_key` is a group dialog. Group media
/// routes through the peer-SFU path instead of the 1:1 mesh/legacy path.
/// Pick the codec we send to everybody in the group call on `dialog_key` and rebuild our self-selected
/// format with it. No-op until our media has published its passport. Called on publish and whenever
/// someone joins, leaves or announces what they decode.
///
/// One codec for all, the best every watcher decodes (`choose_group_video_codec`); H.264 while any
/// watcher's passport is unknown — an unknown watcher must still see us. A change is taken at once
/// when the current codec no longer reaches everybody, otherwise not more often than every 10 s
/// (`should_switch_video_codec`).
pub(crate) fn reselect_group_media(db_path: &str, dialog_key: &str) {
    let Some(group_id) = group_id_from_dialog_key(dialog_key) else {
        return;
    };
    // No call running (media published ahead of it): nobody watches yet — the floor, and a format.
    let (watchers, all_known) = crate::app_group_calls::group_call_watcher_capabilities(db_path, group_id)
        .unwrap_or_else(|| (Vec::new(), true));
    let Ok(mut registry) = lock_realtime_media_negotiation_registry() else {
        return;
    };
    let Some(negotiation) = registry
        .get_mut(db_path)
        .and_then(|state| state.by_dialog.get_mut(dialog_key))
    else {
        return;
    };
    let Some(full) = negotiation.group_full_capabilities.clone() else {
        return;
    };
    let now = current_unix_ms();
    let (width, height, fps) = full
        .video
        .as_ref()
        .map(|video| (video.preferred_width, video.preferred_height, video.preferred_fps))
        .unwrap_or((1280, 720, 30));
    let shape = omni_types::VideoStreamShape {
        content: full.content,
        width,
        height,
        fps,
        h264_bitrate_bps: target_realtime_video_bitrate_bps(width, height, fps, full.content),
    };
    let watcher_refs: Vec<(String, &RealtimeMediaCapabilitiesV1)> =
        watchers.iter().map(|(id, capabilities)| (id.clone(), capabilities)).collect();
    let best = if all_known {
        omni_types::choose_group_video_codec(&full, &watcher_refs, &shape)
    } else {
        omni_types::RealtimeVideoCodec::H264
    };
    // Sum of the watchers' scores for a codec; `None` when one of them cannot take it.
    let total = |codec: omni_types::RealtimeVideoCodec| -> Option<i32> {
        watcher_refs.iter().try_fold(0i32, |sum, (_, receiver)| {
            omni_types::rank_video_codecs(&full, receiver, &shape)
                .into_iter()
                .find(|(candidate, _)| *candidate == codec)
                .map(|(_, score)| sum + score)
        })
    };
    let chosen = match negotiation.group_send_codec {
        // Someone we know nothing about watches: the floor, now — they may decode nothing else.
        _ if !all_known => omni_types::RealtimeVideoCodec::H264,
        Some((current, _)) if current == best => current,
        Some((current, picked_at)) => {
            let switch = omni_types::should_switch_video_codec(
                total(current),
                total(best).unwrap_or(0),
                now - picked_at,
            );
            if switch { best } else { current }
        }
        None => best,
    };
    // H.264 for want of passports is a stopgap, not a choice: the moment they are all in, the real
    // pick goes through at once (stamped as if picked long ago).
    let picked_at = if all_known { now } else { 0 };
    if negotiation.group_send_codec.map(|(codec, _)| codec) != Some(chosen) {
        omni_app_trace_log(
            "group/codec",
            &format!(
                "dialog={dialog_key} send={chosen:?} watchers={} all_known={all_known}",
                watchers.len()
            ),
        );
        negotiation.group_send_codec = Some((chosen, picked_at));
    } else if !all_known {
        negotiation.group_send_codec = Some((chosen, 0));
    }

    // Self-negotiate against a stand-in for "everybody else": it DECODES exactly the codec we send
    // (it was picked because every watcher decodes it) and SENDS H.264 — the receiver every sender
    // gets until its stream shows its own codec (receivers are built per sender from the RTP payload
    // type). Standing in with our own passport, as before, broke a machine that decodes less than its
    // watchers: the PC lost its whole send direction (live 2026-10-14). And `can_send_video`: any of
    // them may own a camera. Standing in with our own flag made a camera-less member pick audio-only —
    // it then could neither decode the others' video nor share its screen (live pass 2026-10-04,
    // docs/bugs/group-call-joiner-saw-no-video.md).
    let side = |hardware: bool| omni_types::RealtimeVideoCodecSideV1 { hardware, max_width: 0, max_height: 0, max_fps: 0 };
    let mut others = full.clone();
    others.capabilities_id = format!("{}-others", full.capabilities_id);
    others.codec_ceiling = None;
    others.codecs = vec![omni_types::RealtimeVideoCodecCapabilityV1 {
        codec: chosen,
        payload_type: omni_types::default_video_payload_type(chosen),
        encode: (chosen == omni_types::RealtimeVideoCodec::H264).then(|| side(true)),
        decode: Some(side(true)),
        implementation: None,
    }];
    if chosen != omni_types::RealtimeVideoCodec::H264 {
        others.codecs.push(omni_types::RealtimeVideoCodecCapabilityV1 {
            codec: omni_types::RealtimeVideoCodec::H264,
            payload_type: omni_types::default_video_payload_type(omni_types::RealtimeVideoCodec::H264),
            encode: Some(side(true)),
            decode: None,
            implementation: None,
        });
    }
    if let Some(video) = others.video.as_mut() {
        video.can_send_video = true;
    }
    let narrowed = full.clone();
    negotiation.local_capabilities = Some(narrowed);
    negotiation.remote_capabilities = Some(others);
    negotiation.updated_at_unix_ms = now;
    refresh_realtime_media_selection(dialog_key, negotiation, now);
}

pub(crate) fn group_id_from_dialog_key(dialog_key: &str) -> Option<&str> {
    dialog_key
        .strip_prefix("group:")
        .filter(|id| !id.is_empty() && !id.contains(GROUP_FEEDBACK_KEY_MARK))
}

/// `group:<id>#fb:<sender tag hex>` — the send target of one group receiver's RTCP: it goes back to
/// that sender only (see `send_group_feedback`). The shell builds it from the group dialog and the
/// sender tag it demuxes on.
pub(crate) const GROUP_FEEDBACK_KEY_MARK: &str = "#fb:";

pub(crate) fn group_feedback_from_dialog_key(dialog_key: &str) -> Option<(&str, [u8; 8])> {
    let (group, tag_hex) = dialog_key.strip_prefix("group:")?.split_once(GROUP_FEEDBACK_KEY_MARK)?;
    let tag: [u8; 8] = hex::decode(tag_hex).ok()?.try_into().ok()?;
    (!group.is_empty()).then_some((group, tag))
}

/// Deliver a group-call media frame (G9) to its group dialog's engine. NOT gated by the 1:1
/// media-negotiation check: the frame was already validated on ingest (opened with the sender's
/// per-call media key — an unauthenticated frame never gets here). The engine demuxes senders by
/// the sender-tag prefix.
fn push_incoming_group_media_datagram(sinks: &RealtimeMediaInboundSinks, dialog_key: &str, payload: &[u8]) {
    if !sinks.deliver(dialog_key, payload) {
        note_media_datagram_without_sink(dialog_key);
    }
}

/// Media that passed the gate but has no engine to go to: a call not wired yet (the first packets
/// race the shell's wiring) or already unwired. Sampled like the other media counters.
fn note_media_datagram_without_sink(dialog_key: &str) {
    let dropped = MEDIA_DATAGRAMS_DROPPED_NO_SINK.fetch_add(1, Ordering::Relaxed) + 1;
    if dropped == 1 || dropped.is_multiple_of(250) {
        omni_app_trace_log(
            "media/datagram-drop",
            &format!("dialog={dialog_key} reason=no-sink total={dropped}"),
        );
    }
}

/// Enqueue an inbound realtime-media datagram for a dialog, gated by the same
/// active-negotiation check as the legacy transport path. Shared by the
/// transport receive callback and the mesh inbound pump so both planes obey
/// the media approval gate identically.
fn push_incoming_realtime_media_datagram_for_dialog(
    sinks: &RealtimeMediaInboundSinks,
    db_path: &str,
    dialog_key: &str,
    payload: &[u8],
) -> OmniResult<()> {
    // Defense-in-depth: drop datagrams for dialogs without an active media
    // negotiation. Registry lookup is cheaper than a storage round-trip and
    // produces equivalent results — see docs/decisions/media-approval-gate.md.
    if !dialog_has_active_media_negotiation(db_path, dialog_key) {
        // This gate closing is INDISTINGUISHABLE from "the peer sent nothing" without a trace: a
        // call whose negotiation never registered drops every packet here and presents as a
        // connected call with no audio and no video. Media runs ~50 packets/s, so report the
        // first drop and then every 250th instead of per packet.
        let dropped = MEDIA_DATAGRAMS_DROPPED_NO_NEGOTIATION.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped == 1 || dropped.is_multiple_of(250) {
            omni_app_trace_log(
                "media/datagram-drop",
                &format!("dialog={dialog_key} reason=no-active-negotiation total={dropped}"),
            );
        }
        return Ok(());
    }
    // The healthy counterpart, at the same cadence: media that PASSES the gate. Between the two
    // counters the trace answers "is media arriving at all, and is it being let through?".
    let accepted = MEDIA_DATAGRAMS_ACCEPTED.fetch_add(1, Ordering::Relaxed) + 1;
    if accepted == 1 || accepted.is_multiple_of(250) {
        // Sampled like its outbound twin: `bytes` is this one packet, the rate is the window.
        let now_ms = current_unix_ms().max(0) as u64;
        let previous_ms = MEDIA_DATAGRAM_IN_LAST_LOG_MS.swap(now_ms, Ordering::Relaxed);
        let window_ms = now_ms.saturating_sub(previous_ms);
        let rate = if previous_ms == 0 || window_ms == 0 {
            0
        } else {
            250_u64.saturating_mul(1_000) / window_ms
        };
        omni_app_trace_log(
            "media/datagram-in",
            &format!(
                "dialog={dialog_key} bytes={} total={accepted} window={window_ms}ms rate={rate}p/s",
                payload.len()
            ),
        );
    }
    if !sinks.deliver(dialog_key, payload) {
        note_media_datagram_without_sink(dialog_key);
    }
    Ok(())
}

// No inbound-media step here: nothing is buffered any more, and the engine's sink belongs to the
// call — the shell unwires it when the call's media stops.
fn clear_realtime_media_runtime_for_context(db_path: &str, receive_context: &ReceivedChatContext) {
    let _ = clear_realtime_media_negotiation_for_dialog(db_path, &receive_context.dialog_key);
}
