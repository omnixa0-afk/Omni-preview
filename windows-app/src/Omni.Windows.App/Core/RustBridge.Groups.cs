using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public GroupSummary CreateGroup(string name)
        => CallJson(
            () => NativeMethods.omni_groups_create(
                handle,
                JsonSerializer.Serialize(
                    new CreateGroupRequest(name),
                    OmniJsonSerializerContext.Default.CreateGroupRequest)),
            OmniJsonSerializerContext.Default.GroupSummary,
            "groups_create");

    public IReadOnlyList<GroupSummary> ListGroups()
        => CallJson(
            () => NativeMethods.omni_groups_list(handle),
            OmniJsonSerializerContext.Default.ListGroupSummary,
            "groups_list") ?? new List<GroupSummary>();

    public IReadOnlyList<GroupMemberView> ListGroupMembers(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_list_members(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            OmniJsonSerializerContext.Default.ListGroupMemberView,
            "groups_list_members") ?? new List<GroupMemberView>();

    public void SendGroupText(string groupId, string body)
    {
        const string operation = "groups_send_text";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_send_text(
                handle,
                JsonSerializer.Serialize(
                    new SendGroupTextRequest(groupId, body),
                    OmniJsonSerializerContext.Default.SendGroupTextRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// A file or photo into the group (the core re-encodes a photo); `body` is its caption.
    public void SendGroupAttachment(string groupId, string body, string sourcePath, string? displayName, bool asPhoto)
    {
        const string operation = "groups_send_attachment";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_send_attachment(
                handle,
                JsonSerializer.Serialize(
                    new SendGroupAttachmentRequest(groupId, body, sourcePath, displayName, asPhoto),
                    OmniJsonSerializerContext.Default.SendGroupAttachmentRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// Invite contacts (their 1:1 dialog keys) — any member may.
    public void InviteToGroup(string groupId, IReadOnlyList<string> dialogKeys)
    {
        const string operation = "groups_invite";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_invite(
                handle,
                JsonSerializer.Serialize(
                    new InviteToGroupRequest(groupId, string.Empty, new List<string>(dialogKeys)),
                    OmniJsonSerializerContext.Default.InviteToGroupRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    public IReadOnlyList<GroupInviteView> ListGroupInvites()
        => CallJson(
            () => NativeMethods.omni_groups_list_invites(handle),
            OmniJsonSerializerContext.Default.ListGroupInviteView,
            "groups_list_invites") ?? new List<GroupInviteView>();

    public IReadOnlyList<GroupMessageView> ListGroupMessages(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_list_messages(handle, GroupIdJson(groupId)),
            OmniJsonSerializerContext.Default.ListGroupMessageView,
            "groups_list_messages") ?? new List<GroupMessageView>();

    public void AcceptGroupInvite(string groupId)
        => CallUnit(() => NativeMethods.omni_groups_accept_invite(handle, GroupIdJson(groupId)), "groups_accept_invite");

    public void DeclineGroupInvite(string groupId)
        => CallUnit(() => NativeMethods.omni_groups_decline_invite(handle, GroupIdJson(groupId)), "groups_decline_invite");

    /// <summary>Leave (if still in) and delete the group's chat from this device.</summary>
    public void LeaveGroup(string groupId)
        => CallUnit(() => NativeMethods.omni_groups_leave(handle, GroupIdJson(groupId)), "groups_leave");

    /// <summary>Admin only: the group is deleted for every member, and goes from here.</summary>
    public void DeleteGroupForEveryone(string groupId)
        => CallUnit(
            () => NativeMethods.omni_groups_delete_for_everyone(handle, GroupIdJson(groupId)),
            "groups_delete_for_everyone");

    public void RemoveGroupMember(string groupId, string memberPub)
        => CallUnit(
            () => NativeMethods.omni_groups_remove_member(
                handle,
                JsonSerializer.Serialize(
                    new GroupMemberRequest(groupId, memberPub),
                    OmniJsonSerializerContext.Default.GroupMemberRequest)),
            "groups_remove_member");

    public void RenameGroup(string groupId, string name)
        => CallUnit(
            () => NativeMethods.omni_groups_rename(
                handle,
                JsonSerializer.Serialize(
                    new RenameGroupRequest(groupId, name),
                    OmniJsonSerializerContext.Default.RenameGroupRequest)),
            "groups_rename");

    /// <summary>Admin only: an image file for the group's picture, or null to clear it.</summary>
    public void SetGroupAvatar(string groupId, string? sourcePath)
        => CallUnit(
            () => NativeMethods.omni_groups_set_avatar(
                handle,
                JsonSerializer.Serialize(
                    new SetGroupAvatarRequest(groupId, sourcePath),
                    OmniJsonSerializerContext.Default.SetGroupAvatarRequest)),
            "groups_set_avatar");

    public void SetGroupAdmin(string groupId, string memberPub, bool admin)
        => CallUnit(
            () => NativeMethods.omni_groups_set_admin(
                handle,
                JsonSerializer.Serialize(
                    new SetGroupAdminRequest(groupId, memberPub, admin),
                    OmniJsonSerializerContext.Default.SetGroupAdminRequest)),
            "groups_set_admin");

    /// An admin turns the recent history for newcomers on or off (on by default).
    public void SetGroupNewcomerHistory(string groupId, bool on)
        => CallUnit(
            () => NativeMethods.omni_groups_set_newcomer_history(
                handle,
                JsonSerializer.Serialize(
                    new SetGroupNewcomerHistoryRequest(groupId, on),
                    OmniJsonSerializerContext.Default.SetGroupNewcomerHistoryRequest)),
            "groups_set_newcomer_history");

    private static string GroupIdJson(string groupId)
        => JsonSerializer.Serialize(new GroupIdRequest(groupId), OmniJsonSerializerContext.Default.GroupIdRequest);

    private void CallUnit(System.Func<System.IntPtr> call, string operation)
    {
        string payload = CallRaw(call, operation);
        ThrowIfRustError(payload, operation);
    }

    public GroupCallSnapshot StartGroupCall(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_start_call(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            OmniJsonSerializerContext.Default.GroupCallSnapshot,
            "groups_start_call");

    /// Join the call going on in a group (answering the ring or the "join" button).
    public GroupCallSnapshot JoinGroupCall(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_join_call(handle, GroupIdJson(groupId)),
            OmniJsonSerializerContext.Default.GroupCallSnapshot,
            "groups_join_call");

    /// Our still-unanswered invitations for a group (only the inviter sees them).
    public IReadOnlyList<GroupSentInvite> ListGroupSentInvites(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_list_sent_invites(handle, GroupIdJson(groupId)),
            OmniJsonSerializerContext.Default.ListGroupSentInvite,
            "groups_list_sent_invites") ?? new List<GroupSentInvite>();

    /// Withdraw an invitation we sent; the invitee's offer disappears.
    public void CancelGroupInvite(string groupId, string dialogKey)
        => CallUnit(
            () => NativeMethods.omni_groups_cancel_invite(
                handle,
                JsonSerializer.Serialize(
                    new InviteToGroupRequest(groupId, dialogKey, new List<string>()),
                    OmniJsonSerializerContext.Default.InviteToGroupRequest)),
            "groups_cancel_invite");

    /// Stop ringing for a group call (it stays listed as "a call is on").
    public void DismissGroupCall(string groupId)
        => CallUnit(() => NativeMethods.omni_groups_dismiss_call(handle, GroupIdJson(groupId)), "groups_dismiss_call");

    /// Every group call we're in or could join — chat-list indicator + ring prompt.
    public IReadOnlyList<GroupCallNotice> ListGroupCallInbox()
        => CallJson(
            () => NativeMethods.omni_groups_call_inbox(handle),
            OmniJsonSerializerContext.Default.ListGroupCallNotice,
            "groups_call_inbox") ?? new List<GroupCallNotice>();

    public void LeaveGroupCall(string groupId)
    {
        const string operation = "groups_leave_call";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_leave_call(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    public GroupCallSnapshot? GetGroupCallSnapshot(string groupId)
        => CallJson(
            () => NativeMethods.omni_groups_call_snapshot(
                handle,
                JsonSerializer.Serialize(
                    new GroupIdRequest(groupId),
                    OmniJsonSerializerContext.Default.GroupIdRequest)),
            OmniJsonSerializerContext.Default.GroupCallSnapshot,
            "groups_call_snapshot");

    // Announce our own mic-muted / camera-off state to the group call so remote members
    // badge our tile. Called on each local mute/camera toggle; the full current state
    // rides one MediaState control (idempotent). Speaking is not sent here (local-only).
    public void SetGroupCallMediaState(string groupId, bool muted, bool cameraOff, bool screenSharing)
    {
        const string operation = "groups_set_call_media_state";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_set_call_media_state(
                handle,
                JsonSerializer.Serialize(
                    new SetGroupCallMediaStateRequest(groupId, muted, cameraOff, screenSharing),
                    OmniJsonSerializerContext.Default.SetGroupCallMediaStateRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    public RealtimeMediaNegotiationSnapshot PublishGroupMediaCapabilities(RealtimeMediaCapabilities capabilities)
        => CallJson(
            () => NativeMethods.omni_groups_publish_media_capabilities(
                handle,
                JsonSerializer.Serialize(
                    new PublishRealtimeMediaCapabilitiesRequest(capabilities),
                    OmniJsonSerializerContext.Default.PublishRealtimeMediaCapabilitiesRequest)),
            OmniJsonSerializerContext.Default.RealtimeMediaNegotiationSnapshot,
            "groups_publish_media_capabilities");

    // --- request DTOs (match omni-ffi requests_groups.rs) ---
    internal sealed record CreateGroupRequest(
        [property: JsonPropertyName("name")] string Name);

    internal sealed record GroupIdRequest(
        [property: JsonPropertyName("group_id")] string GroupId);

    internal sealed record SendGroupTextRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("body")] string Body);

    internal sealed record SendGroupAttachmentRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("body")] string Body,
        [property: JsonPropertyName("source_file_path")] string SourceFilePath,
        [property: JsonPropertyName("display_file_name")] string? DisplayFileName,
        [property: JsonPropertyName("as_photo")] bool AsPhoto);

    internal sealed record InviteToGroupRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("dialog_key")] string DialogKey,
        [property: JsonPropertyName("dialog_keys")] List<string> DialogKeys);

    internal sealed record GroupMemberRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("member_pub")] string MemberPub);

    internal sealed record RenameGroupRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("name")] string Name);

    internal sealed record SetGroupAvatarRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("source_path")] string? SourcePath);

    internal sealed record SetGroupNewcomerHistoryRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("on")] bool On);

    internal sealed record SetGroupAdminRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("member_pub")] string MemberPub,
        [property: JsonPropertyName("admin")] bool Admin);

    internal sealed record SetGroupCallMediaStateRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("muted")] bool Muted,
        [property: JsonPropertyName("camera_off")] bool CameraOff,
        [property: JsonPropertyName("screen_sharing")] bool ScreenSharing);
}

// --- response DTOs (match omni-app GroupSummary / GroupMemberView) ---
internal sealed record GroupSummary(
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("my_role")] string MyRole,
    [property: JsonPropertyName("my_status")] string MyStatus,
    [property: JsonPropertyName("member_count")] int MemberCount,
    [property: JsonPropertyName("active_member_count")] int ActiveMemberCount,
    [property: JsonPropertyName("epoch")] ulong Epoch,
    // "Автор: текст" of the latest message and when — the chat list row.
    [property: JsonPropertyName("last_message_preview")] string LastMessagePreview,
    [property: JsonPropertyName("last_activity_unix_ms")] long LastActivityUnixMs,
    // The group's picture file (set by an admin), or null.
    [property: JsonPropertyName("avatar_path")] string? AvatarPath = null,
    // Newcomers get the recent history (an admin's switch, on by default).
    [property: JsonPropertyName("newcomer_history")] bool NewcomerHistory = true,
    // Members' lines this device has not read.
    [property: JsonPropertyName("unread_count")] int UnreadCount = 0);

internal sealed record GroupCallNotice(
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("group_name")] string GroupName,
    [property: JsonPropertyName("call_id")] string CallId,
    [property: JsonPropertyName("started_by_name")] string StartedByName,
    [property: JsonPropertyName("started_at_unix_ms")] long StartedAtUnixMs,
    [property: JsonPropertyName("participant_count")] int ParticipantCount,
    // A fresh invitation we haven't answered — prompt "Присоединиться".
    [property: JsonPropertyName("ringing")] bool Ringing,
    // We are in this call.
    [property: JsonPropertyName("joined")] bool Joined);

internal sealed record GroupInviteView(
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("inviter_name")] string InviterName,
    [property: JsonPropertyName("inviter_dialog_key")] string InviterDialogKey,
    [property: JsonPropertyName("received_at_unix_ms")] long ReceivedAtUnixMs,
    // pending (unanswered) | accepted (joining — waiting for the Welcome)
    [property: JsonPropertyName("status")] string Status);

// One line of a group thread. SenderPub is null for a system line ("X в группе").
internal sealed record GroupMessageView(
    [property: JsonPropertyName("message_id")] string MessageId,
    [property: JsonPropertyName("sender_pub")] string? SenderPub,
    [property: JsonPropertyName("sender_name")] string SenderName,
    [property: JsonPropertyName("is_mine")] bool IsMine,
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("body")] string Body,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("attachment")] AttachmentRecord? Attachment = null);

internal sealed record GroupMemberView(
    [property: JsonPropertyName("member_pub")] string MemberPub,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("role")] string Role,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("is_me")] bool IsMe,
    [property: JsonPropertyName("account_id")] string AccountId,
    // The contact's photo, else the one from the member's own group card (group-member-profile.md).
    [property: JsonPropertyName("avatar_path")] string? AvatarPath = null);

internal sealed record GroupSentInvite(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("sent_at_unix_ms")] long SentAtUnixMs);

internal sealed record GroupCallSnapshot(
    [property: JsonPropertyName("call_id")] string CallId,
    [property: JsonPropertyName("group_id")] string GroupId,
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("participant_count")] int ParticipantCount,
    [property: JsonPropertyName("topology")] string Topology,
    [property: JsonPropertyName("is_forwarder")] bool IsForwarder,
    // Our place in the cascade tree: "mesh" | "root" | "internal" | "leaf". Honest
    // indicator of our forwarding role (the tree is derived, not signalled).
    [property: JsonPropertyName("tree_role")] string TreeRole,
    [property: JsonPropertyName("started_at_unix_ms")] long StartedAtUnixMs,
    // Everyone in the call (incl. us). SenderTagHex is the 8-byte demux key (hex) the group
    // media receive loop splits on to route each remote sender to its own video tile.
    [property: JsonPropertyName("members")] IReadOnlyList<GroupCallMember> Members,
    // Who may control the screen WE share; null — nobody.
    [property: JsonPropertyName("my_screen_control")] ScreenControlGrant? MyScreenControl = null);

internal sealed record GroupCallMember(
    [property: JsonPropertyName("member_pub")] string MemberPub,
    [property: JsonPropertyName("sender_tag_hex")] string SenderTagHex,
    [property: JsonPropertyName("is_self")] bool IsSelf,
    // Last-known self-reported media state for this member (via MediaState control).
    // The tile draws mic-off / camera-off badges from these; remote reflects the last
    // broadcast we ingested, self stays authoritative from the local toggle.
    [property: JsonPropertyName("is_muted")] bool IsMuted,
    [property: JsonPropertyName("is_camera_off")] bool IsCameraOff,
    // This member is broadcasting their screen instead of the camera. The tile labels
    // ("демонстрирует экран") + auto-pins the sharer (Zoom/Telemost UX).
    [property: JsonPropertyName("is_screen_sharing")] bool IsScreenSharing,
    // This member shares their screen and lets US control it.
    [property: JsonPropertyName("lets_me_control")] bool LetsMeControl = false);
