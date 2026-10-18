fn map_sqlite_error(error: rusqlite::Error) -> OmniError {
    OmniError::Storage(error.to_string())
}

fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn map_serde_error_to_storage(error: serde_json::Error) -> OmniError {
    OmniError::Storage(format!("serialization error: {error}"))
}

fn protected_secret_from_db(
    scheme: &str,
    blob_hex: &str,
) -> OmniResult<ProtectedSecret> {
    Ok(ProtectedSecret {
        scheme: SecretProtectionScheme::from_db_value(scheme)?,
        blob_hex: blob_hex.to_string(),
    })
}

fn protected_secret_from_optional_db(
    scheme: Option<String>,
    blob_hex: Option<String>,
) -> OmniResult<Option<ProtectedSecret>> {
    match (scheme, blob_hex) {
        (Some(scheme), Some(blob_hex)) => Ok(Some(ProtectedSecret {
            scheme: SecretProtectionScheme::from_db_value(&scheme)?,
            blob_hex,
        })),
        (None, None) => Ok(None),
        _ => Err(OmniError::Storage(
            "secret protection scheme/blob columns were only partially populated".to_string(),
        )),
    }
}

fn to_sqlite_user_error(error: OmniError) -> rusqlite::Error {
    into_sqlite_error(error)
}

fn map_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatMessageRecord> {
    let direction = parse_direction(&row.get::<_, String>(2)?).map_err(into_sqlite_error)?;
    let status = parse_status(&row.get::<_, String>(4)?).map_err(into_sqlite_error)?;

    Ok(ChatMessageRecord {
        id: row.get(0)?,
        dialog_key: row.get(1)?,
        direction,
        body: row.get(3)?,
        status,
        remote_delivered_at_unix_ms: row.get(5)?,
        created_at_unix_ms: row.get(6)?,
        updated_at_unix_ms: row.get(7)?,
        attachment: None,
        call_history_event: None,
        told_by_contact: false,
    })
}

fn map_contact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactSnapshot> {
    let relationship_state = omni_identity::ContactRelationshipState::from_db_value(
        &row.get::<_, String>(11)?,
    )
    .map_err(into_sqlite_error)?;
    let presence_state = omni_identity::ContactPresenceState::from_db_value(
        &row.get::<_, String>(15)?,
    )
    .map_err(into_sqlite_error)?;
    let pending_request_direction = row
        .get::<_, Option<String>>(12)?
        .map(|value| ContactRequestDirection::from_db_value(&value))
        .transpose()
        .map_err(into_sqlite_error)?;

    Ok(build_contact_snapshot(
        ContactId(row.get(0)?),
        row.get(1)?,
        row.get::<_, Option<String>>(9)?.map(AccountId),
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        None,
        row.get(10)?,
        row.get(7)?,
        row.get(8)?,
        relationship_state,
        pending_request_direction,
        row.get::<_, i64>(13)? != 0,
        row.get::<_, i64>(14)? != 0,
        presence_state,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
        row.get(20)?,
        row.get(21)?,
    ))
}

fn direction_to_db(direction: ChatDirection) -> &'static str {
    match direction {
        ChatDirection::Incoming => "incoming",
        ChatDirection::Outgoing => "outgoing",
    }
}

fn status_to_db(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Queued => "queued",
        MessageStatus::Sending => "sending",
        MessageStatus::Sent => "sent",
        MessageStatus::Delivered => "delivered",
        MessageStatus::Retrying => "retrying",
        MessageStatus::Failed => "failed",
        MessageStatus::Expired => "expired",
    }
}

fn parse_direction(value: &str) -> Result<ChatDirection, OmniError> {
    match value {
        "incoming" => Ok(ChatDirection::Incoming),
        "outgoing" => Ok(ChatDirection::Outgoing),
        _ => Err(OmniError::Storage(format!(
            "unexpected message direction '{value}' in storage"
        ))),
    }
}

fn parse_status(value: &str) -> Result<MessageStatus, OmniError> {
    match value {
        "queued" => Ok(MessageStatus::Queued),
        "sending" => Ok(MessageStatus::Sending),
        "sent" => Ok(MessageStatus::Sent),
        "delivered" => Ok(MessageStatus::Delivered),
        "retrying" => Ok(MessageStatus::Retrying),
        "failed" => Ok(MessageStatus::Failed),
        "expired" => Ok(MessageStatus::Expired),
        _ => Err(OmniError::Storage(format!(
            "unexpected message status '{value}' in storage"
        ))),
    }
}

fn verification_method_to_db(method: VerificationMethod) -> &'static str {
    match method {
        VerificationMethod::ManualCompare => "manual_compare",
        VerificationMethod::RecoveryAssist => "recovery_assist",
        VerificationMethod::ChallengeResponse => "challenge_response",
    }
}

fn parse_verification_method(value: &str) -> Result<VerificationMethod, OmniError> {
    match value {
        "manual_compare" => Ok(VerificationMethod::ManualCompare),
        "recovery_assist" => Ok(VerificationMethod::RecoveryAssist),
        "challenge_response" => Ok(VerificationMethod::ChallengeResponse),
        _ => Err(OmniError::Storage(format!(
            "unexpected verification method '{value}' in storage"
        ))),
    }
}

fn map_outbound_queue_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboundQueueRecord> {
    let status = parse_status(&row.get::<_, String>(2)?).map_err(into_sqlite_error)?;
    let last_failure_kind = row
        .get::<_, Option<String>>(6)?
        .map(|value| parse_queue_failure_kind(&value))
        .transpose()
        .map_err(into_sqlite_error)?;

    Ok(OutboundQueueRecord {
        message_id: row.get(0)?,
        dialog_key: row.get(1)?,
        status,
        attempt_count: row.get(3)?,
        next_attempt_unix_ms: row.get(4)?,
        last_attempt_unix_ms: row.get(5)?,
        last_failure_kind,
        last_failure_message: row.get(7)?,
        remote_delivered_at_unix_ms: row.get(8)?,
        created_at_unix_ms: row.get(9)?,
        updated_at_unix_ms: row.get(10)?,
    })
}

fn queue_failure_kind_to_db(kind: QueueFailureKind) -> &'static str {
    match kind {
        QueueFailureKind::NoActiveSession => "no_active_session",
        QueueFailureKind::SessionMismatch => "session_mismatch",
        QueueFailureKind::TransportWriteFailed => "transport_write_failed",
        QueueFailureKind::AckTimeout => "ack_timeout",
        QueueFailureKind::StorageFailure => "storage_failure",
        QueueFailureKind::Internal => "internal",
    }
}

fn failure_kind_to_default_message(kind: QueueFailureKind) -> &'static str {
    match kind {
        QueueFailureKind::NoActiveSession => "no active session is available for the dialog",
        QueueFailureKind::SessionMismatch => "active session does not match the target dialog",
        QueueFailureKind::TransportWriteFailed => "transport write failed",
        QueueFailureKind::AckTimeout => "delivery receipt timed out",
        QueueFailureKind::StorageFailure => "storage operation failed",
        QueueFailureKind::Internal => "internal queue error",
    }
}

fn parse_queue_failure_kind(value: &str) -> Result<QueueFailureKind, OmniError> {
    match value {
        "no_active_session" => Ok(QueueFailureKind::NoActiveSession),
        "session_mismatch" => Ok(QueueFailureKind::SessionMismatch),
        "transport_write_failed" => Ok(QueueFailureKind::TransportWriteFailed),
        "ack_timeout" => Ok(QueueFailureKind::AckTimeout),
        "storage_failure" => Ok(QueueFailureKind::StorageFailure),
        "internal" => Ok(QueueFailureKind::Internal),
        _ => Err(OmniError::Storage(format!(
            "unexpected queue failure kind '{value}' in storage"
        ))),
    }
}

#[derive(Debug, Default, Clone)]
struct QueueMetadata {
    attempt_count: i64,
    last_attempt_unix_ms: Option<i64>,
    last_failure_kind: Option<QueueFailureKind>,
    last_failure_message: Option<String>,
}

fn bool_to_sqlite_integer(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn into_sqlite_error(error: OmniError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}

fn generate_local_instance_id() -> String {
    format!(
        "inst-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
