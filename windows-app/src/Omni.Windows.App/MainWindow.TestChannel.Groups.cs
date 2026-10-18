using System.Text;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;

namespace Omni.Windows.App;

/// <summary>
/// Group commands for the UI test channel (docs/decisions/ui-test-channel.md): the live
/// multi-node pass for groups on MLS. Membership/chat go through the same bridge calls the
/// group dialogs make; the call commands press the real buttons (conversation call button,
/// ring bar), so the media pipeline starts exactly as for a user.
/// </summary>
public sealed partial class MainWindow
{
    private async Task<string?> TryExecuteGroupUiTestCommandAsync(string command, IReadOnlyList<string> args)
    {
        string Arg(int i) => i < args.Count ? args[i] : string.Empty;
        switch (command)
        {
            case "groups":
                return BuildUiTestGroupDump();

            case "group.create":
            {
                if (args.Count == 0)
                {
                    return "error: group.create needs a name";
                }
                GroupSummary created = rustBridge.CreateGroup(string.Join(' ', args));
                RefreshChatsList(force: true);
                return $"ok group={created.GroupId}";
            }

            // group.invite <group_id> <dialog_key> [dialog_key…]
            case "group.invite":
                if (args.Count < 2)
                {
                    return "error: group.invite needs a group id and dialog keys";
                }
                rustBridge.InviteToGroup(args[0], args.Skip(1).ToList());
                return "ok";

            // group.invited <group_id> — our unanswered invitations: dialog_key<TAB>name
            case "group.invited":
                return "ok" + string.Concat(rustBridge.ListGroupSentInvites(Arg(0))
                    .Select(i => $"{Environment.NewLine}{i.DialogKey}	{i.DisplayName}"));

            // group.uninvite <group_id> <dialog_key> — withdraw an invitation we sent
            case "group.uninvite":
                rustBridge.CancelGroupInvite(Arg(0), Arg(1));
                return "ok";

            case "group.accept":
                rustBridge.AcceptGroupInvite(Arg(0));
                RefreshChatsList(force: true);
                return "ok";

            case "group.decline":
                rustBridge.DeclineGroupInvite(Arg(0));
                RefreshChatsList(force: true);
                return "ok";

            case "group.open":
                return OpenGroupForUiTest(Arg(0)) ? "ok" : "error: no such group row";

            case "group.send":
                if (args.Count < 2)
                {
                    return "error: group.send needs a group id and text";
                }
                rustBridge.SendGroupText(args[0], string.Join(' ', args.Skip(1)));
                return "ok";

            case "group.file":
            case "group.photo":
                if (args.Count < 2)
                {
                    return $"error: {command} needs a group id and a file path";
                }
                rustBridge.SendGroupAttachment(
                    args[0],
                    string.Join(' ', args.Skip(2)),
                    args[1],
                    null,
                    string.Equals(command, "group.photo", StringComparison.OrdinalIgnoreCase));
                return "ok";

            case "group.photo.tap":
            {
                string tapped = GroupConversation.TapLastPhotoForUiTest();
                await Task.Delay(300);
                return $"{tapped} viewer={ImageViewerHost.IsOpen}";
            }

            case "group.log":
            {
                StringBuilder log = new("ok");
                log.AppendLine();
                foreach (GroupMessageView m in rustBridge.ListGroupMessages(Arg(0)))
                {
                    string file = m.Attachment is { } a
                        ? $"\t[{a.FileName} {a.State} {a.BytesTransferred}/{a.FileSizeBytes} sources={a.SourceCount}{(a.Presentation is { IsPhoto: true } ? " photo" : "")}]"
                        : string.Empty;
                    log.AppendLine($"msg\t{m.MessageId}\t{m.Kind}\t{(m.IsMine ? "me" : m.SenderName)}\t{m.Status}\t{m.Body}{file}");
                }
                return log.ToString().TrimEnd();
            }

            // «Скачать» у файла группы: тот же вызов, что кнопка пузыря. Для живой проверки загрузки у
            // нескольких держателей и суперсида (docs/decisions/attachment-content-tree.md, 3.2, этап 4).
            case "group.download":
            {
                if (args.Count < 2)
                {
                    return "error: group.download needs a message id and a save path";
                }

                await StartAttachmentDownloadAsync(args[0], args[1]);
                return await SettleUiTestCommandAsync();
            }

            case "group.members":
            {
                StringBuilder members = new("ok");
                members.AppendLine();
                foreach (GroupMemberView m in rustBridge.ListGroupMembers(Arg(0)))
                {
                    members.AppendLine($"member\t{m.MemberPub}\t{m.DisplayName}\t{m.Role}\t{m.Status}\t{(m.IsMe ? "me" : string.Empty)}	photo={(m.AvatarPath is { } photo && File.Exists(photo) ? "yes" : "no")}");
                }
                return members.ToString().TrimEnd();
            }

            case "group.remove":
                rustBridge.RemoveGroupMember(Arg(0), Arg(1));
                return "ok";

            case "group.admin":
                rustBridge.SetGroupAdmin(Arg(0), Arg(1), Arg(2) != "off");
                return "ok";

            case "group.rename":
                rustBridge.RenameGroup(Arg(0), string.Join(' ', args.Skip(1)));
                return "ok";

            case "group.leave":
                rustBridge.LeaveGroup(Arg(0));
                RefreshChatsList(force: true);
                return "ok";

            case "group.delete":
                rustBridge.DeleteGroupForEveryone(Arg(0));
                RefreshChatsList(force: true);
                return "ok";

            // The conversation's call button: starts a call, or joins the one going on.
            case "group.call":
                if (!OpenGroupForUiTest(Arg(0)))
                {
                    return "error: no such group row";
                }
                await GroupConversation.StartOrJoinCallAsync();
                await Task.Delay(400);
                return "ok" + Environment.NewLine + BuildUiTestGroupDump();

            // The group's page (a tap on the header): group.info [off].
            case "group.info":
                GroupConversation.SetInfoOpen(Arg(0) != "off");
                return "ok";

            case "group.fold":
                GroupConversation.FoldCallForUiTest(fold: Arg(0) != "off");
                return "ok";

            case "group.hangup":
                if (!OpenGroupForUiTest(Arg(0)))
                {
                    return "error: no such group row";
                }
                await GroupConversation.LeaveCallAsync();
                return "ok";

            // The ring bar's buttons.
            case "group.ring.join":
                if (!GroupCallRingBar.IsOpen)
                {
                    return "error: nothing is ringing";
                }
                OnGroupCallRingJoinClicked(this, new RoutedEventArgs());
                await Task.Delay(800);
                return "ok" + Environment.NewLine + BuildUiTestGroupDump();

            case "group.ring.decline":
                if (!GroupCallRingBar.IsOpen)
                {
                    return "error: nothing is ringing";
                }
                DismissRingingGroupCall();
                return "ok";

            default:
                return null;
        }
    }

    // In a group call the conversation's own buttons announce mic/camera/screen state to the others;
    // the channel's camera/pattern commands drive the media service directly, so they announce too —
    // otherwise remote tiles keep the "camera off" badge and hide the very video under test.
    private void AnnounceGroupMediaStateForUiTest()
    {
        if (callVideoService.IsGroupMediaActive)
        {
            GroupConversation.BroadcastSelfMediaState();
        }
    }

    private bool OpenGroupForUiTest(string groupId)
    {
        ShowChatsSection();
        RefreshChatsList(force: true);
        ChatThreadListItem? row = chatThreadItems.FirstOrDefault(i => i.GroupId == groupId && !i.IsGroupInvite);
        if (row is null)
        {
            return false;
        }
        if (GroupConversation.CurrentGroupId != groupId)
        {
            WithSelectionSyncSuppressed(ref suppressChatSelectionSync, () => ChatsListView.SelectedItem = row);
            OpenGroupRow(row);
        }
        return true;
    }

    // Groups, invitations, calls and the ring bar as tab-separated lines.
    private string BuildUiTestGroupDump()
    {
        StringBuilder dump = new("ok");
        dump.AppendLine();
        foreach (GroupSummary g in rustBridge.ListGroups())
        {
            dump.AppendLine($"group\t{g.GroupId}\t{g.Name}\tstatus={g.MyStatus}\trole={g.MyRole}\tmembers={g.ActiveMemberCount}\tepoch={g.Epoch}\tlast={g.LastMessagePreview}");
        }
        foreach (GroupInviteView i in rustBridge.ListGroupInvites())
        {
            dump.AppendLine($"invite\t{i.GroupId}\t{i.Name}\tfrom={i.InviterName}\tstatus={i.Status}");
        }
        foreach (GroupCallNotice c in rustBridge.ListGroupCallInbox())
        {
            dump.AppendLine($"call\t{c.GroupId}\t{c.CallId}\tparticipants={c.ParticipantCount}\tringing={c.Ringing}\tjoined={c.Joined}\tby={c.StartedByName}");
            GroupCallSnapshot? snapshot = c.Joined ? rustBridge.GetGroupCallSnapshot(c.GroupId) : null;
            if (snapshot is not null)
            {
                dump.AppendLine($"callstate\t{snapshot.Topology}\trole={snapshot.TreeRole}\tmembers={snapshot.Members.Count}\tmedia={(callVideoService.IsGroupMediaActive ? "on" : "off")}\tmuted={callVideoService.IsLocalAudioMuted}");
                // Remote screen control: who may control OUR screen, and who lets US control theirs.
                ScreenControlGrant? grant = snapshot.MyScreenControl;
                dump.AppendLine($"screencontrol\tmine={(grant is null ? "none" : grant.Everyone ? "all" : string.Join(",", grant.Members ?? Array.Empty<string>()))}" +
                    $"\tlets_me={string.Join(",", snapshot.Members.Where(m => m.LetsMeControl).Select(m => m.MemberPub))}");
            }
        }
        dump.AppendLine($"ringbar\topen={GroupCallRingBar.IsOpen}\t{GroupCallRingBar.Title}");
        dump.AppendLine($"opengroup\t{GroupConversation.CurrentGroupId ?? "none"}");
        return dump.ToString().TrimEnd();
    }
}
