use omni_types::{
    AttachmentAccessRevokedV1, AttachmentDownloadRequestV1,
    AttachmentDownloadedV1, AttachmentOfferV1,
    AttachmentTransferFailedV1, OmniError, OmniResult,
};

use crate::validation_identity::{
    validate_ascii_hexdigits_only, validate_max_utf8_len,
    validate_non_empty_field, validate_non_negative_i64, validate_positive_i64,
};
use crate::{
    ATTACHMENT_FILE_NAME_MAX_BYTES, ATTACHMENT_FILE_SIZE_MAX_BYTES,
};

pub fn validate_attachment_offer_v1(offer: &AttachmentOfferV1) -> OmniResult<()> {
    validate_non_empty_field("attachment offer message_id", &offer.message_id)?;
    validate_non_empty_field("attachment offer file_name", &offer.file_name)?;
    validate_non_empty_field("attachment offer content_id_hex", &offer.content_id_hex)?;
    validate_max_utf8_len(
        "attachment offer file_name",
        &offer.file_name,
        ATTACHMENT_FILE_NAME_MAX_BYTES,
    )?;
    validate_non_negative_i64("attachment offer file_size_bytes", offer.file_size_bytes)?;
    if offer.file_size_bytes > ATTACHMENT_FILE_SIZE_MAX_BYTES {
        return Err(OmniError::InvalidInput(format!(
            "attachment offer file_size_bytes exceeds the {} byte limit",
            ATTACHMENT_FILE_SIZE_MAX_BYTES
        )));
    }
    validate_positive_i64(
        "attachment offer created_at_unix_ms",
        offer.created_at_unix_ms,
    )?;
    match (
        offer.content_encryption.as_deref(),
        offer.content_key_hex.as_deref(),
        offer.content_nonce_seed_hex.as_deref(),
        offer.content_chunk_size_bytes,
    ) {
        (None, None, None, None) => {}
        (Some(_scheme), Some(key_hex), Some(nonce_seed_hex), Some(chunk_size_bytes)) => {
            validate_non_empty_field("attachment offer content_key_hex", key_hex)?;
            validate_non_empty_field("attachment offer content_nonce_seed_hex", nonce_seed_hex)?;
            validate_ascii_hexdigits_only("attachment offer content_key_hex", key_hex)?;
            validate_ascii_hexdigits_only(
                "attachment offer content_nonce_seed_hex",
                nonce_seed_hex,
            )?;
            if chunk_size_bytes == 0 {
                return Err(OmniError::InvalidInput(
                    "attachment offer content_chunk_size_bytes must be positive".to_string(),
                ));
            }
        }
        _ => {
            return Err(OmniError::InvalidInput(
                "attachment offer content encryption metadata must be fully specified or omitted"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

pub fn validate_attachment_download_request_v1(
    request: &AttachmentDownloadRequestV1,
) -> OmniResult<()> {
    validate_non_empty_field(
        "attachment download request transfer_id",
        &request.transfer_id,
    )?;
    validate_non_empty_field(
        "attachment download request message_id",
        &request.message_id,
    )?;
    validate_non_negative_i64(
        "attachment download request resume_offset_bytes",
        request.resume_offset_bytes,
    )?;
    validate_positive_i64(
        "attachment download request requested_at_unix_ms",
        request.requested_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_attachment_transfer_failed_v1(
    failure: &AttachmentTransferFailedV1,
) -> OmniResult<()> {
    validate_non_empty_field("attachment transfer failed message_id", &failure.message_id)?;
    validate_non_empty_field(
        "attachment transfer failed reason_code",
        &failure.reason_code,
    )?;
    validate_non_empty_field(
        "attachment transfer failed reason_message",
        &failure.reason_message,
    )?;
    validate_positive_i64(
        "attachment transfer failed failed_at_unix_ms",
        failure.failed_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_attachment_downloaded_v1(downloaded: &AttachmentDownloadedV1) -> OmniResult<()> {
    validate_non_empty_field("attachment downloaded message_id", &downloaded.message_id)?;
    validate_positive_i64(
        "attachment downloaded downloaded_at_unix_ms",
        downloaded.downloaded_at_unix_ms,
    )?;

    Ok(())
}

pub fn validate_attachment_access_revoked_v1(
    revoked: &AttachmentAccessRevokedV1,
) -> OmniResult<()> {
    validate_non_empty_field("attachment access revoked message_id", &revoked.message_id)?;
    validate_positive_i64(
        "attachment access revoked revoked_at_unix_ms",
        revoked.revoked_at_unix_ms,
    )?;

    Ok(())
}
