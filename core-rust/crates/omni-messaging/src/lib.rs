use omni_crypto::{
    decrypt_application_payload, encrypt_application_payload, EncryptedPayload, ReplayWindowState,
    SessionBootstrapState, SessionRecord,
};
use omni_identity::{ContactId, TrustState};
use omni_protocol::{validate_wire_envelope_v1, SessionBootstrapMessageV1};
use omni_types::{AttachmentOfferV1, DeliveryReceiptV1, OmniError, OmniResult};
use omni_types::{EncryptedPayloadV1, LogicalEventEnvelopeV1, WireEnvelopeV1, WirePayloadV1};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageEncryptionState {
    LegacyPlaintext,
    BootstrapSent,
    EncryptedPending,
    EncryptedDelivered,
    EncryptedReceived,
}

impl MessageEncryptionState {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::LegacyPlaintext => "legacy_plaintext",
            Self::BootstrapSent => "bootstrap_sent",
            Self::EncryptedPending => "encrypted_pending",
            Self::EncryptedDelivered => "encrypted_delivered",
            Self::EncryptedReceived => "encrypted_received",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "legacy_plaintext" => Ok(Self::LegacyPlaintext),
            "bootstrap_sent" => Ok(Self::BootstrapSent),
            "encrypted_pending" => Ok(Self::EncryptedPending),
            "encrypted_delivered" => Ok(Self::EncryptedDelivered),
            "encrypted_received" => Ok(Self::EncryptedReceived),
            _ => Err(OmniError::Storage(format!(
                "unsupported message encryption state '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityPayloadKind {
    SessionBootstrap,
    Text,
    PublicProfileDescriptorSync,
    PublicProfileAvatarSync,
    AttachmentOffer,
    AttachmentDownloadRequest,
    AttachmentChunk,
    AttachmentTransferComplete,
    AttachmentTransferFailed,
    AttachmentDownloaded,
    AttachmentAccessRevoked,
    DeliveryReceipt,
    CallInvite,
    CallRinging,
    CallAccept,
    CallDecline,
    CallBusy,
    CallCancel,
    CallHangup,
    CallTimeout,
    CallReconnect,
    CallScreenControl,
    RemoteAccessGrant,
    SentTranscript,
    DeviceRevocation,
    AccountKeyMove,
    ContactSync,
    ContactSyncAck,
    CourierSet,
    AccountPassport,
    HistorySync,
    RealtimeMediaCapabilities,
    RealtimeMediaSelection,
    RealtimeMediaControl,
    ContactPresence,
    ContactPresencePing,
    ContactPresencePong,
    RouteHintsSync,
    RelaySetUpdate,
    PauseAutoReconnect,
    VerificationChallenge,
    VerificationChallengeResponse,
    GroupControl,
}

impl SecurityPayloadKind {
    pub fn as_db_value(self) -> &'static str {
        match self {
            Self::SessionBootstrap => "session_bootstrap",
            Self::Text => "text",
            Self::PublicProfileDescriptorSync => "public_profile_descriptor_sync",
            Self::PublicProfileAvatarSync => "public_profile_avatar_sync",
            Self::AttachmentOffer => "attachment_offer",
            Self::AttachmentDownloadRequest => "attachment_download_request",
            Self::AttachmentChunk => "attachment_chunk",
            Self::AttachmentTransferComplete => "attachment_transfer_complete",
            Self::AttachmentTransferFailed => "attachment_transfer_failed",
            Self::AttachmentDownloaded => "attachment_downloaded",
            Self::AttachmentAccessRevoked => "attachment_access_revoked",
            Self::DeliveryReceipt => "delivery_receipt",
            Self::CallInvite => "call_invite",
            Self::CallRinging => "call_ringing",
            Self::CallAccept => "call_accept",
            Self::CallDecline => "call_decline",
            Self::CallBusy => "call_busy",
            Self::CallCancel => "call_cancel",
            Self::CallHangup => "call_hangup",
            Self::CallTimeout => "call_timeout",
            Self::CallReconnect => "call_reconnect",
            Self::CallScreenControl => "call_screen_control",
            Self::RemoteAccessGrant => "remote_access_grant",
            Self::SentTranscript => "sent_transcript",
            Self::DeviceRevocation => "device_revocation",
            Self::AccountKeyMove => "account_key_move",
            Self::ContactSync => "contact_sync",
            Self::ContactSyncAck => "contact_sync_ack",
            Self::CourierSet => "courier_set",
            Self::AccountPassport => "account_passport",
            Self::HistorySync => "history_sync",
            Self::RealtimeMediaCapabilities => "realtime_media_capabilities",
            Self::RealtimeMediaSelection => "realtime_media_selection",
            Self::RealtimeMediaControl => "realtime_media_control",
            Self::ContactPresence => "contact_presence",
            Self::ContactPresencePing => "contact_presence_ping",
            Self::ContactPresencePong => "contact_presence_pong",
            Self::RouteHintsSync => "route_hints_sync",
            Self::RelaySetUpdate => "relay_set_update",
            Self::PauseAutoReconnect => "pause_auto_reconnect",
            Self::VerificationChallenge => "verification_challenge",
            Self::VerificationChallengeResponse => "verification_challenge_response",
            Self::GroupControl => "group_control",
        }
    }

    pub fn from_db_value(value: &str) -> OmniResult<Self> {
        match value {
            "session_bootstrap" => Ok(Self::SessionBootstrap),
            "text" => Ok(Self::Text),
            "public_profile_descriptor_sync" => Ok(Self::PublicProfileDescriptorSync),
            "public_profile_avatar_sync" => Ok(Self::PublicProfileAvatarSync),
            "attachment_offer" => Ok(Self::AttachmentOffer),
            "attachment_download_request" => Ok(Self::AttachmentDownloadRequest),
            "attachment_chunk" => Ok(Self::AttachmentChunk),
            "attachment_transfer_complete" => Ok(Self::AttachmentTransferComplete),
            "attachment_transfer_failed" => Ok(Self::AttachmentTransferFailed),
            "attachment_downloaded" => Ok(Self::AttachmentDownloaded),
            "attachment_access_revoked" => Ok(Self::AttachmentAccessRevoked),
            "delivery_receipt" => Ok(Self::DeliveryReceipt),
            "call_invite" => Ok(Self::CallInvite),
            "call_ringing" => Ok(Self::CallRinging),
            "call_accept" => Ok(Self::CallAccept),
            "call_decline" => Ok(Self::CallDecline),
            "call_busy" => Ok(Self::CallBusy),
            "call_cancel" => Ok(Self::CallCancel),
            "call_hangup" => Ok(Self::CallHangup),
            "call_timeout" => Ok(Self::CallTimeout),
            "call_reconnect" => Ok(Self::CallReconnect),
            "call_screen_control" => Ok(Self::CallScreenControl),
            "remote_access_grant" => Ok(Self::RemoteAccessGrant),
            "sent_transcript" => Ok(Self::SentTranscript),
            "device_revocation" => Ok(Self::DeviceRevocation),
            "account_key_move" => Ok(Self::AccountKeyMove),
            "contact_sync" => Ok(Self::ContactSync),
            "contact_sync_ack" => Ok(Self::ContactSyncAck),
            "courier_set" => Ok(Self::CourierSet),
            "account_passport" => Ok(Self::AccountPassport),
            "history_sync" => Ok(Self::HistorySync),
            "realtime_media_capabilities" => Ok(Self::RealtimeMediaCapabilities),
            "realtime_media_selection" => Ok(Self::RealtimeMediaSelection),
            "realtime_media_control" => Ok(Self::RealtimeMediaControl),
            "contact_presence" => Ok(Self::ContactPresence),
            "contact_presence_ping" => Ok(Self::ContactPresencePing),
            "contact_presence_pong" => Ok(Self::ContactPresencePong),
            "route_hints_sync" => Ok(Self::RouteHintsSync),
            "relay_set_update" => Ok(Self::RelaySetUpdate),
            "pause_auto_reconnect" => Ok(Self::PauseAutoReconnect),
            "verification_challenge" => Ok(Self::VerificationChallenge),
            "verification_challenge_response" => Ok(Self::VerificationChallengeResponse),
            "group_control" => Ok(Self::GroupControl),
            _ => Err(OmniError::Storage(format!(
                "unsupported security payload kind '{value}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityWarningKind {
    None,
    UnlinkedDialog,
    MissingSession,
    MissingRemoteBundle,
    Unverified,
    ReverifyNeeded,
    IdentityChanged,
    BlockedContact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSecuritySummary {
    pub dialog_key: String,
    pub contact_id: Option<ContactId>,
    pub remote_account_id: Option<String>,
    pub remote_device_id: Option<String>,
    pub trust_state: Option<TrustState>,
    pub warning: SecurityWarningKind,
    pub has_established_session: bool,
    pub has_remote_prekey_bundle: bool,
    pub uses_encrypted_transport: bool,
    pub session_state: Option<SessionBootstrapState>,
    pub last_session_update_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSecurityDetails {
    pub message_id: String,
    pub dialog_key: String,
    pub contact_id: Option<ContactId>,
    pub remote_account_id: Option<String>,
    pub remote_device_id: Option<String>,
    pub remote_device_label: Option<String>,
    pub session_address: Option<String>,
    pub trust_state: Option<TrustState>,
    pub encryption_state: MessageEncryptionState,
    pub bootstrap_state: Option<SessionBootstrapState>,
    pub payload_kind: Option<SecurityPayloadKind>,
    pub has_encrypted_payload: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogMessageRefreshSummary {
    pub dialog_key: String,
    pub message_count: usize,
    pub latest_message_id: Option<String>,
    pub last_message_updated_at_unix_ms: Option<i64>,
    pub last_attachment_updated_at_unix_ms: Option<i64>,
    pub last_attachment_message_id: Option<String>,
    pub last_security_projection_updated_at_unix_ms: Option<i64>,
    pub last_security_projection_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogDeltaCursor {
    pub dialog_key: String,
    pub message_count: usize,
    pub latest_message_id: Option<String>,
    pub last_message_updated_at_unix_ms: Option<i64>,
    pub last_attachment_updated_at_unix_ms: Option<i64>,
    pub last_attachment_message_id: Option<String>,
    pub last_security_projection_updated_at_unix_ms: Option<i64>,
    pub last_security_projection_message_id: Option<String>,
}

impl DialogDeltaCursor {
    pub fn from_refresh_summary(summary: &DialogMessageRefreshSummary) -> Self {
        Self {
            dialog_key: summary.dialog_key.clone(),
            message_count: summary.message_count,
            latest_message_id: summary.latest_message_id.clone(),
            last_message_updated_at_unix_ms: summary.last_message_updated_at_unix_ms,
            last_attachment_updated_at_unix_ms: summary.last_attachment_updated_at_unix_ms,
            last_attachment_message_id: summary.last_attachment_message_id.clone(),
            last_security_projection_updated_at_unix_ms: summary
                .last_security_projection_updated_at_unix_ms,
            last_security_projection_message_id: summary
                .last_security_projection_message_id
                .clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogDeltaSnapshot {
    pub dialog_key: String,
    pub changed_messages: Vec<omni_types::ChatMessageRecord>,
    pub changed_security_details: Vec<MessageSecurityDetails>,
    pub cursor: DialogDeltaCursor,
    pub requires_full_reload: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingWireEnvelope {
    pub envelope: WireEnvelopeV1,
    pub updated_session: SessionRecord,
    pub payload_kind: SecurityPayloadKind,
}

fn encrypted_payload_to_wire(payload: EncryptedPayload) -> EncryptedPayloadV1 {
    EncryptedPayloadV1 {
        suite_version: omni_crypto::CRYPTO_SUITE_VERSION_CURRENT,
        counter: payload.counter,
        ratchet_public_key_hex: payload.ratchet_public_key_hex,
        previous_chain_length: payload.previous_chain_length,
        nonce_hex: payload.nonce_hex,
        ciphertext_hex: payload.ciphertext_hex,
        mac_hex: payload.mac_hex,
    }
}

fn encrypted_payload_from_wire(payload: &EncryptedPayloadV1) -> EncryptedPayload {
    EncryptedPayload {
        counter: payload.counter,
        ratchet_public_key_hex: payload.ratchet_public_key_hex.clone(),
        previous_chain_length: payload.previous_chain_length,
        nonce_hex: payload.nonce_hex.clone(),
        ciphertext_hex: payload.ciphertext_hex.clone(),
        mac_hex: payload.mac_hex.clone(),
    }
}

fn payload_is_encryptable(payload: &WirePayloadV1) -> bool {
    !matches!(
        payload,
        WirePayloadV1::SessionHello(_) | WirePayloadV1::SessionBootstrap(_)
    )
}

pub fn build_encrypted_wire_envelope(
    session: &SessionRecord,
    payload: &WirePayloadV1,
    payload_kind: SecurityPayloadKind,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    if !payload_is_encryptable(payload) {
        return Err(OmniError::InvalidInput(
            "session bootstrap and session hello must not be nested into encrypted payloads"
                .to_string(),
        ));
    }

    let mut working_session = session.clone();
    working_session.updated_at_unix_ms = now_unix_ms;
    let plaintext = serde_json::to_vec(payload).map_err(|error| {
        OmniError::Internal(format!(
            "failed to serialize encrypted wire payload: {error}"
        ))
    })?;
    let encrypted = encrypt_application_payload(&mut working_session, &plaintext)?;
    let envelope = WireEnvelopeV1::encrypted_payload(encrypted_payload_to_wire(encrypted));
    validate_wire_envelope_v1(&envelope)?;
    Ok(OutgoingWireEnvelope {
        envelope,
        updated_session: working_session,
        payload_kind,
    })
}

pub fn build_encrypted_logical_event_envelope(
    session: &SessionRecord,
    event: &LogicalEventEnvelopeV1,
    payload_kind: SecurityPayloadKind,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    build_encrypted_wire_envelope(
        session,
        &WirePayloadV1::LogicalEvent(event.clone()),
        payload_kind,
        now_unix_ms,
    )
}

/// What an authenticated encrypted payload turned out to hold.
#[derive(Debug)]
pub enum DecryptedWirePayload {
    Payload(WirePayloadV1),
    /// AUTHENTIC, BUT NOT SOMETHING THIS BUILD UNDERSTANDS — a peer on a newer (or older) build
    /// sent an event kind we do not have. It was decrypted, so the ratchet has advanced, and the
    /// caller MUST store the session and replay window anyway, then drop the payload.
    ///
    /// Used to be an error, which left the session unstored: the next understood message then
    /// found the stored receive counter behind by one more. 64 unknown events in a row (a peer on
    /// another build sends a media report every second — about a minute of a call) and the next
    /// ordinary message hit `MAX_RECEIVE_GAP` ("counter exceeded") and the session went into a
    /// resync. Mixed builds on the two test machines are the normal state, not an edge case.
    Unrecognized(String),
}

pub fn decrypt_wire_payload(
    session: &mut SessionRecord,
    replay_window: &mut ReplayWindowState,
    payload: &EncryptedPayloadV1,
) -> OmniResult<DecryptedWirePayload> {
    let plaintext = decrypt_application_payload(
        session,
        replay_window,
        &encrypted_payload_from_wire(payload),
    )?;
    let payload: WirePayloadV1 = match serde_json::from_slice(&plaintext) {
        Ok(payload) => payload,
        Err(error) => return Ok(DecryptedWirePayload::Unrecognized(error.to_string())),
    };
    if !payload_is_encryptable(&payload) {
        return Err(OmniError::InvalidInput(
            "encrypted payload contained a forbidden wire payload kind".to_string(),
        ));
    }
    Ok(DecryptedWirePayload::Payload(payload))
}

pub fn warning_from_trust(
    trust_state: Option<TrustState>,
    is_blocked: bool,
    has_established_session: bool,
    has_remote_prekey_bundle: bool,
    has_contact: bool,
) -> SecurityWarningKind {
    if is_blocked {
        return SecurityWarningKind::BlockedContact;
    }
    if !has_contact {
        return SecurityWarningKind::UnlinkedDialog;
    }
    match trust_state {
        Some(TrustState::IdentityChanged) => SecurityWarningKind::IdentityChanged,
        Some(TrustState::ReverifyNeeded) => SecurityWarningKind::ReverifyNeeded,
        Some(TrustState::Unverified) => SecurityWarningKind::Unverified,
        Some(TrustState::Verified) => {
            if has_established_session {
                SecurityWarningKind::None
            } else if has_remote_prekey_bundle {
                SecurityWarningKind::MissingSession
            } else {
                SecurityWarningKind::MissingRemoteBundle
            }
        }
        None => {
            if has_established_session {
                SecurityWarningKind::None
            } else if has_remote_prekey_bundle {
                SecurityWarningKind::MissingSession
            } else {
                SecurityWarningKind::MissingRemoteBundle
            }
        }
    }
}

pub fn wrap_session_bootstrap(bootstrap: SessionBootstrapMessageV1) -> OmniResult<WireEnvelopeV1> {
    let envelope = WireEnvelopeV1::session_bootstrap(bootstrap);
    validate_wire_envelope_v1(&envelope)?;
    Ok(envelope)
}

pub fn build_text_envelope(
    session: &SessionRecord,
    message_id: &str,
    body: &str,
    created_at_unix_ms: i64,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    let mut working_session = session.clone();
    working_session.updated_at_unix_ms = now_unix_ms;
    let envelope = WireEnvelopeV1::chat_text(message_id, body, created_at_unix_ms);
    validate_wire_envelope_v1(&envelope)?;
    Ok(OutgoingWireEnvelope {
        envelope,
        updated_session: working_session,
        payload_kind: SecurityPayloadKind::Text,
    })
}

pub fn build_encrypted_text_envelope(
    session: &SessionRecord,
    message_id: &str,
    body: &str,
    created_at_unix_ms: i64,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    build_encrypted_wire_envelope(
        session,
        &WirePayloadV1::ChatText(omni_types::ChatTextV1 {
            message_id: message_id.to_string(),
            body: body.to_string(),
            created_at_unix_ms,
        }),
        SecurityPayloadKind::Text,
        now_unix_ms,
    )
}

pub fn build_encrypted_attachment_offer_envelope(
    session: &SessionRecord,
    offer: AttachmentOfferV1,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    build_encrypted_wire_envelope(
        session,
        &WirePayloadV1::LogicalEvent(LogicalEventEnvelopeV1::attachment_offer(offer)),
        SecurityPayloadKind::AttachmentOffer,
        now_unix_ms,
    )
}

pub fn build_delivery_receipt_envelope(
    session: &SessionRecord,
    receipt: &DeliveryReceiptV1,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    let mut working_session = session.clone();
    working_session.updated_at_unix_ms = now_unix_ms;
    let envelope = WireEnvelopeV1::logical_event(LogicalEventEnvelopeV1::delivery_receipt(
        receipt.message_id.clone(),
        receipt.delivered_at_unix_ms,
    ));
    validate_wire_envelope_v1(&envelope)?;
    Ok(OutgoingWireEnvelope {
        envelope,
        updated_session: working_session,
        payload_kind: SecurityPayloadKind::DeliveryReceipt,
    })
}

pub fn build_encrypted_delivery_receipt_envelope(
    session: &SessionRecord,
    receipt: &DeliveryReceiptV1,
    now_unix_ms: i64,
) -> OmniResult<OutgoingWireEnvelope> {
    build_encrypted_wire_envelope(
        session,
        &WirePayloadV1::LogicalEvent(LogicalEventEnvelopeV1::delivery_receipt(
            receipt.message_id.clone(),
            receipt.delivered_at_unix_ms,
        )),
        SecurityPayloadKind::DeliveryReceipt,
        now_unix_ms,
    )
}

#[cfg(test)]
mod tests {
    use omni_crypto::{
        bootstrap_initiator_session, build_prekey_bundle, device_public_key_to_wire,
        ephemeral_public_key_to_wire, generate_local_key_hierarchy, identity_public_key_to_wire,
        identity_signing_public_key_to_wire, signature_to_wire,
    };
    use omni_identity::{AccountId, DeviceId};
    use omni_protocol::{SessionBootstrapMessageV1, SESSION_BOOTSTRAP_MESSAGE_VERSION_V1};
    use omni_types::{LogicalEventPayloadV1, WirePayloadV1};

    use super::{
        build_delivery_receipt_envelope, build_text_envelope, decrypt_wire_payload,
        encrypted_payload_to_wire, wrap_session_bootstrap, DecryptedWirePayload,
        MessageEncryptionState, SecurityPayloadKind, SecurityWarningKind,
    };

    fn account_id(value: &str) -> AccountId {
        AccountId(value.to_string())
    }

    fn device_id(value: &str) -> DeviceId {
        DeviceId(value.to_string())
    }

    fn bootstrap_message_from_crypto(
        bootstrap: &omni_crypto::SessionBootstrapMessage,
    ) -> SessionBootstrapMessageV1 {
        SessionBootstrapMessageV1 {
            version: SESSION_BOOTSTRAP_MESSAGE_VERSION_V1,
            initiator_account_id: bootstrap.initiator_account_id.0.clone(),
            initiator_device_id: bootstrap.initiator_device_id.0.clone(),
            initiator_identity_public_key: identity_public_key_to_wire(
                bootstrap.initiator_identity_key,
            ),
            initiator_identity_signing_public_key: identity_signing_public_key_to_wire(
                bootstrap.initiator_identity_signing_key,
            ),
            initiator_device_public_key: device_public_key_to_wire(bootstrap.initiator_device_key),
            initiator_ephemeral_public_key: ephemeral_public_key_to_wire(
                bootstrap.initiator_ephemeral_key,
            ),
            remote_signed_prekey_id: bootstrap.remote_signed_prekey_id,
            remote_one_time_prekey_id: bootstrap.remote_one_time_prekey_id,
            bootstrap_signature_r: signature_to_wire(bootstrap.bootstrap_signature.clone()).0,
            bootstrap_signature_s: signature_to_wire(bootstrap.bootstrap_signature.clone()).1,
            created_at_unix_ms: bootstrap.created_at_unix_ms,
            initiator_instance_id: String::new(),
            initiator_device_certificate_chain_json: None,
        }
    }

    /// An established alice -> bob session pair and bob's replay window.
    fn established_pair() -> (
        omni_crypto::SessionRecord,
        omni_crypto::SessionRecord,
        omni_crypto::ReplayWindowState,
    ) {
        let alice =
            generate_local_key_hierarchy(&account_id("acct-alice"), &device_id("dev-a"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-bob"), &device_id("dev-b"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        let (alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let bob_session = omni_crypto::bootstrap_responder_session(
            &bob.identity_key.account_id,
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            &bootstrap,
            301,
        )
        .unwrap();
        let alice_session = omni_crypto::establish_session(&alice_session, 302);
        let bob_session = omni_crypto::establish_session(&bob_session, 302);
        let bob_replay =
            omni_crypto::new_replay_window_state(bob_session.session_address.clone(), 302);
        (alice_session, bob_session, bob_replay)
    }

    /// An authentic payload whose plaintext no build of ours parses: what a peer on a build with
    /// an event kind we do not have looks like from here.
    fn unknown_kind_payload(alice: &mut omni_crypto::SessionRecord) -> omni_types::EncryptedPayloadV1 {
        let plaintext =
            br#"{"kind":"logical_event","payload":{"logical_version":1,"payload":{"event_type":"from_a_future_build"}}}"#;
        encrypted_payload_to_wire(omni_crypto::encrypt_application_payload(alice, plaintext).unwrap())
    }

    #[test]
    fn unknown_event_kinds_from_another_build_do_not_wedge_the_session() {
        let (mut alice, mut bob, mut bob_replay) = established_pair();

        // A minute of a call with a peer on another build: more unknown events in a row than the
        // receive gap allows. Each is authentic, so each must advance bob's state.
        for _ in 0..(omni_crypto::MAX_RECEIVE_GAP + 6) {
            let payload = unknown_kind_payload(&mut alice);
            match decrypt_wire_payload(&mut bob, &mut bob_replay, &payload).unwrap() {
                DecryptedWirePayload::Unrecognized(_) => {}
                other => panic!("an unknown kind must come back unrecognized: {other:?}"),
            }
        }

        // ...and the next ordinary message still opens.
        let text = super::build_encrypted_logical_event_envelope(
            &alice,
            &omni_types::LogicalEventEnvelopeV1::chat_text("msg-after", "still here", 500),
            SecurityPayloadKind::Text,
            501,
        )
        .unwrap();
        let WirePayloadV1::EncryptedPayload(payload) = &text.envelope.payload else {
            panic!("a logical event envelope must be encrypted");
        };
        match decrypt_wire_payload(&mut bob, &mut bob_replay, payload).unwrap() {
            DecryptedWirePayload::Payload(WirePayloadV1::LogicalEvent(event)) => {
                assert!(matches!(event.payload, LogicalEventPayloadV1::ChatText(ref chat) if chat.body == "still here"));
            }
            other => panic!("the ordinary message must open after unknown ones: {other:?}"),
        }
    }

    /// The other half: what happened before, when an unknown kind was an error and the caller
    /// therefore did not store the session. Emulated by decrypting into a throwaway copy.
    #[test]
    fn not_storing_the_session_after_unknown_kinds_wedges_it() {
        let (mut alice, bob_stored, bob_replay_stored) = established_pair();
        for _ in 0..(omni_crypto::MAX_RECEIVE_GAP + 6) {
            let payload = unknown_kind_payload(&mut alice);
            let mut scratch = bob_stored.clone();
            let mut scratch_replay = bob_replay_stored.clone();
            let _ = decrypt_wire_payload(&mut scratch, &mut scratch_replay, &payload);
        }
        let text = super::build_encrypted_logical_event_envelope(
            &alice,
            &omni_types::LogicalEventEnvelopeV1::chat_text("msg-after", "still here", 500),
            SecurityPayloadKind::Text,
            501,
        )
        .unwrap();
        let WirePayloadV1::EncryptedPayload(payload) = &text.envelope.payload else {
            panic!("a logical event envelope must be encrypted");
        };
        let mut bob = bob_stored;
        let mut bob_replay = bob_replay_stored;
        let error = decrypt_wire_payload(&mut bob, &mut bob_replay, payload).unwrap_err();
        assert!(error.to_string().contains("counter exceeded"), "{error}");
    }

    #[test]
    fn a_forged_payload_is_still_rejected_and_does_not_advance_the_session() {
        let (mut alice, mut bob, mut bob_replay) = established_pair();
        let mut payload = unknown_kind_payload(&mut alice);
        // Flip one ciphertext byte: no longer authentic.
        let flipped = if payload.ciphertext_hex.starts_with('0') { "1" } else { "0" };
        payload.ciphertext_hex.replace_range(0..1, flipped);
        let counter_before = bob.receive_counter;

        assert!(decrypt_wire_payload(&mut bob, &mut bob_replay, &payload).is_err());
        assert_eq!(bob.receive_counter, counter_before);
    }

    #[test]
    fn plain_wire_envelopes_keep_payloads_visible() {
        let alice =
            generate_local_key_hierarchy(&account_id("acct-alice"), &device_id("dev-a"), 100, 1)
                .unwrap();
        let bob =
            generate_local_key_hierarchy(&account_id("acct-bob"), &device_id("dev-b"), 200, 1)
                .unwrap();
        let bob_bundle = build_prekey_bundle(
            &bob.identity_key,
            &bob.device_key,
            &bob.signed_prekey,
            bob.one_time_prekeys.first(),
            200,
        )
        .unwrap();
        let (alice_session, bootstrap) =
            bootstrap_initiator_session(&alice.identity_key, &alice.device_key, &bob_bundle, 300)
                .unwrap();
        let bootstrap_envelope =
            wrap_session_bootstrap(bootstrap_message_from_crypto(&bootstrap)).unwrap();
        match &bootstrap_envelope.payload {
            WirePayloadV1::SessionBootstrap(message) => {
                assert_eq!(message.initiator_account_id, "acct-alice");
                assert_eq!(message.initiator_device_id, "dev-a");
            }
            other => panic!("unexpected bootstrap payload: {other:?}"),
        }
        let bootstrap_json = serde_json::to_string(&bootstrap_envelope).unwrap();
        assert!(bootstrap_json.contains("acct-alice"));
        assert!(bootstrap_json.contains("initiator_account_id"));

        let outgoing =
            build_text_envelope(&alice_session, "msg-1", "hello plaintext", 400, 401).unwrap();
        assert_eq!(outgoing.payload_kind, SecurityPayloadKind::Text);
        match &outgoing.envelope.payload {
            WirePayloadV1::ChatText(message) => {
                assert_eq!(message.message_id, "msg-1");
                assert_eq!(message.body, "hello plaintext");
                assert_eq!(message.created_at_unix_ms, 400);
            }
            other => panic!("unexpected text payload: {other:?}"),
        }
        assert_eq!(outgoing.updated_session.updated_at_unix_ms, 401);
        let text_json = serde_json::to_string(&outgoing.envelope).unwrap();
        assert!(text_json.contains("hello plaintext"));
        assert!(text_json.contains("msg-1"));

        let receipt = build_delivery_receipt_envelope(
            &outgoing.updated_session,
            &omni_types::DeliveryReceiptV1 {
                message_id: "msg-1".to_string(),
                delivered_at_unix_ms: 500,
            },
            500,
        )
        .unwrap();
        assert_eq!(receipt.payload_kind, SecurityPayloadKind::DeliveryReceipt);
        match &receipt.envelope.payload {
            WirePayloadV1::LogicalEvent(event) => match &event.payload {
                LogicalEventPayloadV1::DeliveryReceipt(delivery_receipt) => {
                    assert_eq!(delivery_receipt.message_id, "msg-1");
                    assert_eq!(delivery_receipt.delivered_at_unix_ms, 500);
                }
                other => panic!("unexpected logical event payload: {other:?}"),
            },
            other => panic!("unexpected receipt payload: {other:?}"),
        }
        let receipt_json = serde_json::to_string(&receipt.envelope).unwrap();
        assert!(receipt_json.contains("msg-1"));
        assert!(receipt_json.contains("delivered_at_unix_ms"));

        assert_eq!(
            MessageEncryptionState::from_db_value(
                MessageEncryptionState::LegacyPlaintext.as_db_value()
            )
            .unwrap(),
            MessageEncryptionState::LegacyPlaintext
        );
        assert_eq!(
            super::warning_from_trust(
                Some(omni_identity::TrustState::Unverified),
                false,
                false,
                false,
                true
            ),
            SecurityWarningKind::Unverified
        );
    }
}
