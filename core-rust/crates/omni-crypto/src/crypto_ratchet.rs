pub fn establish_session(session: &SessionRecord, now_unix_ms: i64) -> SessionRecord {
    let mut next = session.clone();
    next.bootstrap_state = SessionBootstrapState::Established;
    next.updated_at_unix_ms = now_unix_ms;
    next
}

pub fn encrypt_application_payload(
    session: &mut SessionRecord,
    plaintext: &[u8],
) -> OmniResult<EncryptedPayload> {
    seal_payload(session, plaintext)
}

pub fn decrypt_application_payload(
    session: &mut SessionRecord,
    replay_window: &mut ReplayWindowState,
    payload: &EncryptedPayload,
) -> OmniResult<Vec<u8>> {
    open_payload(session, replay_window, payload)
}

pub fn encrypt_text_message(
    session: &mut SessionRecord,
    message_id: &str,
    body: &str,
    created_at_unix_ms: i64,
) -> OmniResult<EncryptedPayload> {
    let plaintext = serde_json::to_vec(&serde_json::json!({
        "kind": "text",
        "message_id": message_id.trim(),
        "body": body,
        "created_at_unix_ms": created_at_unix_ms,
    }))
    .map_err(map_serde_to_internal)?;
    seal_payload(session, &plaintext)
}

pub fn decrypt_text_message(
    session: &mut SessionRecord,
    replay_window: &mut ReplayWindowState,
    payload: &EncryptedPayload,
) -> OmniResult<DecryptedTextMessage> {
    let plaintext = open_payload(session, replay_window, payload)?;
    let value: serde_json::Value =
        serde_json::from_slice(&plaintext).map_err(map_serde_to_invalid)?;
    let kind = value
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if kind != "text" {
        return Err(OmniError::InvalidInput(
            "encrypted payload did not contain a text message".to_string(),
        ));
    }

    Ok(DecryptedTextMessage {
        message_id: value
            .get("message_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        body: value
            .get("body")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        created_at_unix_ms: value
            .get("created_at_unix_ms")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| OmniError::InvalidInput("invalid encrypted text timestamp".to_string()))?,
    })
}

pub fn encrypt_delivery_receipt(
    session: &mut SessionRecord,
    message_id: &str,
    delivered_at_unix_ms: i64,
) -> OmniResult<EncryptedPayload> {
    let plaintext = serde_json::to_vec(&serde_json::json!({
        "kind": "receipt",
        "message_id": message_id.trim(),
        "delivered_at_unix_ms": delivered_at_unix_ms,
    }))
    .map_err(map_serde_to_internal)?;
    seal_payload(session, &plaintext)
}

pub fn decrypt_delivery_receipt(
    session: &mut SessionRecord,
    replay_window: &mut ReplayWindowState,
    payload: &EncryptedPayload,
) -> OmniResult<DecryptedDeliveryReceipt> {
    let plaintext = open_payload(session, replay_window, payload)?;
    let value: serde_json::Value =
        serde_json::from_slice(&plaintext).map_err(map_serde_to_invalid)?;
    let kind = value
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if kind != "receipt" {
        return Err(OmniError::InvalidInput(
            "encrypted payload did not contain a delivery receipt".to_string(),
        ));
    }

    Ok(DecryptedDeliveryReceipt {
        message_id: value
            .get("message_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        delivered_at_unix_ms: value
            .get("delivered_at_unix_ms")
            .and_then(|value| value.as_i64())
            .ok_or_else(|| {
                OmniError::InvalidInput("invalid encrypted delivery timestamp".to_string())
            })?,
    })
}

pub fn new_replay_window_state(
    session_address: SessionAddress,
    now_unix_ms: i64,
) -> ReplayWindowState {
    ReplayWindowState {
        session_address,
        highest_counter: 0,
        recent_tags: Vec::new(),
        updated_at_unix_ms: now_unix_ms,
    }
}
