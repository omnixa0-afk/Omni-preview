using System.Text.Json;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public RecoveryInputSummary InspectRecoveryInput(
        string mnemonic,
        string? displayName,
        string? deviceLabel)
        => CallJson(
            () => NativeMethods.omni_identity_inspect_recovery_input(
                handle,
                JsonSerializer.Serialize(
                    new InspectRecoveryInputRequest(mnemonic, displayName, deviceLabel),
                    OmniJsonSerializerContext.Default.InspectRecoveryInputRequest)),
            OmniJsonSerializerContext.Default.RecoveryInputSummary,
            "identity_inspect_recovery_input");

    public LocalAccountSnapshot RestoreLocalProfileFromMnemonic(
        string displayName,
        string mnemonic,
        string deviceLabel)
        => CallJson(
            () => NativeMethods.omni_identity_restore_account_from_input(
                handle,
                JsonSerializer.Serialize(
                    new RestoreAccountFromInputRequest(displayName, mnemonic, deviceLabel),
                    OmniJsonSerializerContext.Default.RestoreAccountFromInputRequest)),
            OmniJsonSerializerContext.Default.LocalAccountSnapshot,
            "identity_restore_account_from_input");

    public AccountExportManifestV1? GetCurrentAccountExportManifest()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_current_account_export_manifest(handle),
            OmniJsonSerializerContext.Default.AccountExportManifestV1,
            "identity_get_current_account_export_manifest");

    public void ExportCurrentDatabaseBackup(string destinationPath)
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_export_current_database_backup(
                handle,
                JsonSerializer.Serialize(
                    new ExportCurrentDatabaseBackupRequest(destinationPath),
                    OmniJsonSerializerContext.Default.ExportCurrentDatabaseBackupRequest)),
            "identity_export_current_database_backup");
        ThrowIfRustError(payload, "identity_export_current_database_backup");
    }

    public AccountBackupInspection InspectAccountBackupManifest(
        string backupDatabasePath,
        string mnemonic)
        => CallJsonNoHandle(
            () => NativeMethods.omni_identity_inspect_account_backup_manifest(
                JsonSerializer.Serialize(
                    new InspectAccountBackupManifestRequest(backupDatabasePath, mnemonic),
                    OmniJsonSerializerContext.Default.InspectAccountBackupManifestRequest)),
            OmniJsonSerializerContext.Default.AccountBackupInspection,
            "identity_inspect_account_backup_manifest");

    public LocalAccountSnapshot RestoreAccountFromExportManifest(
        AccountExportManifestV1 manifest,
        string mnemonic)
        => CallJson(
            () => NativeMethods.omni_identity_restore_account_from_export_manifest(
                handle,
                JsonSerializer.Serialize(
                    new RestoreAccountFromExportManifestRequest(manifest, mnemonic),
                    OmniJsonSerializerContext.Default.RestoreAccountFromExportManifestRequest)),
            OmniJsonSerializerContext.Default.LocalAccountSnapshot,
            "identity_restore_account_from_export_manifest");

    public LocalAccountSnapshot RestoreAccountFromInput(
        string displayName,
        string mnemonic,
        string deviceLabel)
        => RestoreLocalProfileFromMnemonic(displayName, mnemonic, deviceLabel);

    public string? GetActiveProfileId()
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_get_active_profile(handle),
            "identity_get_active_profile");
        ThrowIfRustError(payload, "identity_get_active_profile");
        return payload switch
        {
            "null" => null,
            _ => JsonSerializer.Deserialize(payload, OmniJsonSerializerContext.Default.String),
        };
    }

    public void SelectActiveProfile(string profileId)
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_select_active_profile(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            "identity_select_active_profile");
        ThrowIfRustError(payload, "identity_select_active_profile");
    }

    public void ClearActiveProfile()
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_clear_active_profile(handle),
            "identity_clear_active_profile");
        ThrowIfRustError(payload, "identity_clear_active_profile");
    }

    public SavedProfileList ListLocalProfiles()
        => CallJson(
            () => NativeMethods.omni_identity_list_local_profiles(handle),
            OmniJsonSerializerContext.Default.SavedProfileList,
            "identity_list_local_profiles");

    public LocalProfileSummary? GetLocalProfileSummary(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_local_profile_summary(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_get_local_profile_summary");

    public LocalProfileSummary ReopenLocalProfile(string profileId)
        => CallJson(
            () => NativeMethods.omni_identity_reopen_local_profile(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_reopen_local_profile");

    public ProfileGateSummary? GetCurrentProfileGateSummary()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_current_profile_gate_summary(handle),
            OmniJsonSerializerContext.Default.ProfileGateSummary,
            "identity_get_current_profile_gate_summary");

    public ProfileLifecycleSummary? GetCurrentProfileLifecycleSummary()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_current_profile_lifecycle_summary(handle),
            OmniJsonSerializerContext.Default.ProfileLifecycleSummary,
            "identity_get_current_profile_lifecycle_summary");

    public LifecycleFastSnapshot GetLifecycleFastSnapshot()
        => CallJson(
            () => NativeMethods.omni_identity_get_lifecycle_fast_snapshot(handle),
            OmniJsonSerializerContext.Default.LifecycleFastSnapshot,
            "identity_get_lifecycle_fast_snapshot");

    public LocalProfileSummary SignOutProfile(string profileId)
        => CallJson(
            () => NativeMethods.omni_identity_sign_out_profile(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_sign_out_profile");

    public DangerousActionPreview? PrepareSignOutCurrentProfile()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_prepare_sign_out_current_profile(handle),
            OmniJsonSerializerContext.Default.DangerousActionPreview,
            "identity_prepare_sign_out_current_profile");

    public LocalProfileSummary ConfirmSignOutCurrentProfile(string? confirmationToken)
        => CallJson(
            () => NativeMethods.omni_identity_confirm_sign_out_current_profile(
                handle,
                JsonSerializer.Serialize(
                    new ConfirmCurrentProfileActionRequest(confirmationToken),
                    OmniJsonSerializerContext.Default.ConfirmCurrentProfileActionRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_confirm_sign_out_current_profile");

    public LocalProfileSummary ConfirmSignOutProfile(string profileId, string? confirmationToken)
        => CallJson(
            () => NativeMethods.omni_identity_confirm_sign_out_profile(
                handle,
                JsonSerializer.Serialize(
                    new ConfirmSignOutProfileRequest(profileId, confirmationToken),
                    OmniJsonSerializerContext.Default.ConfirmSignOutProfileRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_confirm_sign_out_profile");

    public LocalProfileSummary UnlockProfileWithPassword(string profileId, string password)
        => CallJson(
            () => NativeMethods.omni_identity_unlock_profile_with_password(
                handle,
                JsonSerializer.Serialize(
                    new UnlockProfileWithPasswordRequest(profileId, password),
                    OmniJsonSerializerContext.Default.UnlockProfileWithPasswordRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_unlock_profile_with_password");

    public LocalProfileSummary UnlockCurrentProfileWithPassword(string password)
        => CallJson(
            () => NativeMethods.omni_identity_unlock_current_profile_with_password(
                handle,
                JsonSerializer.Serialize(
                    new UnlockCurrentProfileWithPasswordRequest(password),
                    OmniJsonSerializerContext.Default.UnlockCurrentProfileWithPasswordRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_unlock_current_profile_with_password");

    public RecoveryPhraseV2 RevealCurrentProfileRecoveryPhraseWithPassword(string password)
        => CallJson(
            () => NativeMethods.omni_identity_reveal_current_profile_recovery_phrase_with_password(
                handle,
                JsonSerializer.Serialize(
                    new RevealCurrentProfileRecoveryPhraseWithPasswordRequest(password),
                    OmniJsonSerializerContext.Default.RevealCurrentProfileRecoveryPhraseWithPasswordRequest)),
            OmniJsonSerializerContext.Default.RecoveryPhraseV2,
            "identity_reveal_current_profile_recovery_phrase_with_password");

    public LocalProfileSummary UnlockProfileWithMnemonic(string profileId, string mnemonic)
        => CallJson(
            () => NativeMethods.omni_identity_unlock_profile_with_mnemonic(
                handle,
                JsonSerializer.Serialize(
                    new UnlockProfileWithMnemonicRequest(profileId, mnemonic),
                    OmniJsonSerializerContext.Default.UnlockProfileWithMnemonicRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_unlock_profile_with_mnemonic");

    public LocalProfileSummary UnlockCurrentProfileWithMnemonic(string mnemonic)
        => CallJson(
            () => NativeMethods.omni_identity_unlock_current_profile_with_mnemonic(
                handle,
                JsonSerializer.Serialize(
                    new UnlockCurrentProfileWithMnemonicRequest(mnemonic),
                    OmniJsonSerializerContext.Default.UnlockCurrentProfileWithMnemonicRequest)),
            OmniJsonSerializerContext.Default.LocalProfileSummary,
            "identity_unlock_current_profile_with_mnemonic");

    public PasswordProtectionSummary SetLocalProfilePassword(string profileId, string password)
        => CallJson(
            () => NativeMethods.omni_identity_set_local_profile_password(
                handle,
                JsonSerializer.Serialize(
                    new SetLocalProfilePasswordRequest(profileId, password),
                    OmniJsonSerializerContext.Default.SetLocalProfilePasswordRequest)),
            OmniJsonSerializerContext.Default.PasswordProtectionSummary,
            "identity_set_local_profile_password");

    public PasswordProtectionSummary ChangeLocalProfilePassword(
        string profileId,
        string currentPassword,
        string newPassword)
        => CallJson(
            () => NativeMethods.omni_identity_change_local_profile_password(
                handle,
                JsonSerializer.Serialize(
                    new ChangeLocalProfilePasswordRequest(profileId, currentPassword, newPassword),
                    OmniJsonSerializerContext.Default.ChangeLocalProfilePasswordRequest)),
            OmniJsonSerializerContext.Default.PasswordProtectionSummary,
            "identity_change_local_profile_password");

    public PasswordProtectionSummary RemoveLocalProfilePassword(string profileId, string currentPassword)
        => CallJson(
            () => NativeMethods.omni_identity_remove_local_profile_password(
                handle,
                JsonSerializer.Serialize(
                    new RemoveLocalProfilePasswordRequest(profileId, currentPassword),
                    OmniJsonSerializerContext.Default.RemoveLocalProfilePasswordRequest)),
            OmniJsonSerializerContext.Default.PasswordProtectionSummary,
            "identity_remove_local_profile_password");

    public PasswordProtectionSummary? GetPasswordProtectionSummary(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_password_protection_summary(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.PasswordProtectionSummary,
            "identity_get_password_protection_summary");

    public LocalProfileFootprintSummary? GetLocalProfileFootprintSummary(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_local_profile_footprint_summary(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.LocalProfileFootprintSummary,
            "identity_get_local_profile_footprint_summary");

    public DangerousActionPreview? PreviewSignOutProfile(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_preview_sign_out_profile(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.DangerousActionPreview,
            "identity_preview_sign_out_profile");

    public DangerousActionPreview? PreviewDeleteLocalProfile(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_preview_delete_local_profile(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.DangerousActionPreview,
            "identity_preview_delete_local_profile");

    public DangerousActionPreview? PrepareDeleteCurrentProfile()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_prepare_delete_current_profile(handle),
            OmniJsonSerializerContext.Default.DangerousActionPreview,
            "identity_prepare_delete_current_profile");

    public void ConfirmDeleteCurrentProfile(string? confirmationToken)
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_confirm_delete_current_profile(
                handle,
                JsonSerializer.Serialize(
                    new ConfirmCurrentProfileActionRequest(confirmationToken),
                    OmniJsonSerializerContext.Default.ConfirmCurrentProfileActionRequest)),
            "identity_confirm_delete_current_profile");
        ThrowIfRustError(payload, "identity_confirm_delete_current_profile");
    }

    public void ConfirmDeleteLocalProfile(string profileId, string? confirmationToken)
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_confirm_delete_local_profile(
                handle,
                JsonSerializer.Serialize(
                    new ConfirmDeleteLocalProfileRequest(profileId, confirmationToken),
                    OmniJsonSerializerContext.Default.ConfirmDeleteLocalProfileRequest)),
            "identity_confirm_delete_local_profile");
        ThrowIfRustError(payload, "identity_confirm_delete_local_profile");
    }

    public DangerousActionPreview? PreviewRevokeCurrentDeviceAndWipe(string profileId)
        => CallOptionalJson(
            () => NativeMethods.omni_identity_preview_revoke_current_device_and_wipe(
                handle,
                JsonSerializer.Serialize(
                    new LocalProfileRequest(profileId),
                    OmniJsonSerializerContext.Default.LocalProfileRequest)),
            OmniJsonSerializerContext.Default.DangerousActionPreview,
            "identity_preview_revoke_current_device_and_wipe");
}
