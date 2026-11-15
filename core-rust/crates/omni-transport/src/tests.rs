#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use omni_types::{SessionHelloV1, TransportMode, WireEnvelopeV1};

    use super::{
        connection_matches_dialog_key, duplicate_connection_prefers_candidate, ConnectionState,
        PeerCarrier, PeerCarrierOutcome, ReceivedTransportEvent, TransportManager,
        WireEnvelopeDelivery,
    };

    /// Blind audit 2026-11-11, pass 13: the queue to the app's callback had no bound — a peer
    /// sending faster than the app handled grew it without end. An event now waits for room.
    #[test]
    fn an_event_waits_for_room_while_the_apps_callback_is_behind() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&handled);
        let (forward, dispatch) = super::ReceiveDispatcher::spawn(Arc::new(move |_event| {
            let _ = held.lock().unwrap().recv();
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let event = || {
            ReceivedTransportEvent::SessionClosed(super::ReceivedChatContext {
                peer_id: String::new(),
                dialog_key: "d".into(),
                remote_instance_id: "i".into(),
                remote_prekey_bundle: None,
                initiated_locally: false,
            })
        };
        // One is taken by the callback (which waits); the queue then fills to its bound.
        for _ in 0..=super::RECEIVE_QUEUE_MAX {
            forward(event());
        }
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (flag, pusher_forward) = (Arc::clone(&done), Arc::clone(&forward));
        let pusher = std::thread::spawn(move || {
            pusher_forward(event());
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!done.load(std::sync::atomic::Ordering::SeqCst), "no room: it waits");
        release.send(()).unwrap();
        pusher.join().unwrap();
        assert!(done.load(std::sync::atomic::Ordering::SeqCst), "room came: queued");
        drop(release);
        dispatch.shutdown();
        assert_eq!(handled.load(std::sync::atomic::Ordering::SeqCst), super::RECEIVE_QUEUE_MAX + 2, "none lost");
    }

    /// Blind audit 2026-11-11, pass 14: past its wait every event was queued over the bound, with
    /// no end to it; and media waited for room like the rest.
    #[test]
    fn the_queue_to_the_app_has_a_ceiling_and_media_does_not_wait() {
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        let (_forward, dispatch) = super::ReceiveDispatcher::spawn(Arc::new(move |_event| {
            let _ = held.lock().unwrap().recv();
        }));
        let context = || super::ReceivedChatContext {
            peer_id: String::new(),
            dialog_key: "d".into(),
            remote_instance_id: "i".into(),
            remote_prekey_bundle: None,
            initiated_locally: false,
        };
        let queued = || dispatch.queue.lock().unwrap().events.len();
        let wire = || ReceivedTransportEvent::WireEnvelope(WireEnvelopeV1::chat_text("m".to_string(), "x".to_string(), 1), context());
        // Not waiting (as the dispatcher's own thread hands in): up to the ceiling, no further.
        for _ in 0..super::RECEIVE_QUEUE_CEILING + 50 {
            dispatch.push(wire(), false);
        }
        assert!(queued() <= super::RECEIVE_QUEUE_CEILING, "{}", queued());
        let full = queued();
        let started = std::time::Instant::now();
        dispatch.push(ReceivedTransportEvent::RealtimeMediaDatagram(vec![0; 8], context()), true);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "media does not wait for room");
        assert_eq!(queued(), full, "and is dropped");
        let states = || dispatch.queue.lock().unwrap().events.iter().filter(|e| super::session_of_state(e).is_some()).count();
        dispatch.push(ReceivedTransportEvent::SessionClosed(context()), false);
        assert_eq!(states(), 1, "a session's state is kept");
        assert_eq!(queued(), full + 1, "over the ceiling, and no other event made room for it (pass 17)");
        // Pass 15: a flapping session does not grow it — over the ceiling, its state replaces the
        // one of it already waiting, and the last one stands.
        for _ in 0..2_000 {
            dispatch.push(ReceivedTransportEvent::SessionActivated(context()), false);
            dispatch.push(ReceivedTransportEvent::SessionClosed(context()), false);
        }
        assert_eq!(queued(), full + 1, "one state per session over the ceiling");
        assert!(matches!(dispatch.queue.lock().unwrap().events.back(), Some(ReceivedTransportEvent::SessionClosed(_))), "the last one stands");
        // Pass 16: nor do states of ever new sessions grow it for good — they have headroom of
        // their own; pass 17: and no other event is taken out for them.
        for n in 0..2 * super::RECEIVE_QUEUE_MAX {
            let other = super::ReceivedChatContext { dialog_key: format!("d-{n}"), ..context() };
            dispatch.push(ReceivedTransportEvent::SessionActivated(other), false);
        }
        assert_eq!(queued(), super::RECEIVE_QUEUE_STATE_CEILING, "the states stop at their own ceiling");
        assert_eq!(queued() - states(), full, "and every event that is no state is still there");
        drop(release);
        dispatch.shutdown();
    }

    /// A stand-in for the mesh carrier: answers every send with `outcome` and every durable hold
    /// with `durable`, and records which dialogs it was asked about.
    struct FakeCarrier {
        outcome: fn() -> PeerCarrierOutcome,
        durable: bool,
        sends: Mutex<Vec<String>>,
        holds: Mutex<Vec<String>>,
    }

    impl FakeCarrier {
        fn new(outcome: fn() -> PeerCarrierOutcome, durable: bool) -> Arc<Self> {
            Arc::new(Self {
                outcome,
                durable,
                sends: Mutex::new(Vec::new()),
                holds: Mutex::new(Vec::new()),
            })
        }
    }

    impl PeerCarrier for FakeCarrier {
        fn try_send_wire_envelope(
            &self,
            dialog_key: &str,
            _envelope: &WireEnvelopeV1,
        ) -> PeerCarrierOutcome {
            self.sends.lock().unwrap().push(dialog_key.to_string());
            (self.outcome)()
        }

        fn hold_durable(&self, dialog_key: &str, _envelope: &WireEnvelopeV1) -> bool {
            self.holds.lock().unwrap().push(dialog_key.to_string());
            self.durable
        }

        fn drop_phantom_link(&self, _dialog_key: &str) -> bool {
            false
        }
    }

    fn manager_with(carrier: Arc<FakeCarrier>) -> TransportManager {
        let manager = TransportManager::new("local".to_string(), "Local".to_string(), |_event| {});
        manager.set_peer_carrier(carrier);
        manager
    }

    fn text_envelope() -> WireEnvelopeV1 {
        WireEnvelopeV1::chat_text("message-1".to_string(), "hello".to_string(), 1)
    }

    fn hello_from(instance_id: &str) -> SessionHelloV1 {
        SessionHelloV1 {
            instance_id: instance_id.to_string(),
            transport_mode: TransportMode::Quic,
            device_name: "Remote".to_string(),
            account_id: Some("acct-remote".to_string()),
            prekey_bundle_json: None,
        }
    }

    #[test]
    fn a_frame_rides_the_mesh_link_when_the_carrier_has_one() {
        let carrier = FakeCarrier::new(|| PeerCarrierOutcome::Sent, true);
        let manager = manager_with(Arc::clone(&carrier));

        let delivery = manager
            .send_wire_envelope_for_dialog_key_reporting("dialog:peer", &text_envelope())
            .unwrap();

        assert_eq!(delivery, WireEnvelopeDelivery::Live);
        assert_eq!(*carrier.sends.lock().unwrap(), vec!["dialog:peer".to_string()]);
        assert!(carrier.holds.lock().unwrap().is_empty(), "a live link never preempts to durable");
    }

    #[test]
    fn without_a_link_the_frame_goes_to_durable_and_without_either_the_send_fails() {
        let with_durable = FakeCarrier::new(|| PeerCarrierOutcome::NotConnected, true);
        let delivery = manager_with(Arc::clone(&with_durable))
            .send_wire_envelope_for_dialog_key_reporting("dialog:peer", &text_envelope())
            .unwrap();
        assert_eq!(delivery, WireEnvelopeDelivery::DurableOnly);

        // No link and no durable route: there is no other engine to fall back to any more.
        let nowhere = FakeCarrier::new(|| PeerCarrierOutcome::NotConnected, false);
        let manager = manager_with(nowhere);
        assert!(manager
            .send_wire_envelope_for_dialog_key("dialog:peer", &text_envelope())
            .is_err());
        assert!(manager.snapshot().last_error.is_some());
    }

    #[test]
    fn an_unconfirmed_link_is_reported_as_such_and_backed_by_durable() {
        let carrier = FakeCarrier::new(|| PeerCarrierOutcome::SentUnconfirmed, true);
        let delivery = manager_with(Arc::clone(&carrier))
            .send_wire_envelope_for_dialog_key_reporting("dialog:peer", &text_envelope())
            .unwrap();
        assert_eq!(delivery, WireEnvelopeDelivery::LiveUnconfirmed);
        assert_eq!(*carrier.holds.lock().unwrap(), vec!["dialog:peer".to_string()]);
    }

    #[test]
    fn a_mesh_hello_registers_the_session_and_disconnect_closes_it() {
        let closed = Arc::new(Mutex::new(Vec::new()));
        let manager = {
            let closed = Arc::clone(&closed);
            TransportManager::new("local".to_string(), "Local".to_string(), move |event| {
                if let ReceivedTransportEvent::SessionClosed(context) = event {
                    closed.lock().unwrap().push(context.dialog_key);
                }
            })
        };
        let context = manager
            .register_mesh_connection_from_hello(
                "node-remote",
                &hello_from("remote-instance"),
                false,
            )
            .unwrap();
        assert_eq!(
            manager.active_dialog_key().as_deref(),
            Some(context.dialog_key.as_str())
        );
        assert!(manager
            .connection_snapshot_for_dialog(&context.dialog_key)
            .is_some());

        manager.disconnect_dialog(&context.dialog_key).unwrap();

        assert!(manager
            .connection_snapshot_for_dialog(&context.dialog_key)
            .is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while closed.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(*closed.lock().unwrap(), vec![context.dialog_key.clone()]);
        // Nothing left to disconnect: the second call is an error, not a silent success.
        assert!(manager.disconnect_dialog(&context.dialog_key).is_err());
    }

    #[test]
    fn duplicate_connection_tie_breaker_prefers_lower_instance_outbound_side() {
        assert!(duplicate_connection_prefers_candidate(
            "alice-instance",
            "bob-instance",
            true,
            false
        ));
        assert!(!duplicate_connection_prefers_candidate(
            "alice-instance",
            "bob-instance",
            false,
            true
        ));
        assert!(!duplicate_connection_prefers_candidate(
            "bob-instance",
            "alice-instance",
            true,
            false
        ));
        assert!(duplicate_connection_prefers_candidate(
            "bob-instance",
            "alice-instance",
            false,
            true
        ));
        assert!(!duplicate_connection_prefers_candidate(
            "alice-instance",
            "bob-instance",
            true,
            true
        ));
    }

    fn test_connection_state(peer_id: &str, instance_id: &str) -> ConnectionState {
        ConnectionState {
            peer_id: peer_id.to_string(),
            dialog_key: omni_types::dialog_key_from_instance_id(instance_id),
            remote_instance_id: instance_id.to_string(),
            _remote_transport_mode: TransportMode::Quic,
            remote_device_name: "Test device".to_string(),
            remote_account_id: None,
            remote_prekey_bundle: None,
            initiated_locally: false,
        }
    }

    #[test]
    fn account_dialog_key_matches_verified_connection_remote_account() {
        let mut connection = test_connection_state("peer-id", "instance-id");
        connection.remote_account_id = Some("acct-remote".to_string());

        assert!(connection_matches_dialog_key(
            &connection,
            &omni_types::dialog_key_from_account_id("acct-remote")
        ));
        assert!(!connection_matches_dialog_key(
            &connection,
            &omni_types::dialog_key_from_account_id("acct-other")
        ));
    }

    /// docs/decisions/attachment-content-tree.md, stage 3: a Start carries the file's whole block
    /// layer — 4 MiB for the 2 TB a file may be — and must ride a stream; a bigger one is refused.
    #[test]
    fn a_start_with_the_layer_of_the_largest_file_rides_a_stream() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let start = |payload_len: usize| {
            super::AttachmentTransferFrameV2::Start(super::AttachmentTransferStartV2 {
                transfer_id: "transfer-layer".to_string(),
                payload: vec![7; payload_len],
                started_at_unix_ms: 1_000,
            })
        };
        let largest = 4 * 1024 * 1024 + 64;
        let mut wire = Vec::new();
        runtime.block_on(super::write_attachment_stream_frame(&mut wire, &start(largest))).unwrap();
        let read = runtime.block_on(super::read_attachment_stream_frame(&mut wire.as_slice())).unwrap();
        match read {
            Some(super::AttachmentTransferFrameV2::Start(read)) => assert_eq!(read.payload.len(), largest),
            other => panic!("expected the Start back, got {other:?}"),
        }
        assert!(super::validate_attachment_transfer_frame_v2(&start(largest)).is_ok());
        assert!(super::validate_attachment_transfer_frame_v2(&start(super::ATTACHMENT_TRANSFER_START_SIZE_MAXIMUM_BYTES + 1)).is_err());
    }
}
