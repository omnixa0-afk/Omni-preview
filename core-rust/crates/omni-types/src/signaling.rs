use serde::{Deserialize, Serialize};

use crate::{
    AttachmentAccessRevokedV1, AttachmentDownloadRequestV1,
    AttachmentDownloadedV1, AttachmentOfferV1,
    AttachmentTransferFailedV1, CallAcceptV1, CallBusyV1, CallCancelV1, CallDeclineReason,
    CallDeclineV1, CallHangupV1, CallInviteV1, CallParticipantDescriptorV1, CallReconnectV1,
    CallRingingV1, CallScreenControlV1, CallTimeoutV1, RemoteAccessGrantV1, RemoteAccessMode, SentTranscriptV1, DeviceRevocationSyncV1, AccountKeyMoveSyncV1, ContactSyncV1, ContactSyncAckV1, CourierSetV1, AccountPassportMessageV1, HistorySyncMessageV1, ChatTextV1, DeliveryReceiptV1, RealtimeMediaCapabilitiesV1, RealtimeMediaControlV1,
    RealtimeMediaSelectionV1, SignalingRouteCandidateV1, TransportMode,
    OMNI_LOGICAL_EVENT_VERSION_V1, OMNI_PROTOCOL_VERSION_V1,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPresenceV1 {
    pub observed_at_unix_ms: i64,
}

/// Что отправитель пинга думает о СВОЁМ звонке с получателем.
///
/// Сверка на случай потерянного управляющего сообщения. Живой прогон 2026-08-30: отбой ушёл по
/// ЖИВОМУ пути (`CallHangup ... sent`), у собеседника строки `call/in end` не появилось, и он
/// остался в активном звонке С ОТКРЫТЫМ МИКРОФОНОМ, пока звонок не завершили руками.
/// См. docs/bugs/a-lost-hangup-leaves-a-phantom-call.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceCallStateV1 {
    /// `None` — «у меня с тобой звонка нет».
    pub active_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPresencePingV1 {
    pub ping_id: String,
    pub sent_at_unix_ms: i64,
    /// Состояние звонка у ОТПРАВИТЕЛЯ пинга, если его сборка умеет о нём говорить.
    ///
    /// Три состояния, и путать их нельзя:
    ///   * `None` — собеседник НЕ СКАЗАЛ (старая сборка). Сверку не делаем вовсе;
    ///   * `Some(active_call_id: None)` — сказал «звонка нет»;
    ///   * `Some(active_call_id: Some(id))` — сказал, какой именно.
    ///
    /// Отсутствие поля обязано означать «не знаю», а не «звонка нет»: иначе новая сторона рвала бы
    /// живые звонки со старой по каждому пингу.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_state: Option<PresenceCallStateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPresencePongV1 {
    pub ping_id: String,
    pub responded_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseAutoReconnectV1 {
    pub paused_at_unix_ms: i64,
}

/// Group consent-flow control over the pairwise 1:1 channel (G3): invite /
/// accept / decline / welcome. `payload_json` carries the omni-protocol group
/// DTO named by `control` — omni-types sits below omni-protocol, so the typed
/// body can't live here. См. docs/decisions/group-envelope-and-transport.md.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupControlV1 {
    /// "invite" | "invite_accept" | "invite_decline" | "welcome" | "device_join" (a device of a
    /// member's account asks in). `invite_decline` means "this
    /// invitation is off" both ways: the invitee says no, or the inviter withdraws it.
    pub control: String,
    pub payload_json: String,
    pub sent_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfileDescriptorSyncV1 {
    pub version: u16,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_hash_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_mime_type: Option<String>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicProfileAvatarSyncV1 {
    pub version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_hash_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_bytes_base64: Option<String>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteHintsSyncV1 {
    pub version: u16,
    pub source_instance_id: String,
    pub exported_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    #[serde(default)]
    pub candidates: Vec<SignalingRouteCandidateV1>,
}

/// R1c relay-set propagation over the LIVE mesh channel (primary path; the per-pair ntfy beacon is the
/// fallback for unlinked contacts). Carries my current SIGNED `om3relay:` bundle to an approved, linked
/// contact, so their mesh auto-learns my (possibly rotated) relay set without a re-invite. The token is
/// already signed by the issuer, so over the authenticated session this only needs an approved-only gate
/// + a first-party check (issuer == sender). See docs/decisions/r1c-rotatable-white-relay.md §B.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaySetUpdateV1 {
    pub version: u16,
    /// The signed `om3relay:` token (base64url of the binary RelayBundleV1 (version 2)). Self-contained: it carries the
    /// issuer pubkey, the bundle version (rotation counter) and the relay set.
    pub bundle_token: String,
    pub exported_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationChallengeV1 {
    pub version: u16,
    pub challenge_id: String,
    pub nonce_hex: String,
    pub issued_at_unix_ms: i64,
    pub expires_at_unix_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationChallengeResponseV1 {
    pub version: u16,
    pub challenge_id: String,
    pub verification_payload_json: String,
    pub signature_r: String,
    pub signature_s: String,
    pub responded_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBootstrapMessageV1 {
    pub version: u16,
    pub initiator_account_id: String,
    pub initiator_device_id: String,
    pub initiator_identity_public_key: String,
    pub initiator_identity_signing_public_key: String,
    pub initiator_device_public_key: String,
    pub initiator_ephemeral_public_key: String,
    pub remote_signed_prekey_id: u32,
    pub remote_one_time_prekey_id: Option<u32>,
    pub bootstrap_signature_r: String,
    pub bootstrap_signature_s: String,
    pub created_at_unix_ms: i64,
    /// The initiator's stable local instance id (the SAME value it puts in its `SessionHello`,
    /// sourced from `storage.get_or_create_local_instance_id()`). Carried so a receiver can derive
    /// the routing `dialog_key` (`dialog_key_from_instance_id`) and process a FIRST-CONTACT bootstrap
    /// that arrives WITHOUT a preceding hello (durable store-and-forward over a lossy link) — the
    /// dialog_key then matches the one a later hello would produce, so no duplicate dialog. This is a
    /// transport-routing hint, not identity (the contact is authenticated by the X3DH keys), so — like
    /// the hello's instance_id — it is not under the bootstrap signature. `#[serde(default)]` keeps an
    /// older message (empty id) parseable; the receiver falls back to the hello path when it is empty.
    #[serde(default)]
    pub initiator_instance_id: String,
    /// The initiator device's certificate chain (master → working key → device), JSON, so a
    /// bootstrap that arrives without a hello still proves which account the device belongs to
    /// (docs/decisions/multi-device-equal-nodes.md, 2.6). Self-authenticating: the receiver checks
    /// it vouches for exactly the initiator keys above.
    #[serde(default)]
    pub initiator_device_certificate_chain_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHelloV1 {
    pub instance_id: String,
    #[serde(default)]
    pub transport_mode: TransportMode,
    pub device_name: String,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub prekey_bundle_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedPayloadV1 {
    pub suite_version: u16,
    pub counter: u64,
    pub ratchet_public_key_hex: String,
    pub previous_chain_length: u64,
    pub nonce_hex: String,
    pub ciphertext_hex: String,
    pub mac_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum LogicalEventPayloadV1 {
    ChatText(ChatTextV1),
    DeliveryReceipt(DeliveryReceiptV1),
    AttachmentOffer(AttachmentOfferV1),
    AttachmentDownloadRequest(AttachmentDownloadRequestV1),
    AttachmentTransferFailed(AttachmentTransferFailedV1),
    AttachmentDownloaded(AttachmentDownloadedV1),
    AttachmentAccessRevoked(AttachmentAccessRevokedV1),
    CallInvite(CallInviteV1),
    CallRinging(CallRingingV1),
    CallAccept(CallAcceptV1),
    CallDecline(CallDeclineV1),
    CallBusy(CallBusyV1),
    CallCancel(CallCancelV1),
    CallHangup(CallHangupV1),
    CallTimeout(CallTimeoutV1),
    CallReconnect(CallReconnectV1),
    CallScreenControl(CallScreenControlV1),
    RemoteAccessGrant(RemoteAccessGrantV1),
    RealtimeMediaCapabilities(RealtimeMediaCapabilitiesV1),
    RealtimeMediaSelection(RealtimeMediaSelectionV1),
    RealtimeMediaControl(RealtimeMediaControlV1),
    ContactPresence(ContactPresenceV1),
    ContactPresencePing(ContactPresencePingV1),
    ContactPresencePong(ContactPresencePongV1),
    PauseAutoReconnect(PauseAutoReconnectV1),
    PublicProfileDescriptorSync(PublicProfileDescriptorSyncV1),
    PublicProfileAvatarSync(PublicProfileAvatarSyncV1),
    RouteHintsSync(RouteHintsSyncV1),
    RelaySetUpdate(RelaySetUpdateV1),
    GroupControl(GroupControlV1),
    SentTranscript(SentTranscriptV1),
    DeviceRevocation(DeviceRevocationSyncV1),
    AccountKeyMove(AccountKeyMoveSyncV1),
    ContactSync(ContactSyncV1),
    ContactSyncAck(ContactSyncAckV1),
    CourierSet(CourierSetV1),
    AccountPassport(AccountPassportMessageV1),
    HistorySync(HistorySyncMessageV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalEventEnvelopeV1 {
    pub logical_version: u16,
    #[serde(flatten)]
    pub payload: LogicalEventPayloadV1,
}

impl LogicalEventEnvelopeV1 {
    pub fn group_control(
        control: impl Into<String>,
        payload_json: impl Into<String>,
        sent_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::GroupControl(GroupControlV1 {
                control: control.into(),
                payload_json: payload_json.into(),
                sent_at_unix_ms,
            }),
        }
    }

    pub fn chat_text(
        message_id: impl Into<String>,
        body: impl Into<String>,
        created_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ChatText(ChatTextV1 {
                message_id: message_id.into(),
                body: body.into(),
                created_at_unix_ms,
            }),
        }
    }

    pub fn delivery_receipt(message_id: impl Into<String>, delivered_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::DeliveryReceipt(DeliveryReceiptV1 {
                message_id: message_id.into(),
                delivered_at_unix_ms,
            }),
        }
    }

    pub fn attachment_offer(offer: AttachmentOfferV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AttachmentOffer(offer),
        }
    }

    pub fn attachment_download_request(
        transfer_id: impl Into<String>,
        message_id: impl Into<String>,
        resume_offset_bytes: i64,
        requested_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AttachmentDownloadRequest(
                AttachmentDownloadRequestV1 {
                    transfer_id: transfer_id.into(),
                    message_id: message_id.into(),
                    resume_offset_bytes,
                    requested_at_unix_ms,
                },
            ),
        }
    }

    pub fn attachment_transfer_failed(
        message_id: impl Into<String>,
        reason_code: impl Into<String>,
        reason_message: impl Into<String>,
        failed_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AttachmentTransferFailed(AttachmentTransferFailedV1 {
                message_id: message_id.into(),
                reason_code: reason_code.into(),
                reason_message: reason_message.into(),
                failed_at_unix_ms,
            }),
        }
    }

    pub fn call_invite(
        call_id: impl Into<String>,
        dialog_key: impl Into<String>,
        participants: Vec<CallParticipantDescriptorV1>,
        created_at_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallInvite(CallInviteV1 {
                call_id: call_id.into(),
                dialog_key: dialog_key.into(),
                participants,
                created_at_unix_ms,
                expires_at_unix_ms,
                remote_control: false,
            }),
        }
    }

    /// A call invite asking to control the callee's computer (docs/decisions/remote-access-for-contacts.md).
    pub fn remote_control_invite(
        call_id: impl Into<String>,
        dialog_key: impl Into<String>,
        participants: Vec<CallParticipantDescriptorV1>,
        created_at_unix_ms: i64,
        expires_at_unix_ms: i64,
    ) -> Self {
        let mut event = Self::call_invite(call_id, dialog_key, participants, created_at_unix_ms, expires_at_unix_ms);
        if let LogicalEventPayloadV1::CallInvite(invite) = &mut event.payload {
            invite.remote_control = true;
        }
        event
    }

    pub fn call_ringing(call_id: impl Into<String>, ringing_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallRinging(CallRingingV1 {
                call_id: call_id.into(),
                ringing_at_unix_ms,
            }),
        }
    }

    pub fn call_accept(call_id: impl Into<String>, accepted_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallAccept(CallAcceptV1 {
                call_id: call_id.into(),
                accepted_at_unix_ms,
            }),
        }
    }

    pub fn call_decline(
        call_id: impl Into<String>,
        reason: CallDeclineReason,
        declined_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallDecline(CallDeclineV1 {
                call_id: call_id.into(),
                reason,
                declined_at_unix_ms,
            }),
        }
    }

    pub fn call_busy(call_id: impl Into<String>, busy_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallBusy(CallBusyV1 {
                call_id: call_id.into(),
                busy_at_unix_ms,
            }),
        }
    }

    pub fn call_cancel(call_id: impl Into<String>, cancelled_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallCancel(CallCancelV1 {
                call_id: call_id.into(),
                cancelled_at_unix_ms,
            }),
        }
    }

    pub fn call_hangup(call_id: impl Into<String>, ended_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallHangup(CallHangupV1 {
                call_id: call_id.into(),
                ended_at_unix_ms,
            }),
        }
    }

    pub fn call_timeout(call_id: impl Into<String>, timed_out_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallTimeout(CallTimeoutV1 {
                call_id: call_id.into(),
                timed_out_at_unix_ms,
            }),
        }
    }

    pub fn call_reconnect(call_id: impl Into<String>, reconnecting_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallReconnect(CallReconnectV1 {
                call_id: call_id.into(),
                reconnecting_at_unix_ms,
            }),
        }
    }

    pub fn account_key_move(move_json: String, working_seed_hex: Option<String>) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AccountKeyMove(AccountKeyMoveSyncV1 {
                move_json,
                working_seed_hex,
            }),
        }
    }

    pub fn contact_sync(sync: ContactSyncV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ContactSync(sync),
        }
    }

    pub fn courier_set(set: CourierSetV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CourierSet(set),
        }
    }

    pub fn account_passport(message: AccountPassportMessageV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AccountPassport(message),
        }
    }

    pub fn history_sync(message: HistorySyncMessageV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::HistorySync(message),
        }
    }

    pub fn contact_sync_ack(through_seq: u64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ContactSyncAck(ContactSyncAckV1 { through_seq }),
        }
    }

    pub fn device_revocation(revocation_json: String) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::DeviceRevocation(DeviceRevocationSyncV1 { revocation_json }),
        }
    }

    pub fn sent_transcript(transcript: SentTranscriptV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::SentTranscript(transcript),
        }
    }

    pub fn remote_access_grant(mode: Option<RemoteAccessMode>, sent_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RemoteAccessGrant(RemoteAccessGrantV1 { mode, sent_at_unix_ms }),
        }
    }

    pub fn call_screen_control(call_id: impl Into<String>, granted: bool, sent_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::CallScreenControl(CallScreenControlV1 {
                call_id: call_id.into(),
                granted,
                sent_at_unix_ms,
                sharer_state: None,
            }),
        }
    }

    pub fn realtime_media_capabilities(capabilities: RealtimeMediaCapabilitiesV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RealtimeMediaCapabilities(capabilities),
        }
    }

    pub fn realtime_media_selection(selection: RealtimeMediaSelectionV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RealtimeMediaSelection(selection),
        }
    }

    pub fn realtime_media_control(control: RealtimeMediaControlV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RealtimeMediaControl(control),
        }
    }

    pub fn attachment_downloaded(
        message_id: impl Into<String>,
        downloaded_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AttachmentDownloaded(AttachmentDownloadedV1 {
                message_id: message_id.into(),
                downloaded_at_unix_ms,
            }),
        }
    }

    pub fn attachment_access_revoked(
        message_id: impl Into<String>,
        revoked_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::AttachmentAccessRevoked(AttachmentAccessRevokedV1 {
                message_id: message_id.into(),
                revoked_at_unix_ms,
            }),
        }
    }

    pub fn contact_presence(observed_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ContactPresence(ContactPresenceV1 {
                observed_at_unix_ms,
            }),
        }
    }

    pub fn contact_presence_ping(ping_id: impl Into<String>, sent_at_unix_ms: i64) -> Self {
        Self::contact_presence_ping_with_call_state(ping_id, sent_at_unix_ms, None)
    }

    /// Пинг, несущий состояние звонка отправителя — см. [`ContactPresencePingV1::call_state`].
    pub fn contact_presence_ping_with_call_state(
        ping_id: impl Into<String>,
        sent_at_unix_ms: i64,
        call_state: Option<PresenceCallStateV1>,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ContactPresencePing(ContactPresencePingV1 {
                ping_id: ping_id.into(),
                sent_at_unix_ms,
                call_state,
            }),
        }
    }

    pub fn contact_presence_pong(ping_id: impl Into<String>, responded_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::ContactPresencePong(ContactPresencePongV1 {
                ping_id: ping_id.into(),
                responded_at_unix_ms,
            }),
        }
    }

    pub fn pause_auto_reconnect(paused_at_unix_ms: i64) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::PauseAutoReconnect(PauseAutoReconnectV1 {
                paused_at_unix_ms,
            }),
        }
    }

    pub fn public_profile_descriptor_sync(
        version: u16,
        display_name: impl Into<String>,
        avatar_hash_hex: Option<String>,
        avatar_mime_type: Option<String>,
        updated_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::PublicProfileDescriptorSync(
                PublicProfileDescriptorSyncV1 {
                    version,
                    display_name: display_name.into(),
                    avatar_hash_hex,
                    avatar_mime_type,
                    updated_at_unix_ms,
                },
            ),
        }
    }

    pub fn public_profile_avatar_sync(
        version: u16,
        avatar_hash_hex: Option<String>,
        avatar_mime_type: Option<String>,
        avatar_bytes_base64: Option<String>,
        updated_at_unix_ms: i64,
    ) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::PublicProfileAvatarSync(PublicProfileAvatarSyncV1 {
                version,
                avatar_hash_hex,
                avatar_mime_type,
                avatar_bytes_base64,
                updated_at_unix_ms,
            }),
        }
    }

    pub fn route_hints_sync(sync: RouteHintsSyncV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RouteHintsSync(sync),
        }
    }

    pub fn relay_set_update(update: RelaySetUpdateV1) -> Self {
        Self {
            logical_version: OMNI_LOGICAL_EVENT_VERSION_V1,
            payload: LogicalEventPayloadV1::RelaySetUpdate(update),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WirePayloadV1 {
    SessionHello(SessionHelloV1),
    SessionBootstrap(SessionBootstrapMessageV1),
    VerificationChallenge(VerificationChallengeV1),
    VerificationChallengeResponse(VerificationChallengeResponseV1),
    EncryptedPayload(EncryptedPayloadV1),
    ChatText(ChatTextV1),
    LogicalEvent(LogicalEventEnvelopeV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEnvelopeV1 {
    pub protocol_version: u16,
    #[serde(flatten)]
    pub payload: WirePayloadV1,
}

impl WireEnvelopeV1 {
    pub fn session_hello(
        instance_id: impl Into<String>,
        transport_mode: TransportMode,
        device_name: impl Into<String>,
        account_id: Option<String>,
        prekey_bundle_json: Option<String>,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::SessionHello(SessionHelloV1 {
                instance_id: instance_id.into(),
                transport_mode,
                device_name: device_name.into(),
                account_id,
                prekey_bundle_json,
            }),
        }
    }

    pub fn verification_challenge(
        challenge_id: impl Into<String>,
        nonce_hex: impl Into<String>,
        issued_at_unix_ms: i64,
        expires_at_unix_ms: i64,
        remote_instance_id: Option<String>,
        remote_account_id: Option<String>,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::VerificationChallenge(VerificationChallengeV1 {
                version: OMNI_PROTOCOL_VERSION_V1,
                challenge_id: challenge_id.into(),
                nonce_hex: nonce_hex.into(),
                issued_at_unix_ms,
                expires_at_unix_ms,
                remote_instance_id,
                remote_account_id,
            }),
        }
    }

    pub fn verification_challenge_response(
        challenge_id: impl Into<String>,
        verification_payload_json: impl Into<String>,
        signature_r: impl Into<String>,
        signature_s: impl Into<String>,
        responded_at_unix_ms: i64,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::VerificationChallengeResponse(
                VerificationChallengeResponseV1 {
                    version: OMNI_PROTOCOL_VERSION_V1,
                    challenge_id: challenge_id.into(),
                    verification_payload_json: verification_payload_json.into(),
                    signature_r: signature_r.into(),
                    signature_s: signature_s.into(),
                    responded_at_unix_ms,
                },
            ),
        }
    }

    pub fn chat_text(
        message_id: impl Into<String>,
        body: impl Into<String>,
        created_at_unix_ms: i64,
    ) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::ChatText(ChatTextV1 {
                message_id: message_id.into(),
                body: body.into(),
                created_at_unix_ms,
            }),
        }
    }

    pub fn encrypted_payload(payload: EncryptedPayloadV1) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::EncryptedPayload(payload),
        }
    }

    pub fn session_bootstrap(bootstrap: SessionBootstrapMessageV1) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::SessionBootstrap(bootstrap),
        }
    }

    pub fn logical_event(event: LogicalEventEnvelopeV1) -> Self {
        Self {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::LogicalEvent(event),
        }
    }
}
