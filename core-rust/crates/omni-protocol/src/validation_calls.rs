use omni_types::{
    CallAcceptV1, CallBusyV1, CallCancelV1, CallDeclineV1, CallHangupV1, CallInviteV1,
    CallParticipantDescriptorV1, CallReconnectV1, CallRingingV1, CallScreenControlV1, CallTimeoutV1, OmniError,
    OmniResult, RealtimeMediaCapabilitiesV1, RealtimeMediaControlType, RealtimeMediaControlV1,
    RealtimeMediaSelectionV1, RealtimeVideoCodecCapabilityV1, RealtimeVideoProfileCapabilityV1,
    RealtimeVideoSelectionV1,
};

use crate::validation_identity::{
    validate_max_utf8_len, validate_non_empty_field, validate_positive_i64,
};
use crate::{CALL_ID_MAX_BYTES, CALL_PARTICIPANT_COUNT_V1};

fn validate_call_participant_descriptor_v1(
    participant: &CallParticipantDescriptorV1,
) -> OmniResult<()> {
    validate_non_empty_field("call participant_id", &participant.participant_id)?;
    validate_non_empty_field("call account_id", &participant.account_id)?;
    validate_non_empty_field("call device_id", &participant.device_id)?;
    Ok(())
}

pub fn validate_call_invite_v1(invite: &CallInviteV1) -> OmniResult<()> {
    validate_non_empty_field("call invite call_id", &invite.call_id)?;
    validate_non_empty_field("call invite dialog_key", &invite.dialog_key)?;
    validate_max_utf8_len("call invite call_id", &invite.call_id, CALL_ID_MAX_BYTES)?;
    if invite.participants.len() != CALL_PARTICIPANT_COUNT_V1 {
        return Err(OmniError::InvalidInput(format!(
            "call invite participants must contain exactly {} items in v1",
            CALL_PARTICIPANT_COUNT_V1
        )));
    }
    for participant in &invite.participants {
        validate_call_participant_descriptor_v1(participant)?;
    }
    if invite.created_at_unix_ms <= 0 || invite.expires_at_unix_ms <= invite.created_at_unix_ms {
        return Err(OmniError::InvalidInput(
            "call invite timestamps must be positive and expires_at_unix_ms must be greater than created_at_unix_ms".to_string(),
        ));
    }
    Ok(())
}

pub fn validate_call_ringing_v1(ringing: &CallRingingV1) -> OmniResult<()> {
    validate_non_empty_field("call ringing call_id", &ringing.call_id)?;
    validate_positive_i64(
        "call ringing ringing_at_unix_ms",
        ringing.ringing_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_call_accept_v1(accept: &CallAcceptV1) -> OmniResult<()> {
    validate_non_empty_field("call accept call_id", &accept.call_id)?;
    validate_positive_i64(
        "call accept accepted_at_unix_ms",
        accept.accepted_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_call_decline_v1(decline: &CallDeclineV1) -> OmniResult<()> {
    validate_non_empty_field("call decline call_id", &decline.call_id)?;
    validate_positive_i64(
        "call decline declined_at_unix_ms",
        decline.declined_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_call_busy_v1(busy: &CallBusyV1) -> OmniResult<()> {
    validate_non_empty_field("call busy call_id", &busy.call_id)?;
    validate_positive_i64("call busy busy_at_unix_ms", busy.busy_at_unix_ms)?;
    Ok(())
}

pub fn validate_call_cancel_v1(cancel: &CallCancelV1) -> OmniResult<()> {
    validate_non_empty_field("call cancel call_id", &cancel.call_id)?;
    validate_positive_i64(
        "call cancel cancelled_at_unix_ms",
        cancel.cancelled_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_call_hangup_v1(hangup: &CallHangupV1) -> OmniResult<()> {
    validate_non_empty_field("call hangup call_id", &hangup.call_id)?;
    validate_positive_i64("call hangup ended_at_unix_ms", hangup.ended_at_unix_ms)?;
    Ok(())
}

pub fn validate_call_screen_control_v1(control: &CallScreenControlV1) -> OmniResult<()> {
    validate_non_empty_field("call screen control call_id", &control.call_id)?;
    validate_positive_i64("call screen control sent_at_unix_ms", control.sent_at_unix_ms)?;
    Ok(())
}

pub fn validate_call_timeout_v1(timeout: &CallTimeoutV1) -> OmniResult<()> {
    validate_non_empty_field("call timeout call_id", &timeout.call_id)?;
    validate_positive_i64(
        "call timeout timed_out_at_unix_ms",
        timeout.timed_out_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_call_reconnect_v1(reconnect: &CallReconnectV1) -> OmniResult<()> {
    validate_non_empty_field("call reconnect call_id", &reconnect.call_id)?;
    validate_positive_i64(
        "call reconnect reconnecting_at_unix_ms",
        reconnect.reconnecting_at_unix_ms,
    )?;
    Ok(())
}

pub fn validate_realtime_media_capabilities_v1(
    capabilities: &RealtimeMediaCapabilitiesV1,
) -> OmniResult<()> {
    validate_non_empty_field("realtime media dialog_key", &capabilities.dialog_key)?;
    validate_positive_i64(
        "realtime media capabilities sent_at_unix_ms",
        capabilities.sent_at_unix_ms,
    )?;
    // No codecs and no video profile is VALID: it is how a peer says "I am here,
    // audio only". Rejecting it (as this did until 2026-08-11) meant a machine
    // without a camera published nothing at all, which took the call's audio down
    // with the video. What must still hold is that whatever IS claimed is coherent.
    if let Some(video) = capabilities.video.as_ref() {
        validate_realtime_video_profile_capability_v1(video)?;
    }
    for codec in &capabilities.codecs {
        validate_realtime_video_codec_capability_v1(codec)?;
    }
    validate_realtime_datagram_budget(capabilities.max_datagram_bytes)?;
    Ok(())
}

pub fn validate_realtime_media_selection_v1(
    selection: &RealtimeMediaSelectionV1,
) -> OmniResult<()> {
    validate_non_empty_field("realtime media selection dialog_key", &selection.dialog_key)?;
    validate_non_empty_field("realtime media selection session_id", &selection.session_id)?;
    validate_positive_i64(
        "realtime media selection selected_at_unix_ms",
        selection.selected_at_unix_ms,
    )?;
    // `video: None` is an audio-only session, which is a normal outcome, not an
    // error. Only a video half that IS present has to be fully specified.
    if let Some(video) = selection.video.as_ref() {
        validate_realtime_video_selection_v1(video)?;
    }
    validate_realtime_datagram_budget(selection.max_datagram_bytes)?;
    Ok(())
}

fn validate_realtime_video_selection_v1(video: &RealtimeVideoSelectionV1) -> OmniResult<()> {
    // A 1:1 call has two directions; a selection with video carries one or both.
    if video.streams.is_empty() || video.streams.len() > 2 {
        return Err(OmniError::InvalidInput(
            "realtime media selection must carry 1..2 video streams".to_string(),
        ));
    }
    for stream in &video.streams {
        if stream.payload_type == 0 || stream.payload_type > 127 {
            return Err(OmniError::InvalidInput(
                "realtime media selection payload_type must be in RTP dynamic range".to_string(),
            ));
        }
        if stream.width == 0 || stream.height == 0 || stream.fps == 0 {
            return Err(OmniError::InvalidInput(
                "realtime media selection stream dimensions and fps must be positive".to_string(),
            ));
        }
        if stream.target_bitrate_bps == 0 {
            return Err(OmniError::InvalidInput(
                "realtime media selection bitrate must be positive".to_string(),
            ));
        }
        validate_non_empty_field(
            "realtime media selection stream sender",
            &stream.sender_capabilities_id,
        )?;
        validate_max_utf8_len("realtime media selection stream sender", &stream.sender_capabilities_id, 64)?;
        validate_max_utf8_len("realtime media selection stream reason", &stream.reason, 256)?;
    }
    Ok(())
}

pub fn validate_realtime_media_control_v1(control: &RealtimeMediaControlV1) -> OmniResult<()> {
    validate_non_empty_field("realtime media control dialog_key", &control.dialog_key)?;
    validate_non_empty_field("realtime media control session_id", &control.session_id)?;
    validate_positive_i64(
        "realtime media control sent_at_unix_ms",
        control.sent_at_unix_ms,
    )?;
    if let Some(reason) = control.reason.as_deref() {
        validate_max_utf8_len("realtime media control reason", reason, 128)?;
    }

    match control.control_type {
        RealtimeMediaControlType::Pli | RealtimeMediaControlType::KeyframeRequest => {
            require_media_ssrc(control)?;
        }
        RealtimeMediaControlType::Fir => {
            require_media_ssrc(control)?;
            if control.fir_sequence_number.is_none() {
                return Err(OmniError::InvalidInput(
                    "realtime media FIR requires fir_sequence_number".to_string(),
                ));
            }
        }
        RealtimeMediaControlType::Nack => {
            require_media_ssrc(control)?;
            if control.packet_ids.is_empty() || control.packet_ids.len() > 128 {
                return Err(OmniError::InvalidInput(
                    "realtime media NACK must include 1..128 packet ids".to_string(),
                ));
            }
        }
        RealtimeMediaControlType::ReceiverReport => {
            require_media_ssrc(control)?;
            if control.fraction_lost.is_none()
                || control.cumulative_lost.is_none()
                || control.highest_sequence.is_none()
                || control.jitter.is_none()
            {
                return Err(OmniError::InvalidInput(
                    "realtime media receiver_report requires loss, highest_sequence, and jitter"
                        .to_string(),
                ));
            }
        }
        RealtimeMediaControlType::SenderReport => {
            require_media_ssrc(control)?;
            if control.rtp_timestamp.is_none()
                || control.packet_count.is_none()
                || control.octet_count.is_none()
            {
                return Err(OmniError::InvalidInput(
                    "realtime media sender_report requires rtp_timestamp, packet_count, and octet_count"
                        .to_string(),
                ));
            }
        }
        RealtimeMediaControlType::BitrateHint => {
            if control.target_bitrate_bps.unwrap_or_default() == 0
                || control.target_fps.unwrap_or_default() == 0
                || control.target_width.unwrap_or_default() == 0
                || control.target_height.unwrap_or_default() == 0
            {
                return Err(OmniError::InvalidInput(
                    "realtime media bitrate_hint requires positive bitrate, fps, width, and height"
                        .to_string(),
                ));
            }
        }
    }

    Ok(())
}

fn require_media_ssrc(control: &RealtimeMediaControlV1) -> OmniResult<()> {
    if control.media_ssrc.is_none() {
        return Err(OmniError::InvalidInput(
            "realtime media control requires media_ssrc".to_string(),
        ));
    }
    Ok(())
}

fn validate_realtime_video_codec_capability_v1(
    codec: &RealtimeVideoCodecCapabilityV1,
) -> OmniResult<()> {
    if codec.payload_type == 0 || codec.payload_type > 127 {
        return Err(OmniError::InvalidInput(
            "realtime media codec payload_type must be in RTP dynamic range".to_string(),
        ));
    }
    if let Some(implementation) = codec.implementation.as_deref() {
        validate_max_utf8_len("realtime media codec implementation", implementation, 128)?;
    }
    if codec.encode.is_none() && codec.decode.is_none() {
        return Err(OmniError::InvalidInput(
            "realtime media codec must say it encodes or decodes".to_string(),
        ));
    }
    Ok(())
}

fn validate_realtime_video_profile_capability_v1(
    capture: &RealtimeVideoProfileCapabilityV1,
) -> OmniResult<()> {
    if capture.preferred_width == 0
        || capture.preferred_height == 0
        || capture.preferred_fps == 0
        || capture.min_width == 0
        || capture.min_height == 0
        || capture.min_fps == 0
        || capture.max_width == 0
        || capture.max_height == 0
        || capture.max_fps == 0
    {
        return Err(OmniError::InvalidInput(
            "realtime media capture dimensions and fps must be positive".to_string(),
        ));
    }
    if capture.min_width > capture.preferred_width
        || capture.preferred_width > capture.max_width
        || capture.min_height > capture.preferred_height
        || capture.preferred_height > capture.max_height
        || capture.min_fps > capture.preferred_fps
        || capture.preferred_fps > capture.max_fps
    {
        return Err(OmniError::InvalidInput(
            "realtime media capture format must satisfy min <= preferred <= max".to_string(),
        ));
    }
    if capture.pixel_formats.is_empty() || capture.pixel_formats.len() > 8 {
        return Err(OmniError::InvalidInput(
            "realtime media capture must include 1..8 pixel formats".to_string(),
        ));
    }
    Ok(())
}

fn validate_realtime_datagram_budget(max_datagram_bytes: u32) -> OmniResult<()> {
    if !(512..=1200).contains(&max_datagram_bytes) {
        return Err(OmniError::InvalidInput(
            "realtime media max_datagram_bytes must be between 512 and 1200".to_string(),
        ));
    }
    Ok(())
}
