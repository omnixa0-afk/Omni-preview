use base64::Engine as _;
use omni_types::{
    ChatTextV1, ContactPresencePingV1, ContactPresencePongV1, ContactPresenceV1, DeliveryReceiptV1,
    EncryptedPayloadV1, LogicalEventEnvelopeV1, OmniError, OmniResult, PauseAutoReconnectV1,
    PublicProfileAvatarSyncV1, PublicProfileDescriptorSyncV1, RelaySetUpdateV1, RouteHintsSyncV1,
    SessionHelloV1, SignalingRouteCandidateV1, VerificationChallengeResponseV1,
    VerificationChallengeV1, WireEnvelopeV1, WirePayloadV1, OMNI_LOGICAL_EVENT_VERSION_V1,
    OMNI_PROTOCOL_VERSION_V1, RELAY_SET_UPDATE_VERSION_V1, ROUTE_HINTS_SYNC_VERSION_V1,
    SIGNALING_ROUTE_CANDIDATE_VERSION_V1,
};

use crate::validation_attachments::{
    validate_attachment_access_revoked_v1,
    validate_attachment_download_request_v1, validate_attachment_downloaded_v1,
    validate_attachment_offer_v1,
    validate_attachment_transfer_failed_v1,
};
use crate::validation_calls::{
    validate_call_accept_v1, validate_call_busy_v1, validate_call_cancel_v1,
    validate_call_decline_v1, validate_call_hangup_v1, validate_call_invite_v1,
    validate_call_reconnect_v1, validate_call_ringing_v1, validate_call_screen_control_v1, validate_call_timeout_v1,
    validate_realtime_media_capabilities_v1, validate_realtime_media_control_v1,
    validate_realtime_media_selection_v1,
};
use crate::validation_identity::{
    validate_ascii_hexdigits_only, validate_hex_decoded_len, validate_hex_field,
    validate_max_utf8_len, validate_my_verification_payload_v1, validate_non_empty_field,
    validate_positive_i64, validate_prekey_bundle_v1, validate_serialized_json_len,
};
use crate::{
    SessionBootstrapMessageV1, CHAT_BODY_MAX_BYTES, CHAT_MESSAGE_ID_MAX_BYTES,
    CONTROL_PLANE_WIRE_ENVELOPE_MAX_BYTES, ENCRYPTED_PAYLOAD_CIPHERTEXT_MAX_BYTES,
    PUBLIC_PROFILE_AVATAR_MAX_BYTES, PUBLIC_PROFILE_AVATAR_MIME_TYPE_JPEG,
    PUBLIC_PROFILE_AVATAR_SYNC_VERSION_V1, PUBLIC_PROFILE_DESCRIPTOR_SYNC_VERSION_V1,
    SESSION_BOOTSTRAP_MESSAGE_VERSION_V1, SESSION_HELLO_PREKEY_BUNDLE_JSON_MAX_BYTES,
    VERIFICATION_PAYLOAD_JSON_MAX_BYTES,
};

pub fn validate_session_hello_v1(hello: &SessionHelloV1) -> OmniResult<()> {
    if hello.instance_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "session hello instance_id must not be empty".to_string(),
        ));
    }
    if hello.device_name.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "session hello device_name must not be empty".to_string(),
        ));
    }
    if let Some(account_id) = &hello.account_id {
        validate_non_empty_field("session hello account_id", account_id)?;
    }
    if let Some(prekey_bundle_json) = &hello.prekey_bundle_json {
        validate_max_utf8_len(
            "session hello prekey_bundle_json",
            prekey_bundle_json,
            SESSION_HELLO_PREKEY_BUNDLE_JSON_MAX_BYTES,
        )?;
        let bundle = serde_json::from_str(prekey_bundle_json).map_err(|error| {
            OmniError::InvalidInput(format!(
                "failed to parse session hello prekey bundle: {error}"
            ))
        })?;
        validate_prekey_bundle_v1(&bundle)?;
    }
    Ok(())
}

pub fn validate_verification_challenge_v1(challenge: &VerificationChallengeV1) -> OmniResult<()> {
    if challenge.version != OMNI_PROTOCOL_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported verification challenge version '{}'",
            challenge.version
        )));
    }
    validate_non_empty_field(
        "verification challenge challenge_id",
        &challenge.challenge_id,
    )?;
    validate_non_empty_field("verification challenge nonce_hex", &challenge.nonce_hex)?;
    validate_hex_field("verification challenge nonce_hex", &challenge.nonce_hex)?;
    if challenge.expires_at_unix_ms <= challenge.issued_at_unix_ms {
        return Err(OmniError::InvalidInput(
            "verification challenge expires_at_unix_ms must be after issued_at_unix_ms".to_string(),
        ));
    }
    if let Some(remote_instance_id) = &challenge.remote_instance_id {
        validate_non_empty_field(
            "verification challenge remote_instance_id",
            remote_instance_id,
        )?;
    }
    if let Some(remote_account_id) = &challenge.remote_account_id {
        validate_non_empty_field(
            "verification challenge remote_account_id",
            remote_account_id,
        )?;
    }
    Ok(())
}

pub fn validate_verification_challenge_response_v1(
    response: &VerificationChallengeResponseV1,
) -> OmniResult<()> {
    if response.version != OMNI_PROTOCOL_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported verification challenge response version '{}'",
            response.version
        )));
    }
    validate_non_empty_field(
        "verification challenge response challenge_id",
        &response.challenge_id,
    )?;
    validate_non_empty_field(
        "verification challenge response verification_payload_json",
        &response.verification_payload_json,
    )?;
    validate_max_utf8_len(
        "verification challenge response verification_payload_json",
        &response.verification_payload_json,
        VERIFICATION_PAYLOAD_JSON_MAX_BYTES,
    )?;
    validate_hex_field(
        "verification challenge response signature_r",
        &response.signature_r,
    )?;
    validate_hex_field(
        "verification challenge response signature_s",
        &response.signature_s,
    )?;
    let payload = serde_json::from_str(&response.verification_payload_json).map_err(|error| {
        OmniError::InvalidInput(format!(
            "failed to parse verification challenge response payload: {error}"
        ))
    })?;
    validate_my_verification_payload_v1(&payload)?;
    Ok(())
}

pub fn validate_session_bootstrap_message_v1(
    message: &SessionBootstrapMessageV1,
) -> OmniResult<()> {
    if message.version != SESSION_BOOTSTRAP_MESSAGE_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported session bootstrap message version '{}'",
            message.version
        )));
    }
    if message.initiator_account_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "session bootstrap initiator_account_id must not be empty".to_string(),
        ));
    }
    if message.initiator_device_id.trim().is_empty() {
        return Err(OmniError::InvalidInput(
            "session bootstrap initiator_device_id must not be empty".to_string(),
        ));
    }
    validate_hex_field(
        "session bootstrap initiator_identity_public_key",
        &message.initiator_identity_public_key,
    )?;
    validate_hex_field(
        "session bootstrap initiator_identity_signing_public_key",
        &message.initiator_identity_signing_public_key,
    )?;
    validate_hex_field(
        "session bootstrap initiator_device_public_key",
        &message.initiator_device_public_key,
    )?;
    validate_hex_field(
        "session bootstrap initiator_ephemeral_public_key",
        &message.initiator_ephemeral_public_key,
    )?;
    validate_hex_field(
        "session bootstrap bootstrap_signature_r",
        &message.bootstrap_signature_r,
    )?;
    validate_hex_field(
        "session bootstrap bootstrap_signature_s",
        &message.bootstrap_signature_s,
    )?;
    Ok(())
}

pub fn validate_chat_text_v1(message: &ChatTextV1) -> OmniResult<()> {
    validate_non_empty_field("chat text message_id", &message.message_id)?;
    validate_non_empty_field("chat text body", &message.body)?;
    validate_max_utf8_len(
        "chat text message_id",
        &message.message_id,
        CHAT_MESSAGE_ID_MAX_BYTES,
    )?;
    validate_max_utf8_len("chat text body", &message.body, CHAT_BODY_MAX_BYTES)?;
    Ok(())
}

pub fn validate_encrypted_payload_v1(payload: &EncryptedPayloadV1) -> OmniResult<()> {
    if payload.suite_version == 0 {
        return Err(OmniError::InvalidInput(
            "encrypted payload suite_version must be positive".to_string(),
        ));
    }
    validate_non_empty_field(
        "encrypted payload ratchet_public_key_hex",
        &payload.ratchet_public_key_hex,
    )?;
    validate_non_empty_field("encrypted payload nonce_hex", &payload.nonce_hex)?;
    validate_non_empty_field("encrypted payload ciphertext_hex", &payload.ciphertext_hex)?;
    validate_non_empty_field("encrypted payload mac_hex", &payload.mac_hex)?;
    validate_hex_decoded_len(
        "encrypted payload ciphertext_hex",
        &payload.ciphertext_hex,
        ENCRYPTED_PAYLOAD_CIPHERTEXT_MAX_BYTES,
    )?;
    if payload.nonce_hex.trim().len() != 24 {
        return Err(OmniError::InvalidInput(
            "encrypted payload nonce_hex must encode 12 bytes".to_string(),
        ));
    }
    if payload.mac_hex.trim().len() != 32 {
        return Err(OmniError::InvalidInput(
            "encrypted payload mac_hex must encode 16 bytes".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_delivery_receipt_v1(receipt: &DeliveryReceiptV1) -> OmniResult<()> {
    validate_non_empty_field("delivery receipt message_id", &receipt.message_id)?;
    Ok(())
}

pub fn validate_contact_presence_v1(presence: &ContactPresenceV1) -> OmniResult<()> {
    validate_positive_i64(
        "contact presence observed_at_unix_ms",
        presence.observed_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_contact_presence_ping_v1(ping: &ContactPresencePingV1) -> OmniResult<()> {
    validate_non_empty_field("contact presence ping ping_id", &ping.ping_id)?;
    validate_positive_i64(
        "contact presence ping sent_at_unix_ms",
        ping.sent_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_contact_presence_pong_v1(pong: &ContactPresencePongV1) -> OmniResult<()> {
    validate_non_empty_field("contact presence pong ping_id", &pong.ping_id)?;
    validate_positive_i64(
        "contact presence pong responded_at_unix_ms",
        pong.responded_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_pause_auto_reconnect_v1(pause: &PauseAutoReconnectV1) -> OmniResult<()> {
    validate_positive_i64(
        "pause auto reconnect paused_at_unix_ms",
        pause.paused_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_public_profile_descriptor_sync_v1(
    profile: &PublicProfileDescriptorSyncV1,
) -> OmniResult<()> {
    if profile.version != PUBLIC_PROFILE_DESCRIPTOR_SYNC_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported public profile descriptor sync version '{}'",
            profile.version
        )));
    }
    validate_non_empty_field("public profile display_name", &profile.display_name)?;
    validate_positive_i64(
        "public profile descriptor updated_at_unix_ms",
        profile.updated_at_unix_ms,
    )?;
    match (
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
    ) {
        (None, None) => Ok(()),
        (Some(hash_hex), Some(mime_type)) => {
            validate_non_empty_field("public profile avatar_hash_hex", hash_hex)?;
            validate_ascii_hexdigits_only("public profile descriptor avatar_hash_hex", hash_hex)?;
            if mime_type != PUBLIC_PROFILE_AVATAR_MIME_TYPE_JPEG {
                return Err(OmniError::InvalidInput(format!(
                    "unsupported public profile descriptor avatar mime type '{}'",
                    mime_type
                )));
            }
            Ok(())
        }
        _ => Err(OmniError::InvalidInput(
            "public profile descriptor avatar metadata must be fully specified or omitted"
                .to_string(),
        )),
    }
}

pub fn validate_public_profile_avatar_sync_v1(
    profile: &PublicProfileAvatarSyncV1,
) -> OmniResult<()> {
    if profile.version != PUBLIC_PROFILE_AVATAR_SYNC_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported public profile avatar sync version '{}'",
            profile.version
        )));
    }
    validate_positive_i64(
        "public profile avatar updated_at_unix_ms",
        profile.updated_at_unix_ms,
    )?;
    match (
        profile.avatar_hash_hex.as_deref(),
        profile.avatar_mime_type.as_deref(),
        profile.avatar_bytes_base64.as_deref(),
    ) {
        (None, None, None) => Ok(()),
        (Some(hash_hex), Some(mime_type), Some(bytes_base64)) => {
            validate_non_empty_field("public profile avatar avatar_hash_hex", hash_hex)?;
            validate_ascii_hexdigits_only("public profile avatar avatar_hash_hex", hash_hex)?;
            if mime_type != PUBLIC_PROFILE_AVATAR_MIME_TYPE_JPEG {
                return Err(OmniError::InvalidInput(format!(
                    "unsupported public profile avatar mime type '{}'",
                    mime_type
                )));
            }
            validate_non_empty_field("public profile avatar avatar_bytes_base64", bytes_base64)?;
            let decoded = crate::BASE64_STANDARD
                .decode(bytes_base64)
                .map_err(|error| {
                    OmniError::InvalidInput(format!(
                        "public profile avatar avatar_bytes_base64 is invalid: {error}"
                    ))
                })?;
            if decoded.len() > PUBLIC_PROFILE_AVATAR_MAX_BYTES {
                return Err(OmniError::InvalidInput(format!(
                    "public profile avatar exceeds the {} byte limit",
                    PUBLIC_PROFILE_AVATAR_MAX_BYTES
                )));
            }
            Ok(())
        }
        _ => Err(OmniError::InvalidInput(
            "public profile avatar metadata must be fully specified or omitted".to_string(),
        )),
    }
}

pub fn validate_route_hints_sync_v1(sync: &RouteHintsSyncV1) -> OmniResult<()> {
    if sync.version != ROUTE_HINTS_SYNC_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported route hints sync version '{}'",
            sync.version
        )));
    }
    validate_non_empty_field(
        "route hints sync source_instance_id",
        &sync.source_instance_id,
    )?;
    if sync.exported_at_unix_ms <= 0 || sync.expires_at_unix_ms <= sync.exported_at_unix_ms {
        return Err(OmniError::InvalidInput(
            "route hints sync has invalid lifetime".to_string(),
        ));
    }
    if sync.candidates.len() > 16 {
        return Err(OmniError::InvalidInput(
            "route hints sync contains too many candidates".to_string(),
        ));
    }
    for candidate in &sync.candidates {
        validate_route_hint_candidate_v1(candidate, sync.exported_at_unix_ms)?;
    }
    Ok(())
}

/// R1c relay-set propagation envelope. Light validation only — the `bundle_token` is a SIGNED
/// `om3relay:` whose issuer signature + bundle version + relay set are verified on consume by
/// `apply_relay_bundle`/`verify_relay_bundle`. Here we just reject the malformed envelope.
pub fn validate_relay_set_update_v1(update: &RelaySetUpdateV1) -> OmniResult<()> {
    if update.version != RELAY_SET_UPDATE_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported relay set update version '{}'",
            update.version
        )));
    }
    validate_non_empty_field("relay set update bundle_token", &update.bundle_token)?;
    if !update.bundle_token.trim_start().starts_with("om3relay:") {
        return Err(OmniError::InvalidInput(
            "relay set update bundle_token is not an om3relay: token".to_string(),
        ));
    }
    if update.exported_at_unix_ms <= 0 {
        return Err(OmniError::InvalidInput(
            "relay set update has invalid exported_at".to_string(),
        ));
    }
    Ok(())
}

fn validate_route_hint_candidate_v1(
    candidate: &SignalingRouteCandidateV1,
    exported_at_unix_ms: i64,
) -> OmniResult<()> {
    if candidate.version != SIGNALING_ROUTE_CANDIDATE_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported route hint candidate version '{}'",
            candidate.version
        )));
    }
    validate_non_empty_field("route hint candidate dial_addr", &candidate.dial_addr)?;
    validate_max_utf8_len("route hint candidate dial_addr", &candidate.dial_addr, 256)?;
    candidate
        .dial_addr
        .parse::<std::net::SocketAddr>()
        .map_err(|error| {
            OmniError::InvalidInput(format!(
                "route hint candidate dial_addr must be ip:port: {error}"
            ))
        })?;
    validate_non_empty_field("route hint candidate source", &candidate.source)?;
    validate_max_utf8_len("route hint candidate source", &candidate.source, 48)?;
    if candidate.expires_at_unix_ms <= exported_at_unix_ms {
        return Err(OmniError::InvalidInput(
            "route hint candidate has invalid lifetime".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_logical_event_envelope_v1(event: &LogicalEventEnvelopeV1) -> OmniResult<()> {
    if event.logical_version != OMNI_LOGICAL_EVENT_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported logical event envelope version '{}'",
            event.logical_version
        )));
    }
    match &event.payload {
        omni_types::LogicalEventPayloadV1::ChatText(message) => validate_chat_text_v1(message),
        omni_types::LogicalEventPayloadV1::DeliveryReceipt(receipt) => {
            validate_delivery_receipt_v1(receipt)
        }
        omni_types::LogicalEventPayloadV1::AttachmentOffer(offer) => {
            validate_attachment_offer_v1(offer)
        }
        omni_types::LogicalEventPayloadV1::AttachmentDownloadRequest(request) => {
            validate_attachment_download_request_v1(request)
        }
        omni_types::LogicalEventPayloadV1::AttachmentTransferFailed(failure) => {
            validate_attachment_transfer_failed_v1(failure)
        }
        omni_types::LogicalEventPayloadV1::AttachmentDownloaded(downloaded) => {
            validate_attachment_downloaded_v1(downloaded)
        }
        omni_types::LogicalEventPayloadV1::AttachmentAccessRevoked(revoked) => {
            validate_attachment_access_revoked_v1(revoked)
        }
        omni_types::LogicalEventPayloadV1::CallInvite(invite) => validate_call_invite_v1(invite),
        omni_types::LogicalEventPayloadV1::CallRinging(ringing) => {
            validate_call_ringing_v1(ringing)
        }
        omni_types::LogicalEventPayloadV1::CallAccept(accept) => validate_call_accept_v1(accept),
        omni_types::LogicalEventPayloadV1::CallDecline(decline) => {
            validate_call_decline_v1(decline)
        }
        omni_types::LogicalEventPayloadV1::CallBusy(busy) => validate_call_busy_v1(busy),
        omni_types::LogicalEventPayloadV1::CallCancel(cancel) => validate_call_cancel_v1(cancel),
        omni_types::LogicalEventPayloadV1::CallHangup(hangup) => validate_call_hangup_v1(hangup),
        omni_types::LogicalEventPayloadV1::CallTimeout(timeout) => {
            validate_call_timeout_v1(timeout)
        }
        omni_types::LogicalEventPayloadV1::CallReconnect(reconnect) => {
            validate_call_reconnect_v1(reconnect)
        }
        omni_types::LogicalEventPayloadV1::CallScreenControl(control) => {
            validate_call_screen_control_v1(control)
        }
        omni_types::LogicalEventPayloadV1::AccountKeyMove(sync) => {
            validate_non_empty_field("account key move", &sync.move_json)?;
            // A working key certificate and a certificate chain per kept device: a few KB. The cap only
            // stops a flood.
            validate_max_utf8_len("account key move", &sync.move_json, 256 * 1024)?;
            if let Some(seed) = &sync.working_seed_hex {
                if seed.len() != 64 || !seed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(OmniError::InvalidInput(
                        "account key move working key seed must be 32 bytes of hex".to_string(),
                    ));
                }
            }
            Ok(())
        }
        omni_types::LogicalEventPayloadV1::ContactSync(sync) => {
            // A handful of contacts per frame (the sender batches); the cap only stops a flood.
            if sync.entries.is_empty() || sync.entries.len() > 64 {
                return Err(OmniError::InvalidInput(
                    "contact sync must carry between 1 and 64 entries".to_string(),
                ));
            }
            for entry in &sync.entries {
                validate_non_empty_field("contact sync account_id", &entry.account_id)?;
                validate_non_empty_field("contact sync state_writer", &entry.state_writer)?;
                validate_non_empty_field("contact sync petname_writer", &entry.petname_writer)?;
                validate_max_utf8_len("contact sync petname", &entry.petname, 1024)?;
                if !matches!(entry.state.as_str(), "approved" | "blocked" | "removed") {
                    return Err(OmniError::InvalidInput(format!(
                        "contact sync state '{}' is unknown",
                        entry.state
                    )));
                }
                if entry.bundles_json.len() > 32 {
                    return Err(OmniError::InvalidInput(
                        "contact sync entry carries too many device bundles".to_string(),
                    ));
                }
                for bundle_json in &entry.bundles_json {
                    validate_max_utf8_len("contact sync bundle", bundle_json, 32 * 1024)?;
                }
            }
            Ok(())
        }
        omni_types::LogicalEventPayloadV1::ContactSyncAck(_) => Ok(()),
        omni_types::LogicalEventPayloadV1::CourierSet(set) => {
            // A handful of couriers each; the caps only stop a flood.
            if set.publish.len() > 32 || set.read.len() > 32 {
                return Err(OmniError::InvalidInput("courier set lists too many couriers".to_string()));
            }
            for url in set.publish.iter().chain(&set.read) {
                validate_non_empty_field("courier url", url)?;
                validate_max_utf8_len("courier url", url, 512)?;
            }
            Ok(())
        }
        omni_types::LogicalEventPayloadV1::AccountPassport(message) => validate_account_passport_message_v1(message),
        omni_types::LogicalEventPayloadV1::HistorySync(message) => validate_history_sync_message_v1(message),
        omni_types::LogicalEventPayloadV1::DeviceRevocation(sync) => {
            validate_non_empty_field("device revocation", &sync.revocation_json)?;
            // Two certificates and a signature: a few hundred bytes. The cap only stops a flood.
            validate_max_utf8_len("device revocation", &sync.revocation_json, 16 * 1024)
        }
        omni_types::LogicalEventPayloadV1::SentTranscript(transcript) => {
            validate_non_empty_field("sent transcript contact_account_id", &transcript.contact_account_id)?;
            // A message with a file may have no text, the way an attachment offer may not.
            if let Some(offer) = &transcript.attachment {
                validate_attachment_offer_v1(offer)?;
                if offer.message_id != transcript.message_id {
                    return Err(OmniError::InvalidInput(
                        "sent transcript attachment belongs to another message".to_string(),
                    ));
                }
                validate_non_empty_field("sent transcript message_id", &transcript.message_id)?;
                return validate_max_utf8_len("sent transcript body", &transcript.body, CHAT_BODY_MAX_BYTES);
            }
            validate_chat_text_v1(&omni_types::ChatTextV1 {
                message_id: transcript.message_id.clone(),
                body: transcript.body.clone(),
                created_at_unix_ms: transcript.created_at_unix_ms,
            })
        }
        omni_types::LogicalEventPayloadV1::RemoteAccessGrant(grant) => {
            if grant.sent_at_unix_ms <= 0 {
                return Err(OmniError::InvalidInput("remote access grant sent_at_unix_ms must be positive".into()));
            }
            Ok(())
        }
        omni_types::LogicalEventPayloadV1::RealtimeMediaCapabilities(capabilities) => {
            validate_realtime_media_capabilities_v1(capabilities)
        }
        omni_types::LogicalEventPayloadV1::RealtimeMediaSelection(selection) => {
            validate_realtime_media_selection_v1(selection)
        }
        omni_types::LogicalEventPayloadV1::RealtimeMediaControl(control) => {
            validate_realtime_media_control_v1(control)
        }
        omni_types::LogicalEventPayloadV1::ContactPresence(presence) => {
            validate_contact_presence_v1(presence)
        }
        omni_types::LogicalEventPayloadV1::ContactPresencePing(ping) => {
            validate_contact_presence_ping_v1(ping)
        }
        omni_types::LogicalEventPayloadV1::ContactPresencePong(pong) => {
            validate_contact_presence_pong_v1(pong)
        }
        omni_types::LogicalEventPayloadV1::PauseAutoReconnect(pause) => {
            validate_pause_auto_reconnect_v1(pause)
        }
        omni_types::LogicalEventPayloadV1::PublicProfileDescriptorSync(profile) => {
            validate_public_profile_descriptor_sync_v1(profile)
        }
        omni_types::LogicalEventPayloadV1::PublicProfileAvatarSync(profile) => {
            validate_public_profile_avatar_sync_v1(profile)
        }
        omni_types::LogicalEventPayloadV1::RouteHintsSync(sync) => {
            validate_route_hints_sync_v1(sync)
        }
        omni_types::LogicalEventPayloadV1::RelaySetUpdate(update) => {
            validate_relay_set_update_v1(update)
        }
        omni_types::LogicalEventPayloadV1::GroupControl(control) => {
            if control.control.trim().is_empty() {
                return Err(OmniError::InvalidInput(
                    "group control kind must not be empty".to_string(),
                ));
            }
            if control.payload_json.len() > crate::GROUP_MLS_MAX_BYTES * 2 {
                return Err(OmniError::InvalidInput(
                    "group control payload too large".to_string(),
                ));
            }
            Ok(())
        }
    }
}

pub fn validate_wire_envelope_v1(envelope: &WireEnvelopeV1) -> OmniResult<()> {
    if envelope.protocol_version != OMNI_PROTOCOL_VERSION_V1 {
        return Err(OmniError::InvalidInput(format!(
            "unsupported wire envelope version '{}'",
            envelope.protocol_version
        )));
    }
    validate_serialized_json_len(
        "wire envelope",
        envelope,
        CONTROL_PLANE_WIRE_ENVELOPE_MAX_BYTES,
    )?;

    match &envelope.payload {
        WirePayloadV1::SessionHello(hello) => validate_session_hello_v1(hello),
        WirePayloadV1::SessionBootstrap(message) => validate_session_bootstrap_message_v1(message),
        WirePayloadV1::VerificationChallenge(challenge) => {
            validate_verification_challenge_v1(challenge)
        }
        WirePayloadV1::VerificationChallengeResponse(response) => {
            validate_verification_challenge_response_v1(response)
        }
        WirePayloadV1::EncryptedPayload(payload) => validate_encrypted_payload_v1(payload),
        WirePayloadV1::ChatText(message) => validate_chat_text_v1(message),
        WirePayloadV1::LogicalEvent(event) => validate_logical_event_envelope_v1(event),
    }
}

/// Sizes and shapes before anything is decoded: a locator and a mark secret are 32 bytes of hex, a
/// sealed passport fits [`crate::ACCOUNT_PASSPORT_SEALED_MAX_BYTES`].
pub fn validate_account_passport_message_v1(message: &omni_types::AccountPassportMessageV1) -> OmniResult<()> {
    fn hex32(field: &str, value: &str) -> OmniResult<()> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(OmniError::InvalidInput(format!("account passport {field} must be 32 bytes of hex")));
        }
        Ok(())
    }
    fn sealed(value: &str, may_be_empty: bool) -> OmniResult<()> {
        let max_b64 = crate::ACCOUNT_PASSPORT_SEALED_MAX_BYTES.div_ceil(3) * 4;
        if value.len() > max_b64 {
            return Err(OmniError::InvalidInput("account passport is too large".to_string()));
        }
        if value.is_empty() && !may_be_empty {
            return Err(OmniError::InvalidInput("account passport is empty".to_string()));
        }
        Ok(())
    }
    match message {
        omni_types::AccountPassportMessageV1::Store { locator_hex, version, sealed_b64, mark_secret_hex } => {
            hex32("locator", locator_hex)?;
            hex32("mark secret", mark_secret_hex)?;
            if *version <= 0 {
                return Err(OmniError::InvalidInput("account passport version must be positive".to_string()));
            }
            sealed(sealed_b64, false)
        }
        omni_types::AccountPassportMessageV1::Stored { locator_hex, version } => {
            hex32("locator", locator_hex)?;
            if *version <= 0 {
                return Err(OmniError::InvalidInput("account passport version must be positive".to_string()));
            }
            Ok(())
        }
        omni_types::AccountPassportMessageV1::Fetch { locator_hex } => hex32("locator", locator_hex),
        omni_types::AccountPassportMessageV1::Blob { locator_hex, version, sealed_b64 } => {
            hex32("locator", locator_hex)?;
            if *version < 0 || (*version == 0) != sealed_b64.is_empty() {
                return Err(OmniError::InvalidInput("account passport answer is inconsistent".to_string()));
            }
            sealed(sealed_b64, true)
        }
    }
}

/// Chats per summary, lines per frame — what a sender batches to; the caps only stop a flood.
pub const HISTORY_SYNC_MAX_CHATS: usize = 512;
pub const HISTORY_SYNC_MAX_LINES: usize = 1024;
pub const HISTORY_SYNC_BUCKETS: usize = 256;

/// Shapes and sizes of a history sync message before any of it is used.
pub fn validate_history_sync_message_v1(message: &omni_types::HistorySyncMessageV1) -> OmniResult<()> {
    fn account(field: &str, value: &str) -> OmniResult<()> {
        validate_non_empty_field(field, value)?;
        validate_max_utf8_len(field, value, 256)
    }
    match message {
        omni_types::HistorySyncMessageV1::Summary { chats } => {
            if chats.len() > HISTORY_SYNC_MAX_CHATS {
                return Err(OmniError::InvalidInput("history summary names too many chats".to_string()));
            }
            for chat in chats {
                account("history chat account", &chat.chat_account_id)?;
                if chat.root_hex.len() != 32 || !chat.root_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(OmniError::InvalidInput("history chat fingerprint must be 16 bytes of hex".to_string()));
                }
            }
            Ok(())
        }
        omni_types::HistorySyncMessageV1::Buckets { chat_account_id, buckets_hex, .. } => {
            account("history chat account", chat_account_id)?;
            if buckets_hex.len() != HISTORY_SYNC_BUCKETS
                || buckets_hex.iter().any(|bucket| bucket.len() != 32 || !bucket.bytes().all(|byte| byte.is_ascii_hexdigit()))
            {
                return Err(OmniError::InvalidInput("history buckets must be 256 fingerprints of 16 bytes".to_string()));
            }
            Ok(())
        }
        omni_types::HistorySyncMessageV1::Lines { chat_account_id, lines } => {
            account("history chat account", chat_account_id)?;
            if lines.is_empty() || lines.len() > HISTORY_SYNC_MAX_LINES {
                return Err(OmniError::InvalidInput("history lines must carry between 1 and 1024 lines".to_string()));
            }
            for line in lines {
                validate_non_empty_field("history line id", &line.message_id)?;
                validate_max_utf8_len("history line id", &line.message_id, CHAT_MESSAGE_ID_MAX_BYTES)?;
                account("history line author", &line.author_account_id)?;
                validate_max_utf8_len("history line body", &line.body, CHAT_BODY_MAX_BYTES)?;
                if let Some(offer) = &line.attachment {
                    validate_attachment_offer_v1(offer)?;
                    if offer.message_id != line.message_id {
                        return Err(OmniError::InvalidInput("a history line carries another message's file".to_string()));
                    }
                }
            }
            Ok(())
        }
    }
}
