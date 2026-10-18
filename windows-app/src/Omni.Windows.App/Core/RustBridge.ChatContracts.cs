using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed record ChatMessageRecord(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("peer_addr")] string PeerKey,
    [property: JsonPropertyName("direction")] string Direction,
    [property: JsonPropertyName("body")] string Body,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("remote_delivered_at_unix_ms")] long? RemoteDeliveredAtUnixMs,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs,
    [property: JsonPropertyName("attachment")] AttachmentRecord? Attachment,
    [property: JsonPropertyName("call_history_event")] CallHistoryEvent? CallHistoryEvent = null,
    // The line came back from the contact's copy of the chat after a restore («со слов собеседника»).
    [property: JsonPropertyName("told_by_contact")] bool ToldByContact = false);

internal sealed record AttachmentRecord(
    [property: JsonPropertyName("message_id")] string MessageId,
    [property: JsonPropertyName("file_name")] string FileName,
    [property: JsonPropertyName("file_size_bytes")] long FileSizeBytes,
    [property: JsonPropertyName("mime_type")] string? MimeType,
    [property: JsonPropertyName("content_id_hex")] string ContentIdHex,
    [property: JsonPropertyName("content_encryption")] string? ContentEncryption,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("bytes_transferred")] long BytesTransferred,
    [property: JsonPropertyName("local_blob_available")] bool LocalBlobAvailable,
    [property: JsonPropertyName("local_serving_enabled")] bool LocalServingEnabled,
    [property: JsonPropertyName("remote_holder_available")] bool RemoteHolderAvailable,
    [property: JsonPropertyName("local_saved_path")] string? LocalSavedPath,
    [property: JsonPropertyName("last_error")] string? LastError,
    [property: JsonPropertyName("presentation")] AttachmentPresentation? Presentation = null,
    [property: JsonPropertyName("local_copy_path")] string? LocalCopyPath = null,
    // Members a group file is coming from right now (several at once, attachment-content-tree.md
    // stage 3); 0 — not counted (a 1:1 download has its one sender).
    [property: JsonPropertyName("source_count")] int SourceCount = 0);

/// How the chat shows a file — only what the core checked and accepted (media-messages.md §1).
internal sealed record AttachmentPresentation(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("width")] int Width,
    [property: JsonPropertyName("height")] int Height,
    [property: JsonPropertyName("preview_jpeg_b64")] string? PreviewJpegBase64)
{
    public bool IsPhoto => string.Equals(Kind, "photo", StringComparison.Ordinal);
}

internal sealed record ListMessagesResponse(
    [property: JsonPropertyName("peer_addr")] string PeerKey,
    [property: JsonPropertyName("messages")] IReadOnlyList<ChatMessageRecord> Messages);

internal sealed record ListMessagesPageResponse(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("messages")] IReadOnlyList<ChatMessageRecord> Messages,
    [property: JsonPropertyName("has_older")] bool HasOlder,
    [property: JsonPropertyName("oldest_loaded_created_at_unix_ms")] long? OldestLoadedCreatedAtUnixMs,
    [property: JsonPropertyName("oldest_loaded_message_id")] string? OldestLoadedMessageId);

internal sealed record ChatHeaderContactSummary(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("title")] string Title,
    [property: JsonPropertyName("subtitle")] string Subtitle,
    [property: JsonPropertyName("contact_card")] ContactCardSnapshot? ContactCard);

internal sealed record ChatSessionSummary(
    [property: JsonPropertyName("is_open_lease")] bool IsOpenLease,
    [property: JsonPropertyName("auto_reconnect_paused")] bool AutoReconnectPaused,
    [property: JsonPropertyName("session_state")] string SessionStateValue,
    [property: JsonPropertyName("peer_endpoint")] string? PeerEndpoint,
    [property: JsonPropertyName("local_endpoint")] string? LocalEndpoint,
    [property: JsonPropertyName("last_interaction_at_unix_ms")] long? LastInteractionAtUnixMs,
    [property: JsonPropertyName("last_interaction_kind")] string? LastInteractionKindValue,
    [property: JsonPropertyName("last_confirmed_keepalive_at_unix_ms")] long? LastConfirmedKeepaliveAtUnixMs,
    [property: JsonPropertyName("last_round_trip_ms")] long? LastRoundTripMs,
    [property: JsonPropertyName("peer_device_id")] string? PeerDeviceId = null,
    [property: JsonPropertyName("peer_device_count")] int? PeerDeviceCount = null);

internal sealed record ChatHeaderScreenSnapshot(
    [property: JsonPropertyName("header_summary")] ChatHeaderContactSummary HeaderSummary,
    [property: JsonPropertyName("security_summary")] ChatSecuritySummary SecuritySummary,
    [property: JsonPropertyName("session_summary")] ChatSessionSummary SessionSummary,
    [property: JsonPropertyName("call_summary")] CallSummary? CallSummary);

internal sealed record ChatSecuritySummary(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("contact_id")] string? ContactId,
    [property: JsonPropertyName("remote_account_id")] string? RemoteAccountId,
    [property: JsonPropertyName("remote_device_id")] string? RemoteDeviceId,
    [property: JsonPropertyName("trust_state")] string? TrustState,
    [property: JsonPropertyName("warning")] string Warning,
    [property: JsonPropertyName("has_established_session")] bool HasEstablishedSession,
    [property: JsonPropertyName("has_remote_prekey_bundle")] bool HasRemotePrekeyBundle,
    [property: JsonPropertyName("uses_encrypted_transport")] bool UsesEncryptedTransport,
    [property: JsonPropertyName("session_state")] string? SessionState,
    [property: JsonPropertyName("last_session_update_unix_ms")] long? LastSessionUpdateUnixMs);

internal sealed record MessageSecurityDetails(
    [property: JsonPropertyName("message_id")] string MessageId,
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("contact_id")] string? ContactId,
    [property: JsonPropertyName("remote_account_id")] string? RemoteAccountId,
    [property: JsonPropertyName("remote_device_id")] string? RemoteDeviceId,
    [property: JsonPropertyName("remote_device_label")] string? RemoteDeviceLabel,
    [property: JsonPropertyName("session_address")] string? SessionAddress,
    [property: JsonPropertyName("trust_state")] string? TrustState,
    [property: JsonPropertyName("encryption_state")] string EncryptionState,
    [property: JsonPropertyName("bootstrap_state")] string? BootstrapState,
    [property: JsonPropertyName("payload_kind")] string? PayloadKind,
    [property: JsonPropertyName("has_encrypted_payload")] bool HasEncryptedPayload);

internal sealed record DialogMessageRefreshSummary(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("message_count")] int MessageCount,
    [property: JsonPropertyName("latest_message_id")] string? LatestMessageId,
    [property: JsonPropertyName("last_message_updated_at_unix_ms")] long? LastMessageUpdatedAtUnixMs,
    [property: JsonPropertyName("last_attachment_updated_at_unix_ms")] long? LastAttachmentUpdatedAtUnixMs,
    [property: JsonPropertyName("last_attachment_message_id")] string? LastAttachmentMessageId,
    [property: JsonPropertyName("last_security_projection_updated_at_unix_ms")] long? LastSecurityProjectionUpdatedAtUnixMs,
    [property: JsonPropertyName("last_security_projection_message_id")] string? LastSecurityProjectionMessageId);

internal sealed record DialogDeltaCursor(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("message_count")] int MessageCount,
    [property: JsonPropertyName("latest_message_id")] string? LatestMessageId,
    [property: JsonPropertyName("last_message_updated_at_unix_ms")] long? LastMessageUpdatedAtUnixMs,
    [property: JsonPropertyName("last_attachment_updated_at_unix_ms")] long? LastAttachmentUpdatedAtUnixMs,
    [property: JsonPropertyName("last_attachment_message_id")] string? LastAttachmentMessageId,
    [property: JsonPropertyName("last_security_projection_updated_at_unix_ms")] long? LastSecurityProjectionUpdatedAtUnixMs,
    [property: JsonPropertyName("last_security_projection_message_id")] string? LastSecurityProjectionMessageId)
{
    public static DialogDeltaCursor FromRefreshSummary(DialogMessageRefreshSummary summary)
        => new(
            summary.DialogKey,
            summary.MessageCount,
            summary.LatestMessageId,
            summary.LastMessageUpdatedAtUnixMs,
            summary.LastAttachmentUpdatedAtUnixMs,
            summary.LastAttachmentMessageId,
            summary.LastSecurityProjectionUpdatedAtUnixMs,
            summary.LastSecurityProjectionMessageId);
}

internal sealed record DialogDeltaSnapshot(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("changed_messages")] IReadOnlyList<ChatMessageRecord> ChangedMessages,
    [property: JsonPropertyName("changed_security_details")] IReadOnlyList<MessageSecurityDetails> ChangedSecurityDetails,
    [property: JsonPropertyName("cursor")] DialogDeltaCursor Cursor,
    [property: JsonPropertyName("requires_full_reload")] bool RequiresFullReload);

internal sealed record ChatThreadListRefreshSummary(
    [property: JsonPropertyName("thread_count")] int ThreadCount,
    [property: JsonPropertyName("last_contact_updated_at_unix_ms")] long? LastContactUpdatedAtUnixMs,
    [property: JsonPropertyName("last_dialog_link_updated_at_unix_ms")] long? LastDialogLinkUpdatedAtUnixMs,
    [property: JsonPropertyName("last_public_profile_updated_at_unix_ms")] long? LastPublicProfileUpdatedAtUnixMs,
    [property: JsonPropertyName("last_message_updated_at_unix_ms")] long? LastMessageUpdatedAtUnixMs,
    [property: JsonPropertyName("last_attachment_updated_at_unix_ms")] long? LastAttachmentUpdatedAtUnixMs,
    [property: JsonPropertyName("last_read_mark_updated_at_unix_ms")] long? LastReadMarkUpdatedAtUnixMs);

internal sealed record ContactsRefreshSummary(
    [property: JsonPropertyName("contact_count")] int ContactCount,
    [property: JsonPropertyName("last_contact_updated_at_unix_ms")] long? LastContactUpdatedAtUnixMs,
    [property: JsonPropertyName("last_public_profile_updated_at_unix_ms")] long? LastPublicProfileUpdatedAtUnixMs,
    [property: JsonPropertyName("last_presence_updated_at_unix_ms")] long? LastPresenceUpdatedAtUnixMs,
    [property: JsonPropertyName("last_verification_updated_at_unix_ms")] long? LastVerificationUpdatedAtUnixMs,
    [property: JsonPropertyName("last_identity_observation_updated_at_unix_ms")] long? LastIdentityObservationUpdatedAtUnixMs,
    [property: JsonPropertyName("last_contact_devices_updated_at_unix_ms")] long? LastContactDevicesUpdatedAtUnixMs);

internal sealed record ContactChatTarget(
    [property: JsonPropertyName("contact_id")] string ContactId,
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("title")] string Title);

internal sealed partial class RustBridge
{
    internal sealed record SendTextRequest(
        [property: JsonPropertyName("peer_addr")] string PeerAddr,
        [property: JsonPropertyName("body")] string Body);

    internal sealed record SendAttachmentRequest(
        [property: JsonPropertyName("peer_addr")] string PeerAddr,
        [property: JsonPropertyName("body")] string Body,
        [property: JsonPropertyName("source_file_path")] string SourceFilePath,
        [property: JsonPropertyName("display_file_name")] string? DisplayFileName,
        [property: JsonPropertyName("media_protection_mode")] string MediaProtectionMode,
        [property: JsonPropertyName("as_photo")] bool AsPhoto);

    internal sealed record AttachmentDownloadRequest(
        [property: JsonPropertyName("message_id")] string MessageId,
        [property: JsonPropertyName("save_path")] string SavePath);

    internal sealed record ListMessagesRequest([property: JsonPropertyName("peer_addr")] string PeerAddr);

    internal sealed record ListMessagesPageRequest(
        [property: JsonPropertyName("dialog_key")] string DialogKey,
        [property: JsonPropertyName("before_created_at_unix_ms")] long? BeforeCreatedAtUnixMs,
        [property: JsonPropertyName("before_message_id")] string? BeforeMessageId,
        [property: JsonPropertyName("limit")] int Limit);

    internal sealed record DialogKeyRequest([property: JsonPropertyName("dialog_key")] string DialogKey);

    internal sealed record DialogDeltaRequest(
        [property: JsonPropertyName("dialog_key")] string DialogKey,
        [property: JsonPropertyName("cursor")] DialogDeltaCursor Cursor);

    internal sealed record MessageIdRequest([property: JsonPropertyName("message_id")] string MessageId);

    internal sealed record MessageIdBatchRequest(
        [property: JsonPropertyName("message_ids")] IReadOnlyList<string> MessageIds);

    internal sealed record QuickAddContactFromDialogRequest(
        [property: JsonPropertyName("dialog_key")] string DialogKey,
        [property: JsonPropertyName("petname")] string Petname,
        [property: JsonPropertyName("public_display_name")] string? PublicDisplayName);

    internal sealed record AttachDialogToContactRequest(
        [property: JsonPropertyName("contact_id")] string ContactId,
        [property: JsonPropertyName("dialog_key")] string DialogKey);

    internal sealed record ChatThreadProjection(
        [property: JsonPropertyName("contact")] ContactSnapshot Contact,
        [property: JsonPropertyName("last_message")] ChatMessageRecord? LastMessage,
        [property: JsonPropertyName("unread_count")] int UnreadCount = 0);
}
