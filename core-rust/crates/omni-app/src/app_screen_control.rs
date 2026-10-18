// Remote screen control — the core half (docs/decisions/remote-screen-control.md).
//
// The sharer grants; the viewer's input batches are signed with the viewer's identity key and
// verified here, against the identity of someone the sharer actually granted, before the shell
// injects a single event. The path is not trusted (a relay can claim any `from`), nor is a group
// media key (every member can derive every member's) — the signature is.
//
// This file holds what 1:1 and groups share (identity cache, sign/open, the shell's injector) and the
// 1:1 state. The group half lives with the call runtime in `app_group_calls.rs`, next to `MediaState`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use omni_protocol::{
    screen_control_signed_bytes, split_screen_control_signature, wrap_screen_control, ScreenControlBatch,
    ScreenControlOutbox, ScreenControlReceiver, ScreenInputEvent,
};
use omni_storage::Storage;
use omni_types::{CallScreenControlV1, CallState, OmniError, OmniResult};

use crate::{current_unix_ms, OmniApp};

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// What the shell injects for one accepted batch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScreenControlDelivery {
    /// The call's dialog: the 1:1 dialog key or `group:<id>`.
    pub dialog_key: String,
    /// Who is controlling (base64 identity key).
    pub viewer: String,
    /// Pointer position normalised to the shared monitor (0..=65535).
    pub pointer: Option<(u16, u16)>,
    pub events: Vec<ScreenInputEvent>,
    /// Release every key and button held for this viewer before applying `events`: events were lost,
    /// or control ended (then `events` is empty).
    pub release: bool,
}

/// The shell's injector.
pub type ScreenControlSink = Arc<dyn Fn(&ScreenControlDelivery) + Send + Sync>;

#[derive(Default)]
pub(crate) struct ScreenControlSinkSlot(RwLock<Option<ScreenControlSink>>);

impl ScreenControlSinkSlot {
    fn set(&self, sink: Option<ScreenControlSink>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = sink;
        }
    }

    pub(crate) fn deliver(&self, delivery: &ScreenControlDelivery) {
        if let Some(sink) = self.0.read().ok().and_then(|slot| slot.clone()) {
            sink(delivery);
        }
    }

    /// Control by `viewer` ended: let go of whatever it holds down.
    pub(crate) fn release(&self, dialog_key: &str, viewer: &str) {
        self.deliver(&ScreenControlDelivery {
            dialog_key: dialog_key.to_string(),
            viewer: viewer.to_string(),
            pointer: None,
            events: Vec::new(),
            release: true,
        });
    }
}

// --- identity, sign / open ----------------------------------------------------

/// Our account Ed25519 key per profile (seed, public). Cached: a viewer signs a batch every
/// 16–100 ms and a sharer verifies at the same rate, and neither may touch storage for it.
static IDENTITY: OnceLock<Mutex<HashMap<String, ([u8; 32], [u8; 32])>>> = OnceLock::new();

fn identity_cache() -> &'static Mutex<HashMap<String, ([u8; 32], [u8; 32])>> {
    IDENTITY.get_or_init(Default::default)
}

pub(crate) fn local_signing_identity(storage: &Storage) -> OmniResult<([u8; 32], [u8; 32])> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    if let Some(cached) = identity_cache().lock().ok().and_then(|cache| cache.get(&db_path).copied()) {
        return Ok(cached);
    }
    let account = storage
        .get_local_account()?
        .ok_or_else(|| OmniError::Internal("no local account".into()))?;
    let identity = storage
        .get_local_identity_key(&account.descriptor.account_id)?
        .ok_or_else(|| OmniError::Internal("no identity key".into()))?;
    let seed = omni_crypto::mesh_identity_signing_key_bytes(&identity)?;
    let public = identity.signing_public_key.0;
    if let Ok(mut cache) = identity_cache().lock() {
        cache.insert(db_path, (seed, public));
    }
    Ok((seed, public))
}

/// Our public identity key, if a grant or a send already loaded it. The receive path uses this: it
/// has no storage, and a sharer always loaded it when granting.
pub(crate) fn cached_public_identity(db_path: &str) -> Option<[u8; 32]> {
    identity_cache().lock().ok().and_then(|cache| cache.get(db_path).map(|(_, public)| *public))
}

/// `body ‖ signature` of a batch for `sharer_pub` in `call_id`.
pub(crate) fn sign_batch(
    seed: &[u8; 32],
    call_id: &str,
    sharer_pub: &[u8; 32],
    batch: &ScreenControlBatch,
) -> OmniResult<Vec<u8>> {
    let body = batch
        .encode_body()
        .ok_or_else(|| OmniError::InvalidInput("too many screen-control events in one batch".into()))?;
    let signature = SigningKey::from_bytes(seed).sign(&screen_control_signed_bytes(call_id, sharer_pub, &body));
    Ok([body, signature.to_bytes().to_vec()].concat())
}

/// The batch inside `signed`, if `viewer_pub` signed it for us (`sharer_pub`) in `call_id`.
pub(crate) fn open_batch(
    viewer_pub: &[u8; 32],
    call_id: &str,
    sharer_pub: &[u8; 32],
    signed: &[u8],
) -> Option<ScreenControlBatch> {
    let (body, signature) = split_screen_control_signature(signed)?;
    VerifyingKey::from_bytes(viewer_pub)
        .ok()?
        .verify_strict(&screen_control_signed_bytes(call_id, sharer_pub, body), &Signature::from_bytes(&signature))
        .ok()?;
    ScreenControlBatch::decode_body(body)
}

pub(crate) fn decode_identity(member_pub: &str) -> Option<[u8; 32]> {
    B64.decode(member_pub).ok()?.try_into().ok()
}

pub(crate) fn encode_identity(public: &[u8; 32]) -> String {
    B64.encode(public)
}

// --- 1:1 ----------------------------------------------------------------------

/// Screen control in the current 1:1 call, either direction.
struct OneToOne {
    call_id: String,
    dialog_key: String,
    peer_pub: [u8; 32],
    /// We share and let the peer control.
    i_grant: bool,
    rx: ScreenControlReceiver,
    /// The peer shares and lets us control.
    peer_grants_me: bool,
    /// What the peer last said about its side (locked, its owner's hand on the mouse).
    peer_state: Option<omni_types::ScreenControlSharerState>,
    outbox: ScreenControlOutbox,
}

static ONE_TO_ONE: OnceLock<Mutex<HashMap<String, OneToOne>>> = OnceLock::new();

fn one_to_one() -> &'static Mutex<HashMap<String, OneToOne>> {
    ONE_TO_ONE.get_or_init(Default::default)
}

/// Run `f` on this call's state, starting fresh if the stored state belongs to another call.
fn with_call_state<R>(
    db_path: &str,
    call_id: &str,
    dialog_key: &str,
    peer_pub: [u8; 32],
    f: impl FnOnce(&mut OneToOne) -> R,
) -> OmniResult<R> {
    let mut states = one_to_one()
        .lock()
        .map_err(|_| OmniError::Internal("screen control registry poisoned".into()))?;
    let state = states.entry(db_path.to_string()).or_insert_with(|| fresh(call_id, dialog_key, peer_pub));
    if state.call_id != call_id {
        *state = fresh(call_id, dialog_key, peer_pub);
    }
    Ok(f(state))
}

fn fresh(call_id: &str, dialog_key: &str, peer_pub: [u8; 32]) -> OneToOne {
    OneToOne {
        call_id: call_id.to_string(),
        dialog_key: dialog_key.to_string(),
        peer_pub,
        i_grant: false,
        rx: ScreenControlReceiver::default(),
        peer_grants_me: false,
        peer_state: None,
        outbox: ScreenControlOutbox::default(),
    }
}

pub(crate) fn clear_screen_control_for_profile(db_path: &str) {
    if let Ok(mut states) = one_to_one().lock() {
        states.remove(db_path);
    }
    if let Ok(mut cache) = identity_cache().lock() {
        cache.remove(db_path);
    }
}

/// The current 1:1 call's id + dialog, if it is live (control means nothing outside one).
fn live_call(db_path: &str) -> Option<(String, String)> {
    crate::current_call_runtime(db_path)
        .ok()
        .flatten()
        .filter(|runtime| runtime.summary.state == CallState::Active)
        .map(|runtime| (runtime.summary.call_id, runtime.summary.dialog_key))
}

/// The peer's identity key for the current call, from its secure session.
fn current_peer_identity(storage: &Storage) -> OmniResult<(String, String, [u8; 32])> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let runtime = crate::current_call_runtime(&db_path)?
        .filter(|runtime| runtime.summary.state == CallState::Active)
        .ok_or_else(|| OmniError::InvalidInput("there is no active call".into()))?;
    let session = storage
        .get_session_record(&omni_crypto::SessionAddress {
            remote_account_id: runtime.remote_account_id.clone(),
            remote_device_id: runtime.remote_device_id.clone(),
        })?
        .ok_or_else(|| OmniError::InvalidInput("no secure session for the active call".into()))?;
    Ok((runtime.summary.call_id, runtime.summary.dialog_key, session.remote_identity_signing_key.0))
}

/// The peer granted or withdrew control of THEIR screen (inbound `CallScreenControlV1`).
pub(crate) fn handle_incoming_call_screen_control(storage: &Storage, control: &CallScreenControlV1) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    let Ok((call_id, dialog_key, peer_pub)) = current_peer_identity(storage) else {
        return Ok(()); // no live call: nothing to control
    };
    if call_id != control.call_id {
        return Ok(());
    }
    // Warm the identity cache now: sending must not touch storage.
    local_signing_identity(storage)?;
    with_call_state(&db_path, &call_id, &dialog_key, peer_pub, |state| {
        state.peer_grants_me = control.granted;
        state.peer_state = if control.granted { control.sharer_state } else { None };
        if !control.granted {
            state.outbox = ScreenControlOutbox::default();
        }
    })
}

/// A 1:1 screen-control datagram from the peer of `dialog_key`. `false` if it was not one.
pub(crate) fn handle_incoming_screen_control_datagram(
    db_path: &str,
    dialog_key: &str,
    payload: &[u8],
    sink: &ScreenControlSinkSlot,
) -> bool {
    let Some(signed) = omni_protocol::parse_screen_control(payload) else {
        return false;
    };
    let Some((call_id, live_dialog)) = live_call(db_path) else {
        return true;
    };
    let Some(me) = cached_public_identity(db_path) else {
        return true;
    };
    let delivery = {
        let Ok(mut states) = one_to_one().lock() else { return true };
        let Some(state) = states.get_mut(db_path) else { return true };
        if !state.i_grant || state.call_id != call_id || state.dialog_key != dialog_key || live_dialog != dialog_key {
            return true; // not granted (any more): dropped before the shell
        }
        let Some(batch) = open_batch(&state.peer_pub, &call_id, &me, signed) else {
            return true;
        };
        let Some(step) = state.rx.accept(&batch) else { return true };
        ScreenControlDelivery {
            dialog_key: dialog_key.to_string(),
            viewer: encode_identity(&state.peer_pub),
            pointer: step.pointer,
            events: step.events,
            release: step.gap,
        }
    };
    sink.deliver(&delivery);
    true
}

/// Screen control in the current 1:1 call: (we let the peer control, the peer lets us).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct CallScreenControlState {
    pub i_grant: bool,
    pub peer_grants_me: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_state: Option<omni_types::ScreenControlSharerState>,
}

impl OmniApp {
    /// Install (or with `None` remove) the shell's injector.
    pub fn set_screen_control_sink(&self, sink: Option<ScreenControlSink>) {
        self.screen_control_sink.set(sink);
    }

    /// Let the peer of the current 1:1 call control the screen we share, or stop letting them. The
    /// shell offers this only while a whole monitor is shared, and withdraws it when sharing stops.
    pub fn set_call_screen_control(&self, call_id: &str, granted: bool) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("set_call_screen_control")?;
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let (live_call_id, dialog_key, peer_pub) = current_peer_identity(&self.storage)?;
        if live_call_id != call_id {
            return Err(OmniError::InvalidInput("requested call does not match the current call".into()));
        }
        local_signing_identity(&self.storage)?;
        let withdrawn = with_call_state(&db_path, call_id, &dialog_key, peer_pub, |state| {
            let withdrawn = state.i_grant && !granted;
            state.i_grant = granted;
            state.rx = ScreenControlReceiver::default();
            withdrawn
        })?;
        if withdrawn {
            self.screen_control_sink.release(&dialog_key, &encode_identity(&peer_pub));
        }
        let runtime = crate::current_call_runtime(&db_path)?
            .ok_or_else(|| OmniError::InvalidInput("there is no active call".into()))?;
        crate::send_call_event_for_session_address(
            &self.storage,
            &self.transport,
            &dialog_key,
            &omni_crypto::SessionAddress {
                remote_account_id: runtime.remote_account_id,
                remote_device_id: runtime.remote_device_id,
            },
            &omni_types::LogicalEventEnvelopeV1::call_screen_control(call_id, granted, current_unix_ms()),
            omni_messaging::SecurityPayloadKind::CallScreenControl,
        )?;
        Ok(())
    }

    /// While we grant control, tell the controller what stands in the way of its input (locked,
    /// our own hand on the mouse) or that it is clear again. Leaves the grant and the replay window
    /// untouched — a state report must never reopen old batches.
    pub fn report_call_screen_control_state(
        &self,
        call_id: &str,
        sharer_state: omni_types::ScreenControlSharerState,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("report_call_screen_control_state")?;
        let _session_guard = crate::lock_app_session_guard(&self.session_lock)?;
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let (live_call_id, dialog_key, _) = current_peer_identity(&self.storage)?;
        if live_call_id != call_id {
            return Err(OmniError::InvalidInput("requested call does not match the current call".into()));
        }
        let granting = one_to_one()
            .lock()
            .ok()
            .and_then(|states| states.get(&db_path).map(|state| state.call_id == call_id && state.i_grant))
            .unwrap_or(false);
        if !granting {
            return Ok(()); // nothing granted: the controller has no input to wait on
        }
        let runtime = crate::current_call_runtime(&db_path)?
            .ok_or_else(|| OmniError::InvalidInput("there is no active call".into()))?;
        let mut event = omni_types::LogicalEventEnvelopeV1::call_screen_control(call_id, true, current_unix_ms());
        if let omni_types::LogicalEventPayloadV1::CallScreenControl(control) = &mut event.payload {
            control.sharer_state = Some(sharer_state);
        }
        crate::send_call_event_for_session_address(
            &self.storage,
            &self.transport,
            &dialog_key,
            &omni_crypto::SessionAddress {
                remote_account_id: runtime.remote_account_id,
                remote_device_id: runtime.remote_device_id,
            },
            &event,
            omni_messaging::SecurityPayloadKind::CallScreenControl,
        )?;
        Ok(())
    }

    pub fn call_screen_control_state(&self) -> CallScreenControlState {
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let Some((call_id, _)) = live_call(&db_path) else {
            return CallScreenControlState::default();
        };
        one_to_one()
            .lock()
            .ok()
            .and_then(|states| {
                states.get(&db_path).filter(|state| state.call_id == call_id).map(|state| CallScreenControlState {
                    i_grant: state.i_grant,
                    peer_grants_me: state.peer_grants_me,
                    peer_state: state.peer_state,
                })
            })
            .unwrap_or_default()
    }

    /// Send our input to the sharer we control: the peer of a 1:1 dialog, or `sharer` (a member_pub)
    /// in a `group:<id>` call. Called by the shell for every input and as a keep-alive.
    pub fn send_screen_control(
        &self,
        dialog_key: &str,
        sharer: Option<&str>,
        pointer: Option<(u16, u16)>,
        events: &[ScreenInputEvent],
    ) -> OmniResult<()> {
        if let Some(group_id) = crate::group_id_from_dialog_key(dialog_key) {
            let sharer = sharer.ok_or_else(|| OmniError::InvalidInput("group screen control needs the sharer".into()))?;
            return self.send_group_screen_control(group_id, sharer, pointer, events);
        }
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let (seed, _) = local_signing_identity(&self.storage)?;
        let (call_id, live_dialog) =
            live_call(&db_path).ok_or_else(|| OmniError::InvalidInput("there is no active call".into()))?;
        if live_dialog != dialog_key {
            return Err(OmniError::InvalidInput("the dialog is not the current call".into()));
        }
        let wire = {
            let mut states = one_to_one()
                .lock()
                .map_err(|_| OmniError::Internal("screen control registry poisoned".into()))?;
            let state = states
                .get_mut(&db_path)
                .filter(|state| state.call_id == call_id && state.peer_grants_me)
                .ok_or_else(|| OmniError::InvalidInput("the peer has not granted screen control".into()))?;
            let batch = state.outbox.batch(current_unix_ms(), pointer, events);
            wrap_screen_control(&sign_batch(&seed, &call_id, &state.peer_pub, &batch)?)
        };
        let (Some(node), Some(mesh)) = (self.mesh_node_for_dialog(dialog_key)?, self.mesh_runtime()) else {
            return Err(OmniError::Transport(format!("no mesh link for screen control in '{dialog_key}'")));
        };
        mesh.send_media(node, &wire)
            .map_err(|error| OmniError::Transport(format!("screen control send failed: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_protocol::ScreenMouseButton;

    fn key(seed: u8) -> ([u8; 32], [u8; 32]) {
        let seed = [seed; 32];
        (seed, SigningKey::from_bytes(&seed).verifying_key().to_bytes())
    }

    fn batch(counter: u64) -> ScreenControlBatch {
        ScreenControlBatch {
            counter,
            pointer: Some((100, 200)),
            first_seq: 0,
            events: vec![ScreenInputEvent::Button { button: ScreenMouseButton::Left, down: true }],
        }
    }

    #[test]
    fn a_batch_opens_only_for_its_signer_call_and_sharer() {
        let (viewer_seed, viewer_pub) = key(1);
        let (_, sharer_pub) = key(2);
        let (_, other_pub) = key(3);
        let signed = sign_batch(&viewer_seed, "call-1", &sharer_pub, &batch(1)).unwrap();
        assert_eq!(open_batch(&viewer_pub, "call-1", &sharer_pub, &signed), Some(batch(1)));

        // Negative: another claimed signer, another call, another sharer, a flipped bit.
        assert_eq!(open_batch(&other_pub, "call-1", &sharer_pub, &signed), None, "forged as someone else");
        assert_eq!(open_batch(&viewer_pub, "call-2", &sharer_pub, &signed), None, "replayed into another call");
        assert_eq!(open_batch(&viewer_pub, "call-1", &other_pub, &signed), None, "replayed at another sharer");
        let mut tampered = signed.clone();
        tampered[9] ^= 1;
        assert_eq!(open_batch(&viewer_pub, "call-1", &sharer_pub, &tampered), None, "tampered body");
        assert_eq!(open_batch(&viewer_pub, "call-1", &sharer_pub, &signed[..10]), None, "truncated");
    }

    fn live_call(db_path: &str, call_id: &str, dialog_key: &str, state: CallState) {
        let now = current_unix_ms();
        crate::set_current_call_runtime(
            db_path,
            Some(crate::ActiveCallRuntime {
                summary: omni_types::CallSummary {
                    call_id: call_id.to_string(),
                    dialog_key: dialog_key.to_string(),
                    contact_id: None,
                    direction: omni_types::CallDirection::Outgoing,
                    state,
                    outcome: None,
                    decline_reason: None,
                    participants: Vec::new(),
                    local_mic_muted: false,
                    network_quality: None,
                    started_at_unix_ms: now,
                    ringing_started_at_unix_ms: None,
                    connected_at_unix_ms: Some(now),
                    ended_at_unix_ms: None,
                    reconnect_deadline_unix_ms: None,
                    updated_at_unix_ms: now,
                    remote_control: None,
                },
                remote_account_id: crate::AccountId("acct-peer".into()),
                remote_device_id: crate::DeviceId("dev-peer".into()),
                invite_expires_at_unix_ms: now,
                invited_devices: Vec::new(),
            }),
        )
        .unwrap();
    }

    /// We share and granted the peer of the live call: the peer's signed batch reaches the injector.
    /// Negative halves: a replay, the same batch on another dialog, a batch forged with another key,
    /// a withdrawn grant and a call that is not live — none of it reaches the injector.
    #[test]
    fn a_1to1_batch_reaches_the_injector_only_from_the_granted_peer_of_the_live_call() {
        let db_path = "test-db-screen-1to1";
        let (call_id, dialog) = ("call-sc", "dialog:instance:sc-peer");
        let (my_seed, my_pub) = key(1);
        let (peer_seed, peer_pub) = key(2);
        let (stranger_seed, _) = key(3);
        identity_cache().lock().unwrap().insert(db_path.to_string(), (my_seed, my_pub));
        live_call(db_path, call_id, dialog, CallState::Active);
        with_call_state(db_path, call_id, dialog, peer_pub, |state| state.i_grant = true).unwrap();
        let delivered = Arc::new(Mutex::new(Vec::<ScreenControlDelivery>::new()));
        let sink = ScreenControlSinkSlot::default();
        let got = Arc::clone(&delivered);
        sink.set(Some(Arc::new(move |d| got.lock().unwrap().push(d.clone()))));
        let datagram = |seed: &[u8; 32], counter: u64| wrap_screen_control(&sign_batch(seed, call_id, &my_pub, &batch(counter)).unwrap());

        assert!(handle_incoming_screen_control_datagram(db_path, dialog, &datagram(&peer_seed, 1), &sink));
        let first = delivered.lock().unwrap().clone();
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].viewer.clone(), first[0].pointer), (encode_identity(&peer_pub), Some((100, 200))));

        // Negative halves.
        assert!(handle_incoming_screen_control_datagram(db_path, dialog, &datagram(&peer_seed, 1), &sink), "replay");
        assert!(handle_incoming_screen_control_datagram(db_path, "dialog:instance:other", &datagram(&peer_seed, 2), &sink));
        assert!(handle_incoming_screen_control_datagram(db_path, dialog, &datagram(&stranger_seed, 3), &sink), "forged");
        assert_eq!(delivered.lock().unwrap().len(), 1, "replay, foreign dialog and forgery are refused");
        live_call(db_path, call_id, dialog, CallState::Reconnecting);
        assert!(handle_incoming_screen_control_datagram(db_path, dialog, &datagram(&peer_seed, 4), &sink));
        live_call(db_path, call_id, dialog, CallState::Active);
        with_call_state(db_path, call_id, dialog, peer_pub, |state| state.i_grant = false).unwrap();
        assert!(handle_incoming_screen_control_datagram(db_path, dialog, &datagram(&peer_seed, 5), &sink));
        assert_eq!(delivered.lock().unwrap().len(), 1, "no live call or no grant: nothing gets through");

        crate::set_current_call_runtime(db_path, None).unwrap();
        clear_screen_control_for_profile(db_path);
    }

    #[test]
    fn an_ungranted_or_foreign_1to1_datagram_never_reaches_the_shell() {
        // No live call at all (the registry is empty for this profile): consumed, not delivered.
        let delivered = Arc::new(Mutex::new(0));
        let sink = ScreenControlSinkSlot::default();
        let counter = Arc::clone(&delivered);
        sink.set(Some(Arc::new(move |_| *counter.lock().unwrap() += 1)));
        let (viewer_seed, _) = key(1);
        let signed = sign_batch(&viewer_seed, "call-1", &key(2).1, &batch(1)).unwrap();
        let datagram = wrap_screen_control(&signed);
        assert!(handle_incoming_screen_control_datagram("no-such-profile", "dlg", &datagram, &sink));
        assert_eq!(*delivered.lock().unwrap(), 0);
        // Negative half: RTP is not screen control and is left for the media engine.
        assert!(!handle_incoming_screen_control_datagram("no-such-profile", "dlg", &[0x80, 0x60, 0, 1], &sink));
    }
}
