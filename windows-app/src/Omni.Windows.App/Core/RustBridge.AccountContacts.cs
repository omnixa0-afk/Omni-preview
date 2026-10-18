using System;
using System.Collections.Generic;
using System.Text.Json;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public LocalAccountSnapshot? GetLocalAccount()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_local_account(handle),
            OmniJsonSerializerContext.Default.LocalAccountSnapshot,
            "identity_get_local_account");

    public LocalDeviceSnapshot? GetLocalDevice()
        => CallOptionalJson(
            () => NativeMethods.omni_identity_get_local_device(handle),
            OmniJsonSerializerContext.Default.LocalDeviceSnapshot,
            "identity_get_local_device");

    public PrekeyBundleV1 GetLocalPrekeyBundle()
        => CallJson(
            () => NativeMethods.omni_identity_get_local_prekey_bundle(handle),
            OmniJsonSerializerContext.Default.PrekeyBundleV1,
            "identity_get_local_prekey_bundle");

    public void StoreRemotePrekeyBundle(PrekeyBundleV1 bundle)
    {
        string payload = CallRaw(
            () => NativeMethods.omni_identity_store_remote_prekey_bundle(
                handle,
                JsonSerializer.Serialize(bundle, OmniJsonSerializerContext.Default.PrekeyBundleV1)),
            "identity_store_remote_prekey_bundle");
        ThrowIfRustError(payload, "identity_store_remote_prekey_bundle");
    }

    public CreateAccountResponse CreateAccount(string displayName, string deviceLabel)
        => CallJson(
            () => NativeMethods.omni_identity_create_account(
                handle,
                JsonSerializer.Serialize(
                    new CreateAccountRequest(displayName, deviceLabel),
                    OmniJsonSerializerContext.Default.CreateAccountRequest)),
            OmniJsonSerializerContext.Default.CreateAccountResponse,
            "identity_create_account");

    public LocalAccountSnapshot UpdateLocalPublicProfile(string displayName, string? avatarSourcePath, bool clearAvatar)
        => CallJson(
            () => NativeMethods.omni_identity_update_local_public_profile(
                handle,
                JsonSerializer.Serialize(
                    new UpdateLocalPublicProfileRequest(displayName, avatarSourcePath, clearAvatar),
                    OmniJsonSerializerContext.Default.UpdateLocalPublicProfileRequest)),
            OmniJsonSerializerContext.Default.LocalAccountSnapshot,
            "identity_update_local_public_profile");

    public List<ContactSnapshot> ListContacts()
        => CallJson(
            () => NativeMethods.omni_contacts_list(handle),
            OmniJsonSerializerContext.Default.ListContactSnapshot,
            "contacts_list");

    public ContactsRefreshSummary GetContactsRefreshSummary()
        => CallJson(
            () => NativeMethods.omni_contacts_get_refresh_summary(handle),
            OmniJsonSerializerContext.Default.ContactsRefreshSummary,
            "contacts_get_refresh_summary");

    public ContactsScreenSnapshot GetContactsScreenSnapshot()
        => CallJson(
            () => NativeMethods.omni_contacts_get_screen_snapshot(handle),
            OmniJsonSerializerContext.Default.ContactsScreenSnapshot,
            "contacts_get_screen_snapshot");

    public ContactSnapshot AddContactByIdentifier(string accountId, string petname, string? publicDisplayName = null)
        => CallJson(
            () => NativeMethods.omni_contacts_add_by_identifier(
                handle,
                JsonSerializer.Serialize(
                    new AddContactByIdentifierRequest(accountId, petname, publicDisplayName),
                    OmniJsonSerializerContext.Default.AddContactByIdentifierRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_add_by_identifier");

    public ContactSnapshot RequestContactByIdentifier(string accountId, string petname, string? publicDisplayName = null)
        => CallJson(
            () => NativeMethods.omni_contacts_request_by_identifier(
                handle,
                JsonSerializer.Serialize(
                    new RequestContactByIdentifierRequest(accountId, petname, publicDisplayName),
                    OmniJsonSerializerContext.Default.RequestContactByIdentifierRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_request_by_identifier");

    public ContactSnapshot AddContactFromPayload(ImportableContactPayloadV1 payload, string petname)
        => CallJson(
            () => NativeMethods.omni_contacts_add_from_payload(
                handle,
                JsonSerializer.Serialize(
                    new AddContactFromPayloadRequest(payload, petname),
                    OmniJsonSerializerContext.Default.AddContactFromPayloadRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_add_from_payload");

    public ContactSnapshot RenameContact(string contactId, string petname)
        => CallJson(
            () => NativeMethods.omni_contacts_rename(
                handle,
                JsonSerializer.Serialize(
                    new RenameContactRequest(contactId, petname),
                    OmniJsonSerializerContext.Default.RenameContactRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_rename");

    public ContactSnapshot BlockContact(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_block(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_block");

    public ContactSnapshot ApproveContact(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_approve(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_approve");

    // Friend-proxy (Axis 2): per-contact "carry this friend's traffic through me" consent.
    // Persisted in Rust; the running mesh + advertised role are updated there too.
    public ContactSnapshot SetContactFriendRelayConsent(string contactId, bool consent)
        => CallJson(
            () => NativeMethods.omni_contacts_set_friend_relay_consent(
                handle,
                JsonSerializer.Serialize(
                    new SetContactFriendRelayConsentRequest(contactId, consent),
                    OmniJsonSerializerContext.Default.SetContactFriendRelayConsentRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_set_friend_relay_consent");

    // Remote access (remote-access-for-contacts.md): the "who may control this computer" setting,
    // both directions. Pure state — reads and writes, no session, no input.
    public ContactRemoteAccessRecord GetContactRemoteAccess(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_get_remote_access(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactRemoteAccessRecord,
            "contacts_get_remote_access");

    public IReadOnlyList<ContactRemoteAccessRecord> ListContactRemoteAccess()
        => CallJson(
            () => NativeMethods.omni_contacts_list_remote_access(handle),
            OmniJsonSerializerContext.Default.ListContactRemoteAccessRecord,
            "contacts_list_remote_access") ?? new List<ContactRemoteAccessRecord>();

    /// `mode` = "ask" | "unattended" to grant, null to revoke. The core refuses a grant to an
    /// unverified contact.
    public ContactRemoteAccessRecord SetContactRemoteAccessGranted(string contactId, string? mode)
        => CallJson(
            () => NativeMethods.omni_contacts_set_remote_access_granted(
                handle,
                JsonSerializer.Serialize(
                    new SetContactRemoteAccessGrantedRequest(contactId, mode),
                    OmniJsonSerializerContext.Default.SetContactRemoteAccessGrantedRequest)),
            OmniJsonSerializerContext.Default.ContactRemoteAccessRecord,
            "contacts_set_remote_access_granted");

    /// Our own account's devices, this one first (Settings «Устройства»).
    public IReadOnlyList<OwnDeviceView> ListOwnDevices()
        => CallJson(
            () => NativeMethods.omni_account_devices_list(handle),
            OmniJsonSerializerContext.Default.ListOwnDeviceView,
            "account_devices_list") ?? new List<OwnDeviceView>();

    /// "The account key was stolen": move the account to a new working key keeping `keepDeviceIds`
    /// (this device always stays). `phrase` is needed when this computer cannot open the stored root
    /// (a copied folder), `password` when the profile has one.
    public AccountKeyMoveSummary MoveAccountToNewWorkingKey(
        IReadOnlyList<string> keepDeviceIds,
        string? phrase,
        string? password)
        => CallJson(
            () => NativeMethods.omni_account_move_working_key(
                handle,
                JsonSerializer.Serialize(
                    new MoveAccountKeyRequest(keepDeviceIds, phrase, password),
                    OmniJsonSerializerContext.Default.MoveAccountKeyRequest)),
            OmniJsonSerializerContext.Default.AccountKeyMoveSummary,
            "account_move_working_key");

    /// The owner has seen the account devices: none is "new" any more.
    public void AcknowledgeOwnDevices()
        => CallUnit(
            () => NativeMethods.omni_account_devices_acknowledge(handle),
            "account_devices_acknowledge");

    /// Take a revoked device (or, with null, all of them) out of the Settings list. The revocation
    /// itself stays: the device is still out of the account.
    public void HideRevokedOwnDevices(string? deviceId)
        => CallUnit(
            () => NativeMethods.omni_account_devices_hide_revoked(
                handle,
                JsonSerializer.Serialize(
                    new HideRevokedOwnDevicesRequest(deviceId),
                    OmniJsonSerializerContext.Default.HideRevokedOwnDevicesRequest)),
            "account_devices_hide_revoked");

    /// Take one of our other devices back: dropped from every route, everybody told, and it erases
    /// its profile when it learns.
    public void RevokeOwnDevice(string deviceId, string? password = null)
        => CallUnit(
            () => NativeMethods.omni_account_device_revoke(
                handle,
                JsonSerializer.Serialize(
                    new RevokeOwnDeviceRequest(deviceId, password),
                    OmniJsonSerializerContext.Default.RevokeOwnDeviceRequest)),
            "account_device_revoke");

    public ContactSnapshot DeclineContact(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_decline(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_decline");

    public ContactSnapshot ForgetContact(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_forget(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_forget");

    public ContactSnapshot ForgetContactAndResetPeer(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_forget_and_reset_peer(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_forget_and_reset_peer");

    public ContactCardSnapshot GetContactCard(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_get_card(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactCardSnapshot,
            "contacts_get_card");

    public EntityRevisionSnapshot? GetContactCardRevision(string contactId)
        => CallOptionalJson(
            () => NativeMethods.omni_contacts_get_card_revision(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.EntityRevisionSnapshot,
            "contacts_get_card_revision");

    public MyVerificationPayloadV1 GetMyVerificationPayload()
        => CallJson(
            () => NativeMethods.omni_identity_get_my_verification_payload(handle),
            OmniJsonSerializerContext.Default.MyVerificationPayloadV1,
            "identity_get_my_verification_payload");

    public ContactCardSnapshot GetContactVerificationState(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_get_verification_state(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactCardSnapshot,
            "contacts_get_verification_state");

    public ContactCardSnapshot ConfirmContactVerification(
        string contactId,
        ContactVerificationPayloadV1 payload,
        string method)
        => CallJson(
            () => NativeMethods.omni_contacts_confirm_verification(
                handle,
                JsonSerializer.Serialize(
                    new ConfirmContactVerificationRequest(contactId, payload, method),
                    OmniJsonSerializerContext.Default.ConfirmContactVerificationRequest)),
            OmniJsonSerializerContext.Default.ContactCardSnapshot,
            "contacts_confirm_verification");

    public ChatHeaderContactSummary GetChatHeaderContactSummary(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_get_header_summary(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.ChatHeaderContactSummary,
            "chat_get_header_summary");

    public ChatHeaderScreenSnapshot GetChatHeaderScreenSnapshot(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_get_screen_snapshot(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.ChatHeaderScreenSnapshot,
            "chat_get_screen_snapshot");

    public ChatSessionSummary OpenChatSessionLease(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_open_session_lease(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.ChatSessionSummary,
            "chat_open_session_lease");

    /// The chat is on screen: its lines so far are read. 1:1 and group dialog keys alike.
    public void MarkChatRead(string dialogKey)
        => CallUnit(
            () => NativeMethods.omni_chat_mark_read(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            "chat_mark_read");

    public ChatSessionSummary EndChatSession(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_end_session(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.ChatSessionSummary,
            "chat_end_session");

    public List<ChatThreadProjection> ListChatThreadProjections()
        => CallJson(
            () => NativeMethods.omni_chat_list_thread_projections(handle),
            OmniJsonSerializerContext.Default.ListChatThreadProjection,
            "chat_list_thread_projections");

    public ChatThreadListRefreshSummary GetChatThreadListRefreshSummary()
        => CallJson(
            () => NativeMethods.omni_chat_get_thread_list_refresh_summary(handle),
            OmniJsonSerializerContext.Default.ChatThreadListRefreshSummary,
            "chat_get_thread_list_refresh_summary");

    public ChatSecuritySummary GetChatSecuritySummary(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_get_security_summary(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.ChatSecuritySummary,
            "chat_get_security_summary");

    public DialogMessageRefreshSummary GetDialogMessageRefreshSummary(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_chat_get_message_refresh_summary(
                handle,
                JsonSerializer.Serialize(
                    new DialogKeyRequest(dialogKey),
                    OmniJsonSerializerContext.Default.DialogKeyRequest)),
            OmniJsonSerializerContext.Default.DialogMessageRefreshSummary,
            "chat_get_message_refresh_summary");

    public DialogDeltaSnapshot GetDialogMessageDelta(string dialogKey, DialogDeltaCursor cursor)
        => CallJson(
            () => NativeMethods.omni_chat_get_message_delta(
                handle,
                JsonSerializer.Serialize(
                    new DialogDeltaRequest(dialogKey, cursor),
                    OmniJsonSerializerContext.Default.DialogDeltaRequest)),
            OmniJsonSerializerContext.Default.DialogDeltaSnapshot,
            "chat_get_message_delta");

    public MessageSecurityDetails? GetMessageSecurityDetails(string messageId)
        => CallOptionalJson(
            () => NativeMethods.omni_chat_get_message_security_details(
                handle,
                JsonSerializer.Serialize(
                    new MessageIdRequest(messageId),
                    OmniJsonSerializerContext.Default.MessageIdRequest)),
            OmniJsonSerializerContext.Default.MessageSecurityDetails,
            "chat_get_message_security_details");

    public List<MessageSecurityDetails> GetMessageSecurityDetailsBatch(IReadOnlyList<string> messageIds)
        => CallJson(
            () => NativeMethods.omni_chat_get_message_security_details_batch(
                handle,
                JsonSerializer.Serialize(
                    new MessageIdBatchRequest(messageIds),
                    OmniJsonSerializerContext.Default.MessageIdBatchRequest)),
            OmniJsonSerializerContext.Default.ListMessageSecurityDetails,
            "chat_get_message_security_details_batch");

    public ContactChatTarget StartChatFromContact(string contactId)
        => CallJson(
            () => NativeMethods.omni_contacts_start_chat(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.ContactChatTarget,
            "contacts_start_chat");

    public ContactSnapshot QuickAddContactFromDialog(
        string dialogKey,
        string petname,
        string? publicDisplayName = null)
        => CallJson(
            () => NativeMethods.omni_contacts_quick_add_from_dialog(
                handle,
                JsonSerializer.Serialize(
                    new QuickAddContactFromDialogRequest(dialogKey, petname, publicDisplayName),
                    OmniJsonSerializerContext.Default.QuickAddContactFromDialogRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_quick_add_from_dialog");

    public ContactSnapshot AttachDialogToContact(string contactId, string dialogKey)
        => CallJson(
            () => NativeMethods.omni_contacts_attach_dialog_to_contact(
                handle,
                JsonSerializer.Serialize(
                    new AttachDialogToContactRequest(contactId, dialogKey),
                    OmniJsonSerializerContext.Default.AttachDialogToContactRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "contacts_attach_dialog_to_contact");
}
