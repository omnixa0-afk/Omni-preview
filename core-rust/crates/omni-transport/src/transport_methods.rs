/// Каким путём ушёл конверт диалога.
///
/// Различие нужно не всем: для чата «доставлю позже» — законный исход, и вызывающий его не
/// разбирает. Оно нужно тем сообщениям, у которых «позже» не бывает — прежде всего согласию на
/// звонок. См. `send_wire_envelope_for_dialog_key_reporting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireEnvelopeDelivery {
    /// Байты ушли по живому пути — mesh-линк или legacy-сессия.
    Live,
    /// Труба есть и байты в неё отданы, НО исходящее плечо доказано мёртвым: пир не отвечает на
    /// наши пробы дольше порога (`SESSION_ACK_DEAD_MS`, сейчас 40 с). Отдельный исход, а не
    /// разновидность `Live`.
    ///
    /// Раньше это состояние схлопывалось в `Live`, и оно не было небрежностью: несущая
    /// действительно отдаёт байты, а `DurableOnly` вернуть нельзя — по нему вызывающий уходит в
    /// legacy-путь, который на живом mesh-линке падает, и пара теряет ровно те управляющие
    /// сообщения, которыми выбирается из заклинивания (docs/bugs/bootstrap-ack-never-held-durable.md).
    /// Ошибка была в том, что различие терялось НА ГРАНИЦЕ: для чата «ушло в живую трубу, плечо
    /// под вопросом» — законный успех, для звонка — нет.
    ///
    /// Живьём 2026-08-31 на локальной паре: контакт показан как `will_deliver_later` (тот же
    /// вердикт, та же улика), `call.place` вернул `ok`, звонок встал в `connecting`, второй конец
    /// не зазвонил. См. docs/bugs/a-call-nobody-could-hear-was-reported-as-placed.md.
    LiveUnconfirmed,
    /// Живого пути не нашлось; конверт лежит в durable store-and-forward до реконсиляции.
    DurableOnly,
}

/// The pre-mesh QUIC engine (listener, dialer, its sessions) is gone (docs/decisions/no-friend-tier.md,
/// part 2): every frame rides the mesh carrier installed by `set_peer_carrier`, or durable
/// store-and-forward when there is no live path. What stays here is the session registry the mesh
/// fills from each peer's `SessionHello` (`register_mesh_connection_from_hello`).
impl TransportManager {
    /// Forget the "active" connection (the last peer whose session was activated) and tell the app
    /// its session closed. The mesh link itself is the carrier's and stays up.
    pub fn disconnect(&self) -> OmniResult<TransportCommandResult> {
        let peer_id = lock_shared_state(&self.shared)
            .connection
            .as_ref()
            .map(|connection| connection.peer_id.clone())
            .ok_or_else(|| OmniError::Transport("no active connection".to_string()))?;
        Ok(self.close_connection(&peer_id, "disconnecting"))
    }

    pub fn disconnect_dialog(&self, dialog_key: &str) -> OmniResult<TransportCommandResult> {
        let dialog_key = normalize_dialog_key(dialog_key);
        if dialog_key.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }
        let peer_id = verified_connection_peer_id_for_dialog_key(&self.shared, &dialog_key)
            .ok_or_else(|| {
                OmniError::Transport(format!(
                    "no verified connection is available for dialog '{dialog_key}'"
                ))
            })?;
        Ok(self.close_connection(&peer_id, "disconnecting"))
    }

    pub fn disconnect_and_reset(&self) -> OmniResult<TransportCommandResult> {
        let active = lock_shared_state(&self.shared)
            .connection
            .as_ref()
            .map(|connection| connection.peer_id.clone());
        match active {
            Some(peer_id) => {
                self.close_connection(&peer_id, "reset");
            }
            None => {
                let mut state = lock_shared_state(&self.shared);
                state.connection = None;
                state.verified_connections.clear();
                state.duplicate_resolved = false;
            }
        }
        Ok(transport_result(&self.shared, "connection state reset"))
    }

    pub fn disconnect_dialog_and_reset(
        &self,
        dialog_key: &str,
    ) -> OmniResult<TransportCommandResult> {
        let dialog_key = normalize_dialog_key(dialog_key);
        if dialog_key.trim().is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }
        match verified_connection_peer_id_for_dialog_key(&self.shared, &dialog_key) {
            Some(peer_id) => Ok(self.close_connection(&peer_id, "reset")),
            None => Ok(transport_result(
                &self.shared,
                format!("no verified connection to reset for {dialog_key}"),
            )),
        }
    }

    fn close_connection(&self, peer_id: &str, verb: &str) -> TransportCommandResult {
        if let Some(context) = remove_shared_connection(&self.shared, peer_id) {
            (self.on_receive)(ReceivedTransportEvent::SessionClosed(context));
        }
        transport_result(&self.shared, format!("{verb} {peer_id}"))
    }

    /// Send to the peer of the "active" connection — the sender's shorthand for "the peer this
    /// session belongs to" when it holds no dialog key. Rides the same mesh path as
    /// `send_wire_envelope_for_dialog_key`.
    pub fn send_wire_envelope(&self, envelope: &WireEnvelopeV1) -> OmniResult<()> {
        let dialog_key = lock_shared_state(&self.shared)
            .connection
            .as_ref()
            .map(|connection| connection.dialog_key.clone())
            .ok_or_else(|| OmniError::Transport("no active connection".to_string()))?;
        self.send_wire_envelope_for_dialog_key(&dialog_key, envelope)
    }

    /// Increment 3 (delivery-reliability-model.md): explicitly back an envelope with durable
    /// store-and-forward, used by the send path when a mesh link reported `Sent` but the message
    /// is still unconfirmed after K ack-timeouts. `send_wire_envelope_for_dialog_key` only engages
    /// durable when there is NO live path (so it never preempts one); this is the belt-and-suspenders
    /// for a relay link that reports up but isn't delivering (route not re-established yet). No-op
    /// (returns false) when there is no mesh carrier or no route. The recipient dedups by message id,
    /// so a double (live + durable) delivery is harmless.
    pub fn hold_durable_for_dialog_key(&self, dialog_key: &str, envelope: &WireEnvelopeV1) -> bool {
        self.peer_carrier_snapshot()
            .map(|carrier| carrier.hold_durable(dialog_key, envelope))
            .unwrap_or(false)
    }

    /// Drop the mesh Link to the peer of `dialog_key` — the phantom-link escalation (see
    /// `PeerCarrier::drop_phantom_link`). Forces a fresh re-dial so a durable-backstopped message
    /// can reconcile over a working path instead of a dead-but-registered one. Returns `true` if a
    /// link was dropped. No-op (false) when there is no mesh carrier or route.
    pub fn drop_phantom_mesh_link_for_dialog_key(&self, dialog_key: &str) -> bool {
        self.peer_carrier_snapshot()
            .map(|carrier| carrier.drop_phantom_link(dialog_key))
            .unwrap_or(false)
    }

    pub fn send_wire_envelope_for_dialog_key(
        &self,
        dialog_key: &str,
        envelope: &WireEnvelopeV1,
    ) -> OmniResult<()> {
        self.send_wire_envelope_for_dialog_key_reporting(dialog_key, envelope)
            .map(|_delivery| ())
    }

    /// То же самое, но ГОВОРИТ, КАК ушёл конверт.
    ///
    /// `Ok(())` у обычного варианта покрывает два разных исхода: «байты ушли по живому пути» и
    /// «живого пути нет, конверт положен в durable до реконсиляции». Для чата это правда одно и то
    /// же — сообщение дойдёт. Для СОГЛАСИЯ НА ЗВОНОК не одно и то же совсем: звонок нельзя принять
    /// «когда-нибудь потом», и вызывающий отваливается по таймауту, пока принимающий сидит в
    /// активном звонке с открытым микрофоном. Ровно это и наблюдалось живьём 2026-08-29 —
    /// docs/bugs/callee-stays-in-a-call-that-ended.md.
    pub fn send_wire_envelope_for_dialog_key_reporting(
        &self,
        dialog_key: &str,
        envelope: &WireEnvelopeV1,
    ) -> OmniResult<WireEnvelopeDelivery> {
        validate_wire_envelope_v1(envelope)?;
        if let Some(carrier) = self.peer_carrier_snapshot() {
            match carrier.try_send_wire_envelope(dialog_key, envelope) {
                PeerCarrierOutcome::Sent => return Ok(WireEnvelopeDelivery::Live),
                PeerCarrierOutcome::SentUnconfirmed => {
                    // Маршрут ЕСТЬ и байты по нему ушли — просто сессия ещё не подтвердила круг.
                    // Durable здесь — подстраховка, а не замена: живой линк мог оказаться
                    // фантомным. Двойная доставка безопасна, приёмник дедуплицирует по id. Исход
                    // свой собственный: вызывающему, которому «позже» не годится (согласие на
                    // звонок), нужно отличать его от настоящей доставки.
                    let _ = carrier.hold_durable(dialog_key, envelope);
                    return Ok(WireEnvelopeDelivery::LiveUnconfirmed);
                }
                PeerCarrierOutcome::NotConnected => {
                    // No live path: hand the envelope to durable store-and-forward (if the carrier
                    // has a route), so an offline peer still gets it via reconcile. It runs only
                    // after the live path was ruled out, so it never preempts a link.
                    if carrier.hold_durable(dialog_key, envelope) {
                        return Ok(WireEnvelopeDelivery::DurableOnly);
                    }
                }
                PeerCarrierOutcome::Failed(error) => {
                    self.set_last_error(error.to_string());
                    return Err(error);
                }
            }
        }
        let error = OmniError::Transport(format!(
            "no path to the peer of dialog '{dialog_key}': no mesh link and no durable route"
        ));
        self.set_last_error(error.to_string());
        Err(error)
    }

    /// Open a BULK stream to the peer of `dialog_key` — the lane for attachment transfer, over the
    /// mesh link (see docs/bugs/attachment-stall-and-silent-receive-errors.md).
    pub fn open_attachment_transfer_stream_for_dialog_key(
        &self,
        dialog_key: &str,
    ) -> OmniResult<AttachmentTransferStream> {
        let outcome = self
            .peer_carrier_snapshot()
            .map(|carrier| carrier.try_open_attachment_stream(dialog_key))
            .unwrap_or(PeerCarrierStreamOutcome::NotConnected);
        match outcome {
            PeerCarrierStreamOutcome::Opened(stream) => Ok(*stream),
            PeerCarrierStreamOutcome::NotConnected => {
                let error = OmniError::Transport(format!(
                    "no mesh link is available for dialog '{dialog_key}'"
                ));
                self.set_last_error(error.to_string());
                Err(error)
            }
            PeerCarrierStreamOutcome::Failed(error) => {
                self.set_last_error(error.to_string());
                Err(error)
            }
        }
    }

    pub fn active_remote_instance_id(&self) -> Option<String> {
        self.snapshot().remote_instance_id
    }

    pub fn active_remote_account_id(&self) -> Option<String> {
        let shared = lock_shared_state(&self.shared);
        shared
            .connection
            .as_ref()
            .and_then(|connection| connection.remote_account_id.clone())
    }

    pub fn active_remote_prekey_bundle(&self) -> Option<PrekeyBundleV1> {
        let shared = lock_shared_state(&self.shared);
        shared
            .connection
            .as_ref()
            .and_then(|connection| connection.remote_prekey_bundle.clone())
    }

    /// Peer ids of every live link, in no particular order. One lock, no per-dialog scan: for a
    /// caller that only needs to notice that the set of links changed.
    pub fn live_peer_ids(&self) -> Vec<String> {
        let state = lock_shared_state(&self.shared);
        let mut ids: Vec<String> = state.verified_connections.keys().cloned().collect();
        if let Some(connection) = state.connection.as_ref() {
            if !state.verified_connections.contains_key(&connection.peer_id) {
                ids.push(connection.peer_id.clone());
            }
        }
        ids
    }

    pub fn active_dialog_key(&self) -> Option<String> {
        self.snapshot().dialog_key
    }

    pub fn snapshot(&self) -> TransportSnapshot {
        snapshot_from_shared(&self.shared)
    }

    pub fn connection_snapshot_for_dialog(
        &self,
        dialog_key: &str,
    ) -> Option<DialogConnectionSnapshot> {
        dialog_connection_snapshot_for_dialog_key(&self.shared, dialog_key)
    }
}

/// Take `peer_id`'s connection out of the registry; the context it carried, for `SessionClosed`.
fn remove_shared_connection(
    shared: &Arc<Mutex<SharedState>>,
    peer_id: &str,
) -> Option<ReceivedChatContext> {
    let mut state = lock_shared_state(shared);
    let active_matches = state
        .connection
        .as_ref()
        .is_some_and(|connection| connection.peer_id == peer_id);
    let closing = if active_matches {
        state.connection.take()
    } else {
        state.verified_connections.remove(peer_id)
    };
    if active_matches {
        state.verified_connections.remove(peer_id);
    }
    if state.connection.is_none() && state.verified_connections.is_empty() {
        state.duplicate_resolved = false;
    }
    closing.map(|connection| received_chat_context_from_connection(&connection))
}

fn snapshot_from_shared(shared: &Arc<Mutex<SharedState>>) -> TransportSnapshot {
    let state = lock_shared_state(shared);
    let connection = state.connection.as_ref();
    TransportSnapshot {
        state: if connection.is_some() {
            TransportState::Connected
        } else {
            TransportState::Idle
        },
        transport_mode: state.transport_mode,
        peer_id: connection.map(|connection| connection.peer_id.clone()),
        dialog_key: connection.map(|connection| connection.dialog_key.clone()),
        remote_instance_id: connection.map(|connection| connection.remote_instance_id.clone()),
        remote_device_name: connection.map(|connection| connection.remote_device_name.clone()),
        remote_account_id: connection.and_then(|connection| connection.remote_account_id.clone()),
        initiated_locally: connection.is_some_and(|connection| connection.initiated_locally),
        duplicate_resolved: state.duplicate_resolved,
        last_error: state.last_error.clone(),
    }
}

fn transport_result(
    shared: &Arc<Mutex<SharedState>>,
    message: impl Into<String>,
) -> TransportCommandResult {
    TransportCommandResult {
        ok: true,
        message: message.into(),
        snapshot: snapshot_from_shared(shared),
    }
}
