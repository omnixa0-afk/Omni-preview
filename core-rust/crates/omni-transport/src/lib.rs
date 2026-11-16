use std::collections::HashMap;
use std::fmt;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::thread::JoinHandle;
/// Стоимость канала для пользователя — платформенный факт, который подаёт оболочка (WinRT на
/// Windows, ConnectivityManager на Android). Решает выбор контроллера перегрузки: BBR везде,
/// кроме метрируемых каналов. См. `omni_net_primitives::quic::select_congestion_control` —
/// там оба замера, на которых это правило стоит.
pub use omni_net_primitives::quic::{
    congestion_preference, congestion_preference_name, effective_congestion_control,
    parse_congestion_preference, set_congestion_preference, set_link_cost, CongestionChoice,
    CongestionPreference, LinkCost,
};


use omni_protocol::{validate_wire_envelope_v1, PrekeyBundleV1};
use omni_types::{
    dialog_key_from_instance_id, normalize_dialog_key,
    OmniError, OmniResult, SessionHelloV1,
    TransportCommandResult, TransportMode, TransportSnapshot, TransportState, WireEnvelopeV1,
    OMNI_PROTOCOL_VERSION_V1,
};
use serde::{Deserialize, Serialize};

/// Потолок одной медиа-датаграммы, байт.
pub const REALTIME_MEDIA_DATAGRAM_MAX_BYTES: usize = 1200;

/// Сколько байт медиа реально просит у канала — с запасом от потолка выше.
///
/// Потолок 1200 это то, что мы согласны пропустить. Просить ровно его нельзя: настоящий предел
/// датаграммы у QUIC свой на каждое соединение (`session.max_datagram_size`), и он бывает МЕНЬШЕ
/// — путь с туннелем, VPN, мобильный оператор. Отправка сверх него не деградирует, а
/// ОТВАЛИВАЕТСЯ с ошибкой (датаграммы mesh-линка), то есть кадр гибнет целиком.
///
/// Двести байт запаса — цена страховки: на 1000 байтах пакетов на кадр примерно на пятую часть
/// больше, чем на 1200. Убирать запас можно будет тогда, когда согласование начнёт брать
/// ЖИВОЙ `max_datagram_size` соединения вместо этой константы; до тех пор число здесь — оценка
/// сверху для худшего пути, и место ей одно на всё ядро.
pub const REALTIME_MEDIA_DATAGRAM_SAFE_BYTES: usize = 1000;




#[derive(Clone)]
pub struct ReceivedChatContext {
    /// The mesh node the frames came over (hex); empty when the context did not come from a link
    /// (a bootstrap held for us, a dialog with no connection).
    pub peer_id: String,
    pub dialog_key: String,
    pub remote_instance_id: String,
    pub remote_prekey_bundle: Option<PrekeyBundleV1>,
    pub initiated_locally: bool,
}

pub enum ReceivedTransportEvent {
    SessionActivated(ReceivedChatContext),
    SessionClosed(ReceivedChatContext),
    WireEnvelope(WireEnvelopeV1, ReceivedChatContext),
    /// A wire-envelope from a durable (store-and-forward) frame: the handler's result goes to the
    /// `DurableWireDone` once the app has handled it — the mesh marks the frame handled only then.
    DurableWireEnvelope(WireEnvelopeV1, ReceivedChatContext, DurableWireDone),
    AttachmentTransferFrame(AttachmentTransferFrameV2, ReceivedChatContext),
    AttachmentTransferStream(AttachmentTransferStream, ReceivedChatContext),
    RealtimeMediaDatagram(Vec<u8>, ReceivedChatContext),
}

/// Told the result of handling a durable wire-envelope. Never called when the event is dropped
/// unhandled (the dispatcher shut down): the frame then stays unhandled, as it should.
pub type DurableWireDone = Box<dyn FnOnce(&OmniResult<()>) + Send>;

type ReceiveCallback = Arc<dyn Fn(ReceivedTransportEvent) + Send + Sync + 'static>;

/// Delivers inbound events to the app callback on one thread, in order. Shutting it down closes
/// the channel FIRST and then joins: the forwarder that owns the sending half lives inside the
/// `TransportManager` itself, so joining with the channel still open would wait forever.
///
/// The queue is bounded: an event handed in while it is full waits for room. It had no bound, and a
/// peer sending faster than the app handles grew it for as long as it kept sending (blind audit
/// 2026-11-11, pass 13). The wait is not for ever — the one handing in may hold what the callback
/// needs, and the callback itself hands events in: past [`RECEIVE_QUEUE_WAIT`] (and from the
/// dispatcher's own thread, at once) the event is queued over the bound — up to
/// [`RECEIVE_QUEUE_CEILING`], which nothing passes but the two events that carry a session's state:
/// past the wait every sender added one more for as long as the app was behind (pass 14). What is
/// dropped there is what the network may lose anyway: a durable frame stays unhandled and is
/// offered again, a plain one and a stream are asked again by their senders. Media does not wait
/// at all: late media is worth nothing, and it is dropped as soon as the queue is full.
struct ReceiveDispatcher {
    queue: Mutex<ReceiveQueue>,
    changed: Condvar,
    join_handle: Mutex<Option<JoinHandle<()>>>,
}

struct ReceiveQueue {
    events: VecDeque<ReceivedTransportEvent>,
    closed: bool,
}

/// How many inbound events wait for the app's callback.
const RECEIVE_QUEUE_MAX: usize = 4096;
/// What the queue never passes ([`ReceiveDispatcher`]).
const RECEIVE_QUEUE_CEILING: usize = 2 * RECEIVE_QUEUE_MAX;
/// What the states of sessions with none waiting may add over [`RECEIVE_QUEUE_CEILING`].
const RECEIVE_QUEUE_STATE_CEILING: usize = RECEIVE_QUEUE_CEILING + RECEIVE_QUEUE_MAX;
/// How long an event waits for room in a full queue before it is queued anyway.
const RECEIVE_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The session (peer, dialog) whose state `event` carries; `None` for any other event.
fn session_of_state(event: &ReceivedTransportEvent) -> Option<(&str, &str)> {
    match event {
        ReceivedTransportEvent::SessionActivated(context) | ReceivedTransportEvent::SessionClosed(context) => {
            Some((context.peer_id.as_str(), context.dialog_key.as_str()))
        }
        _ => None,
    }
}

impl ReceiveDispatcher {
    fn spawn(callback: ReceiveCallback) -> (ReceiveCallback, Arc<Self>) {
        let dispatch = Arc::new(Self {
            queue: Mutex::new(ReceiveQueue { events: VecDeque::new(), closed: false }),
            changed: Condvar::new(),
            join_handle: Mutex::new(None),
        });
        let worker = Arc::clone(&dispatch);
        let join_handle = thread::spawn(move || {
            while let Some(event) = worker.next() {
                callback(event);
            }
        });
        let dispatcher_thread = join_handle.thread().id();
        *lock_mutex_recover(&dispatch.join_handle, "receive dispatcher join handle") = Some(join_handle);
        let forwarder: ReceiveCallback = {
            let dispatch = Arc::clone(&dispatch);
            Arc::new(move |event| dispatch.push(event, thread::current().id() != dispatcher_thread))
        };

        (forwarder, dispatch)
    }

    /// Queue `event`; `may_wait` — for room, when the queue is full.
    fn push(&self, event: ReceivedTransportEvent, may_wait: bool) {
        let mut queue = lock_mutex_recover(&self.queue, "receive dispatcher queue");
        let media = matches!(event, ReceivedTransportEvent::RealtimeMediaDatagram(..));
        if media && queue.events.len() >= RECEIVE_QUEUE_MAX {
            return;
        }
        if may_wait {
            let deadline = std::time::Instant::now() + RECEIVE_QUEUE_WAIT;
            while !queue.closed && queue.events.len() >= RECEIVE_QUEUE_MAX {
                let Some(left) = deadline.checked_duration_since(std::time::Instant::now()).filter(|left| !left.is_zero()) else {
                    break;
                };
                queue = match self.changed.wait_timeout(queue, left) {
                    Ok((queue, _)) => queue,
                    Err(poisoned) => poisoned.into_inner().0,
                };
            }
        }
        let state = session_of_state(&event).map(|(peer, dialog)| (peer.to_string(), dialog.to_string()));
        // Past the ceiling a session's state replaces the one of that session already waiting: the
        // app needs where a session ended up, and every flap of a link added one more for as long
        // as the app was behind (blind audit 2026-11-12, pass 15). So the state events over the
        // ceiling are at most one per session.
        // States of ever new sessions passed the ceiling one by one (pass 16); making room by the
        // oldest event that is no state took another session's durable frame or stream (pass 17).
        // So a state has headroom of its own over the ceiling, and past that a new one is dropped.
        if let Some((peer, dialog)) = &state {
            if queue.events.len() >= RECEIVE_QUEUE_CEILING {
                if let Some(at) = queue.events.iter().position(|queued| session_of_state(queued) == Some((peer, dialog))) {
                    queue.events.remove(at);
                } else if queue.events.len() >= RECEIVE_QUEUE_STATE_CEILING {
                    return;
                }
            }
        }
        if !queue.closed && (state.is_some() || queue.events.len() < RECEIVE_QUEUE_CEILING) {
            queue.events.push_back(event);
            self.changed.notify_all();
        }
    }

    /// The next event for the callback; `None` once the dispatcher is shut down.
    fn next(&self) -> Option<ReceivedTransportEvent> {
        let mut queue = lock_mutex_recover(&self.queue, "receive dispatcher queue");
        loop {
            // What was queued before the shutdown is still delivered.
            if let Some(event) = queue.events.pop_front() {
                self.changed.notify_all();
                return Some(event);
            }
            if queue.closed {
                return None;
            }
            queue = match self.changed.wait(queue) {
                Ok(queue) => queue,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    fn shutdown(&self) {
        {
            let mut queue = lock_mutex_recover(&self.queue, "receive dispatcher queue");
            queue.closed = true;
            self.changed.notify_all();
        }
        if let Some(join_handle) =
            lock_mutex_recover(&self.join_handle, "receive dispatcher join handle").take()
        {
            // The last manager may be dropped by the callback itself, on the dispatcher thread:
            // that thread ends on its own once the callback returns, and joining it would hang.
            if join_handle.thread().id() != thread::current().id() {
                let _ = join_handle.join();
            }
        }
    }
}

#[derive(Clone)]
struct LocalMetadata {
    instance_id: String,
    device_name: String,
    transport_mode: TransportMode,
}

#[derive(Clone)]
struct ConnectionState {
    peer_id: String,
    dialog_key: String,
    remote_instance_id: String,
    _remote_transport_mode: TransportMode,
    remote_device_name: String,
    remote_account_id: Option<String>,
    remote_prekey_bundle: Option<PrekeyBundleV1>,
    initiated_locally: bool,
}

struct SharedState {
    transport_mode: TransportMode,
    connection: Option<ConnectionState>,
    verified_connections: HashMap<String, ConnectionState>,
    duplicate_resolved: bool,
    local_account_id: Option<String>,
    local_prekey_bundle_json: Option<String>,
    last_error: Option<String>,
}

/// Outcome of an alternate carrier's attempt to deliver a wire-envelope.
pub enum PeerCarrierOutcome {
    /// Delivered over the alternate carrier (mesh) — do not use the legacy path.
    Sent,
    /// Байты ушли по живому линку, но СЕССИЯ круг ещё не подтвердила (пир не отвечает на пробы
    /// дольше порога). Маршрут при этом есть, и legacy-путь пробовать НЕЛЬЗЯ.
    ///
    /// Это состояние существует отдельно от `NotConnected`, потому что их склейка стоила петли:
    /// неподтверждённая сессия отдавала `NotConnected`, вызывающий уходил в legacy-путь, тот
    /// упирался в «no active QUIC carrier session» — и УПРАВЛЯЮЩИЕ сообщения переставали ходить
    /// ровно тогда, когда они нужны. В том числе просьба пересобрать сессию: она уходит по
    /// заклинившей сессии, а значит гарантированно при неподтверждённом вердикте.
    /// См. docs/bugs/bootstrap-ack-never-held-durable.md.
    SentUnconfirmed,
    /// No alternate route for this peer right now — fall back to the legacy path.
    NotConnected,
    /// The alternate carrier had a route but the send failed.
    Failed(OmniError),
}

/// Dependency-inversion seam for an alternate per-peer carrier (the mesh
/// overlay). `omni-transport` stays mesh-agnostic: `omni-app` installs an impl
/// (backed by `omni-mesh-runtime`) via [`TransportManager::set_peer_carrier`].
/// When a peer is reachable over the alternate carrier, outbound traffic rides
/// it (one QUIC session per peer); otherwise the legacy session path is used.
/// See `docs/decisions/mesh-transport-integration-seam.md`.
pub trait PeerCarrier: Send + Sync {
    /// Attempt to deliver a wire-envelope for `dialog_key` over the alternate
    /// carrier. Returns `NotConnected` (not an error) when there is no route,
    /// so the caller transparently falls back to the legacy transport.
    fn try_send_wire_envelope(
        &self,
        dialog_key: &str,
        envelope: &WireEnvelopeV1,
    ) -> PeerCarrierOutcome;

    /// Last-resort hand-off: hold `envelope` for durable store-and-forward when
    /// there is no live path (neither this carrier's link nor a legacy connection),
    /// so an offline peer still receives it via reconcile. Returns `true` if the
    /// envelope was held (the carrier has a route to the peer), `false` otherwise.
    /// Called only after both the mesh link and the legacy connection are absent,
    /// so durable never preempts a live path. See docs/decisions/mesh-durable-delivery.md.
    fn hold_durable(&self, dialog_key: &str, envelope: &WireEnvelopeV1) -> bool;

    /// Drop our cached mesh Link to the peer of `dialog_key` — the escalation for a PHANTOM link
    /// (`has_link` reports it up, but the peer receives nothing because a mobile carrier killed the
    /// UDP flow). While the phantom entry persists, live sends AND the durable reconcile both go into
    /// the void, and the auto-connect/rendezvous re-dial skips (it thinks we're linked). Dropping it
    /// forces a fresh dial, over which the durable-backstopped frame finally reconciles. Called only
    /// after a message stays unconfirmed well past the durable backstop — a strong phantom signal.
    /// Returns `true` if a link entry was actually dropped. See docs/decisions/delivery-reliability-model.md.
    fn drop_phantom_link(&self, dialog_key: &str) -> bool;

    /// Attempt to open a BULK stream for `dialog_key` over the alternate carrier — the lane for
    /// attachment transfer. `NotConnected` (not an error) when there is no route, so the caller
    /// transparently falls back to the legacy carrier, mirroring `try_send_wire_envelope`.
    ///
    /// Default: `NotConnected`, so a carrier that has no bulk plane needs no implementation.
    fn try_open_attachment_stream(&self, dialog_key: &str) -> PeerCarrierStreamOutcome {
        let _ = dialog_key;
        PeerCarrierStreamOutcome::NotConnected
    }
}

/// Outcome of asking a [`PeerCarrier`] for a bulk stream. Distinguishes "this carrier has no route"
/// (fall back silently) from a real failure (surface it).
pub enum PeerCarrierStreamOutcome {
    Opened(Box<AttachmentTransferStream>),
    NotConnected,
    Failed(OmniError),
}

#[derive(Clone)]
pub struct TransportManager {
    shared: Arc<Mutex<SharedState>>,
    event_dispatch: Arc<ReceiveDispatcher>,
    /// Optional alternate per-peer carrier (mesh overlay), installed post-init
    /// by omni-app. `None` = legacy-only (default). See [`PeerCarrier`].
    peer_carrier: Arc<Mutex<Option<Arc<dyn PeerCarrier>>>>,
    /// Local node metadata (instance_id, device name, transport mode). Kept so
    /// the mesh-support methods can build a local `SessionHelloV1` identical to
    /// the legacy handshake's.
    local_metadata: LocalMetadata,
    /// The inbound-event forwarder. Lets the
    /// mesh-support methods inject `SessionActivated` / `WireEnvelope` events
    /// through the existing dispatch path.
    on_receive: ReceiveCallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogConnectionSnapshot {
    pub dialog_key: String,
    pub peer_id: String,
    pub local_endpoint: Option<String>,
    pub peer_endpoint: Option<String>,
    pub remote_instance_id: Option<String>,
    pub remote_account_id: Option<String>,
    pub remote_device_id: Option<String>,
    pub initiated_locally: bool,
}

fn lock_mutex_recover<'a, T>(mutex: &'a Mutex<T>, context: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let _ = context;
            poisoned.into_inner()
        }
    }
}

fn lock_shared_state<'a>(shared: &'a Arc<Mutex<SharedState>>) -> MutexGuard<'a, SharedState> {
    lock_mutex_recover(shared.as_ref(), "shared state")
}

fn connection_matches_dialog_key(connection: &ConnectionState, dialog_key: &str) -> bool {
    connection.dialog_key == dialog_key
        || connection.remote_instance_id == dialog_key
        || omni_types::account_id_from_dialog_key(dialog_key).is_some_and(|account_id| {
            connection.remote_account_id.as_deref() == Some(account_id.as_str())
        })
        || connection.peer_id == dialog_key
}

fn received_chat_context_from_connection(connection: &ConnectionState) -> ReceivedChatContext {
    ReceivedChatContext {
        peer_id: connection.peer_id.clone(),
        dialog_key: connection.dialog_key.clone(),
        remote_instance_id: connection.remote_instance_id.clone(),
        remote_prekey_bundle: connection.remote_prekey_bundle.clone(),
        initiated_locally: connection.initiated_locally,
    }
}

fn verified_connection_peer_id_for_dialog_key(
    shared: &Arc<Mutex<SharedState>>,
    dialog_key: &str,
) -> Option<String> {
    let state = lock_shared_state(shared);
    state
        .connection
        .as_ref()
        .and_then(|connection| {
            connection_matches_dialog_key(connection, dialog_key)
                .then(|| connection.peer_id.clone())
        })
        .or_else(|| {
            state
                .verified_connections
                .values()
                .find(|connection| connection_matches_dialog_key(connection, dialog_key))
                .map(|connection| connection.peer_id.clone())
        })
}

fn dialog_connection_snapshot_from_connection(connection: &ConnectionState) -> DialogConnectionSnapshot {
    DialogConnectionSnapshot {
        dialog_key: connection.dialog_key.clone(),
        peer_id: connection.peer_id.clone(),
        local_endpoint: None,
        peer_endpoint: Some(connection.peer_id.clone()),
        remote_instance_id: Some(connection.remote_instance_id.clone()),
        remote_account_id: connection.remote_account_id.clone(),
        remote_device_id: connection
            .remote_prekey_bundle
            .as_ref()
            .map(|bundle| bundle.device.device_id.clone()),
        initiated_locally: connection.initiated_locally,
    }
}

fn dialog_connection_snapshot_for_dialog_key(
    shared: &Arc<Mutex<SharedState>>,
    dialog_key: &str,
) -> Option<DialogConnectionSnapshot> {
    let state = lock_shared_state(shared);
    state
        .verified_connections
        .values()
        .find(|connection| connection_matches_dialog_key(connection, dialog_key))
        .map(dialog_connection_snapshot_from_connection)
        .or_else(|| {
            state.connection.as_ref().and_then(|connection| {
                connection_matches_dialog_key(connection, dialog_key)
                    .then(|| dialog_connection_snapshot_from_connection(connection))
            })
        })
}

impl TransportManager {
    pub fn new<F>(instance_id: String, device_name: String, on_receive: F) -> Self
    where
        F: Fn(ReceivedTransportEvent) + Send + Sync + 'static,
    {
        Self::new_with_mode(instance_id, device_name, TransportMode::default(), on_receive)
    }

    pub fn new_with_mode<F>(
        instance_id: String,
        device_name: String,
        transport_mode: TransportMode,
        on_receive: F,
    ) -> Self
    where
        F: Fn(ReceivedTransportEvent) + Send + Sync + 'static,
    {
        let shared = Arc::new(Mutex::new(SharedState {
            transport_mode,
            connection: None,
            verified_connections: HashMap::new(),
            duplicate_resolved: false,
            local_account_id: None,
            local_prekey_bundle_json: None,
            last_error: None,
        }));
        let local_metadata = LocalMetadata {
            instance_id,
            device_name,
            transport_mode,
        };
        let user_on_receive: ReceiveCallback = Arc::new(on_receive);
        let (on_receive, event_dispatch) = ReceiveDispatcher::spawn(Arc::clone(&user_on_receive));
        Self {
            shared,
            event_dispatch,
            peer_carrier: Arc::new(Mutex::new(None)),
            local_metadata,
            on_receive,
        }
    }

    /// Install the per-peer carrier (the mesh overlay). Every outbound frame rides it; with no
    /// carrier route a frame goes to durable store-and-forward or fails.
    pub fn set_peer_carrier(&self, carrier: Arc<dyn PeerCarrier>) {
        *lock_mutex_recover(&self.peer_carrier, "peer carrier") = Some(carrier);
    }

    fn peer_carrier_snapshot(&self) -> Option<Arc<dyn PeerCarrier>> {
        lock_mutex_recover(&self.peer_carrier, "peer carrier")
            .as_ref()
            .map(Arc::clone)
    }

    // --- Mesh-support API (reused by the omni-app mesh carrier) ---
    //
    // These let the mesh overlay reuse the transport's existing session
    // machinery so a mesh-reached peer becomes a real, connected session
    // app-wide (verified_connections, SessionActivated, the on_receive event
    // path) — instead of omni-app duplicating that logic. They are additive;
    // the legacy paths are untouched. See
    // docs/decisions/mesh-transport-integration-seam.md.

    /// Build the local node's `SessionHello` envelope (identical content to the
    /// legacy QUIC handshake). omni-app serializes this and sends it as the
    /// first reliable frame on a new mesh Link so the peer can reconstruct the
    /// session context.
    pub fn build_local_session_hello_envelope(&self) -> TransportRequestEnvelopeV1 {
        build_local_session_hello_request(&self.shared, &self.local_metadata)
    }

    /// Register a peer reached over the mesh as a verified connection, from the
    /// `SessionHello` it sent on its Link. Reuses the legacy session-admission
    /// machinery (so the peer appears connected app-wide) and emits
    /// `SessionActivated` through the normal event path. Returns the context
    /// used to dispatch that peer's subsequent wire frames.
    pub fn register_mesh_connection_from_hello(
        &self,
        peer_id: &str,
        hello: &SessionHelloV1,
        initiated_locally: bool,
    ) -> OmniResult<ReceivedChatContext> {
        let receive_context = activate_connection_from_session_hello(
            &self.shared,
            peer_id,
            hello,
            &self.local_metadata.instance_id,
            initiated_locally,
        )?;
        (self.on_receive)(ReceivedTransportEvent::SessionActivated(receive_context.clone()));
        Ok(receive_context)
    }

    /// Remove the verified connection(s) for `peer_id` — called when the peer's mesh
    /// link drops, so the send path stops treating it as reachable (via
    /// `verified_connection_peer_id_for_dialog_key`) and routes the next message to
    /// durable store-and-forward instead of a dead link. Inverse of
    /// `register_mesh_connection_from_hello`.
    pub fn deregister_mesh_connection(&self, peer_id: &str) {
        let mut state = lock_shared_state(&self.shared);
        state.verified_connections.remove(peer_id);
        if state
            .connection
            .as_ref()
            .map(|connection| connection.peer_id.as_str())
            == Some(peer_id)
        {
            state.connection = None;
        }
    }

    /// Inject an inbound wire-envelope received over the mesh into the normal
    /// event dispatch, as if it had arrived over the legacy transport.
    pub fn inject_inbound_wire_envelope(
        &self,
        envelope: WireEnvelopeV1,
        receive_context: ReceivedChatContext,
    ) {
        (self.on_receive)(ReceivedTransportEvent::WireEnvelope(envelope, receive_context));
    }

    /// [`Self::inject_inbound_wire_envelope`] for a durable frame: `done` gets the handler's result
    /// after the app has handled it on the dispatcher thread (this call only queues it).
    pub fn inject_inbound_durable_wire_envelope(
        &self,
        envelope: WireEnvelopeV1,
        receive_context: ReceivedChatContext,
        done: DurableWireDone,
    ) {
        (self.on_receive)(ReceivedTransportEvent::DurableWireEnvelope(envelope, receive_context, done));
    }

    /// Build a receive-context for an inbound frame on an established dialog when
    /// there is no active mesh node-ctx — i.e. durable (store-and-forward) delivery
    /// from a peer we never linked this session. If an active verified connection
    /// matches `dialog_key`, reuse its full ctx; otherwise return a minimal ctx
    /// carrying just the dialog_key, which is enough for the receive path to resolve
    /// the persisted contact + ratchet session and decrypt (the dialog_key here is
    /// the contact's primary dialog_key, recorded at pairing). See
    /// docs/decisions/mesh-durable-delivery.md.
    pub fn received_context_for_dialog_key(&self, dialog_key: &str) -> ReceivedChatContext {
        {
            let state = lock_shared_state(&self.shared);
            let active = state
                .connection
                .as_ref()
                .filter(|connection| connection_matches_dialog_key(connection, dialog_key))
                .or_else(|| {
                    state
                        .verified_connections
                        .values()
                        .find(|connection| connection_matches_dialog_key(connection, dialog_key))
                });
            if let Some(connection) = active {
                return received_chat_context_from_connection(connection);
            }
        }
        ReceivedChatContext {
            peer_id: String::new(),
            dialog_key: dialog_key.to_string(),
            remote_instance_id: String::new(),
            remote_prekey_bundle: None,
            initiated_locally: false,
        }
    }

    fn set_last_error(&self, message: String) {
        let mut shared = lock_shared_state(&self.shared);
        shared.last_error = Some(message);
    }

    pub fn set_local_prekey_bundle_json(&self, bundle_json: Option<String>) {
        let mut shared = lock_shared_state(&self.shared);
        shared.local_account_id = bundle_json
            .as_ref()
            .and_then(|value| serde_json::from_str::<PrekeyBundleV1>(value).ok())
            .map(|bundle| bundle.account.account_id);
        shared.local_prekey_bundle_json = bundle_json;
    }

}


impl Drop for TransportManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shared) > 1 {
            return;
        }

        let mut shared = lock_shared_state(&self.shared);
        shared.connection = None;
        shared.verified_connections.clear();
        shared.duplicate_resolved = false;
        drop(shared);
        self.event_dispatch.shutdown();
    }
}

include!("transport_protocol.rs");

include!("attachment_transfer_protocol.rs");

include!("attachment_transfer_stream.rs");


include!("session.rs");



include!("transport_methods.rs");

impl fmt::Debug for TransportManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportManager")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

// Test module last so it doesn't trip clippy::items_after_test_module.
include!("tests.rs");
