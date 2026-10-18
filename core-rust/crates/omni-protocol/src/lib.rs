pub(crate) use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

mod crypto;
mod group_call;
mod group_wire;
mod identity;
mod screen_control;
mod validation_attachments;
mod validation_calls;
mod validation_identity;
mod validation_runtime;
mod versions;

pub use crypto::*;
pub use group_call::*;
pub use group_wire::*;
pub use identity::*;
pub use screen_control::*;
pub use omni_types::SessionBootstrapMessageV1;
pub use validation_attachments::*;
pub use validation_calls::*;
pub use validation_identity::*;
pub use validation_runtime::*;
pub use versions::*;

#[cfg(test)]
mod tests {
    use super::{
        contact_payload_from_my_payload, importable_contact_payload_from_verification,
        validate_attachment_offer_v1, validate_chat_text_v1,
        validate_contact_verification_payload_v1, validate_encrypted_payload_v1, validate_prekey_bundle_v1,
        validate_realtime_media_capabilities_v1, validate_realtime_media_control_v1,
        validate_session_hello_v1, validate_verification_challenge_response_v1,
        validate_wire_envelope_v1, validate_account_passport_message_v1, ACCOUNT_PASSPORT_SEALED_MAX_BYTES,
        AccountCryptoDescriptorV1, AccountPublicDescriptorV1,
        ContactVerificationPayloadV1, DeviceCryptoDescriptorV1, DevicePublicDescriptorV1,
        MyVerificationPayloadV1, OneTimePrekeyDescriptorV1, PrekeyBundleV1,
        SignedPrekeyDescriptorV1, ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
        ATTACHMENT_FILE_NAME_MAX_BYTES,
        ATTACHMENT_FILE_SIZE_MAX_BYTES, CHAT_BODY_MAX_BYTES, CHAT_MESSAGE_ID_MAX_BYTES,
        CONTACT_VERIFICATION_PAYLOAD_VERSION_V1, CONTROL_PLANE_WIRE_ENVELOPE_MAX_BYTES,
        DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1, ENCRYPTED_PAYLOAD_CIPHERTEXT_MAX_BYTES,
        IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1, MY_VERIFICATION_PAYLOAD_VERSION_V1,
        PREKEY_BUNDLE_VERSION_V1, PUBLIC_DESCRIPTOR_VERSION_V1,
        SESSION_HELLO_PREKEY_BUNDLE_JSON_MAX_BYTES, VERIFICATION_PAYLOAD_JSON_MAX_BYTES,
    };
    use omni_types::{
        AttachmentOfferV1, ChatTextV1, EncryptedPayloadV1,
        LogicalEventEnvelopeV1, RealtimeMediaCapabilitiesV1, RealtimeMediaControlType,
        RealtimeMediaControlV1, RealtimeVideoCodec, RealtimeVideoCodecCapabilityV1,
        RealtimeVideoPixelFormat, RealtimeVideoProfileCapabilityV1, SessionHelloV1, TransportMode,
        VerificationChallengeResponseV1, WireEnvelopeV1, WirePayloadV1, OMNI_PROTOCOL_VERSION_V1,
    };

    fn sample_my_payload() -> MyVerificationPayloadV1 {
        MyVerificationPayloadV1 {
            version: MY_VERIFICATION_PAYLOAD_VERSION_V1,
            account: AccountPublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                account_id: "acct-1".to_string(),
                display_name: "Alice".to_string(),
            },
            devices: vec![DevicePublicDescriptorV1 {
                version: PUBLIC_DESCRIPTOR_VERSION_V1,
                device_id: "dev-1".to_string(),
                account_id: "acct-1".to_string(),
                device_label: "Alice Laptop".to_string(),
            }],
            fingerprint: "abc123".to_string(),
            safety_number: "1234 5678 9012".to_string(),
            exported_at_unix_ms: 100,
        }
    }

    #[test]
    fn payload_roundtrip_shapes_stay_serializable() {
        let payload = sample_my_payload();
        let json = serde_json::to_string(&payload).unwrap();
        let decoded: MyVerificationPayloadV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, payload);

        let contact = contact_payload_from_my_payload(&payload).unwrap();
        let json = serde_json::to_string(&contact).unwrap();
        let decoded: ContactVerificationPayloadV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, contact);
    }

    #[test]
    fn realtime_media_control_serializes_snake_case_control_types() {
        let control = sample_realtime_media_control(RealtimeMediaControlType::Nack);

        let json = serde_json::to_string(&control).unwrap();
        let decoded: RealtimeMediaControlV1 = serde_json::from_str(&json).unwrap();

        assert!(json.contains("\"control_type\":\"nack\""));
        assert!(json.contains("\"packet_ids\":[10,11]"));
        assert_eq!(decoded, control);
    }

    #[test]
    fn realtime_media_control_validation_rejects_invalid_payload_for_type() {
        let mut control = sample_realtime_media_control(RealtimeMediaControlType::ReceiverReport);

        let error = validate_realtime_media_control_v1(&control).unwrap_err();
        assert!(error.to_string().contains("receiver_report requires"));

        control.fraction_lost = Some(0);
        control.cumulative_lost = Some(0);
        control.highest_sequence = Some(100);
        control.jitter = Some(20);
        validate_realtime_media_control_v1(&control).unwrap();
    }

    #[test]
    fn realtime_media_capabilities_reject_invalid_capture_range() {
        let mut capabilities = sample_realtime_media_capabilities();
        let video = capabilities.video.as_mut().unwrap();
        video.min_width = 1920;
        video.preferred_width = 1280;

        let error = validate_realtime_media_capabilities_v1(&capabilities).unwrap_err();

        assert!(error.to_string().contains("min <= preferred <= max"));
    }

    #[test]
    fn a_peer_with_no_video_at_all_is_still_allowed_to_announce_itself() {
        // The camera-less machine. It has nothing to offer the video negotiation and
        // says so; that must not disqualify it from the call, because the same
        // announcement is what brings AUDIO up on both sides.
        let mut capabilities = sample_realtime_media_capabilities();
        capabilities.codecs.clear();
        capabilities.video = None;

        validate_realtime_media_capabilities_v1(&capabilities).unwrap();
    }

    fn sample_realtime_media_control(
        control_type: RealtimeMediaControlType,
    ) -> RealtimeMediaControlV1 {
        RealtimeMediaControlV1 {
            dialog_key: "dialog::test".to_string(),
            session_id: "media-v2:test".to_string(),
            control_type,
            media_ssrc: Some(123),
            packet_ids: vec![10, 11],
            fir_sequence_number: None,
            fraction_lost: None,
            cumulative_lost: None,
            highest_sequence: None,
            rtp_timestamp: None,
            packet_count: None,
            octet_count: None,
            jitter: None,
            rtt_ms: None,
            target_bitrate_bps: None,
            target_fps: None,
            target_width: None,
            target_height: None,
            reason: Some("test".to_string()),
            sent_at_unix_ms: 100,
        }
    }

    fn sample_realtime_media_capabilities() -> RealtimeMediaCapabilitiesV1 {
        RealtimeMediaCapabilitiesV1 {
            dialog_key: "dialog::media".to_string(),
            capabilities_id: "caps-test".to_string(),
            codecs: vec![RealtimeVideoCodecCapabilityV1 {
                codec: RealtimeVideoCodec::H264,
                payload_type: 102,
                encode: Some(omni_types::RealtimeVideoCodecSideV1 {
                    hardware: true,
                    max_width: 0,
                    max_height: 0,
                    max_fps: 0,
                }),
                decode: Some(omni_types::RealtimeVideoCodecSideV1 {
                    hardware: true,
                    max_width: 0,
                    max_height: 0,
                    max_fps: 0,
                }),
                implementation: Some("h264/test".to_string()),
            }],
            power: Default::default(),
            cpu_load: Default::default(),
            codec_ceiling: None,
            bitrate_level: Default::default(),
            content: Default::default(),
            video: Some(RealtimeVideoProfileCapabilityV1 {
                min_width: 1,
                min_height: 1,
                min_fps: 1,
                preferred_width: 1280,
                preferred_height: 720,
                preferred_fps: 30,
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                pixel_formats: vec![RealtimeVideoPixelFormat::Bgra],
                can_send_video: true,
            }),
            max_datagram_bytes: 1000,
            supports_rtcp_feedback: true,
            sent_at_unix_ms: 100,
        }
    }

    #[test]
    fn version_guards_reject_unknown_payload_versions() {
        let mut payload = contact_payload_from_my_payload(&sample_my_payload()).unwrap();
        payload.version = 99;

        let error = validate_contact_verification_payload_v1(&payload).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported contact verification payload version"));
    }

    #[test]
    fn payload_mismatch_is_rejected_when_device_account_does_not_match() {
        let mut payload = contact_payload_from_my_payload(&sample_my_payload()).unwrap();
        payload.devices[0].account_id = "acct-other".to_string();

        let error = validate_contact_verification_payload_v1(&payload).unwrap_err();
        assert!(error
            .to_string()
            .contains("device descriptor account_id does not match account descriptor"));
    }

    #[test]
    fn importable_payload_keeps_versioned_public_data() {
        let verification = contact_payload_from_my_payload(&sample_my_payload()).unwrap();
        let importable = importable_contact_payload_from_verification(&verification).unwrap();

        assert_eq!(importable.version, IMPORTABLE_CONTACT_PAYLOAD_VERSION_V1);
        assert_eq!(importable.account.display_name, "Alice");
        assert_eq!(importable.devices.len(), 1);
        assert_eq!(
            verification.version,
            CONTACT_VERIFICATION_PAYLOAD_VERSION_V1
        );
    }

    fn sample_prekey_bundle() -> PrekeyBundleV1 {
        PrekeyBundleV1 {
            version: PREKEY_BUNDLE_VERSION_V1,
            account: AccountCryptoDescriptorV1 {
                version: ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
                account_id: "acct-1".to_string(),
                identity_public_key: "0011223344556677".to_string(),
                identity_signing_public_key: "1122334455667788".to_string(),
            },
            device: DeviceCryptoDescriptorV1 {
                version: DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1,
                device_id: "dev-1".to_string(),
                account_id: "acct-1".to_string(),
                device_label: "Alice Laptop".to_string(),
                device_public_key: "8899aabbccddeeff".to_string(),
            },
            signed_prekey: SignedPrekeyDescriptorV1 {
                prekey_id: 7,
                public_key: "0102030405060708".to_string(),
                signature_r: "0a0b0c0d0e0f1011".to_string(),
                signature_s: "11100f0e0d0c0b0a".to_string(),
                expires_at_unix_ms: None,
            },
            one_time_prekey: Some(OneTimePrekeyDescriptorV1 {
                prekey_id: 8,
                public_key: "1213141516171819".to_string(),
            }),
            generated_at_unix_ms: 123,
            device_certificate_chain_json: None,
        }
    }

    #[test]
    fn stage5_payload_roundtrip_shapes_stay_serializable() {
        let bundle = sample_prekey_bundle();
        let json = serde_json::to_string(&bundle).unwrap();
        let decoded: PrekeyBundleV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn stage5_version_guards_reject_unknown_versions() {
        let mut bundle = sample_prekey_bundle();
        bundle.version = 99;
        let error = validate_prekey_bundle_v1(&bundle).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported prekey bundle version"));
    }

    #[test]
    fn verification_payload_mismatch_rejects_invalid_hex() {
        let bundle = PrekeyBundleV1 {
            signed_prekey: SignedPrekeyDescriptorV1 {
                public_key: "zz".to_string(),
                ..sample_prekey_bundle().signed_prekey
            },
            ..sample_prekey_bundle()
        };

        let error = validate_prekey_bundle_v1(&bundle).unwrap_err();
        assert!(error.to_string().contains("hex"));
    }

    #[test]
    fn stage8_session_hello_rejects_oversized_prekey_bundle_json() {
        let hello = SessionHelloV1 {
            instance_id: "inst-1".to_string(),
            transport_mode: TransportMode::Quic,
            device_name: "Node".to_string(),
            account_id: Some("acct-1".to_string()),
            prekey_bundle_json: Some("x".repeat(SESSION_HELLO_PREKEY_BUNDLE_JSON_MAX_BYTES + 1)),
        };

        let error = validate_session_hello_v1(&hello).unwrap_err();
        assert!(error.to_string().contains("prekey_bundle_json"));
        assert!(error.to_string().contains("limit"));
    }

    #[test]
    fn stage8_verification_response_rejects_oversized_payload_json() {
        let response = VerificationChallengeResponseV1 {
            version: OMNI_PROTOCOL_VERSION_V1,
            challenge_id: "challenge-1".to_string(),
            verification_payload_json: "x".repeat(VERIFICATION_PAYLOAD_JSON_MAX_BYTES + 1),
            signature_r: "aa".repeat(32),
            signature_s: "bb".repeat(32),
            responded_at_unix_ms: 1_000,
        };

        let error = validate_verification_challenge_response_v1(&response).unwrap_err();
        assert!(error.to_string().contains("verification_payload_json"));
        assert!(error.to_string().contains("limit"));
    }

    #[test]
    fn stage8_chat_text_rejects_oversized_fields() {
        let oversized_id = ChatTextV1 {
            message_id: "a".repeat(CHAT_MESSAGE_ID_MAX_BYTES + 1),
            body: "ok".to_string(),
            created_at_unix_ms: 1_000,
        };
        let error = validate_chat_text_v1(&oversized_id).unwrap_err();
        assert!(error.to_string().contains("message_id"));

        let oversized_body = ChatTextV1 {
            message_id: "msg-1".to_string(),
            body: "b".repeat(CHAT_BODY_MAX_BYTES + 1),
            created_at_unix_ms: 1_000,
        };
        let error = validate_chat_text_v1(&oversized_body).unwrap_err();
        assert!(error.to_string().contains("body"));
    }

    #[test]
    fn stage8_encrypted_payload_rejects_oversized_ciphertext() {
        let payload = EncryptedPayloadV1 {
            suite_version: 1,
            counter: 1,
            ratchet_public_key_hex: "aa".repeat(32),
            previous_chain_length: 0,
            nonce_hex: "11".repeat(12),
            ciphertext_hex: "ff".repeat(ENCRYPTED_PAYLOAD_CIPHERTEXT_MAX_BYTES + 1),
            mac_hex: "22".repeat(16),
        };

        let error = validate_encrypted_payload_v1(&payload).unwrap_err();
        assert!(error.to_string().contains("ciphertext_hex"));
        assert!(error.to_string().contains("decoded limit"));
    }

    #[test]
    fn stage8_attachment_offer_rejects_large_metadata() {
        let oversized_name = AttachmentOfferV1 {
            message_id: "msg-1".to_string(),
            body: "hi".to_string(),
            file_name: "a".repeat(ATTACHMENT_FILE_NAME_MAX_BYTES + 1),
            file_size_bytes: 10,
            mime_type: Some("text/plain".to_string()),
            content_id_hex: "11".repeat(32),
            content_encryption: None,
            content_key_hex: None,
            content_nonce_seed_hex: None,
            content_chunk_size_bytes: None,
            created_at_unix_ms: 1_000,
            presentation: None,
        };
        let error = validate_attachment_offer_v1(&oversized_name).unwrap_err();
        assert!(error.to_string().contains("file_name"));

        let oversized_file = AttachmentOfferV1 {
            file_name: "ok.txt".to_string(),
            file_size_bytes: ATTACHMENT_FILE_SIZE_MAX_BYTES + 1,
            ..oversized_name
        };
        let error = validate_attachment_offer_v1(&oversized_file).unwrap_err();
        assert!(error.to_string().contains("file_size_bytes"));
    }

    #[test]
    fn stage8_wire_envelope_rejects_oversized_serialized_payload() {
        let envelope = WireEnvelopeV1 {
            protocol_version: OMNI_PROTOCOL_VERSION_V1,
            payload: WirePayloadV1::LogicalEvent(LogicalEventEnvelopeV1::attachment_offer(
                AttachmentOfferV1 {
                    message_id: "msg-1".to_string(),
                    body: "b".repeat(CONTROL_PLANE_WIRE_ENVELOPE_MAX_BYTES),
                    file_name: "test.bin".to_string(),
                    file_size_bytes: 1024,
                    mime_type: Some("application/octet-stream".to_string()),
                    content_id_hex: "aa".repeat(32),
                    content_encryption: None,
                    content_key_hex: None,
                    content_nonce_seed_hex: None,
                    content_chunk_size_bytes: None,
                    created_at_unix_ms: 1_000,
                    presentation: None,
                },
            )),
        };

        let error = validate_wire_envelope_v1(&envelope).unwrap_err();
        assert!(error.to_string().contains("wire envelope"));
        assert!(error.to_string().contains("serialized limit"));
    }

    /// Garbage from a holder or an owner is refused by shape before anything is decoded.
    #[test]
    fn an_account_passport_message_is_checked_by_shape() {
        use omni_types::AccountPassportMessageV1 as M;
        let locator = "ab".repeat(32);
        let ok = M::Store {
            locator_hex: locator.clone(),
            version: 5,
            sealed_b64: "AAAA".to_string(),
            mark_secret_hex: "cd".repeat(32),
        };
        assert!(validate_account_passport_message_v1(&ok).is_ok());
        let bad = [
            M::Store { locator_hex: "zz".repeat(32), version: 5, sealed_b64: "AAAA".into(), mark_secret_hex: "cd".repeat(32) },
            M::Store { locator_hex: locator.clone(), version: 0, sealed_b64: "AAAA".into(), mark_secret_hex: "cd".repeat(32) },
            M::Store { locator_hex: locator.clone(), version: 5, sealed_b64: String::new(), mark_secret_hex: "cd".repeat(32) },
            M::Store {
                locator_hex: locator.clone(),
                version: 5,
                sealed_b64: "A".repeat(ACCOUNT_PASSPORT_SEALED_MAX_BYTES * 2),
                mark_secret_hex: "cd".repeat(32),
            },
            M::Stored { locator_hex: locator.clone(), version: -1 },
            M::Fetch { locator_hex: "ab".into() },
            M::Blob { locator_hex: locator.clone(), version: 0, sealed_b64: "AAAA".into() },
            M::Blob { locator_hex: locator.clone(), version: 3, sealed_b64: String::new() },
        ];
        for message in bad {
            assert!(validate_account_passport_message_v1(&message).is_err(), "{message:?}");
        }
        assert!(validate_account_passport_message_v1(&M::Blob { locator_hex: locator, version: 0, sealed_b64: String::new() }).is_ok());
    }
}
