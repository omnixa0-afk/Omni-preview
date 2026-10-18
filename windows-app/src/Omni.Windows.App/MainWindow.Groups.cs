using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Groups;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

// Groups live in the chat list next to 1:1 dialogs (Telegram-style): a row per group we're in and
// per open invitation; selecting a group swaps the conversation pane to GroupConversationControl.
// Groups run on MLS in the core — docs/decisions/group-crypto-mls.md.
public sealed partial class MainWindow
{
    // The chat-list key of the open group row (its dialog key), or null when a 1:1 dialog / nothing
    // is open. Keeps the group row selected across list refreshes.
    private string? currentGroupRowKey;

    private const string GroupInviteRowPrefix = "group-invite:";

    private Button NewGroupButton => ResolveSectionElement<Button>(ChatsSectionHost, nameof(NewGroupButton));

    private GroupConversationControl GroupConversation => ChatsSectionHost.GroupConversation;
    private GroupInviteCard GroupInvite => ChatsSectionHost.GroupInviteCard;

    // The group whose call is ringing in GroupCallRingBar, or null.
    private string? ringingGroupId;

    private void WireGroupConversation()
    {
        NewGroupButton.Click += OnNewGroupClicked;
        GroupCallRingJoinButton.Click += OnGroupCallRingJoinClicked;
        GroupCallRingDeclineButton.Click += (_, _) => DismissRingingGroupCall();
        GroupCallRingBar.CloseButtonClick += (_, _) => DismissRingingGroupCall();
        GroupConversationControl group = GroupConversation;
        group.Attach(rustBridge);
        group.PickPeople = PickPeopleForGroupAsync;
        group.ResolveMember = ResolveMember;
        group.NoteMemberCards = NoteGroupMemberCards;
        group.GroupListChanged += () => RefreshChatsList();
        GroupInvite.Attach(rustBridge);
        // Joined from the card: the invitation row turns into the group row — open it.
        GroupInvite.Joined += groupId =>
        {
            CloseGroupConversation();
            GroupSummary? joined = rustBridge.ListGroups().FirstOrDefault(g => g.GroupId == groupId);
            RefreshChatsList(force: true, selectDialogKey: joined?.DialogKey);
            ChatThreadListItem? row = joined is null ? null : chatThreadItems.FirstOrDefault(i => i.DialogKey == joined.DialogKey);
            if (row is not null)
            {
                OpenGroupRow(row);
            }
        };
        GroupInvite.Declined += _ =>
        {
            CloseGroupConversation(clearSelection: true);
            RefreshChatsList(force: true);
        };
        group.PickImageFile = () => ShowDesktopOpenFileDialog(
            "Аватарка группы",
            [
                new DesktopFileDialogFilter("Изображения (*.jpg;*.jpeg;*.png;*.bmp)", "*.jpg;*.jpeg;*.png;*.bmp"),
                new DesktopFileDialogFilter("Все файлы (*.*)", "*.*"),
            ],
            Environment.GetFolderPath(Environment.SpecialFolder.MyPictures));
        group.PickAnyFile = () => ShowDesktopOpenFileDialog(
            "Выберите файл",
            [new DesktopFileDialogFilter("Все файлы (*.*)", "*.*")],
            Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments));
        group.PhotoOpenRequested += (path, caption, fileName) => OpenImageViewer(path, caption, fileName);
        group.PhotoSaveRequested += (path, fileName) => SaveViewedImageAs(path, fileName);
        group.FileActionsRequested += (anchor, messageId, attachment) => ShowAttachmentActions(anchor, messageId, attachment);
        group.GroupLeft += _ =>
        {
            CloseGroupConversation(clearSelection: true);
            RefreshChatsList(force: true);
        };

        // The native media pipeline for the group call (the same CallVideoService the 1:1 call uses).
        group.StartGroupMedia = groupDialogKey => callVideoService.StartGroupMediaAsync(groupDialogKey);
        group.StopGroupMedia = () => callVideoService.StopAsync();
        // Each participant tile hands its video Image to the media service (self = local camera
        // preview, remote = that sender's demuxed stream) via Image.Loaded.
        group.RegisterTileVideo = (tileKey, image) => callVideoService.RegisterGroupTileImage(tileKey, image);
        group.ReadTileSpeakingLevel = tileKey => callVideoService.ReadGroupTileSpeakingLevel(tileKey);
        // Audio-first opt-in camera — the same entry points the 1:1 overlay's camera button uses.
        group.ToggleGroupCamera = () => callVideoService.IsLocalCameraActive
            ? callVideoService.StopLocalCameraAsync()
            : callVideoService.StartLocalCameraAsync();
        group.IsGroupCameraActive = () => callVideoService.IsLocalCameraActive;
        // Screen share: the system picker chooses a monitor/window, then the video source swaps
        // camera -> screen. The picker anchors to this window's HWND.
        group.IsGroupScreenSharing = () => callVideoService.IsScreenSharing;
        group.StartGroupScreenShare = async () =>
        {
            IntPtr hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
            var item = await Features.Calls.ScreenShareCaptureSource.PickAsync(hwnd);
            if (item is not null)
            {
                await callVideoService.StartScreenShareAsync(item);
            }
        };
        group.StopGroupScreenShare = () => callVideoService.StopScreenShareAsync();
        // Remote screen control (docs/decisions/remote-screen-control.md): the same injector and
        // viewer the 1:1 call uses; a group tile's video Image is the viewer's input surface.
        group.IsGroupScreenMonitorShared = () => callVideoService.SharedMonitor is not null;
        group.PrepareScreenControlGrant = InstallScreenControlSink;
        group.StartGroupScreenControl = (dialogKey, sharer, image) =>
        {
            WireRemoteScreenControlSurface(image);
            StartRemoteScreenControl(dialogKey, sharer, image);
        };
        group.StopGroupScreenControl = StopRemoteScreenControl;
        group.ControlledGroupSharer = () => screenControlViewer?.Sharer;
        group.RebindGroupScreenControlSurface = image =>
        {
            WireRemoteScreenControlSurface(image);
            screenControlSurface = image;
        };
        group.SetGroupMuted = muted => callVideoService.SetLocalAudioMuted(muted);
        group.IsGroupMuted = () => callVideoService.IsLocalAudioMuted;
    }

    private IEnumerable<ChatThreadListItem> BuildGroupThreadItems(ChatGroupRows rows)
    {
        Dictionary<string, GroupCallNotice> calls = rows.Calls.ToDictionary(c => c.GroupId, StringComparer.Ordinal);
        foreach (GroupSummary group in rows.Groups.Where(g => g.MyStatus == "member"))
        {
            long sortUnixMs = group.LastActivityUnixMs;
            // A call on in the group: say so on the row and lift the row to the top.
            calls.TryGetValue(group.GroupId, out GroupCallNotice? call);
            string callText = call is null
                ? string.Empty
                : call.Joined ? "● В звонке" : $"📞 Звонок · {call.ParticipantCount}";
            if (call is not null)
            {
                sortUnixMs = Math.Max(sortUnixMs, call.StartedAtUnixMs);
            }
            ChatThreadListItem item = new(group.DialogKey, group.GroupId);
            var groupAvatar = MainWindowPresentation.BuildAvatarImage(group.AvatarPath, 96, group.AvatarPath);
            item.ApplyFrom(
                null,
                MainWindowPresentation.BuildAvatarText(group.Name),
                groupAvatar,
                groupAvatar is null ? Visibility.Collapsed : Visibility.Visible,
                groupAvatar is null ? Visibility.Visible : Visibility.Collapsed,
                Visibility.Collapsed,
                group.Name,
                string.IsNullOrEmpty(group.LastMessagePreview) ? "Группа создана" : group.LastMessagePreview,
                callText,
                sortUnixMs > 0 ? MainWindowPresentation.FormatMessageTime(sortUnixMs) : string.Empty,
                sortUnixMs,
                $"group:{group.GroupId}:{group.Name}:{group.ActiveMemberCount}:{group.LastActivityUnixMs}:{group.LastMessagePreview}:{callText}:{group.AvatarPath}:{group.UnreadCount}",
                nextUnreadCount: group.UnreadCount);
            yield return item;
        }

        HashSet<string> joined = rows.Groups
            .Where(g => g.MyStatus == "member")
            .Select(g => g.GroupId)
            .ToHashSet(StringComparer.Ordinal);
        foreach (GroupInviteView invite in rows.Invites.Where(i => !joined.Contains(i.GroupId)))
        {
            bool joining = invite.Status == "accepted";
            ChatThreadListItem item = new(GroupInviteRowPrefix + invite.GroupId, invite.GroupId, isGroupInvite: true);
            item.ApplyFrom(
                null,
                MainWindowPresentation.BuildAvatarText(invite.Name),
                null,
                Visibility.Collapsed,
                Visibility.Visible,
                Visibility.Collapsed,
                invite.Name,
                joining ? "Вступаю — ждём ответа участника группы…" : $"Приглашение от {invite.InviterName}",
                joining ? "Вступаю" : "Новое",
                MainWindowPresentation.FormatMessageTime(invite.ReceivedAtUnixMs),
                invite.ReceivedAtUnixMs,
                $"invite:{invite.GroupId}:{invite.Name}:{invite.InviterName}:{invite.Status}");
            yield return item;
        }
    }

    // A group row opens its conversation; an invitation row opens the invitation card (Telegram-like:
    // the answer is given in the pane, not in a modal over the list).
    private void OpenGroupRow(ChatThreadListItem item)
    {
        currentGroupRowKey = item.DialogKey;
        if (item.IsGroupInvite)
        {
            ChatsSectionHost.ShowGroupPane(ChatsSectionControl.GroupPane.Invite);
            GroupInvite.Show(item.GroupId!);
            return;
        }

        ChatsSectionHost.ShowGroupPane(ChatsSectionControl.GroupPane.Conversation);
        CloseChatContactInfo();
        GroupConversation.ShowGroup(item.GroupId!);
        ReadChatOnScreen();
    }

    private void CloseGroupConversation(bool clearSelection = false)
    {
        if (currentGroupRowKey is null)
        {
            return;
        }

        currentGroupRowKey = null;
        ChatsSectionHost.ShowGroupPane(ChatsSectionControl.GroupPane.None);
        if (clearSelection)
        {
            WithSelectionSyncSuppressed(ref suppressChatSelectionSync, () => ChatsListView.SelectedItem = null);
        }
    }

    // Show the first ringing group call over the current section (or hide the bar).
    private void UpdateGroupCallRing(IReadOnlyList<GroupCallNotice> calls)
    {
        GroupCallNotice? ringing = calls.FirstOrDefault(c => c.Ringing);
        ringingGroupId = ringing?.GroupId;
        if (ringing is null)
        {
            GroupCallRingBar.IsOpen = false;
            return;
        }

        GroupCallRingBar.Title = string.IsNullOrWhiteSpace(ringing.StartedByName)
            ? $"Звонок в «{ringing.GroupName}»"
            : $"{ringing.StartedByName} звонит в «{ringing.GroupName}»";
        GroupCallRingBar.Message = ringing.ParticipantCount > 1 ? $"в звонке: {ringing.ParticipantCount}" : string.Empty;
        GroupCallRingBar.IsOpen = true;
    }

    private void DismissRingingGroupCall()
    {
        string? groupId = ringingGroupId;
        GroupCallRingBar.IsOpen = false;
        ringingGroupId = null;
        if (groupId is null)
        {
            return;
        }

        try
        {
            rustBridge.DismissGroupCall(groupId);
        }
        catch (Exception ex)
        {
            _ = ShowGroupErrorAsync("Не удалось отклонить звонок", ex);
        }
    }

    // "Присоединиться": open the group in Chats and join its call (the conversation's call button path).
    private async void OnGroupCallRingJoinClicked(object sender, RoutedEventArgs e)
    {
        string? groupId = ringingGroupId;
        GroupCallRingBar.IsOpen = false;
        ringingGroupId = null;
        if (groupId is null)
        {
            return;
        }

        ShowChatsSection();
        GroupSummary? group = rustBridge.ListGroups().FirstOrDefault(g => g.GroupId == groupId);
        if (group is null)
        {
            return;
        }
        RefreshChatsList(force: true, selectDialogKey: group.DialogKey);
        ChatThreadListItem? row = chatThreadItems.FirstOrDefault(i => i.DialogKey == group.DialogKey);
        if (row is null)
        {
            return;
        }
        OpenGroupRow(row);
        await GroupConversation.StartOrJoinCallAsync();
        // The bar was computed before we joined — let the list and the bar catch up now.
        RefreshChatsList(force: true);
    }

    // "+ Группа" — two steps, like Telegram: people first (optional), then the name.
    private async void OnNewGroupClicked(object sender, RoutedEventArgs e)
    {
        GroupPeoplePickerModel model = new(BuildGroupPickCandidates(groupId: null));
        GroupPeoplePicker picker = new(model);
        TextBox nameBox = new() { Header = "Название группы", PlaceholderText = "Например, «Семья»", MaxLength = 64 };
        TextBlock whoLine = new() { TextWrapping = TextWrapping.Wrap, Opacity = 0.75 };
        // Same size as the people step, so switching steps doesn't resize the dialog.
        StackPanel nameStep = new() { Width = GroupPeoplePicker.BodyWidth, Height = GroupPeoplePicker.BodyHeight, Spacing = 12 };
        nameStep.Children.Add(nameBox);
        nameStep.Children.Add(whoLine);

        ContentDialog dialog = new()
        {
            XamlRoot = Content.XamlRoot,
            Title = "Новая группа",
            Content = picker,
            PrimaryButtonText = GroupPeoplePickerModel.NextButtonText(0),
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
        };
        bool onNameStep = false;
        void ShowNameStep(bool show)
        {
            onNameStep = show;
            dialog.Content = show ? nameStep : picker;
            dialog.SecondaryButtonText = show ? "Назад" : string.Empty;
            if (show)
            {
                dialog.PrimaryButtonText = "Создать";
                dialog.IsPrimaryButtonEnabled = nameBox.Text.Trim().Length > 0;
                whoLine.Text = model.Selected.Count == 0
                    ? "Пока в группе будете только вы — пригласить людей можно и потом."
                    : $"Получат приглашение: {string.Join(", ", model.Selected.Select(c => c.Title))}.";
                nameBox.Focus(FocusState.Programmatic);
            }
            else
            {
                dialog.PrimaryButtonText = GroupPeoplePickerModel.NextButtonText(model.Selected.Count);
                dialog.IsPrimaryButtonEnabled = true;
            }
        }
        model.SelectionChanged += () =>
        {
            if (!onNameStep)
            {
                dialog.PrimaryButtonText = GroupPeoplePickerModel.NextButtonText(model.Selected.Count);
            }
        };
        nameBox.TextChanged += (_, _) =>
        {
            if (onNameStep)
            {
                dialog.IsPrimaryButtonEnabled = nameBox.Text.Trim().Length > 0;
            }
        };
        dialog.PrimaryButtonClick += (_, args) =>
        {
            if (!onNameStep)
            {
                args.Cancel = true;
                ShowNameStep(true);
            }
        };
        dialog.SecondaryButtonClick += (_, args) =>
        {
            args.Cancel = true;
            ShowNameStep(false);
        };

        if (await dialog.ShowAsync() != ContentDialogResult.Primary || !onNameStep)
        {
            return;
        }

        try
        {
            GroupSummary created = rustBridge.CreateGroup(nameBox.Text.Trim());
            if (model.Selected.Count > 0)
            {
                rustBridge.InviteToGroup(created.GroupId, model.SelectedDialogKeys);
            }
            RefreshChatsList(force: true, selectDialogKey: created.DialogKey);
            ChatThreadListItem? row = chatThreadItems.FirstOrDefault(i => i.DialogKey == created.DialogKey);
            if (row is not null)
            {
                OpenGroupRow(row);
            }
        }
        catch (Exception ex)
        {
            await ShowGroupErrorAsync("Не удалось создать группу", ex);
        }
    }

    // "Добавить участников" in an open group: the same picker, members and pending invitees greyed out.
    private async Task<IReadOnlyList<string>?> PickPeopleForGroupAsync(string groupId, string groupName)
    {
        GroupPeoplePickerModel model = new(BuildGroupPickCandidates(groupId));
        ContentDialog dialog = new()
        {
            XamlRoot = Content.XamlRoot,
            Title = $"Добавить в «{groupName}»",
            Content = new GroupPeoplePicker(model),
            PrimaryButtonText = GroupPeoplePickerModel.InviteButtonText(0),
            IsPrimaryButtonEnabled = false,
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
        };
        model.SelectionChanged += () =>
        {
            dialog.PrimaryButtonText = GroupPeoplePickerModel.InviteButtonText(model.Selected.Count);
            dialog.IsPrimaryButtonEnabled = model.Selected.Count > 0;
        };
        return await dialog.ShowAsync() == ContentDialogResult.Primary ? model.SelectedDialogKeys : null;
    }

    // Approved contacts with a dialog — the people a group can invite (consent goes over 1:1). The
    // avatar and presence dot come from the contact's chat row, so they look the same as in the list.
    private List<GroupPickCandidate> BuildGroupPickCandidates(string? groupId)
    {
        HashSet<string> memberAccounts = new(StringComparer.Ordinal);
        HashSet<string> invitedDialogs = new(StringComparer.Ordinal);
        if (groupId is not null)
        {
            memberAccounts.UnionWith(rustBridge.ListGroupMembers(groupId)
                .Where(m => m.Status == "member" && !string.IsNullOrEmpty(m.AccountId))
                .Select(m => m.AccountId));
            invitedDialogs.UnionWith(rustBridge.ListGroupSentInvites(groupId).Select(i => i.DialogKey));
        }

        Dictionary<string, ChatThreadListItem> rows = chatThreadItems
            .Where(i => !i.IsGroup)
            .ToDictionary(i => i.DialogKey, StringComparer.Ordinal);
        List<GroupPickCandidate> candidates = new();
        foreach (ContactSnapshot contact in rustBridge.ListChatThreadProjections().Select(p => p.Contact))
        {
            if (contact.RemovedAtUnixMs.HasValue
                || !string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase)
                || ResolveChatDialogKey(contact) is not string dialogKey
                || string.IsNullOrWhiteSpace(dialogKey))
            {
                continue;
            }
            string title = MainWindowPresentation.ContactTitle(contact);
            string accountId = contact.RemoteAccountId ?? string.Empty;
            rows.TryGetValue(dialogKey, out ChatThreadListItem? row);
            candidates.Add(new GroupPickCandidate
            {
                DialogKey = dialogKey,
                AccountId = accountId,
                Title = title,
                Initials = MainWindowPresentation.BuildAvatarText(title),
                State = GroupPeoplePickerModel.StateFor(dialogKey, accountId, memberAccounts, invitedDialogs),
                AvatarImage = row?.AvatarImage,
                AvatarImageVisibility = row?.AvatarImageVisibility ?? Visibility.Collapsed,
                PresenceDotVisibility = row?.PresenceDotVisibility ?? Visibility.Collapsed,
                PresenceDotBrush = row?.PresenceDotBrush,
            });
        }
        return candidates;
    }

    private async Task ShowGroupErrorAsync(string what, Exception ex)
    {
        ContentDialog dialog = new()
        {
            XamlRoot = Content.XamlRoot,
            Title = what,
            Content = NormalizeUserError(ex),
            CloseButtonText = "Закрыть",
        };
        await dialog.ShowAsync();
    }
}
