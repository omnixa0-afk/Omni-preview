using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed record RecoveryPhraseV2(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("mnemonic")] string Mnemonic);

internal sealed record PublicAccountSummary(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("verification_payload")] string VerificationPayload,
    [property: JsonPropertyName("safety_number_material")] string SafetyNumberMaterial);

internal sealed record LocalAccountProfileSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("account_representation_version")] ushort AccountRepresentationVersion,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("has_root_secret")] bool HasRootSecret,
    [property: JsonPropertyName("has_recovery_phrase_v2")] bool HasRecoveryPhraseV2);

internal sealed record PublicAccountDescriptor(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName);

internal sealed record LocalAccountSnapshot(
    [property: JsonPropertyName("descriptor")] PublicAccountDescriptor Descriptor,
    [property: JsonPropertyName("current_device_id")] string CurrentDeviceId,
    [property: JsonPropertyName("restored_from_recovery")] bool RestoredFromRecovery,
    [property: JsonPropertyName("account_representation_version")] ushort AccountRepresentationVersion,
    [property: JsonPropertyName("public_summary")] PublicAccountSummary? PublicSummary,
    [property: JsonPropertyName("local_profile_summary")] LocalAccountProfileSummary? LocalProfileSummary,
    [property: JsonPropertyName("public_avatar_hash_hex")] string? PublicAvatarHashHex,
    [property: JsonPropertyName("public_avatar_mime_type")] string? PublicAvatarMimeType,
    [property: JsonPropertyName("public_avatar_image_path")] string? PublicAvatarImagePath,
    [property: JsonPropertyName("public_profile_updated_at_unix_ms")] long? PublicProfileUpdatedAtUnixMs,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs);

internal sealed record CreateAccountResponse(
    [property: JsonPropertyName("account")] LocalAccountSnapshot Account,
    [property: JsonPropertyName("initial_recovery_phrase_v2")] RecoveryPhraseV2 InitialRecoveryPhraseV2);

internal sealed record AccountExportManifestV1(
    [property: JsonPropertyName("version")] ushort Version,
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("device_label")] string DeviceLabel,
    [property: JsonPropertyName("current_device_id")] string CurrentDeviceId,
    [property: JsonPropertyName("account_representation_version")] ushort AccountRepresentationVersion,
    [property: JsonPropertyName("exported_at_unix_ms")] long ExportedAtUnixMs,
    [property: JsonPropertyName("last_backup_at_unix_ms")] long? LastBackupAtUnixMs = null);

internal sealed record AccountBackupInspection(
    [property: JsonPropertyName("manifest")] AccountExportManifestV1 Manifest,
    [property: JsonPropertyName("inspection_token")] string InspectionToken);

internal sealed record LocalProfileSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("account_representation_version")] ushort AccountRepresentationVersion);

internal sealed record ProfileGateSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("is_unlocked")] bool IsUnlocked,
    [property: JsonPropertyName("requires_unlock")] bool RequiresUnlock,
    [property: JsonPropertyName("destructive_transition_in_progress")] bool DestructiveTransitionInProgress);

internal sealed record ProfileLifecycleSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("is_unlocked")] bool IsUnlocked,
    [property: JsonPropertyName("requires_unlock")] bool RequiresUnlock,
    [property: JsonPropertyName("messaging_allowed")] bool MessagingAllowed,
    [property: JsonPropertyName("transport_allowed")] bool TransportAllowed,
    [property: JsonPropertyName("device_operations_allowed")] bool DeviceOperationsAllowed,
    [property: JsonPropertyName("sync_operations_allowed")] bool SyncOperationsAllowed,
    [property: JsonPropertyName("destructive_action_allowed")] bool DestructiveActionAllowed,
    [property: JsonPropertyName("destructive_transition_in_progress")] bool DestructiveTransitionInProgress);

internal sealed record LifecycleFastSnapshot(
    [property: JsonPropertyName("active_profile_id")] string? ActiveProfileId,
    [property: JsonPropertyName("local_account")] LocalAccountSnapshot? LocalAccount,
    [property: JsonPropertyName("local_device")] LocalDeviceSnapshot? LocalDevice,
    [property: JsonPropertyName("gate_summary")] ProfileGateSummary? GateSummary,
    [property: JsonPropertyName("lifecycle_summary")] ProfileLifecycleSummary? LifecycleSummary);

internal sealed record SavedProfileList(
    [property: JsonPropertyName("profiles")] IReadOnlyList<LocalProfileSummary> Profiles);

internal sealed record PasswordProtectionSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("enabled")] bool Enabled,
    [property: JsonPropertyName("algorithm")] string Algorithm,
    [property: JsonPropertyName("memory_kib")] uint MemoryKib,
    [property: JsonPropertyName("iterations")] uint Iterations,
    [property: JsonPropertyName("parallelism")] uint Parallelism,
    [property: JsonPropertyName("protection_model")] string ProtectionModel,
    [property: JsonPropertyName("protection_summary")] string ProtectionSummary);

internal sealed record LocalProfileFootprintSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("contacts_count")] int ContactsCount,
    [property: JsonPropertyName("sessions_count")] int SessionsCount,
    [property: JsonPropertyName("messages_count")] int MessagesCount,
    [property: JsonPropertyName("has_password_protection")] bool HasPasswordProtection);

internal sealed record RecoveryInputSummary(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("flow_kind")] string FlowKind,
    [property: JsonPropertyName("mnemonic_usage")] string MnemonicUsage,
    [property: JsonPropertyName("display_name")] string? DisplayName,
    [property: JsonPropertyName("device_label")] string? DeviceLabel,
    [property: JsonPropertyName("active_profile_id")] string? ActiveProfileId,
    [property: JsonPropertyName("existing_profile")] LocalProfileSummary? ExistingProfile,
    [property: JsonPropertyName("conflict")] string? Conflict,
    [property: JsonPropertyName("can_unlock")] bool CanUnlock,
    [property: JsonPropertyName("can_restore")] bool CanRestore,
    [property: JsonPropertyName("can_reopen")] bool CanReopen,
    [property: JsonPropertyName("summary")] string Summary);

internal sealed record DangerousActionPreview(
    [property: JsonPropertyName("profile_id")] string ProfileId,
    [property: JsonPropertyName("action")] string Action,
    [property: JsonPropertyName("title")] string Title,
    [property: JsonPropertyName("summary")] string Summary,
    [property: JsonPropertyName("consequences")] IReadOnlyList<string> Consequences,
    [property: JsonPropertyName("requires_confirmation")] bool RequiresConfirmation,
    [property: JsonPropertyName("confirmation_token")] string? ConfirmationToken,
    [property: JsonPropertyName("supported_now")] bool SupportedNow,
    [property: JsonPropertyName("unsupported_reason")] string? UnsupportedReason);

internal sealed record LocalDeviceSnapshot(
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("device_label")] string DeviceLabel,
    [property: JsonPropertyName("is_current")] bool IsCurrent,
    [property: JsonPropertyName("restored_from_recovery")] bool RestoredFromRecovery,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs);

/// Remote access with one contact, both directions (remote-access-for-contacts.md). Modes are
/// "ask" | "unattended" | null. GrantedMode = what WE let them do; ReceivedMode = what they told us
/// they let us do.
internal sealed record ContactRemoteAccessRecord(
    [property: JsonPropertyName("contact_id")] string ContactId,
    [property: JsonPropertyName("granted_mode")] string? GrantedMode,
    [property: JsonPropertyName("granted_changed_at_unix_ms")] long? GrantedChangedAtUnixMs,
    [property: JsonPropertyName("received_mode")] string? ReceivedMode,
    [property: JsonPropertyName("received_changed_at_unix_ms")] long? ReceivedChangedAtUnixMs,
    [property: JsonPropertyName("last_session_at_unix_ms")] long? LastSessionAtUnixMs);

/// One device of our own account. Revoked ones stay in the list, marked.
internal sealed record OwnDeviceView(
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("device_label")] string DeviceLabel,
    [property: JsonPropertyName("is_this_device")] bool IsThisDevice,
    [property: JsonPropertyName("revoked")] bool Revoked,
    [property: JsonPropertyName("first_seen_at_unix_ms")] long FirstSeenAtUnixMs,
    [property: JsonPropertyName("revoked_at_unix_ms")] long? RevokedAtUnixMs,
    [property: JsonPropertyName("online")] bool Online,
    [property: JsonPropertyName("is_new")] bool IsNew);

/// What moving the account to a new working key did.
internal sealed record AccountKeyMoveSummary(
    [property: JsonPropertyName("new_working_key_version")] ulong NewWorkingKeyVersion,
    [property: JsonPropertyName("kept_devices")] int KeptDevices);

internal sealed record ContactSnapshot(
    [property: JsonPropertyName("contact_id")] string ContactId,
    [property: JsonPropertyName("petname")] string Petname,
    [property: JsonPropertyName("remote_account_id")] string? RemoteAccountId,
    [property: JsonPropertyName("public_display_name")] string? PublicDisplayName,
    [property: JsonPropertyName("public_avatar_hash_hex")] string? PublicAvatarHashHex,
    [property: JsonPropertyName("public_avatar_mime_type")] string? PublicAvatarMimeType,
    [property: JsonPropertyName("public_avatar_image_path")] string? PublicAvatarImagePath,
    [property: JsonPropertyName("public_avatar_pending_hash_hex")] string? PublicAvatarPendingHashHex,
    [property: JsonPropertyName("public_avatar_pending_mime_type")] string? PublicAvatarPendingMimeType,
    [property: JsonPropertyName("public_profile_updated_at_unix_ms")] long? PublicProfileUpdatedAtUnixMs,
    [property: JsonPropertyName("remote_instance_id")] string? RemoteInstanceId,
    [property: JsonPropertyName("primary_dialog_key")] string? PrimaryDialogKey,
    [property: JsonPropertyName("relationship_state")] string RelationshipState,
    [property: JsonPropertyName("pending_request_direction")] string? PendingRequestDirection,
    [property: JsonPropertyName("has_met")] bool HasMet,
    [property: JsonPropertyName("friend_relay_consent")] bool FriendRelayConsent,
    [property: JsonPropertyName("is_blocked")] bool IsBlocked,
    [property: JsonPropertyName("presence_state")] string PresenceState,
    [property: JsonPropertyName("presence_last_seen_at_unix_ms")] long? PresenceLastSeenAtUnixMs,
    [property: JsonPropertyName("presence_last_sent_at_unix_ms")] long? PresenceLastSentAtUnixMs,
    [property: JsonPropertyName("presence_last_activity_at_unix_ms")] long? PresenceLastActivityAtUnixMs,
    [property: JsonPropertyName("removed_at_unix_ms")] long? RemovedAtUnixMs,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs);

internal sealed record VerificationSummary(
    [property: JsonPropertyName("last_verified_at_unix_ms")] long? LastVerifiedAtUnixMs,
    [property: JsonPropertyName("verification_method")] string? VerificationMethod,
    [property: JsonPropertyName("current_identity_key")] string? CurrentIdentityKey,
    [property: JsonPropertyName("verified_identity_key")] string? VerifiedIdentityKey,
    // The contact's devices we know: certified by its account's master key, not revoked. Shown, not
    // judged — the safety number is of the account alone (docs/bugs/safety-number-differs-per-own-device.md).
    [property: JsonPropertyName("device_count")] int DeviceCount,
    [property: JsonPropertyName("current_fingerprint")] string? CurrentFingerprint,
    [property: JsonPropertyName("current_safety_number")] string? CurrentSafetyNumber,
    [property: JsonPropertyName("verified_fingerprint")] string? VerifiedFingerprint,
    [property: JsonPropertyName("verified_safety_number")] string? VerifiedSafetyNumber);

internal sealed record ContactCardSnapshot(
    [property: JsonPropertyName("contact")] ContactSnapshot Contact,
    [property: JsonPropertyName("trust_state")] string TrustState,
    [property: JsonPropertyName("verification")] VerificationSummary Verification);

internal sealed record ContactsScreenSnapshot(
    [property: JsonPropertyName("refresh_summary")] ContactsRefreshSummary RefreshSummary,
    [property: JsonPropertyName("contacts")] IReadOnlyList<ContactSnapshot> Contacts);

internal sealed partial class RustBridge
{
    internal sealed record CreateAccountRequest(
        [property: JsonPropertyName("display_name")] string DisplayName,
        [property: JsonPropertyName("device_label")] string DeviceLabel);

    internal sealed record UpdateLocalPublicProfileRequest(
        [property: JsonPropertyName("display_name")] string DisplayName,
        [property: JsonPropertyName("avatar_source_path")] string? AvatarSourcePath,
        [property: JsonPropertyName("clear_avatar")] bool ClearAvatar);

    internal sealed record RestoreAccountFromInputRequest(
        [property: JsonPropertyName("display_name")] string DisplayName,
        [property: JsonPropertyName("mnemonic")] string Mnemonic,
        [property: JsonPropertyName("device_label")] string DeviceLabel);

    internal sealed record RestoreAccountFromExportManifestRequest(
        [property: JsonPropertyName("manifest")] AccountExportManifestV1 Manifest,
        [property: JsonPropertyName("mnemonic")] string Mnemonic);

    internal sealed record ExportCurrentDatabaseBackupRequest(
        [property: JsonPropertyName("destination_path")] string DestinationPath);

    internal sealed record InspectAccountBackupManifestRequest(
        [property: JsonPropertyName("backup_db_path")] string BackupDbPath,
        [property: JsonPropertyName("mnemonic")] string Mnemonic);

    internal sealed record ImportDatabaseBackupRequest(
        [property: JsonPropertyName("backup_db_path")] string BackupDbPath,
        [property: JsonPropertyName("inspection_token")] string InspectionToken);

    internal sealed record InspectRecoveryInputRequest(
        [property: JsonPropertyName("mnemonic")] string Mnemonic,
        [property: JsonPropertyName("display_name")] string? DisplayName,
        [property: JsonPropertyName("device_label")] string? DeviceLabel);

    internal sealed record LocalProfileRequest([property: JsonPropertyName("profile_id")] string ProfileId);

    internal sealed record UnlockProfileWithPasswordRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("password")] string Password);

    internal sealed record UnlockProfileWithMnemonicRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("mnemonic")] string Mnemonic);

    internal sealed record UnlockCurrentProfileWithPasswordRequest(
        [property: JsonPropertyName("password")] string Password);

    internal sealed record RevealCurrentProfileRecoveryPhraseWithPasswordRequest(
        [property: JsonPropertyName("password")] string Password);

    internal sealed record UnlockCurrentProfileWithMnemonicRequest(
        [property: JsonPropertyName("mnemonic")] string Mnemonic);

    internal sealed record SetLocalProfilePasswordRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("password")] string Password);

    internal sealed record ChangeLocalProfilePasswordRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("current_password")] string CurrentPassword,
        [property: JsonPropertyName("new_password")] string NewPassword);

    internal sealed record RemoveLocalProfilePasswordRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("current_password")] string CurrentPassword);

    internal sealed record ConfirmCurrentProfileActionRequest(
        [property: JsonPropertyName("confirmation_token")] string? ConfirmationToken);

    internal sealed record ConfirmSignOutProfileRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("confirmation_token")] string? ConfirmationToken);

    internal sealed record ConfirmDeleteLocalProfileRequest(
        [property: JsonPropertyName("profile_id")] string ProfileId,
        [property: JsonPropertyName("confirmation_token")] string? ConfirmationToken);

    internal sealed record AddContactByIdentifierRequest(
        [property: JsonPropertyName("account_id")] string AccountId,
        [property: JsonPropertyName("petname")] string Petname,
        [property: JsonPropertyName("public_display_name")] string? PublicDisplayName);

    internal sealed record RequestContactByIdentifierRequest(
        [property: JsonPropertyName("account_id")] string AccountId,
        [property: JsonPropertyName("petname")] string Petname,
        [property: JsonPropertyName("public_display_name")] string? PublicDisplayName);

    internal sealed record AddContactFromPayloadRequest(
        [property: JsonPropertyName("payload")] ImportableContactPayloadV1 Payload,
        [property: JsonPropertyName("petname")] string Petname);

    internal sealed record ContactIdRequest([property: JsonPropertyName("contact_id")] string ContactId);
    internal sealed record SetContactFriendRelayConsentRequest(
        [property: JsonPropertyName("contact_id")] string ContactId,
        [property: JsonPropertyName("consent")] bool Consent);

    internal sealed record MoveAccountKeyRequest(
        [property: JsonPropertyName("keep_device_ids")] IReadOnlyList<string> KeepDeviceIds,
        [property: JsonPropertyName("phrase")] string? Phrase,
        [property: JsonPropertyName("password")] string? Password);

    internal sealed record HideRevokedOwnDevicesRequest(
        [property: JsonPropertyName("device_id")] string? DeviceId);

    internal sealed record RevokeOwnDeviceRequest(
        [property: JsonPropertyName("device_id")] string DeviceId,
        [property: JsonPropertyName("password")] string? Password);

    // Remote access (remote-access-for-contacts.md): Mode is "ask" | "unattended" | null (revoke).
    internal sealed record SetContactRemoteAccessGrantedRequest(
        [property: JsonPropertyName("contact_id")] string ContactId,
        [property: JsonPropertyName("mode")] string? Mode);

    internal sealed record RenameContactRequest(
        [property: JsonPropertyName("contact_id")] string ContactId,
        [property: JsonPropertyName("petname")] string Petname);

    internal sealed record ConfirmContactVerificationRequest(
        [property: JsonPropertyName("contact_id")] string ContactId,
        [property: JsonPropertyName("payload")] ContactVerificationPayloadV1 Payload,
        [property: JsonPropertyName("method")] string Method);
}
