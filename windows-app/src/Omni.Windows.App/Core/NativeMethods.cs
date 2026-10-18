using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private const string LibraryFileName = "omni_ffi.dll";
    private static readonly object SyncRoot = new();
    private static IntPtr libraryHandle = IntPtr.Zero;
    private static OmniChatSendTextDelegate? omniChatSendText;
    private static OmniChatSendAttachmentDelegate? omniChatSendAttachment;
    private static OmniChatRequestAttachmentDownloadDelegate? omniChatRequestAttachmentDownload;
    private static OmniChatRevokeAttachmentAccessDelegate? omniChatRevokeAttachmentAccess;
    private static OmniChatListMessagesDelegate? omniChatListMessages;
    private static OmniChatListMessagesPageDelegate? omniChatListMessagesPage;
    private static OmniIdentityGetCurrentAccountExportManifestDelegate? omniIdentityGetCurrentAccountExportManifest;
    private static OmniIdentityCreateAccountDelegate? omniIdentityCreateAccount;
    private static OmniIdentityRestoreAccountFromExportManifestDelegate? omniIdentityRestoreAccountFromExportManifest;
    private static OmniIdentityRestoreAccountFromInputDelegate? omniIdentityRestoreAccountFromInput;
    private static OmniIdentityInspectRecoveryInputDelegate? omniIdentityInspectRecoveryInput;
    private static OmniIdentityGetLocalAccountDelegate? omniIdentityGetLocalAccount;
    private static OmniIdentityUpdateLocalPublicProfileDelegate? omniIdentityUpdateLocalPublicProfile;
    private static OmniIdentityGetLocalDeviceDelegate? omniIdentityGetLocalDevice;
    private static OmniIdentityGetLocalPrekeyBundleDelegate? omniIdentityGetLocalPrekeyBundle;
    private static OmniIdentityStoreRemotePrekeyBundleDelegate? omniIdentityStoreRemotePrekeyBundle;
    private static OmniIdentityListLocalProfilesDelegate? omniIdentityListLocalProfiles;
    private static OmniIdentityGetLocalProfileSummaryDelegate? omniIdentityGetLocalProfileSummary;
    private static OmniIdentityGetActiveProfileDelegate? omniIdentityGetActiveProfile;
    private static OmniIdentitySelectActiveProfileDelegate? omniIdentitySelectActiveProfile;
    private static OmniIdentityClearActiveProfileDelegate? omniIdentityClearActiveProfile;
    private static OmniIdentityReopenLocalProfileDelegate? omniIdentityReopenLocalProfile;
    private static OmniIdentityGetCurrentProfileGateSummaryDelegate? omniIdentityGetCurrentProfileGateSummary;
    private static OmniIdentityGetCurrentProfileLifecycleSummaryDelegate? omniIdentityGetCurrentProfileLifecycleSummary;
    private static OmniIdentityGetLifecycleFastSnapshotDelegate? omniIdentityGetLifecycleFastSnapshot;
    private static OmniIdentityPrepareSignOutCurrentProfileDelegate? omniIdentityPrepareSignOutCurrentProfile;
    private static OmniIdentityConfirmSignOutCurrentProfileDelegate? omniIdentityConfirmSignOutCurrentProfile;
    private static OmniIdentityConfirmSignOutProfileDelegate? omniIdentityConfirmSignOutProfile;
    private static OmniIdentitySignOutProfileDelegate? omniIdentitySignOutProfile;
    private static OmniIdentityUnlockCurrentProfileWithPasswordDelegate? omniIdentityUnlockCurrentProfileWithPassword;
    private static OmniIdentityRevealCurrentProfileRecoveryPhraseWithPasswordDelegate? omniIdentityRevealCurrentProfileRecoveryPhraseWithPassword;
    private static OmniIdentityUnlockCurrentProfileWithMnemonicDelegate? omniIdentityUnlockCurrentProfileWithMnemonic;
    private static OmniIdentityUnlockProfileWithPasswordDelegate? omniIdentityUnlockProfileWithPassword;
    private static OmniIdentityUnlockProfileWithMnemonicDelegate? omniIdentityUnlockProfileWithMnemonic;
    private static OmniIdentitySetLocalProfilePasswordDelegate? omniIdentitySetLocalProfilePassword;
    private static OmniIdentityChangeLocalProfilePasswordDelegate? omniIdentityChangeLocalProfilePassword;
    private static OmniIdentityRemoveLocalProfilePasswordDelegate? omniIdentityRemoveLocalProfilePassword;
    private static OmniIdentityGetPasswordProtectionSummaryDelegate? omniIdentityGetPasswordProtectionSummary;
    private static OmniIdentityGetLocalProfileFootprintSummaryDelegate? omniIdentityGetLocalProfileFootprintSummary;
    private static OmniIdentityPreviewSignOutProfileDelegate? omniIdentityPreviewSignOutProfile;
    private static OmniIdentityPreviewDeleteLocalProfileDelegate? omniIdentityPreviewDeleteLocalProfile;
    private static OmniIdentityPrepareDeleteCurrentProfileDelegate? omniIdentityPrepareDeleteCurrentProfile;
    private static OmniIdentityConfirmDeleteLocalProfileDelegate? omniIdentityConfirmDeleteLocalProfile;
    private static OmniIdentityConfirmDeleteCurrentProfileDelegate? omniIdentityConfirmDeleteCurrentProfile;
    private static OmniIdentityPreviewRevokeCurrentDeviceAndWipeDelegate? omniIdentityPreviewRevokeCurrentDeviceAndWipe;
    private static OmniIdentityDeleteLocalProfileDelegate? omniIdentityDeleteLocalProfile;
    private static OmniContactsListDelegate? omniContactsList;
    private static OmniContactsGetRefreshSummaryDelegate? omniContactsGetRefreshSummary;
    private static OmniContactsGetScreenSnapshotDelegate? omniContactsGetScreenSnapshot;
    private static OmniContactsAddByIdentifierDelegate? omniContactsAddByIdentifier;
    private static OmniContactsRequestByIdentifierDelegate? omniContactsRequestByIdentifier;
    private static OmniContactsAddFromPayloadDelegate? omniContactsAddFromPayload;
    private static OmniContactsRenameDelegate? omniContactsRename;
    private static OmniContactsBlockDelegate? omniContactsBlock;
    private static OmniContactsApproveDelegate? omniContactsApprove;
    private static OmniContactsSetFriendRelayConsentDelegate? omniContactsSetFriendRelayConsent;
    private static OmniContactsSetFriendRelayConsentDelegate? omniContactsGetRemoteAccess;
    private static OmniContactsSetFriendRelayConsentDelegate? omniContactsSetRemoteAccessGranted;
    private static OmniContactsListDelegate? omniContactsListRemoteAccess;
    private static OmniContactsListDelegate? omniAccountDevicesList;
    private static OmniContactsListDelegate? omniAccountDevicesAcknowledge;
    private static OmniContactsSetFriendRelayConsentDelegate? omniAccountMoveWorkingKey;
    private static OmniContactsSetFriendRelayConsentDelegate? omniAccountDeviceRevoke;
    private static OmniContactsSetFriendRelayConsentDelegate? omniAccountDevicesHideRevoked;
    private static OmniContactsDeclineDelegate? omniContactsDecline;
    private static OmniContactsForgetDelegate? omniContactsForget;
    private static OmniContactsForgetAndResetPeerDelegate? omniContactsForgetAndResetPeer;
    private static OmniContactsGetCardDelegate? omniContactsGetCard;
    private static OmniContactsGetCardRevisionDelegate? omniContactsGetCardRevision;
    private static OmniIdentityGetMyVerificationPayloadDelegate? omniIdentityGetMyVerificationPayload;
    private static OmniContactsGetVerificationStateDelegate? omniContactsGetVerificationState;
    private static OmniContactsConfirmVerificationDelegate? omniContactsConfirmVerification;
    private static OmniChatGetHeaderSummaryDelegate? omniChatGetHeaderSummary;
    private static OmniChatGetScreenSnapshotDelegate? omniChatGetScreenSnapshot;
    private static OmniChatOpenSessionLeaseDelegate? omniChatOpenSessionLease;
    private static OmniChatOpenSessionLeaseDelegate? omniChatMarkRead;
    private static OmniChatEndSessionDelegate? omniChatEndSession;
    private static OmniChatListThreadProjectionsDelegate? omniChatListThreadProjections;
    private static OmniChatGetThreadListRefreshSummaryDelegate? omniChatGetThreadListRefreshSummary;
    private static OmniChatGetSecuritySummaryDelegate? omniChatGetSecuritySummary;
    private static OmniChatGetMessageRefreshSummaryDelegate? omniChatGetMessageRefreshSummary;
    private static OmniChatGetMessageDeltaDelegate? omniChatGetMessageDelta;
    private static OmniChatGetMessageSecurityDetailsDelegate? omniChatGetMessageSecurityDetails;
    private static OmniChatGetMessageSecurityDetailsBatchDelegate? omniChatGetMessageSecurityDetailsBatch;
    private static OmniContactsStartChatDelegate? omniContactsStartChat;
    private static OmniContactsQuickAddFromDialogDelegate? omniContactsQuickAddFromDialog;
    private static OmniContactsAttachDialogToContactDelegate? omniContactsAttachDialogToContact;
    private static OmniStringFreeDelegate? omniStringFree;
    private static OmniIdentityExportCurrentDatabaseBackupDelegate? omniIdentityExportCurrentDatabaseBackup;
    private static OmniIdentityExportCurrentDatabaseBackupBundleDelegate? omniIdentityExportCurrentDatabaseBackupBundle;
    private static OmniIdentityInspectAccountBackupManifestDelegate? omniIdentityInspectAccountBackupManifest;

    internal static string? LoadedLibraryPath { get; private set; }

    internal static string LastLoadDiagnostic { get; private set; } = string.Empty;

    private static void LoadExport<TDelegate>(ref TDelegate? target, string exportName)
        where TDelegate : Delegate
        => target = GetExport<TDelegate>(exportName);

    internal static void EnsureLoaded()
    {
        if (libraryHandle != IntPtr.Zero)
        {
            return;
        }

        lock (SyncRoot)
        {
            if (libraryHandle != IntPtr.Zero)
            {
                return;
            }

            libraryHandle = LoadLibraryOrThrow();
            LoadCoreAppExports();
            LoadTransportDiscoveryExports();
            LoadAccountContactExports();
            LoadChatDeviceExports();
            LoadCallExports();
            LoadRealtimeMediaExports();
            LoadMeshExports();
            LoadGroupExports();
            LoadDiscoverabilityExports();
            LoadScreenControlExports();
            LoadMediaFileExports();
            LoadExport(ref omniStringFree, nameof(omni_string_free));
        }
    }

    internal static IntPtr omni_chat_send_text(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatSendText!(handle, requestJson);
    }

    internal static IntPtr omni_chat_send_attachment(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatSendAttachment!(handle, requestJson);
    }

    internal static IntPtr omni_chat_request_attachment_download(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatRequestAttachmentDownload!(handle, requestJson);
    }

    internal static IntPtr omni_chat_revoke_attachment_access(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatRevokeAttachmentAccess!(handle, requestJson);
    }

    internal static IntPtr omni_chat_list_messages(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatListMessages!(handle, requestJson);
    }

    internal static IntPtr omni_chat_list_messages_page(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatListMessagesPage!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_current_account_export_manifest(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetCurrentAccountExportManifest!(handle);
    }

    internal static IntPtr omni_identity_export_current_database_backup(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityExportCurrentDatabaseBackup!(handle, requestJson);
    }

    internal static IntPtr omni_identity_export_current_database_backup_bundle(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityExportCurrentDatabaseBackupBundle!(handle, requestJson);
    }

    internal static IntPtr omni_identity_inspect_account_backup_manifest(string requestJson)
    {
        EnsureLoaded();
        return omniIdentityInspectAccountBackupManifest!(requestJson);
    }

    internal static IntPtr omni_identity_create_account(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityCreateAccount!(handle, requestJson);
    }

    internal static IntPtr omni_identity_restore_account_from_export_manifest(
        IntPtr handle,
        string requestJson)
    {
        EnsureLoaded();
        return omniIdentityRestoreAccountFromExportManifest!(handle, requestJson);
    }

    internal static IntPtr omni_identity_restore_account_from_input(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityRestoreAccountFromInput!(handle, requestJson);
    }

    internal static IntPtr omni_identity_inspect_recovery_input(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityInspectRecoveryInput!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_local_account(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetLocalAccount!(handle);
    }

    internal static IntPtr omni_identity_update_local_public_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityUpdateLocalPublicProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_local_device(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetLocalDevice!(handle);
    }

    internal static IntPtr omni_identity_get_local_prekey_bundle(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetLocalPrekeyBundle!(handle);
    }

    internal static IntPtr omni_identity_store_remote_prekey_bundle(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityStoreRemotePrekeyBundle!(handle, requestJson);
    }

    internal static IntPtr omni_identity_list_local_profiles(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityListLocalProfiles!(handle);
    }

    internal static IntPtr omni_identity_get_local_profile_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityGetLocalProfileSummary!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_active_profile(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetActiveProfile!(handle);
    }

    internal static IntPtr omni_identity_select_active_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentitySelectActiveProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_clear_active_profile(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityClearActiveProfile!(handle);
    }

    internal static IntPtr omni_identity_reopen_local_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityReopenLocalProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_current_profile_gate_summary(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetCurrentProfileGateSummary!(handle);
    }

    internal static IntPtr omni_identity_get_current_profile_lifecycle_summary(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetCurrentProfileLifecycleSummary!(handle);
    }

    internal static IntPtr omni_identity_get_lifecycle_fast_snapshot(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetLifecycleFastSnapshot!(handle);
    }

    internal static IntPtr omni_identity_prepare_sign_out_current_profile(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityPrepareSignOutCurrentProfile!(handle);
    }

    internal static IntPtr omni_identity_confirm_sign_out_current_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityConfirmSignOutCurrentProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_confirm_sign_out_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityConfirmSignOutProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_sign_out_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentitySignOutProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_unlock_profile_with_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityUnlockProfileWithPassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_unlock_current_profile_with_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityUnlockCurrentProfileWithPassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_reveal_current_profile_recovery_phrase_with_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityRevealCurrentProfileRecoveryPhraseWithPassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_unlock_profile_with_mnemonic(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityUnlockProfileWithMnemonic!(handle, requestJson);
    }

    internal static IntPtr omni_identity_unlock_current_profile_with_mnemonic(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityUnlockCurrentProfileWithMnemonic!(handle, requestJson);
    }

    internal static IntPtr omni_identity_set_local_profile_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentitySetLocalProfilePassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_change_local_profile_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityChangeLocalProfilePassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_remove_local_profile_password(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityRemoveLocalProfilePassword!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_password_protection_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityGetPasswordProtectionSummary!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_local_profile_footprint_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityGetLocalProfileFootprintSummary!(handle, requestJson);
    }

    internal static IntPtr omni_identity_preview_sign_out_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityPreviewSignOutProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_preview_delete_local_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityPreviewDeleteLocalProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_prepare_delete_current_profile(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityPrepareDeleteCurrentProfile!(handle);
    }

    internal static IntPtr omni_identity_confirm_delete_local_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityConfirmDeleteLocalProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_confirm_delete_current_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityConfirmDeleteCurrentProfile!(handle, requestJson);
    }

    internal static IntPtr omni_identity_preview_revoke_current_device_and_wipe(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityPreviewRevokeCurrentDeviceAndWipe!(handle, requestJson);
    }

    internal static IntPtr omni_identity_delete_local_profile(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniIdentityDeleteLocalProfile!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_list(IntPtr handle)
    {
        EnsureLoaded();
        return omniContactsList!(handle);
    }

    internal static IntPtr omni_contacts_get_refresh_summary(IntPtr handle)
    {
        EnsureLoaded();
        return omniContactsGetRefreshSummary!(handle);
    }

    internal static IntPtr omni_contacts_get_screen_snapshot(IntPtr handle)
    {
        EnsureLoaded();
        return omniContactsGetScreenSnapshot!(handle);
    }

    internal static IntPtr omni_contacts_add_by_identifier(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsAddByIdentifier!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_request_by_identifier(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsRequestByIdentifier!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_add_from_payload(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsAddFromPayload!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_rename(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsRename!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_block(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsBlock!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_approve(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsApprove!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_set_friend_relay_consent(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsSetFriendRelayConsent!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_get_remote_access(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsGetRemoteAccess!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_list_remote_access(IntPtr handle)
    {
        EnsureLoaded();
        return omniContactsListRemoteAccess!(handle);
    }

    internal static IntPtr omni_contacts_set_remote_access_granted(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsSetRemoteAccessGranted!(handle, requestJson);
    }

    internal static IntPtr omni_account_devices_list(IntPtr handle)
    {
        EnsureLoaded();
        return omniAccountDevicesList!(handle);
    }

    internal static IntPtr omni_account_move_working_key(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniAccountMoveWorkingKey!(handle, requestJson);
    }

    internal static IntPtr omni_account_devices_acknowledge(IntPtr handle)
    {
        EnsureLoaded();
        return omniAccountDevicesAcknowledge!(handle);
    }

    internal static IntPtr omni_account_devices_hide_revoked(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniAccountDevicesHideRevoked!(handle, requestJson);
    }

    internal static IntPtr omni_account_device_revoke(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniAccountDeviceRevoke!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_decline(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsDecline!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_forget(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsForget!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_forget_and_reset_peer(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsForgetAndResetPeer!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_get_card(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsGetCard!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_get_card_revision(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsGetCardRevision!(handle, requestJson);
    }

    internal static IntPtr omni_identity_get_my_verification_payload(IntPtr handle)
    {
        EnsureLoaded();
        return omniIdentityGetMyVerificationPayload!(handle);
    }

    internal static IntPtr omni_contacts_get_verification_state(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsGetVerificationState!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_confirm_verification(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsConfirmVerification!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_header_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetHeaderSummary!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_screen_snapshot(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetScreenSnapshot!(handle, requestJson);
    }

    internal static IntPtr omni_chat_open_session_lease(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatOpenSessionLease!(handle, requestJson);
    }

    internal static IntPtr omni_chat_mark_read(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatMarkRead!(handle, requestJson);
    }

    internal static IntPtr omni_chat_end_session(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatEndSession!(handle, requestJson);
    }

    internal static IntPtr omni_chat_list_thread_projections(IntPtr handle)
    {
        EnsureLoaded();
        return omniChatListThreadProjections!(handle);
    }

    internal static IntPtr omni_chat_get_thread_list_refresh_summary(IntPtr handle)
    {
        EnsureLoaded();
        return omniChatGetThreadListRefreshSummary!(handle);
    }

    internal static IntPtr omni_chat_get_security_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetSecuritySummary!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_message_security_details(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetMessageSecurityDetails!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_message_refresh_summary(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetMessageRefreshSummary!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_message_delta(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetMessageDelta!(handle, requestJson);
    }

    internal static IntPtr omni_chat_get_message_security_details_batch(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniChatGetMessageSecurityDetailsBatch!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_start_chat(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsStartChat!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_quick_add_from_dialog(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsQuickAddFromDialog!(handle, requestJson);
    }

    internal static IntPtr omni_contacts_attach_dialog_to_contact(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniContactsAttachDialogToContact!(handle, requestJson);
    }

    internal static void omni_string_free(IntPtr ptr)
    {
        EnsureLoaded();
        omniStringFree!(ptr);
    }

    private static T GetExport<T>(string exportName) where T : Delegate
    {
        try
        {
            IntPtr export = NativeLibrary.GetExport(libraryHandle, exportName);
            return Marshal.GetDelegateForFunctionPointer<T>(export);
        }
        catch (Exception ex)
        {
            throw new NativeLibraryLoadException(
                "Ядро Omni другой версии — переустановите приложение.",
                AppendLine(LastLoadDiagnostic, $"Missing export: {exportName}. {ex.GetType().Name}: {ex.Message}"),
                ex);
        }
    }

    private static IntPtr LoadLibraryOrThrow()
    {
        StringBuilder diagnostic = new();
        diagnostic.AppendLine($"BaseDirectory={AppContext.BaseDirectory}");
        diagnostic.AppendLine($"ProcessPath={Environment.ProcessPath}");
        diagnostic.AppendLine($"OSArchitecture={RuntimeInformation.OSArchitecture}");
        diagnostic.AppendLine($"ProcessArchitecture={RuntimeInformation.ProcessArchitecture}");

        foreach (string candidate in GetCandidatePaths())
        {
            bool exists = File.Exists(candidate);
            diagnostic.AppendLine($"Candidate={candidate}; Exists={exists}");

            if (!exists)
            {
                continue;
            }

            try
            {
                IntPtr handle = NativeLibrary.Load(candidate);
                LoadedLibraryPath = candidate;
                LastLoadDiagnostic = AppendLine(diagnostic.ToString(), $"Loaded={candidate}");
                return handle;
            }
            catch (BadImageFormatException ex)
            {
                LastLoadDiagnostic = AppendLine(diagnostic.ToString(), $"BadImageFormatException: {ex.Message}");
                throw new NativeLibraryLoadException(
                    "Эта сборка Omni не подходит к компьютеру: нужна версия для другой архитектуры.",
                    LastLoadDiagnostic,
                    ex);
            }
            catch (DllNotFoundException ex)
            {
                LastLoadDiagnostic = AppendLine(diagnostic.ToString(), $"DllNotFoundException: {ex.Message}");
                throw new NativeLibraryLoadException(
                    "Файл omni_ffi.dll найден, но не загружается из-за отсутствующей зависимости.",
                    LastLoadDiagnostic,
                    ex);
            }
            catch (Exception ex)
            {
                LastLoadDiagnostic = AppendLine(diagnostic.ToString(), $"{ex.GetType().Name}: {ex.Message}");
                throw new NativeLibraryLoadException(
                    "Не удалось загрузить ядро Omni — файлы приложения повреждены или неполны.",
                    LastLoadDiagnostic,
                    ex);
            }
        }

        LastLoadDiagnostic = AppendLine(diagnostic.ToString(), "Library file was not found in any candidate location.");
        throw new NativeLibraryLoadException(
            "Файл omni_ffi.dll не найден в выходной папке приложения.",
            LastLoadDiagnostic,
            new DllNotFoundException(LibraryFileName));
    }

    private static IEnumerable<string> GetCandidatePaths()
    {
        HashSet<string> candidates = new(StringComparer.OrdinalIgnoreCase)
        {
            Path.Combine(AppContext.BaseDirectory, LibraryFileName),
        };

        if (!string.IsNullOrWhiteSpace(Environment.ProcessPath))
        {
            string? processDirectory = Path.GetDirectoryName(Environment.ProcessPath);
            if (!string.IsNullOrWhiteSpace(processDirectory))
            {
                candidates.Add(Path.Combine(processDirectory, LibraryFileName));
            }
        }

        string assemblyDirectory = Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location)
            ?? AppContext.BaseDirectory;
        candidates.Add(Path.Combine(assemblyDirectory, LibraryFileName));

        return candidates;
    }

    private static string AppendLine(string source, string line)
        => string.IsNullOrWhiteSpace(source)
            ? line
            : $"{source}{Environment.NewLine}{line}";

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatSendTextDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatSendAttachmentDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatRequestAttachmentDownloadDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatRevokeAttachmentAccessDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatListMessagesDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatListMessagesPageDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityCreateAccountDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityRestoreAccountFromInputDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityInspectRecoveryInputDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLocalAccountDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityUpdateLocalPublicProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLocalDeviceDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLocalPrekeyBundleDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityStoreRemotePrekeyBundleDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityListLocalProfilesDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLocalProfileSummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetActiveProfileDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentitySelectActiveProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityClearActiveProfileDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityReopenLocalProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetCurrentProfileGateSummaryDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetCurrentProfileLifecycleSummaryDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLifecycleFastSnapshotDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityPrepareSignOutCurrentProfileDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityConfirmSignOutCurrentProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityConfirmSignOutProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentitySignOutProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityUnlockCurrentProfileWithPasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityRevealCurrentProfileRecoveryPhraseWithPasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityUnlockCurrentProfileWithMnemonicDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityUnlockProfileWithPasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityUnlockProfileWithMnemonicDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentitySetLocalProfilePasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityChangeLocalProfilePasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityRemoveLocalProfilePasswordDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetPasswordProtectionSummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetLocalProfileFootprintSummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityPreviewSignOutProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityPreviewDeleteLocalProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityPrepareDeleteCurrentProfileDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityConfirmDeleteLocalProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityConfirmDeleteCurrentProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityPreviewRevokeCurrentDeviceAndWipeDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityDeleteLocalProfileDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsListDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsGetRefreshSummaryDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsGetScreenSnapshotDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsAddByIdentifierDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsRequestByIdentifierDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsAddFromPayloadDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsRenameDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsBlockDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsApproveDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsSetFriendRelayConsentDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsDeclineDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsForgetDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsForgetAndResetPeerDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsGetCardDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsGetCardRevisionDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetMyVerificationPayloadDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsGetVerificationStateDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsConfirmVerificationDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetHeaderSummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetScreenSnapshotDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatOpenSessionLeaseDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatEndSessionDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatListThreadProjectionsDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetThreadListRefreshSummaryDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetSecuritySummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetMessageRefreshSummaryDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetMessageDeltaDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetMessageSecurityDetailsDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniChatGetMessageSecurityDetailsBatchDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsStartChatDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsQuickAddFromDialogDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniContactsAttachDialogToContactDelegate(IntPtr handle, [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityGetCurrentAccountExportManifestDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityRestoreAccountFromExportManifestDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityExportCurrentDatabaseBackupDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityExportCurrentDatabaseBackupBundleDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniIdentityInspectAccountBackupManifestDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OmniStringFreeDelegate(IntPtr ptr);
}

internal sealed class NativeLibraryLoadException : Exception
{
    public NativeLibraryLoadException(string userMessage, string diagnosticDetails, Exception innerException)
        : base(userMessage, innerException)
    {
        DiagnosticDetails = diagnosticDetails;
    }

    public string DiagnosticDetails { get; }
}
