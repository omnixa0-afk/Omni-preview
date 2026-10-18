using System;
using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniGroupsCreateDelegate? omniGroupsCreate;
    private static OmniGroupsListDelegate? omniGroupsList;
    private static OmniGroupsListMembersDelegate? omniGroupsListMembers;
    private static OmniGroupsSendTextDelegate? omniGroupsSendText;
    private static OmniGroupsJsonDelegate? omniGroupsSendAttachment;
    private static OmniGroupsInviteDelegate? omniGroupsInvite;
    private static OmniGroupsStartCallDelegate? omniGroupsStartCall;
    private static OmniGroupsLeaveCallDelegate? omniGroupsLeaveCall;
    private static OmniGroupsCallSnapshotDelegate? omniGroupsCallSnapshot;
    private static OmniGroupsSetCallMediaStateDelegate? omniGroupsSetCallMediaState;
    private static OmniGroupsPublishMediaCapabilitiesDelegate? omniGroupsPublishMediaCapabilities;
    // Groups on MLS (docs/decisions/group-crypto-mls.md): invitations, membership, the thread.
    private static OmniGroupsHandleOnlyDelegate? omniGroupsListInvites;
    private static OmniGroupsHandleOnlyDelegate? omniGroupsCallInbox;
    private static OmniGroupsJsonDelegate? omniGroupsJoinCall;
    private static OmniGroupsJsonDelegate? omniGroupsDismissCall;
    private static OmniGroupsJsonDelegate? omniGroupsListSentInvites;
    private static OmniGroupsJsonDelegate? omniGroupsCancelInvite;
    private static OmniGroupsJsonDelegate? omniGroupsAcceptInvite;
    private static OmniGroupsJsonDelegate? omniGroupsDeclineInvite;
    private static OmniGroupsJsonDelegate? omniGroupsLeave;
    private static OmniGroupsJsonDelegate? omniGroupsDeleteForEveryone;
    private static OmniGroupsJsonDelegate? omniGroupsRemoveMember;
    private static OmniGroupsJsonDelegate? omniGroupsRename;
    private static OmniGroupsJsonDelegate? omniGroupsSetAvatar;
    private static OmniGroupsJsonDelegate? omniGroupsSetAdmin;
    private static OmniGroupsJsonDelegate? omniGroupsSetNewcomerHistory;
    private static OmniGroupsJsonDelegate? omniGroupsListMessages;

    private static void LoadGroupExports()
    {
        LoadExport(ref omniGroupsCreate, nameof(omni_groups_create));
        LoadExport(ref omniGroupsList, nameof(omni_groups_list));
        LoadExport(ref omniGroupsListMembers, nameof(omni_groups_list_members));
        LoadExport(ref omniGroupsSendText, nameof(omni_groups_send_text));
        LoadExport(ref omniGroupsSendAttachment, nameof(omni_groups_send_attachment));
        LoadExport(ref omniGroupsInvite, nameof(omni_groups_invite));
        LoadExport(ref omniGroupsStartCall, nameof(omni_groups_start_call));
        LoadExport(ref omniGroupsLeaveCall, nameof(omni_groups_leave_call));
        LoadExport(ref omniGroupsCallSnapshot, nameof(omni_groups_call_snapshot));
        LoadExport(ref omniGroupsSetCallMediaState, nameof(omni_groups_set_call_media_state));
        LoadExport(ref omniGroupsPublishMediaCapabilities, nameof(omni_groups_publish_media_capabilities));
        LoadExport(ref omniGroupsListInvites, nameof(omni_groups_list_invites));
        LoadExport(ref omniGroupsCallInbox, nameof(omni_groups_call_inbox));
        LoadExport(ref omniGroupsJoinCall, nameof(omni_groups_join_call));
        LoadExport(ref omniGroupsDismissCall, nameof(omni_groups_dismiss_call));
        LoadExport(ref omniGroupsListSentInvites, nameof(omni_groups_list_sent_invites));
        LoadExport(ref omniGroupsCancelInvite, nameof(omni_groups_cancel_invite));
        LoadExport(ref omniGroupsAcceptInvite, nameof(omni_groups_accept_invite));
        LoadExport(ref omniGroupsDeclineInvite, nameof(omni_groups_decline_invite));
        LoadExport(ref omniGroupsLeave, nameof(omni_groups_leave));
        LoadExport(ref omniGroupsDeleteForEveryone, nameof(omni_groups_delete_for_everyone));
        LoadExport(ref omniGroupsRemoveMember, nameof(omni_groups_remove_member));
        LoadExport(ref omniGroupsRename, nameof(omni_groups_rename));
        LoadExport(ref omniGroupsSetAvatar, nameof(omni_groups_set_avatar));
        LoadExport(ref omniGroupsSetAdmin, nameof(omni_groups_set_admin));
        LoadExport(ref omniGroupsSetNewcomerHistory, nameof(omni_groups_set_newcomer_history));
        LoadExport(ref omniGroupsListMessages, nameof(omni_groups_list_messages));
    }

    internal static IntPtr omni_groups_publish_media_capabilities(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsPublishMediaCapabilities!(handle, requestJson);
    }

    internal static IntPtr omni_groups_start_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsStartCall!(handle, requestJson);
    }

    internal static IntPtr omni_groups_leave_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsLeaveCall!(handle, requestJson);
    }

    internal static IntPtr omni_groups_call_snapshot(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsCallSnapshot!(handle, requestJson);
    }

    internal static IntPtr omni_groups_set_call_media_state(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSetCallMediaState!(handle, requestJson);
    }

    internal static IntPtr omni_groups_create(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsCreate!(handle, requestJson);
    }

    internal static IntPtr omni_groups_list(IntPtr handle)
    {
        EnsureLoaded();
        return omniGroupsList!(handle);
    }

    internal static IntPtr omni_groups_list_members(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsListMembers!(handle, requestJson);
    }

    internal static IntPtr omni_groups_send_attachment(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSendAttachment!(handle, requestJson);
    }

    internal static IntPtr omni_groups_send_text(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSendText!(handle, requestJson);
    }

    internal static IntPtr omni_groups_invite(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsInvite!(handle, requestJson);
    }

    internal static IntPtr omni_groups_call_inbox(IntPtr handle)
    {
        EnsureLoaded();
        return omniGroupsCallInbox!(handle);
    }

    internal static IntPtr omni_groups_join_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsJoinCall!(handle, requestJson);
    }

    internal static IntPtr omni_groups_dismiss_call(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsDismissCall!(handle, requestJson);
    }

    internal static IntPtr omni_groups_list_sent_invites(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsListSentInvites!(handle, requestJson);
    }

    internal static IntPtr omni_groups_cancel_invite(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsCancelInvite!(handle, requestJson);
    }

    internal static IntPtr omni_groups_list_invites(IntPtr handle)
    {
        EnsureLoaded();
        return omniGroupsListInvites!(handle);
    }

    internal static IntPtr omni_groups_accept_invite(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsAcceptInvite!(handle, requestJson);
    }

    internal static IntPtr omni_groups_decline_invite(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsDeclineInvite!(handle, requestJson);
    }

    internal static IntPtr omni_groups_leave(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsLeave!(handle, requestJson);
    }

    internal static IntPtr omni_groups_delete_for_everyone(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsDeleteForEveryone!(handle, requestJson);
    }

    internal static IntPtr omni_groups_remove_member(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsRemoveMember!(handle, requestJson);
    }

    internal static IntPtr omni_groups_set_avatar(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSetAvatar!(handle, requestJson);
    }

    internal static IntPtr omni_groups_rename(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsRename!(handle, requestJson);
    }

    internal static IntPtr omni_groups_set_admin(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSetAdmin!(handle, requestJson);
    }

    internal static IntPtr omni_groups_set_newcomer_history(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsSetNewcomerHistory!(handle, requestJson);
    }

    internal static IntPtr omni_groups_list_messages(IntPtr handle, string requestJson)
    {
        EnsureLoaded();
        return omniGroupsListMessages!(handle, requestJson);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsHandleOnlyDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsJsonDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsCreateDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsListDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsListMembersDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsSendTextDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsInviteDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsStartCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsLeaveCallDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsCallSnapshotDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsSetCallMediaStateDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniGroupsPublishMediaCapabilitiesDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);
}
